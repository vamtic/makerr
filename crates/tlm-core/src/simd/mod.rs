//! SIMD-accelerated source-over blending kernels.
//!
//! The pipeline's unchanged-frame bookkeeping depends on bit-exact parity with
//! `image`'s float `Blend` implementation (see [`crate::compose::blend_pixel`]).
//! These kernels therefore never change the arithmetic: every block that is not
//! provably uniform falls back to the scalar `blend_pixel`. What the SIMD paths
//! accelerate is the overwhelmingly common pixel-art case — runs of up to eight
//! fully transparent (leave the destination untouched) or fully opaque (replace
//! the destination) pixels — using wide compares and 32-byte copies.

#[cfg(target_arch = "x86_64")]
mod avx2;
mod fallback;
#[cfg(target_arch = "aarch64")]
mod neon;

use std::sync::OnceLock;

/// A source-over blend implementation over RGBA8 rows.
pub trait BlendKernel: Send + Sync {
    /// Blends `src` (RGBA8, `src_stride` pixels per row) onto `dst` in place.
    ///
    /// `width`/`height` are in pixels. Returns `true` if at least one
    /// destination pixel changed.
    fn blend_rect(
        &self,
        dst: &mut [u8],
        dst_stride: usize,
        src: &[u8],
        src_stride: usize,
        width: usize,
        height: usize,
    ) -> bool;

    /// Human-readable kernel name (`avx2`, `scalar`, …), for logs and tests.
    fn name(&self) -> &'static str;
}

/// Picks the fastest kernel the CPU actually supports.
pub fn select_best_kernel() -> Box<dyn BlendKernel> {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            return Box::new(avx2::Avx2Kernel::default());
        }
        Box::new(fallback::ScalarKernel)
    }
    #[cfg(target_arch = "aarch64")]
    {
        // NEON is baseline on aarch64.
        Box::new(neon::NeonKernel)
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        Box::new(fallback::ScalarKernel)
    }
}

/// The process-wide kernel, selected once.
pub fn kernel() -> &'static dyn BlendKernel {
    static KERNEL: OnceLock<Box<dyn BlendKernel>> = OnceLock::new();
    KERNEL.get_or_init(select_best_kernel).as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::blend_pixel;

    /// Deterministic xorshift PRNG, so the parity sweep needs no dependency.
    struct Rng(u64);

    impl Rng {
        fn next_u8(&mut self) -> u8 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 24) as u8
        }
    }

    /// The kernels must agree with the scalar reference *exactly*, including
    /// the transient change flag.
    fn assert_parity(kernel: &dyn BlendKernel) {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for case in 0..2_000 {
            let width = 1 + (rng.next_u8() % 37) as usize;
            let height = 1 + (rng.next_u8() % 11) as usize;
            let src_stride = width + (rng.next_u8() % 5) as usize;
            let dst_stride = width + (rng.next_u8() % 5) as usize;

            let mut src = vec![0u8; src_stride * height * 4];
            let mut dst_a = vec![0u8; dst_stride * height * 4];
            for b in src.iter_mut() {
                *b = rng.next_u8();
            }
            // Pepper in uniform runs: the real fast paths.
            if case % 3 == 0 {
                for px in src.as_chunks_mut::<4>().0 {
                    px[3] = 0;
                }
            } else if case % 3 == 1 {
                for px in src.as_chunks_mut::<4>().0 {
                    px[3] = 255;
                }
            }
            for b in dst_a.iter_mut() {
                *b = rng.next_u8();
            }
            let mut dst_b = dst_a.clone();

            let mut expected_changed = false;
            for y in 0..height {
                for x in 0..width {
                    let sp = (y * src_stride + x) * 4;
                    let dp = (y * dst_stride + x) * 4;
                    let before = [dst_a[dp], dst_a[dp + 1], dst_a[dp + 2], dst_a[dp + 3]];
                    let mut px = before;
                    blend_pixel(&mut px, &[src[sp], src[sp + 1], src[sp + 2], src[sp + 3]]);
                    dst_a[dp..dp + 4].copy_from_slice(&px);
                    if px != before {
                        expected_changed = true;
                    }
                }
            }

            let changed =
                kernel.blend_rect(&mut dst_b, dst_stride, &src, src_stride, width, height);
            assert_eq!(changed, expected_changed, "change flag, case {case}");
            assert_eq!(dst_a, dst_b, "pixels, case {case}");
        }
    }

    #[test]
    fn scalar_kernel_matches_reference() {
        assert_parity(&fallback::ScalarKernel);
    }

    #[test]
    fn selected_kernel_matches_reference() {
        for k in [
            select_best_kernel(),
            Box::new(fallback::ScalarKernel) as Box<dyn BlendKernel>,
        ] {
            assert_parity(k.as_ref());
        }
    }
}
