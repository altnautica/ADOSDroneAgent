//! A minimal protobuf wire codec: the varint, length-delimited and 32-bit
//! wire types the Meshtastic client API messages use (64-bit fields are
//! skipped over, groups are refused).

use std::io;
use std::ops::Range;

const WT_VARINT: u8 = 0;
const WT_I64: u8 = 1;
const WT_LEN: u8 = 2;
const WT_I32: u8 = 5;

pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_key(out: &mut Vec<u8>, field: u32, wire_type: u8) {
    put_varint(out, (u64::from(field) << 3) | u64::from(wire_type));
}

/// `uint32`, `uint64` and enum fields.
pub fn put_uint(out: &mut Vec<u8>, field: u32, v: u64) {
    put_key(out, field, WT_VARINT);
    put_varint(out, v);
}

/// `int32`: negative values are sign-extended to ten bytes.
pub fn put_int32(out: &mut Vec<u8>, field: u32, v: i32) {
    put_uint(out, field, i64::from(v) as u64);
}

pub fn put_bool(out: &mut Vec<u8>, field: u32, v: bool) {
    put_uint(out, field, u64::from(v));
}

/// `fixed32`.
pub fn put_fixed32(out: &mut Vec<u8>, field: u32, v: u32) {
    put_key(out, field, WT_I32);
    out.extend_from_slice(&v.to_le_bytes());
}

/// `sfixed32`.
pub fn put_sfixed32(out: &mut Vec<u8>, field: u32, v: i32) {
    put_fixed32(out, field, v as u32);
}

/// `float` (only the node sends floats; used to script node replies).
#[cfg(test)]
pub fn put_float(out: &mut Vec<u8>, field: u32, v: f32) {
    put_fixed32(out, field, v.to_bits());
}

/// `bytes`, `string` and embedded messages.
pub fn put_bytes(out: &mut Vec<u8>, field: u32, v: &[u8]) {
    put_key(out, field, WT_LEN);
    put_varint(out, v.len() as u64);
    out.extend_from_slice(v);
}

/// One decoded field value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value<'a> {
    Varint(u64),
    I64(u64),
    Len(&'a [u8]),
    I32(u32),
}

impl<'a> Value<'a> {
    /// `uint32` / enum (truncated like protobuf does).
    pub fn u32(self) -> Option<u32> {
        match self {
            Value::Varint(v) => Some(v as u32),
            _ => None,
        }
    }

    /// `int32` (a sign-extended varint).
    pub fn i32(self) -> Option<i32> {
        match self {
            Value::Varint(v) => Some(v as i64 as i32),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn bool(self) -> Option<bool> {
        match self {
            Value::Varint(v) => Some(v != 0),
            _ => None,
        }
    }

    /// `fixed32`.
    pub fn fixed32(self) -> Option<u32> {
        match self {
            Value::I32(v) => Some(v),
            _ => None,
        }
    }

    /// `sfixed32`.
    pub fn sfixed32(self) -> Option<i32> {
        self.fixed32().map(|v| v as i32)
    }

    /// `float`.
    pub fn f32(self) -> Option<f32> {
        self.fixed32().map(f32::from_bits)
    }

    /// `bytes`, `string` and embedded messages.
    pub fn bytes(self) -> Option<&'a [u8]> {
        match self {
            Value::Len(b) => Some(b),
            _ => None,
        }
    }
}

/// A field and the byte range it occupies (key included).
#[derive(Debug, Clone, PartialEq)]
pub struct Field<'a> {
    pub num: u32,
    pub value: Value<'a>,
    pub span: Range<usize>,
}

/// The fields of one message, in wire order.
pub struct Fields<'a> {
    buf: &'a [u8],
    pos: usize,
}

pub fn fields(buf: &[u8]) -> Fields<'_> {
    Fields { buf, pos: 0 }
}

