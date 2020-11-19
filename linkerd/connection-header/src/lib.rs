use bytes::{
    buf::{Buf, BufMut},
    BytesMut,
};
use linkerd2_conditional::Conditional;
use linkerd2_dns_name::Name;
use linkerd2_error::Error;
use linkerd2_io::{self as io, AsyncReadExt};
use linkerd2_proxy_transport::Detect;
use prost::Message;
use std::str::FromStr;
use tokio::time;

mod proto {
    include!(concat!(env!("OUT_DIR"), "/header.proxy.l5d.io.rs"));
}

#[derive(Clone, Debug)]
pub struct Header {
    /// The target port.
    port: u16,

    /// The logical name of the target (service), if one is known.
    pub name: Option<Name>,
}

#[derive(Clone, Debug)]
pub struct DetectHeader {
    capacity: usize,
    timeout: time::Duration,
}

const PREFACE: &'static [u8] = b"proxy.l5d.io/connect\r\n\r\n";
const PREFACE_LEN: usize = PREFACE.len() + 4;
const BUFFER_CAPACITY: usize = 65_536;

#[derive(Copy, Clone, Debug)]
pub enum Reason {
    NoPreface,
    Timeout,
}

pub type ConditionalHeader = Conditional<Header, Reason>;

#[async_trait::async_trait]
impl<I: io::AsyncRead + Send + Unpin + 'static> Detect<I> for DetectHeader {
    type Kind = ConditionalHeader;

    async fn detect(&self, mut io: I) -> Result<(ConditionalHeader, io::PrefixedIo<I>), Error> {
        let mut buf = BytesMut::with_capacity(BUFFER_CAPACITY);
        let read = Header::read_prefaced(&mut io, &mut buf);
        let header = match time::timeout(self.timeout, read).await {
            Ok(res) => match res? {
                Some(h) => Conditional::Some(h),
                None => Conditional::None(Reason::NoPreface),
            },
            Err(_) => Conditional::None(Reason::Timeout),
        };
        Ok((header, io::PrefixedIo::new(buf, io)))
    }
}

impl Header {
    /// Encodes the connection header to a byte buffer.
    #[inline]
    pub fn encode_prefaced(&self, buf: &mut BytesMut) -> Result<(), Error> {
        buf.reserve(PREFACE_LEN);
        buf.put(PREFACE);

        debug_assert!(buf.capacity() > 4);
        // Safety: These bytes must be initialized below once the message has
        // been encoded.
        unsafe {
            buf.advance_mut(4);
        }

        self.encode(buf)?;

        // Once the message length is known, we back-fill the length at the
        // start of the buffer.
        let len = buf.len() - PREFACE_LEN;
        {
            let mut buf = &mut buf[PREFACE.len()..PREFACE_LEN];
            assert!(len <= std::u32::MAX as usize);
            buf.put_u32(len as u32);
        }

        Ok(())
    }

    pub fn encode(&self, buf: &mut BytesMut) -> Result<(), Error> {
        let header = proto::Header {
            port: self.port as i32,
            name: self
                .name
                .as_ref()
                .map(|n| n.to_string())
                .unwrap_or_default(),
        };
        header.encode(buf)?;
        Ok(())
    }

    /// Attempts to decode a connection header from an I/O stream.
    ///
    /// If the header is not present, the non-header bytes that were read are
    /// returned.
    ///
    /// An I/O error is returned if the connection header is invalid.
    #[inline]
    async fn read_prefaced<I: io::AsyncRead + Unpin + 'static>(
        io: &mut I,
        buf: &mut BytesMut,
    ) -> io::Result<Option<Self>> {
        // Read at least enough data to determine whether a connection header is
        // present and, if so, how long it is.
        while buf.len() < PREFACE_LEN {
            if io.read_buf(buf).await? == 0 {
                return Ok(None);
            }
        }

        // Advance the buffer past the preface if it matches.
        if &buf.bytes()[..PREFACE.len()] != PREFACE {
            return Ok(None);
        }
        buf.advance(PREFACE.len());

        // Read the message length. If it is larger than our allowed buffer
        // capacity, fail the connection.
        let msg_len = buf.get_u32() as usize;
        if msg_len > buf.capacity() + PREFACE_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Message length exceeds capacity",
            ));
        }

        // Free up parsed preface data and ensure there's enough capacity for
        // the message.
        buf.reserve(msg_len);
        while buf.len() < msg_len {
            if io.read_buf(buf).await? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Full header message not provided",
                ));
            }
        }

        // Take the bytes needed to parse the message and leave the remaining
        // bytes in the caller-provided buffer.
        let msg = buf.split_to(msg_len);
        Self::decode(msg.freeze())
    }

    // Decodes a protobuf message from the buffer.
    fn decode<B: Buf>(buf: B) -> io::Result<Option<Self>> {
        let h = proto::Header::decode(buf)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid header message"))?;

        let name = if h.name.len() == 0 {
            None
        } else {
            let n = Name::from_str(&h.name)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid name"))?;
            Some(n)
        };

        let header = Self {
            name,
            port: h.port as u16,
        };

        Ok(Some(header))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip_prefaced() {
        let header = Header {
            port: 4040,
            name: Some(Name::from_str("foo.bar.example.com").unwrap()),
        };
        let mut rx = {
            let mut buf = BytesMut::new();
            header.encode_prefaced(&mut buf).expect("must encode");
            buf.put_slice(b"12345");
            std::io::Cursor::new(buf.freeze())
        };

        let mut buf = BytesMut::new();
        let h = Header::read_prefaced(&mut rx, &mut buf)
            .await
            .expect("decodes")
            .expect("decodes");
        assert_eq!(header.port, h.port);
        assert_eq!(header.name, h.name);
        assert_eq!(buf.as_ref(), b"12345");
    }
}
