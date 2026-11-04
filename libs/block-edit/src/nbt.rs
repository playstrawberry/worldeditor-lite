//! Named Binary Tag (NBT), the Java edition's big-endian format: enough to read and write
//! schematic files and the block entity bytes the server hands out.
//!
//! Reading is bounded (depth, list and array lengths, total input) and never panics on bad
//! input: it returns an error.

use std::fmt;

/// Tag type ids.
const END: u8 = 0;
const BYTE: u8 = 1;
const SHORT: u8 = 2;
const INT: u8 = 3;
const LONG: u8 = 4;
const FLOAT: u8 = 5;
const DOUBLE: u8 = 6;
const BYTE_ARRAY: u8 = 7;
const STRING: u8 = 8;
const LIST: u8 = 9;
const COMPOUND: u8 = 10;
const INT_ARRAY: u8 = 11;
const LONG_ARRAY: u8 = 12;

/// How deep compounds and lists may nest.
pub const MAX_DEPTH: usize = 64;

/// One NBT value.
#[derive(Debug, Clone, PartialEq)]
pub enum Tag {
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    ByteArray(Vec<u8>),
    String(String),
    /// A list keeps the element type of an empty list, so it writes back the same.
    List(u8, Vec<Tag>),
    Compound(Compound),
    IntArray(Vec<i32>),
    LongArray(Vec<i64>),
}

/// A compound: named tags in the order they were read or added.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Compound(pub Vec<(String, Tag)>);

impl Compound {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Tag> {
        self.0.iter().find(|(n, _)| n == name).map(|(_, t)| t)
    }

    /// Sets `name`, replacing an earlier value of that name.
    pub fn put(&mut self, name: &str, tag: Tag) {
        if let Some(slot) = self.0.iter_mut().find(|(n, _)| n == name) {
            slot.1 = tag;
        } else {
            self.0.push((name.to_owned(), tag));
        }
    }

    pub fn remove(&mut self, name: &str) -> Option<Tag> {
        let index = self.0.iter().position(|(n, _)| n == name)?;
        Some(self.0.remove(index).1)
    }

    #[must_use]
    pub fn compound(&self, name: &str) -> Option<&Compound> {
        match self.get(name) {
            Some(Tag::Compound(c)) => Some(c),
            _ => None,
        }
    }

    /// A whole number of any integer type, widened.
    #[must_use]
    pub fn int(&self, name: &str) -> Option<i64> {
        match self.get(name)? {
            Tag::Byte(v) => Some(i64::from(*v)),
            Tag::Short(v) => Some(i64::from(*v)),
            Tag::Int(v) => Some(i64::from(*v)),
            Tag::Long(v) => Some(*v),
            _ => None,
        }
    }

    #[must_use]
    pub fn string(&self, name: &str) -> Option<&str> {
        match self.get(name) {
            Some(Tag::String(s)) => Some(s),
            _ => None,
        }
    }
}

impl Tag {
    const fn id(&self) -> u8 {
        match self {
            Self::Byte(_) => BYTE,
            Self::Short(_) => SHORT,
            Self::Int(_) => INT,
            Self::Long(_) => LONG,
            Self::Float(_) => FLOAT,
            Self::Double(_) => DOUBLE,
            Self::ByteArray(_) => BYTE_ARRAY,
            Self::String(_) => STRING,
            Self::List(..) => LIST,
            Self::Compound(_) => COMPOUND,
            Self::IntArray(_) => INT_ARRAY,
            Self::LongArray(_) => LONG_ARRAY,
        }
    }
}

/// Why bytes are not valid NBT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NbtError {
    Truncated,
    BadType(u8),
    TooDeep,
    TooLong,
    BadString,
    NotCompound,
}

impl fmt::Display for NbtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => write!(f, "the data ends too early"),
            Self::BadType(t) => write!(f, "unknown tag type {t}"),
            Self::TooDeep => write!(f, "nested too deep"),
            Self::TooLong => write!(f, "a list or array is longer than the data"),
            Self::BadString => write!(f, "a text is not valid"),
            Self::NotCompound => write!(f, "the root is not a compound"),
        }
    }
}

struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], NbtError> {
        let end = self.at.checked_add(n).ok_or(NbtError::Truncated)?;
        let slice = self.data.get(self.at..end).ok_or(NbtError::Truncated)?;
        self.at = end;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], NbtError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, NbtError> {
        Ok(self.take(1)?[0])
    }

    fn i32(&mut self) -> Result<i32, NbtError> {
        Ok(i32::from_be_bytes(self.array()?))
    }

    /// A length prefix, refused when the rest of the data cannot hold `size` bytes per item.
    fn length(&mut self, size: usize) -> Result<usize, NbtError> {
        let n = usize::try_from(self.i32()?.max(0)).map_err(|_| NbtError::TooLong)?;
        let left = self.data.len() - self.at;
        if n.checked_mul(size.max(1)).is_none_or(|bytes| bytes > left) {
            return Err(NbtError::TooLong);
        }
        Ok(n)
    }

    fn string(&mut self) -> Result<String, NbtError> {
        let n = usize::from(u16::from_be_bytes(self.array()?));
        let bytes = self.take(n)?;
        decode_modified_utf8(bytes).ok_or(NbtError::BadString)
    }

    fn payload(&mut self, id: u8, depth: usize) -> Result<Tag, NbtError> {
        if depth > MAX_DEPTH {
            return Err(NbtError::TooDeep);
        }
        Ok(match id {
            BYTE => Tag::Byte(i8::from_be_bytes(self.array()?)),
            SHORT => Tag::Short(i16::from_be_bytes(self.array()?)),
            INT => Tag::Int(self.i32()?),
            LONG => Tag::Long(i64::from_be_bytes(self.array()?)),
            FLOAT => Tag::Float(f32::from_be_bytes(self.array()?)),
            DOUBLE => Tag::Double(f64::from_be_bytes(self.array()?)),
            BYTE_ARRAY => {
                let n = self.length(1)?;
                Tag::ByteArray(self.take(n)?.to_vec())
            }
            STRING => Tag::String(self.string()?),
            LIST => {
                let element = self.u8()?;
                let n = self.length(1)?;
                if element == END && n > 0 {
                    return Err(NbtError::BadType(END));
                }
                let mut items = Vec::with_capacity(n.min(4096));
                for _ in 0..n {
                    items.push(self.payload(element, depth + 1)?);
                }
                Tag::List(element, items)
            }
            COMPOUND => Tag::Compound(self.compound(depth + 1)?),
            INT_ARRAY => {
                let n = self.length(4)?;
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(self.i32()?);
                }
                Tag::IntArray(items)
            }
            LONG_ARRAY => {
                let n = self.length(8)?;
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(i64::from_be_bytes(self.array()?));
                }
                Tag::LongArray(items)
            }
            other => return Err(NbtError::BadType(other)),
        })
    }

    fn compound(&mut self, depth: usize) -> Result<Compound, NbtError> {
        let mut out = Compound::default();
        loop {
            let id = self.u8()?;
            if id == END {
                return Ok(out);
            }
            let name = self.string()?;
            let tag = self.payload(id, depth)?;
            out.0.push((name, tag));
        }
    }
}

/// NBT's "modified UTF-8": UTF-8, except that U+0000 is two bytes and characters above U+FFFF
/// are surrogate pairs of three bytes each. Plain UTF-8 text reads as itself.
fn decode_modified_utf8(bytes: &[u8]) -> Option<String> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Some(text.to_owned());
    }
    let mut units = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b < 0x80 {
            units.push(u16::from(b));
            i += 1;
        } else if b & 0xE0 == 0xC0 {
            let b2 = *bytes.get(i + 1)?;
            units.push((u16::from(b & 0x1F) << 6) | u16::from(b2 & 0x3F));
            i += 2;
        } else if b & 0xF0 == 0xE0 {
            let b2 = *bytes.get(i + 1)?;
            let b3 = *bytes.get(i + 2)?;
            units.push(
                (u16::from(b & 0x0F) << 12) | (u16::from(b2 & 0x3F) << 6) | u16::from(b3 & 0x3F),
            );
            i += 3;
        } else {
            return None;
        }
    }
    String::from_utf16(&units).ok()
}

fn encode_modified_utf8(text: &str, out: &mut Vec<u8>) {
    let mut bytes = Vec::with_capacity(text.len());
    for unit in text.encode_utf16() {
        match unit {
            0x0001..=0x007F => bytes.push(unit as u8),
            0x0000 | 0x0080..=0x07FF => {
                bytes.push(0xC0 | ((unit >> 6) as u8 & 0x1F));
                bytes.push(0x80 | (unit as u8 & 0x3F));
            }
            _ => {
                bytes.push(0xE0 | ((unit >> 12) as u8 & 0x0F));
                bytes.push(0x80 | ((unit >> 6) as u8 & 0x3F));
                bytes.push(0x80 | (unit as u8 & 0x3F));
            }
        }
    }
    // A longer text cannot be written; it is cut at a character boundary of the encoding.
    let mut n = bytes.len().min(usize::from(u16::MAX));
    while n < bytes.len() && n > 0 && bytes[n] & 0xC0 == 0x80 {
        n -= 1;
    }
    out.extend_from_slice(&(n as u16).to_be_bytes());
    out.extend_from_slice(&bytes[..n]);
}