impl<'a> Fields<'a> {
    fn field(&mut self) -> io::Result<Field<'a>> {
        let start = self.pos;
        let key = varint(self.buf, &mut self.pos)?;
        let num = u32::try_from(key >> 3).map_err(|_| bad("field number out of range"))?;
        if num == 0 {
            return Err(bad("field number 0"));
        }
        let value = match (key & 7) as u8 {
            WT_VARINT => Value::Varint(varint(self.buf, &mut self.pos)?),
            WT_I64 => Value::I64(u64::from_le_bytes(
                self.take(8)?.try_into().expect("8 bytes"),
            )),
            WT_LEN => {
                let len = varint(self.buf, &mut self.pos)?;
                let len = usize::try_from(len).map_err(|_| bad("length out of range"))?;
                Value::Len(self.take(len)?)
            }
            WT_I32 => Value::I32(u32::from_le_bytes(
                self.take(4)?.try_into().expect("4 bytes"),
            )),
            wt => return Err(bad(&format!("unsupported wire type {wt}"))),
        };
        Ok(Field {
            num,
            value,
            span: start..self.pos,
        })
    }

    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|e| *e <= self.buf.len())
            .ok_or_else(|| bad("truncated field"))?;
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = io::Result<Field<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let f = self.field();
        if f.is_err() {
            self.pos = self.buf.len();
        }
        Some(f)
    }
}

fn varint(buf: &[u8], pos: &mut usize) -> io::Result<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let b = *buf.get(*pos).ok_or_else(|| bad("truncated varint"))?;
        *pos += 1;
        v |= u64::from(b & 0x7F) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(bad("varint over ten bytes"))
}

/// `msg` re-encoded without the fields numbered in `drop`; every other field
/// is kept byte for byte.
pub fn strip(msg: &[u8], drop: &[u32]) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(msg.len());
    for f in fields(msg) {
        let f = f?;
        if !drop.contains(&f.num) {
            out.extend_from_slice(&msg[f.span]);
        }
    }
    Ok(out)
}

pub fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("protobuf: {msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_round_trip() {
        let mut b = Vec::new();
        put_uint(&mut b, 1, 300);
        put_int32(&mut b, 2, -5);
        put_bool(&mut b, 3, true);
        put_sfixed32(&mut b, 4, -123_456_789);
        put_float(&mut b, 5, 6.25);
        put_bytes(&mut b, 6, b"abc");
        put_uint(&mut b, 100, 1);
        let f: Vec<Field> = fields(&b).collect::<io::Result<_>>().unwrap();
        assert_eq!(f[0].value.u32(), Some(300));
        assert_eq!(f[1].value.i32(), Some(-5));
        assert_eq!(f[1].span.len(), 11);
        assert_eq!(f[2].value.bool(), Some(true));
        assert_eq!(f[3].value.sfixed32(), Some(-123_456_789));
        assert_eq!(f[4].value.f32(), Some(6.25));
        assert_eq!(f[5].value.bytes(), Some(&b"abc"[..]));
        assert_eq!(f[6].num, 100);
        // 300 = 0xAC 0x02 after the key 0x08.
        assert_eq!(&b[..3], &[0x08, 0xAC, 0x02]);
    }

    #[test]
    fn strip_keeps_other_fields_verbatim() {
        let mut b = Vec::new();
        put_uint(&mut b, 1, 1);
        put_uint(&mut b, 7, 10);
        put_bytes(&mut b, 103, &[1, 2]);
        let s = strip(&b, &[7]).unwrap();
        let mut want = Vec::new();
        put_uint(&mut want, 1, 1);
        put_bytes(&mut want, 103, &[1, 2]);
        assert_eq!(s, want);
    }

    #[test]
    fn truncation_and_groups_are_errors() {
        assert!(fields(&[0x0A, 0x05, 1]).next().unwrap().is_err());
        assert!(fields(&[0x0B]).next().unwrap().is_err());
        assert!(fields(&[0x08, 0x80]).next().unwrap().is_err());
    }
}
