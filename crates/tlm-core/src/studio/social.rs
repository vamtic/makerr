//! Social export presets and letterboxing.
//!
//! A vertical (9:16) export centres the selected region and fills the top and
//! bottom with a pixelated, zoomed mirror of the artwork instead of black bars.
//! Everything is deterministic integer sampling, so pixel art stays crisp and
//! every frame costs a single pass over the output buffer.

use image::RgbaImage;

use crate::settings::SocialPreset;

/// The output dimensions for a preset, always at least as large as the source
/// (so the region is never downscaled).
pub fn target_dimensions(preset: SocialPreset, width: u32, height: u32) -> (u32, u32) {
    let (w, h) = (width.max(1), height.max(1));
    match preset {
        SocialPreset::Source => (w, h),
        SocialPreset::Vertical916 => {
            // Exact 9:16 with even dimensions: heights are rounded to a
            // multiple of 32, so the matching width (h * 9 / 16) is even too.
            let out_h = round_up(h.max(div_ceil(w.saturating_mul(16), 9)), 32);
            let out_w = out_h / 16 * 9;
            (out_w, out_h)
        }
        SocialPreset::Square11 => {
            let side = round_up(w.max(h), 2);
            (side, side)
        }
        SocialPreset::Wide169 => {
            // Exact 16:9 with even dimensions (width multiple of 32).
            let out_w = round_up(w.max(div_ceil(h.saturating_mul(16), 9)), 32);
            let out_h = out_w / 16 * 9;
            (out_w, out_h)
        }
    }
}

/// Renders `src` into the preset's frame.
pub fn apply_social(src: &RgbaImage, preset: SocialPreset) -> RgbaImage {
    if preset == SocialPreset::Source || src.width() == 0 || src.height() == 0 {
        return src.clone();
    }
    let (tw, th) = target_dimensions(preset, src.width(), src.height());
    let mut out = letterbox_background(src, tw, th);
    blit_centered(&mut out, src);
    out
}

/// Fills the frame with a heavily downscaled cover-crop of `src`, upscaled with
/// nearest-neighbour. The small working copy is what makes this a cheap
/// "blur" (a mosaic that still reads as the artwork) instead of a real
/// per-frame gaussian pass.
fn letterbox_background(src: &RgbaImage, tw: u32, th: u32) -> RgbaImage {
    let small_w = (tw / 12).max(2);
    let small_h = (th / 12).max(2);
    let mut small = RgbaImage::new(small_w, small_h);
    let (sw, sh) = (src.width() as f64, src.height() as f64);
    let scale = (small_w as f64 / sw).max(small_h as f64 / sh);
    let off_x = (sw - small_w as f64 / scale) / 2.0;
    let off_y = (sh - small_h as f64 / scale) / 2.0;
    for y in 0..small_h {
        for x in 0..small_w {
            let sx = (off_x + x as f64 / scale).floor().clamp(0.0, sw - 1.0) as u32;
            let sy = (off_y + y as f64 / scale).floor().clamp(0.0, sh - 1.0) as u32;
            let mut px = *src.get_pixel(sx, sy);
            // Darken the backdrop so the centred region stays the focus.
            px[0] = (px[0] as u16 * 2 / 3) as u8;
            px[1] = (px[1] as u16 * 2 / 3) as u8;
            px[2] = (px[2] as u16 * 2 / 3) as u8;
            px[3] = 255;
            small.put_pixel(x, y, px);
        }
    }
    let mut out = RgbaImage::new(tw, th);
    for y in 0..th {
        let sy = (y as u64 * small_h as u64 / th as u64) as u32;
        for x in 0..tw {
            let sx = (x as u64 * small_w as u64 / tw as u64) as u32;
            out.put_pixel(
                x,
                y,
                *small.get_pixel(sx.min(small_w - 1), sy.min(small_h - 1)),
            );
        }
    }
    out
}

fn blit_centered(dst: &mut RgbaImage, src: &RgbaImage) {
    let x0 = (dst.width().saturating_sub(src.width())) / 2;
    let y0 = (dst.height().saturating_sub(src.height())) / 2;
    let copy_w = src.width().min(dst.width());
    let copy_h = src.height().min(dst.height());
    for y in 0..copy_h {
        for x in 0..copy_w {
            dst.put_pixel(x0 + x, y0 + y, *src.get_pixel(x, y));
        }
    }
}

fn div_ceil(a: u32, b: u32) -> u32 {
    a.div_euclid(b) + u32::from(a.rem_euclid(b) != 0)
}

/// Rounds `v` up to the next multiple of `step`.
fn round_up(v: u32, step: u32) -> u32 {
    let rem = v % step;
    if rem == 0 {
        v
    } else {
        v.saturating_add(step - rem)
    }
}

/// True when the preset needs a different frame size than the region.
pub fn changes_dimensions(preset: SocialPreset, width: u32, height: u32) -> bool {
    let (tw, th) = target_dimensions(preset, width, height);
    tw != width.max(1) || th != height.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    fn gradient(w: u32, h: u32) -> RgbaImage {
        let mut img = RgbaImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                img.put_pixel(x, y, Rgba([(x % 256) as u8, (y % 256) as u8, 40, 255]));
            }
        }
        img
    }

    #[test]
    fn source_preset_is_identity() {
        let src = gradient(64, 32);
        assert_eq!(apply_social(&src, SocialPreset::Source), src);
        assert!(!changes_dimensions(SocialPreset::Source, 64, 32));
    }

    #[test]
    fn vertical_preset_is_nine_by_sixteen_and_keeps_the_source() {
        let src = gradient(64, 32);
        let out = apply_social(&src, SocialPreset::Vertical916);
        let (w, h) = (out.width(), out.height());
        assert!(w >= 64 && h >= 32);
        // 9:16 within one pixel of even rounding.
        assert!((w as i64 * 16 - h as i64 * 9).abs() <= 16, "{w}x{h}");
        // Centred source pixel is preserved (not resampled).
        let x0 = (w - 64) / 2;
        let y0 = (h - 32) / 2;
        assert_eq!(out.get_pixel(x0, y0), src.get_pixel(0, 0));
        assert_eq!(out.get_pixel(x0 + 63, y0 + 31), src.get_pixel(63, 31));
        // The letterbox area is filled with darkened artwork, not black.
        let bar = out.get_pixel(w / 2, 1);
        assert!(bar[0] > 0 || bar[1] > 0 || bar[2] > 0, "{bar:?}");
    }

    #[test]
    fn square_and_wide_presets_never_crop_the_source() {
        for preset in [SocialPreset::Square11, SocialPreset::Wide169] {
            let src = gradient(40, 20);
            let out = apply_social(&src, preset);
            assert!(out.width() >= 40 && out.height() >= 20);
            if preset == SocialPreset::Square11 {
                assert_eq!(out.width(), out.height());
            } else {
                assert!((out.width() as i64 * 9 - out.height() as i64 * 16).abs() <= 16);
            }
        }
    }

    #[test]
    fn target_dimensions_never_shrink_the_region() {
        for preset in [
            SocialPreset::Vertical916,
            SocialPreset::Square11,
            SocialPreset::Wide169,
        ] {
            for (w, h) in [(512, 256), (256, 512), (128, 128), (1, 4096)] {
                let (tw, th) = target_dimensions(preset, w, h);
                assert!(tw >= w, "{preset:?} {w}x{h} -> {tw}x{th}");
                assert!(th >= h, "{preset:?} {w}x{h} -> {tw}x{th}");
                assert_eq!(tw % 2, 0);
                assert_eq!(th % 2, 0);
            }
        }
    }
}