fn write_payload(tag: &Tag, out: &mut Vec<u8>) {
    match tag {
        Tag::Byte(v) => out.extend_from_slice(&v.to_be_bytes()),
        Tag::Short(v) => out.extend_from_slice(&v.to_be_bytes()),
        Tag::Int(v) => out.extend_from_slice(&v.to_be_bytes()),
        Tag::Long(v) => out.extend_from_slice(&v.to_be_bytes()),
        Tag::Float(v) => out.extend_from_slice(&v.to_be_bytes()),
        Tag::Double(v) => out.extend_from_slice(&v.to_be_bytes()),
        Tag::ByteArray(v) => {
            out.extend_from_slice(&len32(v.len()).to_be_bytes());
            out.extend_from_slice(v);
        }
        Tag::String(s) => encode_modified_utf8(s, out),
        Tag::List(element, items) => {
            let element = items.first().map_or(*element, Tag::id);
            out.push(element);
            out.extend_from_slice(&len32(items.len()).to_be_bytes());
            for item in items {
                write_payload(item, out);
            }
        }
        Tag::Compound(c) => write_compound(c, out),
        Tag::IntArray(v) => {
            out.extend_from_slice(&len32(v.len()).to_be_bytes());
            for x in v {
                out.extend_from_slice(&x.to_be_bytes());
            }
        }
        Tag::LongArray(v) => {
            out.extend_from_slice(&len32(v.len()).to_be_bytes());
            for x in v {
                out.extend_from_slice(&x.to_be_bytes());
            }
        }
    }
}

fn len32(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

fn write_compound(c: &Compound, out: &mut Vec<u8>) {
    for (name, tag) in &c.0 {
        out.push(tag.id());
        encode_modified_utf8(name, out);
        write_payload(tag, out);
    }
    out.push(END);
}

/// Reads a named root compound (the file form): type, name, payload. Returns the name too.
pub fn read_named(data: &[u8]) -> Result<(String, Compound), NbtError> {
    let mut reader = Reader { data, at: 0 };
    if reader.u8()? != COMPOUND {
        return Err(NbtError::NotCompound);
    }
    let name = reader.string()?;
    Ok((name, reader.compound(1)?))
}

/// Reads an unnamed root compound (the network form the server uses for block entities): type,
/// payload.
pub fn read_unnamed(data: &[u8]) -> Result<Compound, NbtError> {
    let mut reader = Reader { data, at: 0 };
    if reader.u8()? != COMPOUND {
        return Err(NbtError::NotCompound);
    }
    reader.compound(1)
}

#[must_use]
pub fn write_named(name: &str, root: &Compound) -> Vec<u8> {
    let mut out = vec![COMPOUND];
    encode_modified_utf8(name, &mut out);
    write_compound(root, &mut out);
    out
}

#[must_use]
pub fn write_unnamed(root: &Compound) -> Vec<u8> {
    let mut out = vec![COMPOUND];
    write_compound(root, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Compound {
        let mut inner = Compound::default();
        inner.put("Name", Tag::String("Grüße \u{1F600} \0".into()));
        let mut root = Compound::default();
        root.put("b", Tag::Byte(-3));
        root.put("s", Tag::Short(300));
        root.put("i", Tag::Int(-70_000));
        root.put("l", Tag::Long(1 << 40));
        root.put("f", Tag::Float(1.5));
        root.put("d", Tag::Double(-2.25));
        root.put("ba", Tag::ByteArray(vec![1, 2, 255]));
        root.put("ia", Tag::IntArray(vec![1, -2, 3]));
        root.put("la", Tag::LongArray(vec![i64::MIN]));
        root.put("empty", Tag::List(END, vec![]));
        root.put(
            "list",
            Tag::List(COMPOUND, vec![Tag::Compound(inner.clone())]),
        );
        root.put("c", Tag::Compound(inner));
        root
    }

    #[test]
    fn named_and_unnamed_round_trip() {
        let root = sample();
        let bytes = write_named("Schematic", &root);
        assert_eq!(
            read_named(&bytes),
            Ok(("Schematic".to_owned(), root.clone()))
        );
        assert_eq!(read_unnamed(&write_unnamed(&root)), Ok(root));
    }

    #[test]
    fn hostile_input_is_an_error_not_a_panic() {
        let bytes = write_named("", &sample());
        for cut in 0..bytes.len() {
            assert!(read_named(&bytes[..cut]).is_err(), "cut at {cut}");
        }
        // A huge list length with no data behind it.
        let mut huge = vec![COMPOUND, 0, 0, LIST, 0, 1, b'x', INT];
        huge.extend_from_slice(&i32::MAX.to_be_bytes());
        assert_eq!(read_named(&huge), Err(NbtError::TooLong));
        // Deep nesting.
        let mut deep = vec![COMPOUND, 0, 0];
        for _ in 0..200 {
            deep.extend_from_slice(&[COMPOUND, 0, 0]);
        }
        assert_eq!(read_named(&deep), Err(NbtError::TooDeep));
        assert_eq!(read_named(&[INT, 0, 0]), Err(NbtError::NotCompound));
        assert_eq!(
            read_named(&[COMPOUND, 0, 0, 99, 0, 0]),
            Err(NbtError::BadType(99))
        );
    }
}
