//! Frame overlays: animated date/time clock, progress line and watermarks.
//!
//! The overlay engine is intentionally dependency-free. It ships a compact
//! 5×7 bitmap font (scaled with integer nearest-neighbour) so timestamps and
//! branding look identical on every platform and in headless CI builds, with no
//! system-font lookup or font-file shipping.

use std::sync::Arc;

use chrono::NaiveDateTime;
use image::{Rgba, RgbaImage};

use crate::compose::blend_pixel;
use crate::settings::{Settings, WatermarkPosition};

/// Everything the overlay engine needs for one frame.
#[derive(Debug, Clone)]
pub struct OverlayConfig {
    pub show_timestamp: bool,
    pub timestamp_format: String,
    pub show_progress_bar: bool,
    pub watermark_text: Option<String>,
    pub watermark_logo: Option<Arc<RgbaImage>>,
    pub watermark_position: WatermarkPosition,
    /// Integer scale of the built-in 5×7 bitmap font (1..=8).
    pub font_scale: u32,
}

impl OverlayConfig {
    pub fn from_settings(settings: &Settings) -> Self {
        let text = settings.watermark_text.trim();
        Self {
            show_timestamp: settings.show_timestamp,
            timestamp_format: settings.timestamp_format.clone(),
            show_progress_bar: settings.show_progress_bar,
            watermark_text: (!text.is_empty()).then(|| text.to_string()),
            watermark_logo: None,
            watermark_position: settings.watermark_position,
            font_scale: settings.overlay_font_scale.clamp(1, 8) as u32,
        }
    }

    /// True when this configuration would not touch a single pixel.
    pub fn is_empty(&self) -> bool {
        !self.show_timestamp
            && !self.show_progress_bar
            && self.watermark_text.is_none()
            && self.watermark_logo.is_none()
    }
}

/// Draws every enabled overlay onto `frame` in place.
pub fn apply_frame_overlays(
    frame: &mut RgbaImage,
    config: &OverlayConfig,
    current_time: &NaiveDateTime,
    frame_index: usize,
    total_frames: usize,
) {
    if config.is_empty() || frame.width() == 0 || frame.height() == 0 {
        return;
    }
    let scale = config.font_scale.clamp(1, 8);
    let margin = (scale * 4) as i32;
    let (fw, fh) = (frame.width() as i32, frame.height() as i32);

    if config.show_timestamp {
        let text = format_timestamp(&config.timestamp_format, current_time);
        let (tw, th) = text_size(&text, scale);
        let pad = (scale * 2) as i32;
        let x = margin;
        let y = margin;
        fill_rect(
            frame,
            x - pad,
            y - pad,
            tw + pad * 2,
            th + pad * 2,
            Rgba([5, 10, 20, 150]),
        );
        draw_text(frame, x, y, &text, scale);
    }

    if let Some(text) = &config.watermark_text {
        let (tw, th) = text_size(text, scale);
        let (x, y) = anchored(config.watermark_position, fw, fh, tw, th, margin);
        draw_text(frame, x, y, text, scale);
    }

    if let Some(logo) = &config.watermark_logo {
        let (x, y) = anchored(
            config.watermark_position,
            fw,
            fh,
            logo.width() as i32,
            logo.height() as i32,
            margin,
        );
        draw_image(frame, x, y, logo);
    }

    if config.show_progress_bar {
        let total = total_frames.max(1);
        let frac = ((frame_index + 1) as f64 / total as f64).clamp(0.0, 1.0);
        let bar_h = (scale * 2).max(3) as i32;
        let x = margin;
        let y = fh - margin - bar_h;
        let full_w = (fw - margin * 2).max(1);
        fill_rect(frame, x, y, full_w, bar_h, Rgba([5, 10, 20, 150]));
        let filled = ((full_w as f64) * frac).round() as i32;
        if filled > 0 {
            fill_rect(frame, x, y, filled, bar_h, Rgba([56, 189, 248, 255]));
            // A brighter leading edge makes the bar readable on dark canvases.
            let edge = filled.saturating_sub(scale as i32).max(1);
            fill_rect(
                frame,
                x + filled - edge,
                y,
                edge,
                bar_h,
                Rgba([125, 211, 252, 255]),
            );
        }
    }
}

