// SPDX-License-Identifier: Apache-2.0
//! PNG decoding (RFC 2083), to 8-bit RGBA.
//!
//! Covers what a PNG in the wild actually is: all five colour types, bit
//! depths 1/2/4/8/16, palettes with `tRNS` transparency, the five scanline
//! filters, and Adam7 interlacing. 16-bit samples are reduced to 8, because
//! the destination is a screen texture.
//!
//! Not covered, deliberately: gamma, colour profiles and ancillary chunks
//! beyond transparency. Forge shows a picture of a file; it is not a colour-
//! managed viewer, and pretending otherwise would mean carrying ICC handling.

use super::inflate::zlib_decompress;
use super::Image;

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

pub fn looks_like_png(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && bytes[..8] == SIGNATURE
}

struct Header {
    width: u32,
    height: u32,
    depth: u8,
    colour: u8,
    interlaced: bool,
}

impl Header {
    /// Samples per pixel for this colour type.
    fn channels(&self) -> usize {
        match self.colour {
            0 => 1, // greyscale
            2 => 3, // truecolour
            3 => 1, // palette index
            4 => 2, // greyscale + alpha
            6 => 4, // truecolour + alpha
            _ => 0,
        }
    }

    fn bits_per_pixel(&self) -> usize {
        self.channels() * self.depth as usize
    }
}

pub fn decode(bytes: &[u8]) -> Result<Image, String> {
    if !looks_like_png(bytes) {
        return Err("png: not a PNG (signature does not match)".into());
    }

    let mut pos = 8;
    let mut header: Option<Header> = None;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut palette_alpha: Vec<u8> = Vec::new();
    let mut idat: Vec<u8> = Vec::new();

    while pos + 8 <= bytes.len() {
        let len = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let kind = &bytes[pos + 4..pos + 8];
        let body_at = pos + 8;
        if body_at + len + 4 > bytes.len() {
            return Err("png: chunk runs past the end of the file".into());
        }
        let body = &bytes[body_at..body_at + len];

        match kind {
            b"IHDR" => {
                if len < 13 {
                    return Err("png: IHDR is too short".into());
                }
                let h = Header {
                    width: u32::from_be_bytes(body[0..4].try_into().unwrap()),
                    height: u32::from_be_bytes(body[4..8].try_into().unwrap()),
                    depth: body[8],
                    colour: body[9],
                    interlaced: body[12] == 1,
                };
                if h.width == 0 || h.height == 0 {
                    return Err("png: image has a zero dimension".into());
                }
                if h.channels() == 0 {
                    return Err(format!("png: unknown colour type {}", h.colour));
                }
                if !matches!(h.depth, 1 | 2 | 4 | 8 | 16) {
                    return Err(format!("png: unsupported bit depth {}", h.depth));
                }
                if body[10] != 0 {
                    return Err("png: unknown compression method".into());
                }
                if body[11] != 0 {
                    return Err("png: unknown filter method".into());
                }
                header = Some(h);
            }
            b"PLTE" => {
                palette = body.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect();
            }
            b"tRNS" => {
                // For a palette this is per-entry alpha. For the two colour
                // types without an alpha channel it names a single transparent
                // colour, which is handled after the pixels are unpacked.
                palette_alpha = body.to_vec();
            }
            b"IDAT" => idat.extend_from_slice(body),
            b"IEND" => break,
            _ => {}
        }
        pos = body_at + len + 4; // skip the body and its CRC
    }

    let h = header.ok_or("png: no IHDR chunk")?;
    if idat.is_empty() {
        return Err("png: no image data".into());
    }
    let raw = zlib_decompress(&idat)?;

    let pixels = if h.interlaced {
        deinterlace(&h, &raw)?
    } else {
        let stride = (h.width as usize * h.bits_per_pixel() + 7) / 8;
        unfilter(&raw, stride, h.bits_per_pixel(), h.height as usize)?
    };

    to_rgba(&h, &pixels, &palette, &palette_alpha)
}

/// Undo the per-scanline filters. Each row is prefixed with its filter type,
/// and every filter refers to the byte one pixel to the left and the byte
/// directly above, both of which are already-reconstructed values.
fn unfilter(raw: &[u8], stride: usize, bits_per_pixel: usize, rows: usize) -> Result<Vec<u8>, String> {
    let bpp = ((bits_per_pixel + 7) / 8).max(1);
    let mut out = vec![0u8; stride * rows];
    let mut at = 0;

    for row in 0..rows {
        let filter = *raw.get(at).ok_or("png: image data ended early")?;
        at += 1;
        let line = raw.get(at..at + stride).ok_or("png: image data ended mid-row")?;
        at += stride;

        let (before, current) = out.split_at_mut(row * stride);
        let up = if row == 0 { None } else { Some(&before[(row - 1) * stride..]) };
        let cur = &mut current[..stride];

        for i in 0..stride {
            let x = line[i] as i32;
            let a = if i >= bpp { cur[i - bpp] as i32 } else { 0 };
            let b = up.map_or(0, |u| u[i] as i32);
            let c = if i >= bpp { up.map_or(0, |u| u[i - bpp] as i32) } else { 0 };
            cur[i] = match filter {
                0 => x,
                1 => x + a,
                2 => x + b,
                3 => x + (a + b) / 2,
                4 => x + paeth(a, b, c),
                _ => return Err(format!("png: unknown filter type {filter}")),
            } as u8;
        }
    }
    Ok(out)
}

