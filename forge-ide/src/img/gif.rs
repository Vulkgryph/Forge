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


pub fn looks_like_gif(bytes: &[u8]) -> bool {
    bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a")
}

/// One frame's metadata and its still-compressed pixel data.
///
/// The LZW bytes are kept rather than the pixels: a frame's compressed form is
/// a few kilobytes where its composited form is the whole canvas. For a
/// two-thousand-frame recording that is the difference between a few megabytes
/// and several gigabytes.
struct FrameMeta {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    interlaced: bool,
    transparent: Option<u8>,
    disposal: u8,
    delay_ms: u32,
    /// Empty when the frame uses the global colour table.
    local_palette: Vec<[u8; 3]>,
    min_code: u8,
    data: Vec<u8>,
}

/// A GIF, parsed but not yet drawn.
///
/// Frames are composited one at a time, on demand. The alternative — decoding
/// every frame up front — costs `width * height * 4` bytes *per frame* and a
/// texture upload each, which for a screen recording is gigabytes and a visible
/// stall before anything appears.
pub struct Animation {
    pub width: usize,
    pub height: usize,
    global: Vec<[u8; 3]>,
    frames: Vec<FrameMeta>,
    /// The composited picture, reused across frames.
    canvas: Vec<u8>,
    /// Kept only while a frame asks for "restore to previous".
    previous: Option<Vec<u8>>,
    /// What to do to the canvas before the next frame is drawn, and where.
    pending: Option<(u8, usize, usize, usize, usize)>,
    next: usize,
    /// Saved canvases, so seeking backwards does not replay from frame zero.
    ///
    /// A GIF frame is a patch over the one before it, so the picture at frame
    /// *n* is the sum of everything up to *n*. Without these, dragging a scrub
    /// bar backwards on a two-thousand-frame recording re-composites two
    /// thousand frames for every pixel of movement.
    keyframes: Vec<Keyframe>,
    /// One saved canvas every `keyframe_every` frames, chosen so the whole
    /// cache stays within a budget rather than scaling with the file.
    keyframe_every: usize,
}

/// Everything needed to resume compositing from a point.
struct Keyframe {
    /// The frame index this state is *before* drawing.
    next: usize,
    canvas: Vec<u8>,
    previous: Option<Vec<u8>>,
    pending: Option<(u8, usize, usize, usize, usize)>,
}

impl Animation {
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// How long frame `i` is shown. Zero delays are normalised to 100ms, which
    /// is what browsers do with the many GIFs that ask for zero.
    pub fn delay_ms(&self, i: usize) -> u32 {
        self.frames.get(i).map_or(100, |f| if f.delay_ms == 0 { 100 } else { f.delay_ms })
    }

    /// Which frame is due `elapsed_ms` into the loop.
    pub fn frame_at(&self, elapsed_ms: u64) -> usize {
        let total: u64 = (0..self.frames.len()).map(|i| self.delay_ms(i) as u64).sum::<u64>().max(1);
        let mut at = elapsed_ms % total;
        for i in 0..self.frames.len() {
            let d = self.delay_ms(i) as u64;
            if at < d {
                return i;
            }
            at -= d;
        }
        self.frames.len().saturating_sub(1)
    }

    /// How much memory the seek cache is holding, for the test that keeps it
    /// bounded.
    #[cfg(test)]
    pub fn keyframe_bytes(&self) -> usize {
        self.keyframes.iter().map(|k| k.canvas.len() + k.previous.as_ref().map_or(0, |p| p.len())).sum()
    }

    /// Total run time of one loop.
    pub fn total_ms(&self) -> u64 {
        (0..self.frames.len()).map(|i| self.delay_ms(i) as u64).sum::<u64>().max(1)
    }

    /// Composite until `target` is the picture on the canvas.
    ///
    /// Going forward is just more compositing. Going backwards restarts from
    /// the nearest saved canvas at or before the target, which bounds the work
    /// to `keyframe_every` frames however far back the seek goes — without
    /// that, dragging a scrub bar backwards through a long recording
    /// re-composites everything from the beginning on every movement.
    pub fn seek(&mut self, target: usize) -> Result<&[u8], String> {
        let target = target.min(self.frames.len().saturating_sub(1));
        if target < self.next.saturating_sub(1) {
            self.restore_before(target);
        }
        while self.next <= target {
            self.step()?;
        }
        Ok(&self.canvas)
    }

    /// Rewind to the nearest saved state at or before `target`.
    fn restore_before(&mut self, target: usize) {
        let best = self.keyframes.iter().rposition(|k| k.next <= target);
        match best {
            Some(i) => {
                let k = &self.keyframes[i];
                self.canvas.copy_from_slice(&k.canvas);
                self.previous = k.previous.clone();
                self.pending = k.pending;
                self.next = k.next;
            }
            None => {
                self.canvas.iter_mut().for_each(|b| *b = 0);
                self.previous = None;
                self.pending = None;
                self.next = 0;
            }
        }
    }