/// Formats `time` with `format`, falling back to the default format when the
/// user supplied an invalid chrono pattern (which would otherwise panic while
/// being displayed).
pub fn format_timestamp(format: &str, time: &NaiveDateTime) -> String {
    use chrono::format::{Item, StrftimeItems};
    let valid =
        !format.is_empty() && !StrftimeItems::new(format).any(|item| matches!(item, Item::Error));
    if valid {
        time.format(format).to_string()
    } else {
        time.format("%Y-%m-%d %H:%M").to_string()
    }
}

fn anchored(
    position: WatermarkPosition,
    fw: i32,
    fh: i32,
    w: i32,
    h: i32,
    margin: i32,
) -> (i32, i32) {
    match position {
        WatermarkPosition::TopLeft => (margin, margin),
        WatermarkPosition::TopRight => ((fw - margin - w).max(0), margin),
        WatermarkPosition::BottomLeft => (margin, (fh - margin - h).max(0)),
        WatermarkPosition::BottomRight => ((fw - margin - w).max(0), (fh - margin - h).max(0)),
    }
}

/// Pixel size of `text` at `scale`, including a one-column inter-glyph gap.
pub fn text_size(text: &str, scale: u32) -> (i32, i32) {
    let scale = scale.max(1) as i32;
    let lines: Vec<&str> = text.split('\n').collect();
    let longest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as i32;
    let glyph_w = 5 * scale + scale; // 5 px glyph + 1 px gap
    let line_h = 7 * scale + 2 * scale;
    let width = if longest > 0 {
        (longest * glyph_w - scale).max(0)
    } else {
        0
    };
    let height = (lines.len().max(1) as i32) * line_h - 2 * scale;
    (width, height.max(0))
}

fn draw_text(frame: &mut RgbaImage, x: i32, y: i32, text: &str, scale: u32) {
    let scale = scale.max(1);
    let line_h = (7 * scale + 2 * scale) as i32;
    let glyph_w = (5 * scale + scale) as i32;
    let mut cursor_y = y;
    for line in text.split('\n') {
        let mut cursor_x = x;
        for ch in line.chars() {
            let glyph = glyph(ch.to_ascii_uppercase());
            // Drop shadow first, offset by half a pixel-scale.
            draw_glyph(
                frame,
                cursor_x + scale as i32,
                cursor_y + scale as i32,
                &glyph,
                scale,
                Rgba([0, 0, 0, 190]),
            );
            draw_glyph(
                frame,
                cursor_x,
                cursor_y,
                &glyph,
                scale,
                Rgba([255, 255, 255, 255]),
            );
            cursor_x += glyph_w;
        }
        cursor_y += line_h;
    }
}

fn draw_glyph(frame: &mut RgbaImage, x: i32, y: i32, rows: &[u8; 7], scale: u32, color: Rgba<u8>) {
    let (fw, fh) = (frame.width() as i32, frame.height() as i32);
    for (ry, bits) in rows.iter().enumerate() {
        for rx in 0..5 {
            if bits & (1 << (4 - rx)) == 0 {
                continue;
            }
            let px = x + rx * scale as i32;
            let py = y + ry as i32 * scale as i32;
            fill_rect(frame, px, py, scale as i32, scale as i32, color);
        }
    }
    let _ = (fw, fh);
}

/// Alpha-blends a solid rectangle. Fully opaque colours use a fast copy.
fn fill_rect(frame: &mut RgbaImage, x: i32, y: i32, w: i32, h: i32, color: Rgba<u8>) {
    let (fw, fh) = (frame.width() as i32, frame.height() as i32);
    if w <= 0 || h <= 0 {
        return;
    }
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + w).min(fw);
    let y1 = (y + h).min(fh);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    if color[3] == 255 {
        for py in y0..y1 {
            let row = (py as usize * fw as usize + x0 as usize) * 4;
            let end = (py as usize * fw as usize + x1 as usize) * 4;
            let raw = frame.as_mut();
            for px in raw[row..end].as_chunks_mut::<4>().0 {
                *px = [color[0], color[1], color[2], color[3]];
            }
        }
        return;
    }
    for py in y0..y1 {
        for px in x0..x1 {
            let dst = frame.get_pixel_mut(px as u32, py as u32);
            let mut out = [dst[0], dst[1], dst[2], dst[3]];
            blend_pixel(&mut out, &[color[0], color[1], color[2], color[3]]);
            *dst = Rgba(out);
        }
    }
}

