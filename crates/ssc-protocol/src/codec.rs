//! Minimal little-endian binary codec. No external dependencies.

use std::fmt;

#[derive(Debug)]
pub struct DecodeError(pub String);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "decode error: {}", self.0)
    }
}

impl std::error::Error for DecodeError {}

pub type DecodeResult<T> = Result<T, DecodeError>;

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct ByteWriter {
    buf: Vec<u8>,
}

impl ByteWriter {
    pub fn new() -> Self {
        Self { buf: Vec::with_capacity(64) }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn put_u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn put_u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn put_i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn put_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn put_f32(&mut self, v: f32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn put_bytes(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }

    pub fn put_string(&mut self, v: &str) {
        let bytes = v.as_bytes();
        debug_assert!(bytes.len() <= u16::MAX as usize);
        self.put_u16(bytes.len() as u16);
        self.buf.extend_from_slice(bytes);
    }

    pub fn put_block_array(&mut self, blocks: &[u16]) {
        debug_assert!(blocks.len() <= u16::MAX as usize);
        self.put_u16(blocks.len() as u16);
        for b in blocks {
            self.put_u16(*b);
        }
    }
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

pub struct ByteReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    fn take(&mut self, n: usize) -> DecodeResult<&'a [u8]> {
        if self.remaining() < n {
            return Err(DecodeError("unexpected end of packet".into()));
        }
        let out = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub fn get_u8(&mut self) -> DecodeResult<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn get_u16(&mut self) -> DecodeResult<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn get_i32(&mut self) -> DecodeResult<i32> {
        let b = self.take(4)?;
        Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn get_u32(&mut self) -> DecodeResult<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn get_f32(&mut self) -> DecodeResult<f32> {
        let b = self.take(4)?;
        Ok(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn get_vec_f32(&mut self, n: usize) -> DecodeResult<Vec<f32>> {
        (0..n).map(|_| self.get_f32()).collect()
    }

    pub fn get_string(&mut self) -> DecodeResult<String> {
        let len = self.get_u16()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| DecodeError("invalid utf-8 in string".into()))
    }

    pub fn get_block_array(&mut self) -> DecodeResult<Vec<u16>> {
        let len = self.get_u16()? as usize;
        (0..len).map(|_| self.get_u16()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Ensure the packet has no trailing bytes (protocol strictness).
    pub fn expect_empty(&self) -> DecodeResult<()> {
        if !self.is_empty() {
            return Err(DecodeError("trailing bytes in packet".into()));
        }
        Ok(())
    }
}
