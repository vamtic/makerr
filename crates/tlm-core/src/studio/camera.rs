//! Dynamic camera motion (Ken Burns pan/zoom).
//!
//! The camera never changes the output resolution: it samples a moving window
//! of the composed frame into the same-sized output buffer. Pixel art is
//! sampled with nearest-neighbour, which keeps hard edges crisp (bilinear
//! filtering would blur the very thing the video is about).

use image::RgbaImage;

use crate::settings::CameraMotion;

/// A resolved camera move for one render.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CameraPlan {
    pub motion: CameraMotion,
    /// How far the camera travels, as a fraction of the frame size (0.0..0.9).
    pub strength: f32,
}

impl CameraPlan {
    pub fn new(motion: CameraMotion, strength_percent: i32) -> Self {
        Self {
            motion,
            strength: (strength_percent as f32 / 100.0).clamp(0.0, 0.9),
        }
    }

    pub fn is_static(&self) -> bool {
        self.motion == CameraMotion::None || self.strength <= 0.0
    }
}

/// Applies the camera move at `progress` (0.0 = first frame, 1.0 = last).
///
/// Returns a new image; callers keep the untouched composed frame for
/// cumulative-snapshot bookkeeping.
pub fn apply_camera(src: &RgbaImage, plan: &CameraPlan, progress: f32) -> RgbaImage {
    if plan.is_static() || src.width() < 2 || src.height() < 2 {
        return src.clone();
    }
    let (w, h) = (src.width() as i64, src.height() as i64);
    let t = (progress as f64).clamp(0.0, 1.0);
    let strength = plan.strength as f64;

    // Window size in source pixels. A strength of 0.2 means the camera is
    // zoomed in by 1/1.2 at the tight end of the move.
    let (view_w, view_h, x_frac, y_frac) = match plan.motion {
        CameraMotion::None => (w as f64, h as f64, 0.0, 0.0),
        CameraMotion::ZoomOut => {
            let zoom = 1.0 + strength * (1.0 - t);
            (w as f64 / zoom, h as f64 / zoom, 0.5, 0.5)
        }
        CameraMotion::ZoomIn => {
            let zoom = 1.0 + strength * t;
            (w as f64 / zoom, h as f64 / zoom, 0.5, 0.5)
        }
        CameraMotion::PanLeftToRight => {
            let zoom = 1.0 + strength;
            (w as f64 / zoom, h as f64 / zoom, t, 0.5)
        }
        CameraMotion::PanRightToLeft => {
            let zoom = 1.0 + strength;
            (w as f64 / zoom, h as f64 / zoom, 1.0 - t, 0.5)
        }
    };

    let view_w = view_w.max(1.0).min(w as f64);
    let view_h = view_h.max(1.0).min(h as f64);
    let max_x = (w as f64 - view_w).max(0.0);
    let max_y = (h as f64 - view_h).max(0.0);
    let x0 = max_x * x_frac;
    let y0 = max_y * y_frac;
    let x0 = x0.round().clamp(0.0, max_x);
    let y0 = y0.round().clamp(0.0, max_y);

    let mut out = RgbaImage::new(src.width(), src.height());
    let stride = src.width() as usize * 4;
    let src_raw = src.as_raw();

    // Source column of every output column (byte offset into a row). The
    // mapping only depends on x, so compute it once per frame instead of
    // recomputing two f64 expressions for every one of the millions of
    // output pixels.
    let mut col_offsets = Vec::with_capacity(w as usize);
    for ox in 0..w {
        let sx = (x0 + (ox as f64 + 0.5) * view_w / w as f64)
            .floor()
            .clamp(0.0, w as f64 - 1.0) as usize;
        col_offsets.push(sx * 4);
    }

    let dst_raw = out.as_mut();
    let mut last_sy: Option<usize> = None;
    for oy in 0..h as usize {
        // Map output row -> source row through the moving window.
        let sy = (y0 + (oy as f64 + 0.5) * view_h / h as f64)
            .floor()
            .clamp(0.0, h as f64 - 1.0) as usize;

        if last_sy == Some(sy) && oy > 0 {
            // Zoomed-in rows repeat: copy the row we just rendered instead of
            // resampling every pixel again.
            let (done, rest) = dst_raw.split_at_mut(oy * stride);
            rest[..stride].copy_from_slice(&done[(oy - 1) * stride..oy * stride]);
            continue;
        }
        last_sy = Some(sy);

        let src_row = &src_raw[sy * stride..(sy + 1) * stride];
        let dst_row = &mut dst_raw[oy * stride..(oy + 1) * stride];
        for (px, &off) in dst_row
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(col_offsets.iter())
        {
            px.copy_from_slice(&src_row[off..off + 4]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    fn gradient(w: u32, h: u32) -> RgbaImage {
        let mut img = RgbaImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                img.put_pixel(
                    x,
                    y,
                    Rgba([(x * 255 / (w - 1)) as u8, (y * 255 / (h - 1)) as u8, 0, 255]),
                );
            }
        }
        img
    }

    #[test]
    fn static_camera_is_identity() {
        let src = gradient(32, 16);
        let plan = CameraPlan::new(CameraMotion::None, 20);
        assert_eq!(apply_camera(&src, &plan, 0.3), src);
    }

    #[test]
    fn zoom_out_starts_tighter_and_ends_at_the_full_frame() {
        let src = gradient(64, 32);
        let plan = CameraPlan::new(CameraMotion::ZoomOut, 50);
        let start = apply_camera(&src, &plan, 0.0);
        let end = apply_camera(&src, &plan, 1.0);
        assert_eq!((start.width(), start.height()), (64, 32));
        // Zooming in on the centre means the top-left output pixel comes from
        // inside the source, not from (0,0).
        assert_ne!(start.get_pixel(0, 0), src.get_pixel(0, 0));
        assert_eq!(end.get_pixel(0, 0), src.get_pixel(0, 0));
        // The centre pixel is invariant under a centre zoom.
        let centre = src.get_pixel(32, 16);
        assert_eq!(start.get_pixel(32, 16), centre);
    }

    #[test]
    fn pan_moves_the_visible_window() {
        let src = gradient(64, 32);
        let plan = CameraPlan::new(CameraMotion::PanLeftToRight, 25);
        let left = apply_camera(&src, &plan, 0.0);
        let right = apply_camera(&src, &plan, 1.0);
        // The left edge of the window differs between the two ends.
        assert_ne!(left.get_pixel(0, 16), right.get_pixel(0, 16));
        // Pan is a crop, so output dimensions stay put.
        assert_eq!(right.dimensions(), (64, 32));
    }
}
