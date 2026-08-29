// SPDX-License-Identifier: Apache-2.0
//! DEFLATE decompression (RFC 1951) and the zlib wrapper (RFC 1950).
//!
//! PNG stores its pixels as a zlib stream, so decoding a PNG means inflating
//! one first. This is the whole of DEFLATE: stored blocks, fixed Huffman
//! blocks, and dynamic Huffman blocks with their code-length alphabet.
//!
//! Decoding only. Nothing here compresses, because nothing in Forge writes a
//! PNG — and leaving the compressor out removes the half of the format that
//! carries all the parameter tuning.

/// A bit reader in DEFLATE's order: least-significant bit of each byte first,
/// which is the opposite of the bit order used by the Huffman codes it reads.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    bit: u32,
    acc: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, bit: 0, acc: 0 }
    }

    fn need(&mut self, n: u32) -> Result<(), String> {
        while self.bit < n {
            let byte = *self.data.get(self.pos).ok_or("deflate: input ended mid-stream")?;
            self.pos += 1;
            self.acc |= (byte as u32) << self.bit;
            self.bit += 8;
        }
        Ok(())
    }

    fn take(&mut self, n: u32) -> Result<u32, String> {
        if n == 0 {
            return Ok(0);
        }
        self.need(n)?;
        let v = self.acc & ((1u32 << n) - 1);
        self.acc >>= n;
        self.bit -= n;
        Ok(v)
    }

    /// Discard the rest of the current byte, as a stored block requires.
    fn align(&mut self) {
        let drop = self.bit % 8;
        self.acc >>= drop;
        self.bit -= drop;
    }
}

