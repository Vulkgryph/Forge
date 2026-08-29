// SPDX-License-Identifier: Apache-2.0
//! Small PNGs covering the paths a screenshot does not reach: greyscale, a
//! 4-bit palette with `tRNS` transparency, and 16-bit samples with the Paeth
//! filter. Written by hand with zlib and struct packing, so the fixtures do
//! not depend on an encoder having produced them.
#![cfg(test)]

pub fn pngs() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("greyscale 8-bit, Sub filter", include_bytes!("testdata/gray8.png").to_vec()),
        ("4-bit palette with tRNS",     include_bytes!("testdata/pal4.png").to_vec()),
        ("16-bit RGBA, Paeth filter",   include_bytes!("testdata/rgba16.png").to_vec()),
    ]
}
