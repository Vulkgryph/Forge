// SPDX-License-Identifier: Apache-2.0
//! Image decoding, written here rather than taken from a crate.
//!
//! Forge decodes two formats — PNG because screenshots and icons are PNGs, and
//! GIF because a recording of a terminal is a GIF. Both are frozen, fully
//! specified formats, and between them they are about a thousand lines
//! including the DEFLATE decoder PNG needs. That is the whole cost, and in
//! exchange the editor carries no image dependency and can be read end to end.
//!
//! Decoding only, and to one representation: 8-bit RGBA, which is what a GPU
//! texture wants.

pub mod gif;
pub mod inflate;
pub mod png;
mod testdata;

/// A decoded still, as straight RGBA rows.
#[derive(Clone, PartialEq)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    /// `width * height * 4` bytes, row-major, no padding.
    pub rgba: Vec<u8>,
}

pub use gif::Animation;

/// What a file turned out to be.
pub enum Decoded {
    Still(Image),
    /// Parsed, but not yet drawn: frames are composited on demand. Decoding
    /// every frame of a screen recording up front would cost gigabytes.
    Animated(Animation),
}

/// Whether Forge can show this file, by extension.
pub fn is_supported_ext(ext: &str) -> bool {
    matches!(ext.to_ascii_lowercase().as_str(), "png" | "gif")
}