/// A canonical Huffman decoding table, built from code lengths alone — which
/// is all DEFLATE transmits, since the codes themselves follow from the
/// lengths by the canonical rule.
struct Huffman {
    /// counts[n] = how many codes have length n.
    counts: [u16; 16],
    /// Symbols ordered by (length, symbol), the canonical order.
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Self, String> {
        let mut counts = [0u16; 16];
        for &l in lengths {
            counts[l as usize] += 1;
        }
        counts[0] = 0; // length 0 means "unused", not a code

        let mut offsets = [0u16; 16];
        for n in 1..16 {
            offsets[n] = offsets[n - 1] + counts[n - 1];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offsets[l as usize] as usize] = sym as u16;
                offsets[l as usize] += 1;
            }
        }
        Ok(Self { counts, symbols })
    }

    /// Read one symbol. Huffman codes are stored most-significant-bit first,
    /// so each bit is appended to the accumulated code rather than prepended.
    fn decode(&self, bits: &mut Bits) -> Result<u16, String> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;
        for len in 1..16 {
            code |= bits.take(1)? as i32;
            let count = self.counts[len] as i32;
            if code - first < count {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err("deflate: no symbol matches the code".into())
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

/// Inflate a raw DEFLATE stream.
pub fn inflate(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut bits = Bits::new(data);
    let mut out: Vec<u8> = Vec::new();

    loop {
        let last = bits.take(1)?;
        match bits.take(2)? {
            0 => {
                bits.align();
                let len = bits.take(16)? as usize;
                let nlen = bits.take(16)? as usize;
                if len != (!nlen & 0xFFFF) {
                    return Err("deflate: stored block length check failed".into());
                }
                for _ in 0..len {
                    out.push(bits.take(8)? as u8);
                }
            }
            1 => {
                let (lit, dist) = fixed_tables()?;
                inflate_block(&mut bits, &lit, &dist, &mut out)?;
            }
            2 => {
                let (lit, dist) = dynamic_tables(&mut bits)?;
                inflate_block(&mut bits, &lit, &dist, &mut out)?;
            }
            _ => return Err("deflate: reserved block type".into()),
        }
        if last == 1 {
            return Ok(out);
        }
    }
}

/// Inflate a zlib stream: a two-byte header, the DEFLATE data, and an Adler-32
/// checksum which is verified, since a silently wrong image is worse than an
/// error.
pub fn zlib_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 6 {
        return Err("zlib: stream too short".into());
    }
    let cmf = data[0];
    let flg = data[1];
    if cmf & 0x0F != 8 {
        return Err(format!("zlib: compression method {} is not deflate", cmf & 0x0F));
    }
    if ((cmf as u16) << 8 | flg as u16) % 31 != 0 {
        return Err("zlib: header check failed".into());
    }
    if flg & 0x20 != 0 {
        return Err("zlib: preset dictionaries are not supported".into());
    }
    let body = &data[2..data.len() - 4];
    let out = inflate(body)?;

    let want = u32::from_be_bytes([
        data[data.len() - 4], data[data.len() - 3], data[data.len() - 2], data[data.len() - 1],
    ]);
    let got = adler32(&out);
    if want != got {
        return Err(format!("zlib: checksum mismatch (want {want:#010x}, got {got:#010x})"));
    }
    Ok(out)
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn fixed_tables() -> Result<(Huffman, Huffman), String> {
    let mut lit = [0u8; 288];
    for (i, l) in lit.iter_mut().enumerate() {
        *l = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    Ok((Huffman::new(&lit)?, Huffman::new(&[5u8; 30])?))
}

fn dynamic_tables(bits: &mut Bits) -> Result<(Huffman, Huffman), String> {
    const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
    let hlit = bits.take(5)? as usize + 257;
    let hdist = bits.take(5)? as usize + 1;
    let hclen = bits.take(4)? as usize + 4;

    let mut code_lengths = [0u8; 19];
    for &slot in ORDER.iter().take(hclen) {
        code_lengths[slot] = bits.take(3)? as u8;
    }
    let code_table = Huffman::new(&code_lengths)?;

    let mut lengths = vec![0u8; hlit + hdist];
    let mut i = 0;
    while i < lengths.len() {
        match code_table.decode(bits)? {
            n @ 0..=15 => {
                lengths[i] = n as u8;
                i += 1;
            }
            16 => {
                let prev = *lengths.get(i.wrapping_sub(1))
                    .ok_or("deflate: repeat with nothing to repeat")?;
                for _ in 0..(3 + bits.take(2)?) {
                    if i >= lengths.len() { return Err("deflate: repeat runs past the table".into()); }
                    lengths[i] = prev;
                    i += 1;
                }
            }
            17 => { i += 3 + bits.take(3)? as usize; }
            18 => { i += 11 + bits.take(7)? as usize; }
            _ => return Err("deflate: bad code-length symbol".into()),
        }
    }
    if i > lengths.len() {
        return Err("deflate: code lengths overrun".into());
    }
    Ok((Huffman::new(&lengths[..hlit])?, Huffman::new(&lengths[hlit..])?))
}

fn inflate_block(
    bits: &mut Bits,
    lit: &Huffman,
    dist: &Huffman,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    loop {
        let sym = lit.decode(bits)?;
        match sym {
            0..=255 => out.push(sym as u8),
            256 => return Ok(()),
            257..=285 => {
                let i = sym as usize - 257;
                let length = LENGTH_BASE[i] as usize + bits.take(LENGTH_EXTRA[i] as u32)? as usize;
                let d = dist.decode(bits)? as usize;
                if d >= DIST_BASE.len() {
                    return Err("deflate: distance symbol out of range".into());
                }
                let distance = DIST_BASE[d] as usize + bits.take(DIST_EXTRA[d] as u32)? as usize;
                if distance > out.len() {
                    return Err("deflate: back-reference before the start of the output".into());
                }
                // Byte at a time on purpose: runs may overlap the region being
                // written, which is how DEFLATE encodes a repeated pattern.
                let start = out.len() - distance;
                for k in 0..length {
                    let b = out[start + k];
                    out.push(b);
                }
            }
            _ => return Err("deflate: literal/length symbol out of range".into()),
        }
    }
}