fn draw_image(frame: &mut RgbaImage, x: i32, y: i32, logo: &RgbaImage) {
    let (fw, fh) = (frame.width() as i32, frame.height() as i32);
    for ly in 0..logo.height() as i32 {
        for lx in 0..logo.width() as i32 {
            let src = *logo.get_pixel(lx as u32, ly as u32);
            if src[3] == 0 {
                continue;
            }
            let dx = x + lx;
            let dy = y + ly;
            if dx < 0 || dy < 0 || dx >= fw || dy >= fh {
                continue;
            }
            let dst = frame.get_pixel_mut(dx as u32, dy as u32);
            let mut out = [dst[0], dst[1], dst[2], dst[3]];
            blend_pixel(&mut out, &[src[0], src[1], src[2], src[3]]);
            *dst = Rgba(out);
        }
    }
}

/// 5×7 bitmap glyphs, one byte per row, MSB = leftmost pixel.
///
/// Only printable ASCII is needed for timestamps, watermarks and progress
/// captions. Unknown characters render as blank space.
fn glyph(ch: char) -> [u8; 7] {
    match ch {
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => [
            0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => [
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
        ],
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'B' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
        ],
        'C' => [
            0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
        ],
        'D' => [
            0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'F' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'G' => [
            0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111,
        ],
        'H' => [
            0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'I' => [
            0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        'J' => [
            0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100,
        ],
        'K' => [
            0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001,
        ],
        'L' => [
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'M' => [
            0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001,
        ],
        'N' => [
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'P' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'Q' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'U' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'V' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
        ],
        'W' => [
            0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b10101, 0b01010,
        ],
        'X' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001,
        ],
        'Y' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'Z' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111,
        ],
        '-' => [
            0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000,
        ],
        ':' => [
            0b00000, 0b00100, 0b00100, 0b00000, 0b00100, 0b00100, 0b00000,
        ],
        '.' => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00110, 0b00110,
        ],
        '/' => [
            0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000,
        ],
        '_' => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b11111,
        ],
        '+' => [
            0b00000, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0b00000,
        ],
        '%' => [
            0b11001, 0b11010, 0b00010, 0b00100, 0b01000, 0b01011, 0b10011,
        ],
        '!' => [
            0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00000, 0b00100,
        ],
        '?' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b00000, 0b00100,
        ],
        '(' => [
            0b00010, 0b00100, 0b01000, 0b01000, 0b01000, 0b00100, 0b00010,
        ],
        ')' => [
            0b01000, 0b00100, 0b00010, 0b00010, 0b00010, 0b00100, 0b01000,
        ],
        '#' => [
            0b01010, 0b01010, 0b11111, 0b01010, 0b11111, 0b01010, 0b01010,
        ],
        '\'' => [
            0b00100, 0b00100, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000,
        ],
        _ => [0; 7],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn time() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2000, 1, 2)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap()
    }

    #[test]
    fn timestamp_text_uses_the_requested_pattern() {
        assert_eq!(
            format_timestamp("%Y-%m-%d %H:%M", &time()),
            "2000-01-02 14:30"
        );
        assert_eq!(format_timestamp("DAY %d / %m", &time()), "DAY 02 / 01");
    }

    #[test]
    fn invalid_timestamp_pattern_falls_back_instead_of_panicking() {
        assert_eq!(format_timestamp("%Q", &time()), "2000-01-02 14:30");
        assert_eq!(format_timestamp("", &time()), "2000-01-02 14:30");
    }

    #[test]
    fn text_size_scales_with_the_font_scale() {
        let (w1, h1) = text_size("AB", 1);
        let (w2, h2) = text_size("AB", 2);
        assert_eq!(w2, w1 * 2);
        assert_eq!(h2, h1 * 2);
        // Two glyphs at scale 1: 5+1+5 = 11 px wide, 7 px tall.
        assert_eq!((w1, h1), (11, 7));
    }

    #[test]
    fn timestamp_and_progress_actually_change_pixels() {
        let mut frame = RgbaImage::from_pixel(320, 180, Rgba([0, 0, 0, 255]));
        let before = frame.clone();
        let cfg = OverlayConfig {
            show_timestamp: true,
            timestamp_format: "%Y-%m-%d %H:%M".into(),
            show_progress_bar: true,
            watermark_text: Some("PIXELPLANET".into()),
            watermark_logo: None,
            watermark_position: WatermarkPosition::TopRight,
            font_scale: 2,
        };
        apply_frame_overlays(&mut frame, &cfg, &time(), 30, 120);
        assert_ne!(frame, before, "overlays must be visible");

        // The timestamp is in the top-left corner…
        assert!(frame
            .pixels()
            .take(320 * 40)
            .any(|p| p[0] > 100 && p[1] > 100));
        // ...and the progress line touches the bottom-left area.
        let bar_pixel = frame.get_pixel(12, 180 - 8 - 4);
        assert!(
            bar_pixel[2] > 150,
            "progress bar should be visible: {bar_pixel:?}"
        );
    }

    #[test]
    fn progress_bar_grows_with_the_frame_index() {
        let cfg = OverlayConfig {
            show_timestamp: false,
            timestamp_format: String::new(),
            show_progress_bar: true,
            watermark_text: None,
            watermark_logo: None,
            watermark_position: WatermarkPosition::TopRight,
            font_scale: 1,
        };
        let count = |idx: usize| {
            let mut frame = RgbaImage::from_pixel(200, 100, Rgba([0, 0, 0, 255]));
            apply_frame_overlays(&mut frame, &cfg, &time(), idx, 100);
            // Count blue-ish pixels in the bottom strip.
            frame.pixels().filter(|p| p[2] > 150).count()
        };
        assert!(count(90) > count(10));
    }

    #[test]
    fn watermark_anchors_follow_the_configured_corner() {
        let cfg = |pos| OverlayConfig {
            show_timestamp: false,
            timestamp_format: String::new(),
            show_progress_bar: false,
            watermark_text: Some("X".into()),
            watermark_logo: None,
            watermark_position: pos,
            font_scale: 2,
        };
        let mut top_right = RgbaImage::from_pixel(100, 60, Rgba([0, 0, 0, 255]));
        apply_frame_overlays(
            &mut top_right,
            &cfg(WatermarkPosition::TopRight),
            &time(),
            0,
            1,
        );
        let bright = |img: &RgbaImage| {
            img.enumerate_pixels()
                .filter(|(_, _, p)| p[0] > 200)
                .map(|(x, _, _)| x)
                .max()
                .unwrap_or(0)
        };
        assert!(bright(&top_right) > 80, "top-right watermark expected");

        let mut top_left = RgbaImage::from_pixel(100, 60, Rgba([0, 0, 0, 255]));
        apply_frame_overlays(
            &mut top_left,
            &cfg(WatermarkPosition::TopLeft),
            &time(),
            0,
            1,
        );
        let min_x = top_left
            .enumerate_pixels()
            .filter(|(_, _, p)| p[0] > 200)
            .map(|(x, _, _)| x)
            .min()
            .unwrap_or(99);
        assert!(min_x < 20, "top-left watermark expected");
    }

    #[test]
    fn empty_config_is_a_no_op() {
        let mut frame = RgbaImage::from_pixel(8, 8, Rgba([1, 2, 3, 255]));
        let before = frame.clone();
        apply_frame_overlays(
            &mut frame,
            &OverlayConfig::from_settings(&Settings::default()),
            &time(),
            0,
            10,
        );
        assert_eq!(frame, before);
    }
}