/// Decode by content rather than by extension, so a mislabelled file still
/// opens and a `.png` that is really a GIF does the right thing.
pub fn decode(bytes: &[u8]) -> Result<Decoded, String> {
    if png::looks_like_png(bytes) {
        return Ok(Decoded::Still(png::decode(bytes)?));
    }
    if gif::looks_like_gif(bytes) {
        return Ok(Decoded::Animated(gif::parse(bytes)?));
    }
    Err("unrecognised image format (Forge decodes PNG and GIF)".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FNV-1a over the decoded pixels. A digest rather than the pixels
    /// themselves because five megabytes of expected output does not belong in
    /// a source file, and any difference at all changes it.
    fn digest(bytes: &[u8]) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    /// The digests below were taken from the `image` crate, decoding these
    /// exact files, while both decoders were in the tree. Every one matched
    /// ours byte for byte — the screenshot is 25 megabytes of RGBA and not one
    /// byte differed — so the crate was removed and its verdict kept.
    ///
    /// That is what makes these numbers worth having: they are not what our
    /// decoder happened to produce, they are what an independent implementation
    /// produced and ours agreed with.
    #[test]
    fn png_still_decodes_what_the_reference_decoder_did() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let bytes = std::fs::read(root.join("assets/forge-ide.png")).expect("the screenshot");
        let ours = png::decode(&bytes).expect("our decoder");
        assert_eq!((ours.width, ours.height), (3024, 2088));
        assert_eq!(digest(&ours.rgba), 0x6f05ff15df381154, "pixels differ from the reference");
    }

    /// The paths a screenshot does not reach: greyscale with the Sub filter, a
    /// 4-bit palette with `tRNS` transparency, and 16-bit samples with Paeth.
    #[test]
    fn png_handles_the_other_colour_types() {
        let expected: [(u64, usize, usize); 3] = [
            (0x547eeaa7a06010fc, 4, 2),
            (0xe6e5d00b72ad5229, 4, 2),
            (0x7954fbf8dcc83a09, 3, 2),
        ];
        for ((name, bytes), (want, w, h)) in testdata::pngs().into_iter().zip(expected) {
            let ours = png::decode(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!((ours.width, ours.height), (w, h), "{name}");
            assert_eq!(digest(&ours.rgba), want, "{name}: pixels differ from the reference");
        }
    }

    /// A malformed file must be refused, not panic — these decoders are handed
    /// whatever a user opens.
    #[test]
    fn rubbish_is_refused_rather_than_fatal() {
        assert!(decode(b"").is_err());
        assert!(decode(b"not an image at all").is_err());
        // A valid signature followed by nothing.
        assert!(png::decode(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]).is_err());
        assert!(gif::parse(b"GIF89a").is_err());
        // A truncated PNG: real header, body cut off mid-stream.
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let full = std::fs::read(root.join("assets/forge-ide.png")).unwrap();
        assert!(png::decode(&full[..full.len() / 3]).is_err());
    }

    /// The GIF the terminal recording produced: every frame composited to full
    /// size, so a viewer can show frame n without replaying the ones before it.
    #[test]
    fn gif_decodes_every_frame_at_full_size() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let path = root.parent().unwrap().join("forge-tui-rs/assets/forge-tui-demo.gif");
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("no demo gif in the tree; skipping");
            return;
        };
        let mut anim = gif::parse(&bytes).expect("our decoder");
        assert!(anim.frame_count() > 100, "only {} frames", anim.frame_count());
        assert_eq!((anim.width, anim.height), (896, 454));

        // Walk the whole animation, one frame at a time. The canvas is reused,
        // so this holds one frame's worth of pixels however many there are —
        // decoding them all at once would be gigabytes.
        let count = anim.frame_count();
        for i in 0..count {
            let canvas = anim.seek(i).unwrap_or_else(|e| panic!("frame {i}: {e}"));
            assert_eq!(canvas.len(), anim.width * anim.height * 4, "frame {i} is not full size");
            assert!(anim.delay_ms(i) > 0, "frame {i} has no delay");
        }

        // Seeking backwards replays from the start rather than showing a
        // half-composited picture.
        let first_again = anim.seek(0).expect("rewind").to_vec();
        let first = { let mut a = gif::parse(&bytes).unwrap(); a.seek(0).unwrap().to_vec() };
        assert_eq!(first_again, first, "seeking backwards did not reproduce frame 0");
    }

    /// Scrubbing backwards has to land on exactly the picture that playing
    /// forwards would show. A GIF frame is a patch over its predecessor, so a
    /// seek that resumes from the wrong saved state produces a plausible but
    /// wrong image — smeared remnants of frames that should have been disposed
    /// of — and nothing about it looks like an error.
    ///
    /// So: play forwards recording what each frame looks like, then jump around
    /// out of order and demand the same pixels back.
    #[test]
    fn seeking_backwards_matches_playing_forwards() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let path = root.parent().unwrap().join("forge-tui-rs/assets/forge-tui-demo.gif");
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("no demo gif in the tree; skipping");
            return;
        };

        // Forward pass: the truth, one frame at a time.
        let mut forward = gif::parse(&bytes).expect("parse");
        let count = forward.frame_count();
        let probes: Vec<usize> = vec![0, 1, 7, count / 4, count / 2, count - 2, count - 1];
        let mut expected = std::collections::HashMap::new();
        for i in 0..count {
            let canvas = forward.seek(i).expect("forward");
            if probes.contains(&i) {
                expected.insert(i, digest(canvas));
            }
        }

        // Now jump about: backwards, forwards, and repeated.
        let mut scrub = gif::parse(&bytes).expect("parse");
        let order = [count - 1, 0, count / 2, 7, count - 2, 1, count / 4, 0, count / 2];
        for &i in &order {
            let got = digest(scrub.seek(i).expect("seek"));
            if let Some(&want) = expected.get(&i) {
                assert_eq!(got, want, "frame {i} differs when reached by seeking");
            }
        }
    }

    /// The cache has to stay bounded: it exists so a long recording can be
    /// scrubbed, not so a long recording can exhaust memory.
    #[test]
    fn the_keyframe_cache_is_bounded() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let path = root.parent().unwrap().join("forge-tui-rs/assets/forge-tui-demo.gif");
        let Ok(bytes) = std::fs::read(&path) else { return };
        let mut anim = gif::parse(&bytes).expect("parse");
        let count = anim.frame_count();
        for i in 0..count {
            anim.seek(i).expect("play through");
        }
        let held = anim.keyframe_bytes();
        assert!(
            held <= 32 * 1024 * 1024,
            "keyframes hold {:.1} MB after playing {count} frames", held as f64 / 1e6,
        );
    }

    /// Round-trip through our own inflate: a stored block, and a compressed one.
    #[test]
    fn inflate_handles_both_block_kinds() {
        // "hello hello hello" at level 9: a Huffman block whose back-reference
        // overlaps the output being written, which is how DEFLATE encodes a
        // repeat and the case a naive copy gets wrong.
        let compressed: &[u8] = &[
            0x78, 0xda, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x90, 0x00,
            0x3a, 0x2e, 0x06, 0x7d,
        ];
        assert_eq!(
            inflate::zlib_decompress(compressed).expect("inflate"),
            b"hello hello hello",
        );

        // The same text at level 0: a stored block, which is a different path
        // entirely — byte-aligned, with a length and its complement.
        let stored: &[u8] = &[
            0x78, 0x01, 0x01, 0x11, 0x00, 0xee, 0xff, 0x68, 0x65, 0x6c, 0x6c, 0x6f,
            0x20, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x20, 0x68, 0x65, 0x6c, 0x6c, 0x6f,
            0x3a, 0x2e, 0x06, 0x7d,
        ];
        assert_eq!(
            inflate::zlib_decompress(stored).expect("stored block"),
            b"hello hello hello",
        );
        // A corrupted checksum must be refused rather than returning wrong data.
        let mut bad = compressed.to_vec();
        let last = bad.len() - 1;
        bad[last] ^= 0xff;
        assert!(inflate::zlib_decompress(&bad).is_err());
    }
}