    /// Save the state before frame `next`, if this is a keyframe boundary.
    fn maybe_save_keyframe(&mut self) {
        if self.keyframe_every == 0 || self.next % self.keyframe_every != 0 {
            return;
        }
        if self.keyframes.iter().any(|k| k.next == self.next) {
            return;
        }
        self.keyframes.push(Keyframe {
            next: self.next,
            canvas: self.canvas.clone(),
            previous: self.previous.clone(),
            pending: self.pending,
        });
        self.keyframes.sort_by_key(|k| k.next);
    }

    /// Draw the next frame onto the canvas.
    fn step(&mut self) -> Result<(), String> {
        self.maybe_save_keyframe();
        // Whatever the previous frame asked to happen afterwards.
        if let Some((disposal, x, y, w, h)) = self.pending.take() {
            match disposal {
                2 => self.clear_rect(x, y, w, h),
                3 => {
                    if let Some(prev) = self.previous.take() {
                        self.canvas = prev;
                    }
                }
                _ => {}
            }
        }

        let i = self.next;
        let f = self.frames.get(i).ok_or("gif: frame index past the end")?;
        let palette = if f.local_palette.is_empty() { &self.global } else { &f.local_palette };
        if palette.is_empty() {
            return Err("gif: frame has no colour table".into());
        }
        let indices = lzw(&f.data, f.min_code, f.w * f.h)?;

        if f.disposal == 3 {
            self.previous = Some(self.canvas.clone());
        }

        for row in 0..f.h {
            let dst_row = if f.interlaced { interlaced_row(row, f.h) } else { row };
            if f.y + dst_row >= self.height {
                continue;
            }
            for col in 0..f.w {
                if f.x + col >= self.width {
                    continue;
                }
                let idx = indices[row * f.w + col];
                if Some(idx) == f.transparent {
                    continue;
                }
                let c = palette.get(idx as usize).copied().unwrap_or([0, 0, 0]);
                let at = ((f.y + dst_row) * self.width + (f.x + col)) * 4;
                self.canvas[at..at + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
            }
        }

        self.pending = Some((f.disposal, f.x, f.y, f.w, f.h));
        self.next = i + 1;
        Ok(())
    }

    fn clear_rect(&mut self, x: usize, y: usize, w: usize, h: usize) {
        for row in y..(y + h).min(self.height) {
            for col in x..(x + w).min(self.width) {
                let at = (row * self.width + col) * 4;
                self.canvas[at..at + 4].copy_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
}

/// Read the structure — sizes, palettes, timing, and each frame's compressed
/// bytes — without decoding any pixels.
pub fn parse(bytes: &[u8]) -> Result<Animation, String> {
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

    let mut frames: Vec<FrameMeta> = Vec::new();

    // Values from the most recent Graphic Control Extension, which applies to
    // the next image block only.
    let mut delay_ms = 0u32;
    let mut transparent: Option<u8> = None;
    let mut disposal = 0u8;
    let _ = background;

    loop {
        match r.u8()? {
            0x2C => {
                let x = r.u16()? as usize;
                let y = r.u16()? as usize;
                let w = r.u16()? as usize;
                let h = r.u16()? as usize;
                let f = r.u8()?;
                let local_palette: Vec<[u8; 3]> = if f & 0x80 != 0 {
                    r.palette(2usize << (f & 7))?
                } else {
                    Vec::new()
                };
                let interlaced = f & 0x40 != 0;
                let min_code = r.u8()?;
                let data = r.blocks()?;

                frames.push(FrameMeta {
                    x, y, w, h, interlaced, transparent, disposal, delay_ms,
                    local_palette, min_code, data,
                });

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
                    for _ in 4..len {
                        r.u8()?;
                    }
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
    // Keep the saved canvases under about 24 MB whatever the recording's
    // length: one every `keyframe_every` frames, never more often than every
    // eight, so a short GIF does not pay for a cache it cannot use.
    const KEYFRAME_BUDGET: usize = 24 * 1024 * 1024;
    let frame_bytes = width * height * 4;
    let affordable = (KEYFRAME_BUDGET / frame_bytes.max(1)).max(1);
    let keyframe_every = (frames.len() / affordable).max(8);

    Ok(Animation {
        width,
        height,
        global,
        frames,
        canvas: vec![0u8; width * height * 4],
        previous: None,
        pending: None,
        next: 0,
        keyframes: Vec::new(),
        keyframe_every,
    })
}/// GIF interlacing: rows every 8 from 0, every 8 from 4, every 4 from 2, then
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
