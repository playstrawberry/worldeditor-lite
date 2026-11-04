//! gzip (RFC 1952) around deflate (RFC 1951), written for schematic files.
//!
//! - `gunzip` reads every deflate block type (stored, fixed and dynamic Huffman), checks the
//!   CRC-32 and the length, and stops with an error past `limit` output bytes.
//! - `gzip` writes LZ77 matches with the fixed Huffman code: small code, good on the repetitive
//!   data of block arrays.

/// CRC-32 (IEEE), table driven.
fn crc_table() -> &'static [u32; 256] {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (n, slot) in table.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 == 1 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *slot = c;
        }
        table
    })
}

#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    let table = crc_table();
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = table[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GzipError {
    NotGzip,
    Truncated,
    BadBlock,
    BadCode,
    BadDistance,
    TooLarge,
    BadChecksum,
}

impl std::fmt::Display for GzipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::NotGzip => "not a gzip file",
            Self::Truncated => "the file ends too early",
            Self::BadBlock => "a compressed block is not valid",
            Self::BadCode => "a compressed code is not valid",
            Self::BadDistance => "a back reference points before the start",
            Self::TooLarge => "the file unpacks to more than the limit",
            Self::BadChecksum => "the checksum does not match",
        };
        f.write_str(text)
    }
}

struct Bits<'a> {
    data: &'a [u8],
    at: usize,
    bit: u32,
    held: u32,
}

impl Bits<'_> {
    fn need(&mut self, n: u32) -> Result<(), GzipError> {
        while self.bit < n {
            let byte = *self.data.get(self.at).ok_or(GzipError::Truncated)?;
            self.at += 1;
            self.held |= u32::from(byte) << self.bit;
            self.bit += 8;
        }
        Ok(())
    }

    fn take(&mut self, n: u32) -> Result<u32, GzipError> {
        if n == 0 {
            return Ok(0);
        }
        self.need(n)?;
        let value = self.held & ((1u32 << n) - 1);
        self.held >>= n;
        self.bit -= n;
        Ok(value)
    }

    fn align(&mut self) {
        self.held = 0;
        self.bit = 0;
    }
}

