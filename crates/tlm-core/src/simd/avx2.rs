//! AVX2 blend kernel.
//!
//! Eight RGBA pixels (32 bytes) are classified at a time:
//! * every alpha is 0   → the whole block is a provable no-op;
//! * every alpha is 255 → 32-byte compare + copy (the opaque fast path);
//! * anything else      → the block is handed to the scalar reference blend,
//!   which keeps bit-exact parity with `image`'s float implementation.
//!
//! The classification uses `vpcmpeqb` on the alpha bytes only, so it never
//! touches the colour channels of partially transparent pixels.

use std::arch::x86_64::*;

use super::BlendKernel;
use crate::compose::blend_pixel;

pub(crate) struct Avx2Kernel {
    /// `0x80` in every alpha byte of the eight pixels (and 0 elsewhere).
    alpha_lanes: __m256i,
}

impl Default for Avx2Kernel {
    fn default() -> Self {
        // Safety: constant construction only; AVX2 registers are available
        // because `select_best_kernel` checks `is_x86_feature_detected!`.
        Self {
            alpha_lanes: unsafe { _mm256_set1_epi32(0x8000_0000u32 as i32) },
        }
    }
}

impl BlendKernel for Avx2Kernel {
    fn blend_rect(
        &self,
        dst: &mut [u8],
        dst_stride: usize,
        src: &[u8],
        src_stride: usize,
        width: usize,
        height: usize,
    ) -> bool {
        // Safety: every pointer derives from a slice whose length is checked
        // below; the target CPU supports AVX2 (selected by feature detection).
        unsafe { blend_rect_avx2(self, dst, dst_stride, src, src_stride, width, height) }
    }

    fn name(&self) -> &'static str {
        "avx2"
    }
}

#[target_feature(enable = "avx2")]
unsafe fn blend_rect_avx2(
    kernel: &Avx2Kernel,
    dst: &mut [u8],
    dst_stride: usize,
    src: &[u8],
    src_stride: usize,
    width: usize,
    height: usize,
) -> bool {
    debug_assert!(dst.len() >= dst_stride * height * 4);
    debug_assert!(src.len() >= src_stride * height * 4);
    let zero = _mm256_setzero_si256();
    let opaque = _mm256_set1_epi8(-1);
    let alpha_lanes = kernel.alpha_lanes;
    // Bit positions of the alpha bytes in a `movemask_epi8` result.
    const ALPHA_BITS: u32 = 0x8080_8080;
    let mut changed = false;

    for y in 0..height {
        let src_row = &src[y * src_stride * 4..][..width * 4];
        let dst_row = &mut dst[y * dst_stride * 4..][..width * 4];
        let mut x = 0usize;
        while x < width {
            if x + 8 <= width {
                let sp = src_row.as_ptr().add(x * 4) as *const __m256i;
                let dp = dst_row.as_mut_ptr().add(x * 4) as *mut __m256i;
                let sv = _mm256_loadu_si256(sp);
                let dv = _mm256_loadu_si256(dp);
                let eq_transparent = _mm256_cmpeq_epi8(sv, zero);
                let eq_opaque = _mm256_cmpeq_epi8(sv, opaque);
                let transparent_mask = _mm256_and_si256(eq_transparent, alpha_lanes);
                let opaque_mask = _mm256_and_si256(eq_opaque, alpha_lanes);

                if _mm256_movemask_epi8(transparent_mask) as u32 & ALPHA_BITS == ALPHA_BITS {
                    // All eight source pixels are fully transparent: the blend
                    // provably leaves the destination untouched.
                    x += 8;
                    continue;
                }
                if _mm256_movemask_epi8(opaque_mask) as u32 & ALPHA_BITS == ALPHA_BITS {
                    // Fully opaque: `blend_pixel` copies the whole pixel.
                    let differs =
                        _mm256_movemask_epi8(_mm256_cmpeq_epi8(sv, dv)) as u32 != u32::MAX;
                    if differs {
                        _mm256_storeu_si256(dp, sv);
                        changed = true;
                    }
                    x += 8;
                    continue;
                }

                // Mixed block: scalar reference semantics.
                let (dst_px, _) = dst_row[x * 4..][..32].as_chunks_mut::<4>();
                let (src_px, _) = src_row[x * 4..][..32].as_chunks::<4>();
                for (d, s) in dst_px.iter_mut().zip(src_px) {
                    if s[3] == 0 || (s[3] == 255 && *d == *s) {
                        continue;
                    }
                    let before = *d;
                    blend_pixel(d, s);
                    if *d != before {
                        changed = true;
                    }
                }
                x += 8;
            } else {
                let (dst_px, _) = dst_row[x * 4..].as_chunks_mut::<4>();
                let (src_px, _) = src_row[x * 4..].as_chunks::<4>();
                for (d, s) in dst_px.iter_mut().zip(src_px) {
                    if s[3] == 0 || (s[3] == 255 && *d == *s) {
                        continue;
                    }
                    let before = *d;
                    blend_pixel(d, s);
                    if *d != before {
                        changed = true;
                    }
                }
                break;
            }
        }
    }
    changed
}
