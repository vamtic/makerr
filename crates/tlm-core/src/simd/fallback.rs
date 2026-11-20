//! Portable fallback blend kernel.

use super::BlendKernel;
use crate::compose::blend_pixel;

pub(crate) struct ScalarKernel;

impl BlendKernel for ScalarKernel {
    fn blend_rect(
        &self,
        dst: &mut [u8],
        dst_stride: usize,
        src: &[u8],
        src_stride: usize,
        width: usize,
        height: usize,
    ) -> bool {
        let mut changed = false;
        for y in 0..height {
            let src_row = &src[y * src_stride * 4..][..width * 4];
            let dst_row = &mut dst[y * dst_stride * 4..][..width * 4];
            let (dst_px, _) = dst_row.as_chunks_mut::<4>();
            let (src_px, _) = src_row.as_chunks::<4>();
            for (dp, sp) in dst_px.iter_mut().zip(src_px) {
                if sp[3] == 0 {
                    continue;
                }
                // Opaque identical pixels are provably inert. An identical
                // *semi-transparent* pixel is not (see compose::blend_pixel).
                if sp[3] == 255 && *dp == *sp {
                    continue;
                }
                let before = *dp;
                blend_pixel(dp, sp);
                if *dp != before {
                    changed = true;
                }
            }
        }
        changed
    }

    fn name(&self) -> &'static str {
        "scalar"
    }
}
