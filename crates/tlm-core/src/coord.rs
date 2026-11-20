//! Parsing of the `x_y` corner coordinates used by the GUI and templates.

/// Largest accepted absolute coordinate.
///
/// The biggest real canvas (PixMap Earth) is 65536 px per side, so its
/// coordinates live in ±32768. Keeping input inside this bound means every
/// downstream pixel computation — tile offsets are `i32`, region areas are
/// `i64` — stays far away from overflow, even in debug builds where wrapping
/// arithmetic panics.
pub const MAX_COORD: i64 = 1 << 31;

/// Ceiling on the pixel count of one composed frame (256 Mi px ≈ 1 GiB RGBA).
///
/// A frame buffer is allocated as `width * height * 4` bytes up front, so a
/// mistyped region has to be rejected *before* `compose_base` runs. The tile
/// count alone does not bound the allocation: the chunk grid is clamped to the
/// canvas while the composed frame keeps the requested size, so a selection
/// larger than the canvas can have a handful of tiles and still ask for a
/// multi-terabyte buffer.
pub const MAX_FRAME_PIXELS: i64 = 256 * 1024 * 1024;

/// A parsed pair of canvas-pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coord {
    pub x: i64,
    pub y: i64,
}

impl std::str::FromStr for Coord {
    type Err = ParseCoordError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_one(s).ok_or(ParseCoordError)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("expected an `x_y` pair of integers")]
pub struct ParseCoordError;

fn parse_one(s: &str) -> Option<Coord> {
    let s = s.trim();
    let (x, y) = s.split_once('_')?;
    // Reject empty parts and anything with internal whitespace so "1 _2"
    // is not silently accepted.
    if x.is_empty() || y.is_empty() {
        return None;
    }
    let x = parse_int(x)?;
    let y = parse_int(y)?;
    Some(Coord { x, y })
}

fn parse_int(s: &str) -> Option<i64> {
    let (neg, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Reject leading zeros beyond a single "0" to keep the same strictness
    // as the QML validator's regex `-?\d+_-?\d+` (which also accepts them —
    // but "007" style input is a typo more often than intent).
    if digits.len() > 1 && digits.starts_with('0') {
        return None;
    }
    // Reject "-0"
    if neg && digits == "0" {
        return None;
    }
    // Parse the full string (including sign) so i64::MIN parses without
    // overflowing positive i64, then reject values outside the canvas range.
    let value = s.parse::<i64>().ok()?;
    if !(-MAX_COORD..=MAX_COORD).contains(&value) {
        return None;
    }
    Some(value)
}

/// Parses two corner strings into `(x1, y1, x2, y2)` integers.
///
/// Mirrors `coord::parsePair` from the V1 implementation.
pub fn parse_pair(c1: &str, c2: &str) -> Option<(i64, i64, i64, i64)> {
    let a = parse_one(c1)?;
    let b = parse_one(c2)?;
    Some((a.x, a.y, b.x, b.y))
}

/// Normalizes two corners into the top-left origin `(x, y)` and inclusive
/// extents `(width, height)`. Order-independent.
pub fn region_from_corners(c1: &str, c2: &str) -> Option<Region> {
    let (x1, y1, x2, y2) = parse_pair(c1, c2)?;
    let x = x1.min(x2);
    let y = y1.min(y2);
    // Checked: the span is bounded by `MAX_COORD`, but arithmetic overflow in
    // the parser must never panic the UI or the CLI.
    let width = x1.max(x2).checked_sub(x)?.checked_add(1)?;
    let height = y1.max(y2).checked_sub(y)?.checked_add(1)?;
    Some(Region {
        x,
        y,
        width,
        height,
    })
}

/// An inclusive rectangular pixel region on the canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

impl Region {
    /// `width * height`, or `None` when the product does not fit an `i64`.
    pub fn checked_area(self) -> Option<i64> {
        self.width.checked_mul(self.height)
    }
}

/// Why a region cannot be rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RegionError {
    #[error("the selected area is empty")]
    Empty,
    #[error("the selected area is too large ({width}x{height} = {pixels} px)")]
    TooLarge {
        width: i64,
        height: i64,
        pixels: i64,
    },
}

