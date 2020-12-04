use crate::Version;
use bytes::BytesMut;
use futures::prelude::*;
use linkerd2_error::Error;
use linkerd2_io::{self as io, AsyncReadExt};
use linkerd2_proxy_transport::{Detect, NewDetectService};
use linkerd2_stack::layer;
use tokio::time;
use tracing::{debug, trace};

const BUFFER_CAPACITY: usize = 8192;
const H2_PREFACE: &'static [u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const H2_FIRST_LINE_LEN: usize = 16;

#[derive(Clone, Debug)]
pub struct DetectHttp {
    capacity: usize,
    timeout: time::Duration,
}

impl DetectHttp {
    pub fn new(timeout: time::Duration) -> Self {
        Self {
            timeout,
            capacity: BUFFER_CAPACITY,
        }
    }

    pub fn layer<N>(
        timeout: time::Duration,
    ) -> impl layer::Layer<N, Service = NewDetectService<N, Self>> + Clone {
        NewDetectService::layer(Self::new(timeout))
    }
}

#[async_trait::async_trait]
impl<I> Detect<I> for DetectHttp
where
    I: io::AsyncRead + Send + Unpin + 'static,
{
    type Kind = Option<Version>;

    async fn detect(&self, mut io: I) -> Result<(Option<Version>, io::PrefixedIo<I>), Error> {
        let mut buf = BytesMut::with_capacity(self.capacity);
        let mut i = 0;
        let mut maybe_h2 = true;

        let mut timeout = time::sleep(self.timeout).fuse();
        loop {
            trace!(capacity = buf.capacity() - i, "Reading");
            let sz = futures::select_biased! {
                res = io.read_buf(&mut buf).fuse() => res?,
                _ = (&mut timeout) => {
                    debug!(ms = %self.timeout.as_millis(), "Read timeout");
                    0
                }
            };

            if sz == 0 {
                debug!(read = buf.len(), "Could not detect protocol");
                return Ok((None, io::PrefixedIo::new(buf.freeze(), io)));
            }

            trace!(
                buf = buf.len(),
                h2 = H2_PREFACE.len(),
                "Checking H2 preface"
            );
            if maybe_h2 && buf.len() >= H2_PREFACE.len() {
                if &buf[..H2_PREFACE.len()] == H2_PREFACE {
                    trace!("Matched HTTP/2 prefix");
                    return Ok((Some(Version::H2), io::PrefixedIo::new(buf.freeze(), io)));
                } else {
                    maybe_h2 = false;
                }
            }

            for j in i..(buf.len() - 1) {
                // If we've reached the end of a line, we have enough information to know whether
                // the protocol is HTTP/1.1 or not.
                if &buf[j..j + 2] == b"\r\n" {
                    trace!(offset = j, "Found newline");
                    if !is_h2_first_line(&buf[..j + 2]) {
                        trace!("Atempt to parse HTTP/1 message");
                        let mut p = httparse::Request::new(&mut [httparse::EMPTY_HEADER; 0]);
                        // Check whether the first line looks like HTTP/1.1.
                        let kind = match p.parse(&buf[..]) {
                            Ok(_) | Err(httparse::Error::TooManyHeaders) => {
                                trace!("Matched HTTP/1");
                                Some(Version::Http1)
                            }
                            Err(_) => {
                                trace!("Unknown protocol");
                                None
                            }
                        };
                        return Ok((kind, io::PrefixedIo::new(buf.freeze(), io)));
                    } else if !maybe_h2 {
                        trace!("Unknown protocol");
                        return Ok((None, io::PrefixedIo::new(buf.freeze(), io)));
                    }
                }
            }

            i += sz - 1;
            if buf[i] == b'\r' {
                i -= 1;
            }
            trace!(offset = %i, "Continuing to read");
        }
    }
}

fn is_h2_first_line(line: &[u8]) -> bool {
    line.len() == H2_FIRST_LINE_LEN && line == &H2_PREFACE[..H2_FIRST_LINE_LEN]
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BufMut;
    use tokio_test::io;

    const HTTP11_LINE: &'static [u8] = b"GET / HTTP/1.1\r\n";
    const TIMEOUT: time::Duration = time::Duration::from_secs(10);

    #[tokio::test]
    async fn h2() {
        let (kind, _) = DetectHttp::new(TIMEOUT)
            .detect(io::Builder::new().read(H2_PREFACE).build())
            .await
            .unwrap();
        assert_eq!(kind, Some(Version::H2));

        let mut buf = BytesMut::with_capacity(H2_PREFACE.len() * 2);
        buf.put(H2_PREFACE);
        buf.put(H2_PREFACE);
        let (kind, _) = DetectHttp::new(TIMEOUT)
            .detect(io::Builder::new().read(&buf[..]).build())
            .await
            .unwrap();
        assert_eq!(kind, Some(Version::H2));

        let (kind, _) = DetectHttp::new(TIMEOUT)
            .detect(
                io::Builder::new()
                    .read(&H2_PREFACE[0..H2_FIRST_LINE_LEN])
                    .read(&H2_PREFACE[H2_FIRST_LINE_LEN..])
                    .build(),
            )
            .await
            .unwrap();
        assert_eq!(kind, Some(Version::H2));
    }

    #[tokio::test]
    async fn http1() {
        let (kind, io) = DetectHttp::new(TIMEOUT)
            .detect(io::Builder::new().read(HTTP11_LINE).build())
            .await
            .unwrap();
        assert_eq!(kind, Some(Version::Http1));
        assert_eq!(io.prefix(), HTTP11_LINE);

        let (kind, io) = DetectHttp::new(TIMEOUT)
            .detect(
                io::Builder::new()
                    .read(&HTTP11_LINE[..16])
                    .read(&HTTP11_LINE[16..])
                    .build(),
            )
            .await
            .unwrap();
        assert_eq!(kind, Some(Version::Http1));
        assert_eq!(io.prefix(), HTTP11_LINE);

        const REQ: &'static [u8] =
            b"GET /foo/bar/bar/blah HTTP/1.1\r\nHost: foob.example.com\r\n\r\n";
        let (kind, io) = DetectHttp::new(TIMEOUT)
            .detect(io::Builder::new().read(&REQ).build())
            .await
            .unwrap();
        assert_eq!(kind, Some(Version::Http1));
        assert_eq!(io.prefix(), REQ);
    }

    #[tokio::test]
    async fn unknown() {
        let (kind, io) = DetectHttp::new(TIMEOUT)
            .detect(io::Builder::new().read(b"foo.bar.blah\n").build())
            .await
            .unwrap();
        assert_eq!(kind, None);
        assert_eq!(&io.prefix()[..], b"foo.bar.blah\n");

        let (kind, io) = DetectHttp::new(TIMEOUT)
            .detect(
                io::Builder::new()
                    .read(&HTTP11_LINE[..14])
                    .read(b"\n")
                    .build(),
            )
            .await
            .unwrap();
        assert_eq!(kind, None);
        assert_eq!(&io.prefix()[..14], &HTTP11_LINE[..14]);
        assert_eq!(&io.prefix()[14..], b"\n");
    }

    #[tokio::test]
    async fn timeout() {
        let (io, _handle) = io::Builder::new().read(b"GET").build_with_handle();
        let (kind, io) = DetectHttp::new(time::Duration::from_millis(1))
            .detect(io)
            .await
            .unwrap();
        assert_eq!(kind, None);
        assert_eq!(&io.prefix()[..], b"GET");
    }
}
