// SPDX-License-Identifier: Apache-2.0
//! GIF decoding (GIF89a), to a sequence of 8-bit RGBA frames.
//!
//! Every frame is composited to full size, so a consumer can show frame *n*
//! without replaying the ones before it. That matters because a GIF's frames
//! are usually partial — a small rectangle patching the previous picture —
//! and because the disposal rules that say what to do between frames are the
//! part everyone gets wrong.
//!
//! LZW here is GIF's variant, not DEFLATE's: codes are least-significant-bit
//! first, the code width grows as the table fills, and the stream carries its
//! own clear and end markers.

use super::{Frame, Image};

pub fn looks_like_gif(bytes: &[u8]) -> bool {
    bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a")
}

/// Decode every frame, each composited to the full canvas.
pub fn decode(bytes: &[u8]) -> Result<Vec<Frame>, String> {
    if !looks_like_gif(bytes) {
        return Err("gif: not a GIF (signature does not match)".into());
    }
    let mut r = Reader { data: bytes, pos: 6 };

    let width = r.u16()? as usize;
    let height = r.u16()? as usize;
    let flags = r.u8()?;
    let background = r.u8()?;
    let _aspect = r.u8()?;
    if width == 0 || height == 0 {
        return Err("gif: image has a zero dimension".into());
    }

    let global: Vec<[u8; 3]> = if flags & 0x80 != 0 {
        r.palette(2usize << (flags & 7))?
    } else {
        Vec::new()
    };

    // The canvas everything composites onto, and the copy kept for the
    // "restore to previous" disposal method.
    let mut canvas = vec![0u8; width * height * 4];
    let mut frames: Vec<Frame> = Vec::new();

    // Values from the most recent Graphic Control Extension, which applies to
    // the next image block only.
    let mut delay_ms = 0u32;
    let mut transparent: Option<u8> = None;
    let mut disposal = 0u8;

    let _ = background;

    loop {
        match r.u8()? {
            0x2C => {
                let fx = r.u16()? as usize;
                let fy = r.u16()? as usize;
                let fw = r.u16()? as usize;
                let fh = r.u16()? as usize;
                let f = r.u8()?;
                let local: Vec<[u8; 3]> = if f & 0x80 != 0 {
                    r.palette(2usize << (f & 7))?
                } else {
                    Vec::new()
                };
                let palette = if local.is_empty() { &global } else { &local };
                if palette.is_empty() {
                    return Err("gif: frame has no colour table".into());
                }
                let interlaced = f & 0x40 != 0;

                let min_code = r.u8()?;
                let data = r.blocks()?;
                let indices = lzw(&data, min_code, fw * fh)?;

                // Kept before the frame is drawn: "restore to previous" means
                // the canvas as it was *before* this frame.
                let previous = canvas.clone();

                for row in 0..fh {
                    // Interlaced frames arrive in four passes over the rows.
                    let dst_row = if interlaced { interlaced_row(row, fh) } else { row };
                    if fy + dst_row >= height { continue; }
                    for col in 0..fw {
                        if fx + col >= width { continue; }
                        let idx = indices[row * fw + col];
                        if Some(idx) == transparent { continue; }
                        let c = palette.get(idx as usize).copied().unwrap_or([0, 0, 0]);
                        let at = ((fy + dst_row) * width + (fx + col)) * 4;
                        canvas[at..at + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
                    }
                }

                frames.push(Frame {
                    image: Image { width, height, rgba: canvas.clone() },
                    delay_ms: if delay_ms == 0 { 100 } else { delay_ms },
                });

                match disposal {
                    // Restore the area this frame covered to transparent.
                    2 => {
                        for row in 0..fh {
                            let dst_row = if interlaced { interlaced_row(row, fh) } else { row };
                            if fy + dst_row >= height { continue; }
                            for col in 0..fw {
                                if fx + col >= width { continue; }
                                let at = ((fy + dst_row) * width + (fx + col)) * 4;
                                canvas[at..at + 4].copy_from_slice(&[0, 0, 0, 0]);
                            }
                        }
                    }
                    3 => canvas = previous,
                    _ => {}
                }

                delay_ms = 0;
                transparent = None;
                disposal = 0;
            }
            0x21 => {
                let label = r.u8()?;
                if label == 0xF9 {
                    let len = r.u8()? as usize;
                    if len < 4 {
                        return Err("gif: graphic control block is too short".into());
                    }
                    let packed = r.u8()?;
                    // Delay is in hundredths of a second.
                    delay_ms = r.u16()? as u32 * 10;
                    let t = r.u8()?;
                    transparent = (packed & 1 != 0).then_some(t);
                    disposal = (packed >> 2) & 7;
                    for _ in 4..len { r.u8()?; }
                    r.blocks()?; // the block terminator
                } else {
                    r.blocks()?;
                }
            }
            0x3B => break,
            other => return Err(format!("gif: unexpected block introducer {other:#04x}")),
        }
    }

    if frames.is_empty() {
        return Err("gif: no frames".into());
    }
    Ok(frames)
}

/// GIF interlacing: rows every 8 from 0, every 8 from 4, every 4 from 2, then
/// every 2 from 1.
fn interlaced_row(row: usize, height: usize) -> usize {
    let p1 = (height + 7) / 8;
    let p2 = (height + 3) / 8;
    let p3 = (height + 1) / 4;
    if row < p1 { row * 8 }
    else if row < p1 + p2 { (row - p1) * 8 + 4 }
    else if row < p1 + p2 + p3 { (row - p1 - p2) * 4 + 2 }
    else { (row - p1 - p2 - p3) * 2 + 1 }
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Result<u8, String> {
        let b = *self.data.get(self.pos).ok_or("gif: file ended early")?;
        self.pos += 1;
        Ok(b)
    }
    fn u16(&mut self) -> Result<u16, String> {
        Ok(self.u8()? as u16 | (self.u8()? as u16) << 8)
    }
    fn palette(&mut self, entries: usize) -> Result<Vec<[u8; 3]>, String> {
        let end = self.pos + entries * 3;
        let raw = self.data.get(self.pos..end).ok_or("gif: colour table ended early")?;
        self.pos = end;
        Ok(raw.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect())
    }
    /// Read a chain of length-prefixed sub-blocks, ending at a zero length.
    fn blocks(&mut self) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        loop {
            let n = self.u8()? as usize;
            if n == 0 {
                return Ok(out);
            }
            let end = self.pos + n;
            out.extend_from_slice(self.data.get(self.pos..end).ok_or("gif: sub-block ended early")?);
            self.pos = end;
        }
    }
}

