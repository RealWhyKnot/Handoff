use std::io;

use zstd::stream::raw::{CParameter, DParameter, Decoder, Encoder, InBuffer, Operation, OutBuffer};

pub const HEADER: &str = "X-Handoff-Compression";
pub const ZSTD: &str = "zstd";
pub const WINDOW_LOG: u32 = 20;
pub const LEVEL: i32 = 9;

const CHUNK: usize = 64 << 10;

pub fn offers_zstd(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(ZSTD)))
}

pub struct Packer {
    enc: Encoder<'static>,
}

impl Packer {
    pub fn new(level: i32) -> io::Result<Packer> {
        let mut enc = Encoder::new(level)?;
        enc.set_parameter(CParameter::WindowLog(WINDOW_LOG))?;
        Ok(Packer { enc })
    }

    pub fn pack(&mut self, msg: &[u8]) -> io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(msg.len() / 3 + 64);
        let mut input = InBuffer::around(msg);
        while input.pos() < msg.len() {
            out.reserve(CHUNK);
            let at = out.len();
            self.enc
                .run(&mut input, &mut OutBuffer::around_pos(&mut out, at))?;
        }
        loop {
            out.reserve(CHUNK);
            let at = out.len();
            if self.enc.flush(&mut OutBuffer::around_pos(&mut out, at))? == 0 {
                return Ok(out);
            }
        }
    }
}

pub struct Unpacker {
    dec: Decoder<'static>,
    max: usize,
}

impl Unpacker {
    pub fn new(max: usize) -> io::Result<Unpacker> {
        let mut dec = Decoder::new()?;
        dec.set_parameter(DParameter::WindowLogMax(WINDOW_LOG))?;
        Ok(Unpacker { dec, max })
    }

    pub fn unpack(&mut self, chunk: &[u8]) -> io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity((chunk.len() * 4).clamp(256, CHUNK));
        let mut input = InBuffer::around(chunk);
        loop {
            if out.capacity() - out.len() < CHUNK / 4 {
                out.reserve(CHUNK);
            }
            let before = out.len();
            let room = out.capacity() - before;
            self.dec
                .run(&mut input, &mut OutBuffer::around_pos(&mut out, before))?;
            let filled = out.len() - before;
            if out.len() > self.max {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("compressed message is over {} bytes", self.max),
                ));
            }
            if input.pos() == chunk.len() && filled < room {
                return Ok(out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_comes_back_whole_and_later_ones_shrink() {
        let mut p = Packer::new(15).unwrap();
        let mut u = Unpacker::new(8 << 20).unwrap();
        let first =
            br#"{"ts":1,"kind":"command_result","payload":{"id":"c_1","ok":true,"rows":[1,2,3]}}"#;
        let second =
            br#"{"ts":2,"kind":"command_result","payload":{"id":"c_2","ok":true,"rows":[1,2,3]}}"#;
        let a = p.pack(first).unwrap();
        let b = p.pack(second).unwrap();
        assert_eq!(u.unpack(&a).unwrap(), first);
        assert_eq!(u.unpack(&b).unwrap(), second);
        assert!(b.len() < a.len() / 2, "{} vs {}", b.len(), a.len());
    }

    #[test]
    fn large_messages_round_trip() {
        let big: Vec<u8> = (0..3_000_000u32)
            .flat_map(|i| (i % 251).to_le_bytes())
            .collect();
        let mut p = Packer::new(3).unwrap();
        let mut u = Unpacker::new(64 << 20).unwrap();
        for _ in 0..2 {
            let z = p.pack(&big).unwrap();
            assert_eq!(u.unpack(&z).unwrap(), big);
        }
        assert_eq!(u.unpack(&p.pack(b"").unwrap()).unwrap(), b"");
    }

    #[test]
    fn oversized_output_is_refused() {
        let mut p = Packer::new(3).unwrap();
        let mut u = Unpacker::new(1000).unwrap();
        let z = p.pack(&vec![b'a'; 100_000]).unwrap();
        assert!(u.unpack(&z).is_err());
    }

    #[test]
    fn header_offer_parsing() {
        assert!(offers_zstd(Some("zstd")));
        assert!(offers_zstd(Some("br, ZSTD")));
        assert!(!offers_zstd(Some("gzip")));
        assert!(!offers_zstd(None));
    }
}
