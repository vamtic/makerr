//! Exhaustive-ish parity proof for `tlm_core::compose::blend_pixel`.
//!
//! The pipeline's unchanged-frame hardlink optimization relies on the
//! in-place painter producing *exactly* the same pixels as
//! `image::imageops::overlay` (which uses `image`'s private float
//! `Blend for Rgba<u8>`). A one-off rounding difference would silently
//! corrupt frames that ffmpeg consumes, so this test compares our integer
//! blend against the real float implementation from the `image` crate.
//!
//! To reach the private `Blend` trait we depend on `image_parity`: a
//! byte-identical copy of the same `image` version with `mod color` and
//! `pub trait Blend` made public (test-only dev-dependency, never shipped).
#![allow(clippy::all)] // exhaustive pixel sweep: lint noise is not useful here

use image_parity::color::Blend;
use image_parity::Rgba;
use tlm_core::compose::blend_pixel;

/// Reference: exactly what `imageops::overlay` does to one pixel.
fn reference_blend(dst: &mut [u8], src: &[u8]) {
    let mut d = Rgba([dst[0], dst[1], dst[2], dst[3]]);
    let s = Rgba([src[0], src[1], src[2], src[3]]);
    d.blend(&s);
    dst.copy_from_slice(&d.0);
}

#[test]
fn blend_parity_all_alpha_pairs() {
    // Every combination of source and destination alpha (the only inputs
    // that change the arithmetic structure), crossed with a deterministic
    // spread of channel values including the extremes and near-tie cases
    // where float rounding is most likely to disagree with integers.
    const COLORS: [u8; 9] = [0, 1, 2, 64, 127, 128, 129, 200, 255];
    for sa in 0u8..=255 {
        for ba in 0u8..=255 {
            for &sr in &COLORS {
                for &sg in &[0u8, 128, 255] {
                    for &dr in &COLORS {
                        for &dg in &[0u8, 63, 255] {
                            let src = [sr, sg, 255 - sr, sa];
                            let dst = [dr, dg, 128, ba];
                            let mut a = dst;
                            let mut b = dst;
                            blend_pixel(&mut a, &src);
                            reference_blend(&mut b, &src);
                            assert_eq!(a, b, "dst={dst:?} src={src:?}");
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn blend_parity_random_sweep() {
    // Cheap xorshift PRNG — no external rand dependency needed.
    let mut state: u64 = 0x9E3779B97F4A7C15;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 24) as u8
    };
    for _ in 0..200_000 {
        let src = [next(), next(), next(), next()];
        let dst = [next(), next(), next(), next()];
        let mut a = dst;
        let mut b = dst;
        blend_pixel(&mut a, &src);
        reference_blend(&mut b, &src);
        assert_eq!(a, b, "dst={dst:?} src={src:?}");
    }
}
