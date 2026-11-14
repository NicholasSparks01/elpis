//! Minimal protobuf encoding and decoding: just what the shim's messages need.

use anyhow::{Result, bail};

#[derive(Default)]
pub struct Pb(Vec<u8>);

impl Pb {
    pub fn new() -> Self {
        Self::default()
    }

    /// Strings are skipped when empty, matching proto3's default-value rule.
    pub fn str(self, field: u32, s: &str) -> Self {
        if s.is_empty() { self } else { self.bytes(field, s.as_bytes()) }
    }

    pub fn bytes(mut self, field: u32, b: &[u8]) -> Self {
        self.tag(field, 2);
        self.varint(b.len() as u64);
        self.0.extend_from_slice(b);
        self
    }

    pub fn u64(mut self, field: u32, v: u64) -> Self {
        if v != 0 {
            self.tag(field, 0);
            self.varint(v);
        }
        self
    }

    pub fn bool(self, field: u32, v: bool) -> Self {
        if v { self.u64(field, 1) } else { self }
    }

    pub fn msg(self, field: u32, m: Pb) -> Self {
        self.bytes(field, &m.0)
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }

    fn tag(&mut self, field: u32, wire: u32) {
        self.varint(u64::from((field << 3) | wire));
    }

    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.0.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
    }
}

pub enum Value {
    Varint(u64),
    Bytes(Vec<u8>),
    Fixed(u64),
}

pub struct Field {
    pub no: u32,
    pub value: Value,
}

pub fn parse(b: &[u8]) -> Result<Vec<Field>> {
    let mut fields = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let key = read_varint(b, &mut i)?;
        let no = (key >> 3) as u32;
        let value = match key & 7 {
            0 => Value::Varint(read_varint(b, &mut i)?),
            1 => Value::Fixed(u64::from_le_bytes(take(b, &mut i, 8)?.try_into()?)),
            2 => {
                let len = read_varint(b, &mut i)? as usize;
                Value::Bytes(take(b, &mut i, len)?.to_vec())
            }
            5 => Value::Fixed(u64::from(u32::from_le_bytes(take(b, &mut i, 4)?.try_into()?))),
            wire => bail!("unsupported wire type {wire}"),
        };
        fields.push(Field { no, value });
    }
    Ok(fields)
}

fn take<'a>(b: &'a [u8], i: &mut usize, n: usize) -> Result<&'a [u8]> {
    let Some(slice) = b.get(*i..*i + n) else { bail!("truncated field") };
    *i += n;
    Ok(slice)
}

fn read_varint(b: &[u8], i: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let Some(&x) = b.get(*i) else { bail!("truncated varint") };
        *i += 1;
        v |= u64::from(x & 0x7f) << shift;
        if x < 0x80 {
            return Ok(v);
        }
    }
    bail!("varint too long")
}

/// Field accessors; each returns the first matching field, or the proto3 default.
pub trait Fields {
    fn raw(&self, no: u32) -> Option<&[u8]>;
    fn u(&self, no: u32) -> u64;

    fn str(&self, no: u32) -> String {
        self.raw(no).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default()
    }

    fn msg(&self, no: u32) -> Option<Vec<Field>> {
        self.raw(no).and_then(|b| parse(b).ok())
    }
}

impl Fields for [Field] {
    fn raw(&self, no: u32) -> Option<&[u8]> {
        self.iter().find_map(|f| match &f.value {
            Value::Bytes(b) if f.no == no => Some(b.as_slice()),
            _ => None,
        })
    }

    fn u(&self, no: u32) -> u64 {
        self.iter()
            .find_map(|f| match f.value {
                Value::Varint(v) if f.no == no => Some(v),
                _ => None,
            })
            .unwrap_or(0)
    }
}

/// Best-effort one-line view of an unknown message: strings, numbers, nested messages.
pub fn summarize(b: &[u8], depth: u32) -> String {
    let Ok(fields) = parse(b) else { return format!("<{} bytes>", b.len()) };
    fields
        .iter()
        .map(|f| match &f.value {
            Value::Varint(v) | Value::Fixed(v) => format!("{}={v}", f.no),
            Value::Bytes(b) if !b.is_empty() && b.iter().all(|c| (32..127).contains(c)) => {
                format!("{}=\"{}\"", f.no, String::from_utf8_lossy(b))
            }
            Value::Bytes(b) if depth < 2 && parse(b).is_ok() => format!("{}={{{}}}", f.no, summarize(b, depth + 1)),
            Value::Bytes(b) => format!("{}=<{} bytes>", f.no, b.len()),
        })
        .collect::<Vec<_>>()
        .join(" ")
}