/// A canonical Huffman decoder: counts per length and symbols by code order.
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Self, GzipError> {
        let mut counts = [0u16; 16];
        for &l in lengths {
            counts[usize::from(l)] += 1;
        }
        counts[0] = 0;
        let mut left: i32 = 1;
        for &count in &counts[1..] {
            left = (left << 1) - i32::from(count);
            if left < 0 {
                return Err(GzipError::BadCode);
            }
        }
        let mut offsets = [0u16; 16];
        for len in 1..15 {
            offsets[len + 1] = offsets[len] + counts[len];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (symbol, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[usize::from(offsets[usize::from(l)])] = symbol as u16;
                offsets[usize::from(l)] += 1;
            }
        }
        Ok(Self { counts, symbols })
    }

    fn decode(&self, bits: &mut Bits<'_>) -> Result<u16, GzipError> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..16 {
            code |= bits.take(1)? as i32;
            let count = i32::from(self.counts[len]);
            if code - count < first {
                return self
                    .symbols
                    .get((index + (code - first)) as usize)
                    .copied()
                    .ok_or(GzipError::BadCode);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err(GzipError::BadCode)
    }
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

fn fixed_lengths() -> (Vec<u8>, Vec<u8>) {
    let mut lit = vec![8u8; 288];
    lit[144..256].fill(9);
    lit[256..280].fill(7);
    (lit, vec![5u8; 30])
}

fn inflate_block(
    bits: &mut Bits<'_>,
    out: &mut Vec<u8>,
    lit: &Huffman,
    dist: &Huffman,
    limit: usize,
) -> Result<(), GzipError> {
    loop {
        let symbol = lit.decode(bits)?;
        match symbol {
            0..=255 => {
                if out.len() >= limit {
                    return Err(GzipError::TooLarge);
                }
                out.push(symbol as u8);
            }
            256 => return Ok(()),
            257..=285 => {
                let i = usize::from(symbol - 257);
                let len =
                    usize::from(LENGTH_BASE[i]) + bits.take(u32::from(LENGTH_EXTRA[i]))? as usize;
                let d = usize::from(dist.decode(bits)?);
                if d >= 30 {
                    return Err(GzipError::BadCode);
                }
                let distance =
                    usize::from(DIST_BASE[d]) + bits.take(u32::from(DIST_EXTRA[d]))? as usize;
                if distance > out.len() {
                    return Err(GzipError::BadDistance);
                }
                if out.len() + len > limit {
                    return Err(GzipError::TooLarge);
                }
                let start = out.len() - distance;
                for k in 0..len {
                    let b = out[start + k];
                    out.push(b);
                }
            }
            _ => return Err(GzipError::BadCode),
        }
    }
}

/// Unpacks raw deflate data. Returns the output and the number of input bytes used.
fn inflate(data: &[u8], limit: usize) -> Result<(Vec<u8>, usize), GzipError> {
    let mut bits = Bits {
        data,
        at: 0,
        bit: 0,
        held: 0,
    };
    let mut out = Vec::new();
    let (fixed_lit, fixed_dist) = fixed_lengths();
    loop {
        let last = bits.take(1)? == 1;
        match bits.take(2)? {
            0 => {
                bits.align();
                let header = data.get(bits.at..bits.at + 4).ok_or(GzipError::Truncated)?;
                let len = usize::from(u16::from_le_bytes([header[0], header[1]]));
                let nlen = u16::from_le_bytes([header[2], header[3]]);
                if nlen != !(len as u16) {
                    return Err(GzipError::BadBlock);
                }
                bits.at += 4;
                let block = data
                    .get(bits.at..bits.at + len)
                    .ok_or(GzipError::Truncated)?;
                if out.len() + len > limit {
                    return Err(GzipError::TooLarge);
                }
                out.extend_from_slice(block);
                bits.at += len;
            }
            1 => {
                let lit = Huffman::new(&fixed_lit)?;
                let dist = Huffman::new(&fixed_dist)?;
                inflate_block(&mut bits, &mut out, &lit, &dist, limit)?;
            }
            2 => {
                let hlit = bits.take(5)? as usize + 257;
                let hdist = bits.take(5)? as usize + 1;
                let hclen = bits.take(4)? as usize + 4;
                const ORDER: [usize; 19] = [
                    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
                ];
                let mut code_lengths = [0u8; 19];
                for &slot in ORDER.iter().take(hclen) {
                    code_lengths[slot] = bits.take(3)? as u8;
                }
                let codes = Huffman::new(&code_lengths)?;
                let mut lengths = vec![0u8; hlit + hdist];
                let mut i = 0;
                while i < lengths.len() {
                    let symbol = codes.decode(&mut bits)?;
                    let (value, repeat) = match symbol {
                        0..=15 => (symbol as u8, 1),
                        16 => {
                            let previous =
                                *lengths.get(i.wrapping_sub(1)).ok_or(GzipError::BadBlock)?;
                            (previous, 3 + bits.take(2)? as usize)
                        }
                        17 => (0, 3 + bits.take(3)? as usize),
                        18 => (0, 11 + bits.take(7)? as usize),
                        _ => return Err(GzipError::BadCode),
                    };
                    if i + repeat > lengths.len() {
                        return Err(GzipError::BadBlock);
                    }
                    lengths[i..i + repeat].fill(value);
                    i += repeat;
                }
                if lengths[256] == 0 {
                    return Err(GzipError::BadBlock);
                }
                let lit = Huffman::new(&lengths[..hlit])?;
                let dist = Huffman::new(&lengths[hlit..])?;
                inflate_block(&mut bits, &mut out, &lit, &dist, limit)?;
            }
            _ => return Err(GzipError::BadBlock),
        }
        if last {
            return Ok((out, bits.at));
        }
    }
}

/// Unpacks a gzip file of at most `limit` bytes of output. Several members are joined.
pub fn gunzip(data: &[u8], limit: usize) -> Result<Vec<u8>, GzipError> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let rest = &data[at..];
        if rest.len() < 18 || rest[0] != 0x1F || rest[1] != 0x8B || rest[2] != 8 {
            return Err(if out.is_empty() {
                GzipError::NotGzip
            } else {
                GzipError::BadBlock
            });
        }
        let flags = rest[3];
        let mut p = 10;
        if flags & 4 != 0 {
            let x = rest.get(p..p + 2).ok_or(GzipError::Truncated)?;
            p += 2 + usize::from(u16::from_le_bytes([x[0], x[1]]));
        }
        for bit in [8u8, 16] {
            if flags & bit != 0 {
                let end = rest
                    .get(p..)
                    .ok_or(GzipError::Truncated)?
                    .iter()
                    .position(|&b| b == 0)
                    .ok_or(GzipError::Truncated)?;
                p += end + 1;
            }
        }
        if flags & 2 != 0 {
            p += 2;
        }
        let body = rest.get(p..).ok_or(GzipError::Truncated)?;
        let (member, used) = inflate(body, limit.saturating_sub(out.len()))?;
        let trailer = body.get(used..used + 8).ok_or(GzipError::Truncated)?;
        let crc = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
        let size = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]);
        if crc != crc32(&member) || size != member.len() as u32 {
            return Err(GzipError::BadChecksum);
        }
        out.extend_from_slice(&member);
        at += p + used + 8;
    }
    if out.is_empty() && data.is_empty() {
        return Err(GzipError::NotGzip);
    }
    Ok(out)
}