/// GIF's LZW: variable-width codes, least-significant-bit first, with a
/// dictionary that resets when it is told to and whenever it fills.
fn lzw(data: &[u8], min_code_size: u8, expected: usize) -> Result<Vec<u8>, String> {
    if !(2..=11).contains(&min_code_size) {
        return Err(format!("gif: bad LZW code size {min_code_size}"));
    }
    let clear = 1u16 << min_code_size;
    let end = clear + 1;

    let mut dict: Vec<Vec<u8>> = Vec::new();
    let reset = |dict: &mut Vec<Vec<u8>>| {
        dict.clear();
        for i in 0..clear {
            dict.push(vec![i as u8]);
        }
        dict.push(Vec::new()); // clear
        dict.push(Vec::new()); // end
    };
    reset(&mut dict);

    let mut width = min_code_size as u32 + 1;
    let mut out: Vec<u8> = Vec::with_capacity(expected);
    let mut prev: Option<u16> = None;
    let (mut acc, mut bits, mut pos) = (0u32, 0u32, 0usize);

    loop {
        while bits < width {
            let Some(&byte) = data.get(pos) else {
                // Truncated streams are common in the wild; pad rather than
                // lose the frame entirely.
                return Ok(pad(out, expected));
            };
            pos += 1;
            acc |= (byte as u32) << bits;
            bits += 8;
        }
        let code = (acc & ((1 << width) - 1)) as u16;
        acc >>= width;
        bits -= width;

        if code == clear {
            reset(&mut dict);
            width = min_code_size as u32 + 1;
            prev = None;
            continue;
        }
        if code == end {
            return Ok(pad(out, expected));
        }

        let entry = if (code as usize) < dict.len() {
            dict[code as usize].clone()
        } else if let Some(p) = prev {
            // The one legal forward reference: a code defined by this very
            // step, which is always the previous entry plus its own first byte.
            let mut e = dict[p as usize].clone();
            e.push(dict[p as usize][0]);
            e
        } else {
            return Err("gif: LZW stream starts with an undefined code".into());
        };

        out.extend_from_slice(&entry);
        if let Some(p) = prev {
            let mut nw = dict[p as usize].clone();
            nw.push(entry[0]);
            dict.push(nw);
            if dict.len() == (1 << width) && width < 12 {
                width += 1;
            }
        }
        prev = Some(code);

        if out.len() >= expected {
            return Ok(pad(out, expected));
        }
    }
}

fn pad(mut out: Vec<u8>, expected: usize) -> Vec<u8> {
    out.resize(expected, 0);
    out
}
