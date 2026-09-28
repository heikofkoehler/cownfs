//! XDR (RFC 4506) encode/decode primitives. Big-endian, 4-byte units,
//! length-prefixed variable data padded to a 4-byte boundary.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XdrError {
    /// Truncated input.
    Truncated,
    /// Value out of range for the target type.
    OutOfRange,
    /// Trailing bytes after a complete value.
    Trailing,
    /// Invalid boolean or enum discriminant.
    Invalid(&'static str),
}

/// Sequential XDR decoder over a byte slice.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], XdrError> {
        if self.pos + n > self.buf.len() {
            return Err(XdrError::Truncated);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn u32(&mut self) -> Result<u32, XdrError> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes(b.try_into().unwrap()))
    }

    pub fn i32(&mut self) -> Result<i32, XdrError> {
        Ok(self.u32()? as i32)
    }

    pub fn u64(&mut self) -> Result<u64, XdrError> {
        let b = self.take(8)?;
        Ok(u64::from_be_bytes(b.try_into().unwrap()))
    }

    pub fn i64(&mut self) -> Result<i64, XdrError> {
        Ok(self.u64()? as i64)
    }

    pub fn bool(&mut self) -> Result<bool, XdrError> {
        match self.u32()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(XdrError::Invalid("bool")),
        }
    }

    /// Variable-length opaque: u32 length, then bytes padded to 4.
    pub fn opaque(&mut self) -> Result<&'a [u8], XdrError> {
        let len = self.u32()? as usize;
        let data = self.take(len)?;
        let pad = (4 - len % 4) % 4;
        self.take(pad)?;
        Ok(data)
    }

    /// Fixed-length opaque (no length prefix, still padded).
    pub fn opaque_fixed(&mut self, len: usize) -> Result<&'a [u8], XdrError> {
        let data = self.take(len)?;
        let pad = (4 - len % 4) % 4;
        self.take(pad)?;
        Ok(data)
    }

    pub fn string(&mut self) -> Result<&'a [u8], XdrError> {
        self.opaque()
    }

    /// Ensure no trailing bytes remain.
    pub fn end(&self) -> Result<(), XdrError> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(XdrError::Trailing)
        }
    }
}

/// Sequential XDR encoder.
#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Writer { buf: Vec::new() }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn i32(&mut self, v: i32) {
        self.u32(v as u32);
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn i64(&mut self, v: i64) {
        self.u64(v as u64);
    }

    pub fn bool(&mut self, v: bool) {
        self.u32(v as u32);
    }

    fn pad_to(&mut self, len: usize) {
        let pad = (4 - len % 4) % 4;
        self.buf.extend(std::iter::repeat(0).take(pad));
    }

    pub fn opaque(&mut self, data: &[u8]) {
        self.u32(data.len() as u32);
        self.buf.extend_from_slice(data);
        self.pad_to(data.len());
    }

    pub fn opaque_fixed(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        self.pad_to(data.len());
    }

    pub fn string(&mut self, s: &[u8]) {
        self.opaque(s);
    }

    /// Append already-encoded bytes verbatim.
    pub fn raw(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_primitives() {
        let mut w = Writer::new();
        w.u32(0xdeadbeef);
        w.i32(-42);
        w.u64(0x0102030405060708);
        w.bool(true);
        w.opaque(b"hello");
        w.opaque(b"");
        w.string(b"nfs");
        let bytes = w.into_bytes();

        let mut r = Reader::new(&bytes);
        assert_eq!(r.u32().unwrap(), 0xdeadbeef);
        assert_eq!(r.i32().unwrap(), -42);
        assert_eq!(r.u64().unwrap(), 0x0102030405060708);
        assert!(r.bool().unwrap());
        assert_eq!(r.opaque().unwrap(), b"hello");
        assert_eq!(r.opaque().unwrap(), b"");
        assert_eq!(r.string().unwrap(), b"nfs");
        r.end().unwrap();
    }

    #[test]
    fn truncated_errors() {
        let mut r = Reader::new(&[0, 0]);
        assert_eq!(r.u32(), Err(XdrError::Truncated));
        let mut r = Reader::new(&[0, 0, 0, 5, b'a', b'b']);
        assert_eq!(r.opaque(), Err(XdrError::Truncated));
    }

    #[test]
    fn padding() {
        // "abc" (3 bytes) pads to 4; "abcde" (5 bytes) pads to 8.
        let mut w = Writer::new();
        w.opaque(b"abc");
        w.opaque(b"abcde");
        let bytes = w.into_bytes();
        assert_eq!(bytes.len(), 4 + 4 + 4 + 8);
        let mut r = Reader::new(&bytes);
        assert_eq!(r.opaque().unwrap(), b"abc");
        assert_eq!(r.opaque().unwrap(), b"abcde");
        r.end().unwrap();
    }
}