struct BitWriter {
    out: Vec<u8>,
    held: u64,
    bit: u32,
}

impl BitWriter {
    fn put(&mut self, value: u32, n: u32) {
        self.held |= u64::from(value) << self.bit;
        self.bit += n;
        while self.bit >= 8 {
            self.out.push(self.held as u8);
            self.held >>= 8;
            self.bit -= 8;
        }
    }

    /// Writes a Huffman code, most significant bit first.
    fn code(&mut self, code: u32, len: u32) {
        let mut reversed = 0;
        for i in 0..len {
            reversed |= ((code >> i) & 1) << (len - 1 - i);
        }
        self.put(reversed, len);
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bit > 0 {
            self.out.push(self.held as u8);
        }
        self.out
    }
}

fn fixed_literal(w: &mut BitWriter, symbol: u16) {
    let s = u32::from(symbol);
    match symbol {
        0..=143 => w.code(0x30 + s, 8),
        144..=255 => w.code(0x190 + s - 144, 9),
        256..=279 => w.code(s - 256, 7),
        _ => w.code(0xC0 + s - 280, 8),
    }
}

fn put_length(w: &mut BitWriter, len: usize) {
    let i = LENGTH_BASE
        .iter()
        .rposition(|&b| usize::from(b) <= len)
        .unwrap_or(0);
    fixed_literal(w, 257 + i as u16);
    w.put(
        (len - usize::from(LENGTH_BASE[i])) as u32,
        u32::from(LENGTH_EXTRA[i]),
    );
}

fn put_distance(w: &mut BitWriter, distance: usize) {
    let i = DIST_BASE
        .iter()
        .rposition(|&b| usize::from(b) <= distance)
        .unwrap_or(0);
    w.code(i as u32, 5);
    w.put(
        (distance - usize::from(DIST_BASE[i])) as u32,
        u32::from(DIST_EXTRA[i]),
    );
}