/// The Paeth predictor: pick whichever of left, above and above-left is
/// closest to their linear estimate.
fn paeth(a: i32, b: i32, c: i32) -> i32 {
    let p = a + b - c;
    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
    if pa <= pb && pa <= pc { a } else if pb <= pc { b } else { c }
}

const ADAM7: [(usize, usize, usize, usize); 7] = [
    // (x offset, y offset, x step, y step)
    (0, 0, 8, 8), (4, 0, 8, 8), (0, 4, 4, 8), (2, 0, 4, 4),
    (0, 2, 2, 4), (1, 0, 2, 2), (0, 1, 1, 2),
];

/// Adam7 sends seven progressively finer passes, each a complete little image.
/// Each is unfiltered on its own — the filters refer to neighbours within the
/// pass, not within the final picture — then scattered into place.
fn deinterlace(h: &Header, raw: &[u8]) -> Result<Vec<u8>, String> {
    let bpp = h.bits_per_pixel();
    let full_stride = (h.width as usize * bpp + 7) / 8;
    let mut out = vec![0u8; full_stride * h.height as usize];
    let mut at = 0;

    for &(x0, y0, dx, dy) in ADAM7.iter() {
        let cols = (h.width as usize + dx - 1 - x0) / dx;
        let rows = (h.height as usize + dy - 1 - y0) / dy;
        if cols == 0 || rows == 0 {
            continue;
        }
        let stride = (cols * bpp + 7) / 8;
        let need = (stride + 1) * rows;
        let chunk = raw.get(at..at + need).ok_or("png: interlaced data ended early")?;
        at += need;
        let pass = unfilter(chunk, stride, bpp, rows)?;

        for r in 0..rows {
            for c in 0..cols {
                let src_bit = c * bpp;
                let dst_bit = (x0 + c * dx) * bpp;
                copy_bits(&pass[r * stride..], src_bit, &mut out[(y0 + r * dy) * full_stride..], dst_bit, bpp);
            }
        }
    }
    Ok(out)
}

/// Move one pixel's worth of bits, which for depths below 8 is not byte-aligned.
fn copy_bits(src: &[u8], src_bit: usize, dst: &mut [u8], dst_bit: usize, bits: usize) {
    for i in 0..bits {
        let s = src_bit + i;
        let d = dst_bit + i;
        let bit = (src[s / 8] >> (7 - s % 8)) & 1;
        let mask = 1u8 << (7 - d % 8);
        if bit == 1 { dst[d / 8] |= mask; } else { dst[d / 8] &= !mask; }
    }
}

/// Pull sample `i` out of a row, whatever the bit depth.
fn sample(row: &[u8], i: usize, depth: u8) -> u16 {
    match depth {
        16 => u16::from_be_bytes([row[i * 2], row[i * 2 + 1]]),
        8 => row[i] as u16,
        _ => {
            let per_byte = 8 / depth as usize;
            let byte = row[i / per_byte];
            let shift = 8 - depth as usize * (i % per_byte + 1);
            ((byte >> shift) & ((1 << depth) - 1)) as u16
        }
    }
}

fn to_rgba(h: &Header, pixels: &[u8], palette: &[[u8; 3]], trns: &[u8]) -> Result<Image, String> {
    let (w, ht) = (h.width as usize, h.height as usize);
    let stride = (w * h.bits_per_pixel() + 7) / 8;
    let channels = h.channels();
    let mut out = Vec::with_capacity(w * ht * 4);

    // Scale a sample of this depth up to 0..=255.
    let max = ((1u32 << h.depth) - 1) as u32;
    let to8 = |v: u16| -> u8 { ((v as u32 * 255 + max / 2) / max) as u8 };

    for y in 0..ht {
        let row = pixels.get(y * stride..(y + 1) * stride).ok_or("png: short row")?;
        for x in 0..w {
            let s = |c: usize| sample(row, x * channels + c, h.depth);
            let px: [u8; 4] = match h.colour {
                0 => { let g = to8(s(0)); [g, g, g, 255] }
                2 => [to8(s(0)), to8(s(1)), to8(s(2)), 255],
                3 => {
                    let idx = s(0) as usize;
                    let c = *palette.get(idx).ok_or("png: palette index out of range")?;
                    [c[0], c[1], c[2], *trns.get(idx).unwrap_or(&255)]
                }
                4 => { let g = to8(s(0)); [g, g, g, to8(s(1))] }
                _ => [to8(s(0)), to8(s(1)), to8(s(2)), to8(s(3))],
            };
            out.extend_from_slice(&px);
        }
    }

    // A tRNS chunk on a colour type without alpha names one fully transparent
    // colour rather than per-pixel alpha.
    if !trns.is_empty() && (h.colour == 0 || h.colour == 2) {
        let key: Vec<u8> = trns.chunks(2)
            .map(|c| to8(u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)])))
            .collect();
        let rgb = if h.colour == 0 && !key.is_empty() {
            [key[0], key[0], key[0]]
        } else if key.len() >= 3 {
            [key[0], key[1], key[2]]
        } else {
            [0, 0, 0]
        };
        for p in out.chunks_exact_mut(4) {
            if p[0] == rgb[0] && p[1] == rgb[1] && p[2] == rgb[2] {
                p[3] = 0;
            }
        }
    }

    Ok(Image { width: w, height: ht, rgba: out })
}