/// Intersects `region` with the canvas rectangle `[-size/2, size-size/2)`.
/// (For even sizes — every real canvas — that is exactly `[-size/2, size/2)`.)
///
/// Pixels outside the canvas can never have tiles (the sites only store chunks
/// inside the canvas), so cropping them here is exact: the composed frame has
/// the same pixels, without allocating background-only borders the size of the
/// request. A region entirely outside the canvas becomes empty and is rejected
/// by [`check_frame_size`].
pub fn clamp_to_canvas(region: Region, canvas_size_px: i64) -> Region {
    if canvas_size_px <= 0 {
        return region;
    }
    let half = canvas_size_px / 2;
    let lo = -half;
    // `size/2` rounded down for the origin, rounded up for the exclusive end,
    // so an odd canvas size keeps its last pixel column/row.
    let hi = canvas_size_px - half;
    let x0 = region.x.max(lo);
    let x1 = region.x.saturating_add(region.width).min(hi);
    let y0 = region.y.max(lo);
    let y1 = region.y.saturating_add(region.height).min(hi);
    Region {
        x: x0,
        y: y0,
        width: (x1 - x0).max(0),
        height: (y1 - y0).max(0),
    }
}

/// Rejects empty or unreasonably large frames. Call this after
/// [`clamp_to_canvas`], which bounds the area to the canvas.
pub fn check_frame_size(region: &Region) -> Result<(), RegionError> {
    if region.width <= 0 || region.height <= 0 {
        return Err(RegionError::Empty);
    }
    let pixels = region.checked_area().ok_or(RegionError::TooLarge {
        width: region.width,
        height: region.height,
        pixels: i64::MAX,
    })?;
    if pixels > MAX_FRAME_PIXELS {
        return Err(RegionError::TooLarge {
            width: region.width,
            height: region.height,
            pixels,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_pairs() {
        assert_eq!(
            parse_pair("-128_-128", "128_128"),
            Some((-128, -128, 128, 128))
        );
        assert_eq!(parse_pair("0_0", "10_20"), Some((0, 0, 10, 20)));
        // Values at the accepted coordinate bound parse...
        let max = format!("{}_0", MAX_COORD);
        assert_eq!(parse_pair(&max, "0_0"), Some((MAX_COORD, 0, 0, 0)));
        // ...and beyond it they are rejected instead of flowing into pixel
        // math that would overflow.
        let too_big = format!("{}_0", MAX_COORD + 1);
        assert_eq!(parse_pair(&too_big, "0_0"), None);
        let min_str = format!("{}_0", i64::MIN);
        assert_eq!(parse_pair(&min_str, "0_0"), None);
    }

    #[test]
    fn rejects_malformed_input() {
        assert_eq!(parse_pair("128", "0_0"), None);
        assert_eq!(parse_pair("1_2_3", "0_0"), None);
        assert_eq!(parse_pair("a_b", "0_0"), None);
        assert_eq!(parse_pair("1_", "0_0"), None);
        assert_eq!(parse_pair("_2", "0_0"), None);
        assert_eq!(parse_pair("007_0", "0_0"), None);
        assert_eq!(parse_pair("-0_0", "0_0"), None);
    }

    #[test]
    fn region_is_order_independent() {
        let r = region_from_corners("128_128", "-128_-128").unwrap();
        assert_eq!((r.x, r.y, r.width, r.height), (-128, -128, 257, 257));
    }

    #[test]
    fn clamps_to_the_canvas_rectangle() {
        // A selection that sticks out on every side is cropped to the canvas.
        let r = clamp_to_canvas(
            Region {
                x: -3000,
                y: -3000,
                width: 6000,
                height: 6000,
            },
            4096,
        );
        assert_eq!((r.x, r.y, r.width, r.height), (-2048, -2048, 4096, 4096));

        // Fully outside: nothing left to render.
        let outside = clamp_to_canvas(
            Region {
                x: 5000,
                y: 0,
                width: 100,
                height: 100,
            },
            4096,
        );
        assert_eq!(check_frame_size(&outside), Err(RegionError::Empty));

        // Ordered corners produce a positive area.
        let inside = clamp_to_canvas(
            Region {
                x: -10,
                y: 5,
                width: 20,
                height: 15,
            },
            65536,
        );
        assert_eq!(
            (inside.x, inside.y, inside.width, inside.height),
            (-10, 5, 20, 15)
        );
        assert!(check_frame_size(&inside).is_ok());
    }

    #[test]
    fn rejects_unreasonable_frames() {
        // The canvas clamp keeps this at 65536² = 4.3 G px, which must not be
        // handed to `RgbaImage::from_pixel`.
        let huge = clamp_to_canvas(
            Region {
                x: -1_000_000,
                y: -1_000_000,
                width: 2_000_000,
                height: 2_000_000,
            },
            65536,
        );
        assert!(matches!(
            check_frame_size(&huge),
            Err(RegionError::TooLarge { .. })
        ));

        // A square just under the ceiling is accepted.
        let edge = (MAX_FRAME_PIXELS as f64).sqrt() as i64;
        let ok = Region {
            x: 0,
            y: 0,
            width: edge,
            height: edge,
        };
        assert!(check_frame_size(&ok).is_ok());
    }
}