/// Raw deflate: one fixed-Huffman block, LZ77 with a hash of 3 bytes and a 32 KiB window.
fn deflate(data: &[u8]) -> Vec<u8> {
    const WINDOW: usize = 32 * 1024;
    const HASH_BITS: u32 = 15;
    const CHAIN: usize = 32;
    let mut w = BitWriter {
        out: Vec::with_capacity(data.len() / 4 + 16),
        held: 0,
        bit: 0,
    };
    w.put(1, 1);
    w.put(1, 2);
    let mut head = vec![usize::MAX; 1 << HASH_BITS];
    let mut prev = vec![usize::MAX; WINDOW];
    let hash = |i: usize| -> usize {
        let v = (u32::from(data[i]) << 16) | (u32::from(data[i + 1]) << 8) | u32::from(data[i + 2]);
        (v.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
    };
    let insert = |i: usize, head: &mut Vec<usize>, prev: &mut Vec<usize>| {
        if i + 2 < data.len() {
            let h = hash(i);
            prev[i % WINDOW] = head[h];
            head[h] = i;
        }
    };
    let mut i = 0;
    while i < data.len() {
        let mut best_len = 0;
        let mut best_dist = 0;
        if i + 2 < data.len() {
            let mut candidate = head[hash(i)];
            let mut steps = 0;
            while candidate != usize::MAX && i - candidate <= WINDOW && steps < CHAIN {
                let max = (data.len() - i).min(258);
                let mut len = 0;
                while len < max && data[candidate + len] == data[i + len] {
                    len += 1;
                }
                if len > best_len {
                    best_len = len;
                    best_dist = i - candidate;
                    if len == max {
                        break;
                    }
                }
                let next = prev[candidate % WINDOW];
                if next == usize::MAX || next >= candidate {
                    break;
                }
                candidate = next;
                steps += 1;
            }
        }
        if best_len >= 3 {
            put_length(&mut w, best_len);
            put_distance(&mut w, best_dist);
            for k in 0..best_len {
                insert(i + k, &mut head, &mut prev);
            }
            i += best_len;
        } else {
            fixed_literal(&mut w, u16::from(data[i]));
            insert(i, &mut head, &mut prev);
            i += 1;
        }
    }
    fixed_literal(&mut w, 256);
    w.finish()
}

#[must_use]
pub fn gzip(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1F, 0x8B, 8, 0, 0, 0, 0, 0, 0, 0xFF];
    out.extend_from_slice(&deflate(data));
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_of_a_known_text() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn round_trip_on_repetitive_and_random_data() {
        let mut data = Vec::new();
        for i in 0..50_000u32 {
            data.push((i % 7) as u8);
        }
        let mut x: u32 = 12345;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            data.push(x as u8);
        }
        let packed = gzip(&data);
        assert!(packed.len() < data.len());
        assert_eq!(gunzip(&packed, data.len()), Ok(data.clone()));
        assert_eq!(gunzip(&packed, data.len() - 1), Err(GzipError::TooLarge));
        assert_eq!(gunzip(&gzip(&[]), 10), Ok(vec![]));
    }

    /// `testdata/dynamic_gz.rs` holds the bytes Python's `gzip.compress(text, 9)` wrote, which
    /// chose a dynamic Huffman block (type 2) for this text.
    #[test]
    fn reads_a_dynamic_block_and_a_stored_block() {
        let text = include_bytes!("../testdata/dynamic.txt");
        let packed: &[u8] = &include!("../testdata/dynamic_gz.rs");
        assert_eq!(
            (packed[10] >> 1) & 3,
            2,
            "the fixture holds a dynamic block"
        );
        assert_eq!(gunzip(packed, 10_000), Ok(text.to_vec()));

        // Stored block, written by hand.
        let payload = b"stored";
        let mut stored = vec![0x1F, 0x8B, 8, 0, 0, 0, 0, 0, 0, 0xFF, 1];
        stored.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        stored.extend_from_slice(&(!(payload.len() as u16)).to_le_bytes());
        stored.extend_from_slice(payload);
        stored.extend_from_slice(&crc32(payload).to_le_bytes());
        stored.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        assert_eq!(gunzip(&stored, 100), Ok(payload.to_vec()));

        let text: Vec<u8> = b"abcabcabc"
            .repeat(20)
            .into_iter()
            .chain(b"xyz".iter().copied())
            .collect();
        assert_eq!(gunzip(&gzip(&text), 1000), Ok(text));
    }

    #[test]
    fn broken_files_are_errors() {
        let packed = gzip(b"some text some text some text");
        for cut in 0..packed.len() {
            assert!(gunzip(&packed[..cut], 1000).is_err(), "cut at {cut}");
        }
        let mut bad = packed.clone();
        let n = bad.len();
        bad[n - 8] ^= 1;
        assert_eq!(gunzip(&bad, 1000), Err(GzipError::BadChecksum));
        assert_eq!(
            gunzip(b"plain text, no gzip", 1000),
            Err(GzipError::NotGzip)
        );
    }
}
