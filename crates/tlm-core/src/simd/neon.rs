//! NEON blend kernel for aarch64.
//!
//! Mirrors the AVX2 kernel four pixels at a time: fully transparent blocks are
//! skipped, fully opaque blocks are compared and copied with one 16-byte
//! vector, and anything else uses the scalar reference blend to preserve exact
//! parity with `image`'s float src-over.

use std::arch::aarch64::*;

use super::BlendKernel;
use crate::compose::blend_pixel;

pub(crate) struct NeonKernel;

impl BlendKernel for NeonKernel {
    fn blend_rect(
        &self,
        dst: &mut [u8],
        dst_stride: usize,
        src: &[u8],
        src_stride: usize,
        width: usize,
        height: usize,
    ) -> bool {
        // NEON is guaranteed on aarch64.
        unsafe { blend_rect_neon(dst, dst_stride, src, src_stride, width, height) }
    }

    fn name(&self) -> &'static str {
        "neon"
    }
}

#[target_feature(enable = "neon")]
unsafe fn blend_rect_neon(
    dst: &mut [u8],
    dst_stride: usize,
    src: &[u8],
    src_stride: usize,
    width: usize,
    height: usize,
) -> bool {
    debug_assert!(dst.len() >= dst_stride * height * 4);
    debug_assert!(src.len() >= src_stride * height * 4);
    let zero = vdupq_n_u8(0);
    let opaque = vdupq_n_u8(0xFF);
    // 0xFF in every alpha byte of the four pixels.
    let alpha_lanes = vreinterpretq_u8_u32(vdupq_n_u32(0xFF00_0000));
    let mut changed = false;

    for y in 0..height {
        let src_row = &src[y * src_stride * 4..][..width * 4];
        let dst_row = &mut dst[y * dst_stride * 4..][..width * 4];
        let mut x = 0usize;
        while x < width {
            if x + 4 <= width {
                let sp = src_row.as_ptr().add(x * 4);
                let dp = dst_row.as_mut_ptr().add(x * 4);
                let sv = vld1q_u8(sp);
                let dv = vld1q_u8(dp);
                let transparent = vandq_u8(vceqq_u8(sv, zero), alpha_lanes);
                let opaque_lanes = vandq_u8(vceqq_u8(sv, opaque), alpha_lanes);

                if vminvq_u8(transparent) == 0xFF {
                    x += 4;
                    continue;
                }
                if vminvq_u8(opaque_lanes) == 0xFF {
                    if vminvq_u8(vceqq_u8(sv, dv)) != 0xFF {
                        vst1q_u8(dp, sv);
                        changed = true;
                    }
                    x += 4;
                    continue;
                }

                let (dst_px, _) = dst_row[x * 4..][..16].as_chunks_mut::<4>();
                let (src_px, _) = src_row[x * 4..][..16].as_chunks::<4>();
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
                x += 4;
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
