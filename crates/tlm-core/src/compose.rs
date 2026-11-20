//! Frame composition: parallel tile decode (rayon), background fill and
//! source-over painting into the frame canvas, plus the cumulative-snapshot
//! compositor that restores pixels dropped from later history snapshots.

use image::{DynamicImage, Rgba, RgbaImage};
use rayon::prelude::*;
use std::sync::{Arc, Mutex};

use crate::coord::Region;
use crate::site::CHUNK_PX;

/// One tile's position: its chunk coordinates on the canvas grid and its
/// pixel offset inside the composed frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileCoord {
    pub xi: i64,
    pub yi: i64,
    /// Pixel offset of the tile's top-left inside the frame image.
    pub ox: i32,
    pub oy: i32,
}

/// Computes the tile grid covering `region` on a canvas of edge
/// `canvas_size_px`, clamped to the canvas bounds like the reference PixMap
/// downloader: chunks outside the canvas never exist, so requesting them
/// only produces 404s and the affected pixels keep the background fill.
pub fn build_tile_coords(region: &Region, canvas_size_px: i64) -> Vec<TileCoord> {
    let (x_start, x_end, y_start, y_end) = tile_ranges(region, canvas_size_px);
    let count = usize::try_from(tile_count(region, canvas_size_px)).unwrap_or(usize::MAX);
    // Cap the pre-allocation: a corrupt API canvas size (or a mistyped region)
    // must not turn into a multi-gigabyte `with_capacity` that aborts the
    // process. The grid itself is still built, and callers that care check
    // [`tile_count`] first.
    let mut coords = Vec::with_capacity(count.min(1 << 20));
    let canvas_offset = offset(canvas_size_px);
    for yi in y_start..y_end {
        for xi in x_start..x_end {
            // Saturating arithmetic: the region parser already bounds the
            // corners, but a bogus API canvas size or a direct API caller must
            // never wrap into a negative pixel offset (which would panic in
            // debug builds and corrupt the frame in release builds).
            let ox = xi
                .saturating_mul(CHUNK_PX)
                .saturating_add(canvas_offset)
                .saturating_sub(region.x)
                .clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            let oy = yi
                .saturating_mul(CHUNK_PX)
                .saturating_add(canvas_offset)
                .saturating_sub(region.y)
                .clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            coords.push(TileCoord { xi, yi, ox, oy });
        }
    }
    coords
}

/// How many tiles [`build_tile_coords`] would return, without allocating.
pub fn tile_count(region: &Region, canvas_size_px: i64) -> i64 {
    let (x_start, x_end, y_start, y_end) = tile_ranges(region, canvas_size_px);
    (x_end - x_start)
        .max(0)
        .saturating_mul((y_end - y_start).max(0))
}

fn offset(canvas_size_px: i64) -> i64 {
    // Dividing first keeps the negation in range even for `i64::MIN`.
    -(canvas_size_px / 2)
}

/// The half-open chunk ranges covering `region`, clamped to the canvas.
fn tile_ranges(region: &Region, canvas_size_px: i64) -> (i64, i64, i64, i64) {
    let offset = offset(canvas_size_px);
    let x_start = div_floor(region.x.saturating_sub(offset), CHUNK_PX);
    let x_end = div_ceil(
        region.x.saturating_add(region.width).saturating_sub(offset),
        CHUNK_PX,
    );
    let y_start = div_floor(region.y.saturating_sub(offset), CHUNK_PX);
    let y_end = div_ceil(
        region
            .y
            .saturating_add(region.height)
            .saturating_sub(offset),
        CHUNK_PX,
    );

    if canvas_size_px > 0 {
        let size_chunks = canvas_size_px.saturating_add(CHUNK_PX - 1) / CHUNK_PX;
        let xs = x_start.clamp(0, size_chunks);
        let xe = x_end.clamp(xs, size_chunks);
        let ys = y_start.clamp(0, size_chunks);
        let ye = y_end.clamp(ys, size_chunks);
        (xs, xe, ys, ye)
    } else {
        (x_start, x_end, y_start, y_end)
    }
}

fn div_floor(a: i64, b: i64) -> i64 {
    a.div_euclid(b)
}

fn div_ceil(a: i64, b: i64) -> i64 {
    a.div_euclid(b) + i64::from(a.rem_euclid(b) != 0)
}

/// Decodes one tile PNG into an RGBA image.
///
/// Fast path: `zune-png`'s SIMD deflate/filter reconstruction (2–4× faster than
/// the default decoder on the small chunk PNGs timelapses are made of). The
/// `image` crate remains the fallback for exotic encodings (16-bit without
/// stripping, interlaced malformed files, non-PNG payloads) so a tile that used
/// to decode can never regress to `None`.
pub fn decode_tile(data: &[u8]) -> Option<RgbaImage> {
    if data.is_empty() {
        return None;
    }
    decode_tile_zune(data).or_else(|| {
        let img = image::load_from_memory(data).ok()?;
        Some(match img {
            DynamicImage::ImageRgba8(rgba) => rgba,
            other => other.to_rgba8(),
        })
    })
}

/// zune-png decoder path. Returns `None` whenever the output layout is one this
/// converter does not understand, triggering the `image` fallback.
fn decode_tile_zune(data: &[u8]) -> Option<RgbaImage> {
    use zune_png::zune_core::bytestream::ZCursor;
    use zune_png::zune_core::colorspace::ColorSpace;
    use zune_png::zune_core::options::DecoderOptions;
    use zune_png::PngDecoder;

    let options = DecoderOptions::default()
        .png_set_strip_to_8bit(true)
        .png_set_add_alpha_channel(true);
    let mut decoder = PngDecoder::new_with_options(ZCursor::new(data), options);
    let raw = decoder.decode_raw().ok()?;
    let (w, h) = decoder.dimensions()?;
    if w == 0 || h == 0 {
        return None;
    }
    let pixels = w.checked_mul(h)?;
    let rgba: Vec<u8> = match decoder.colorspace()? {
        ColorSpace::RGBA => {
            if raw.len() < pixels * 4 {
                return None;
            }
            raw[..pixels * 4].to_vec()
        }
        ColorSpace::ARGB => {
            if raw.len() < pixels * 4 {
                return None;
            }
            let mut out = Vec::with_capacity(pixels * 4);
            for px in raw[..pixels * 4].as_chunks::<4>().0 {
                out.extend_from_slice(&[px[1], px[2], px[3], px[0]]);
            }
            out
        }
        ColorSpace::RGB => {
            if raw.len() < pixels * 3 {
                return None;
            }
            let mut out = Vec::with_capacity(pixels * 4);
            for px in raw[..pixels * 3].as_chunks::<3>().0 {
                out.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            out
        }
        ColorSpace::BGR => {
            if raw.len() < pixels * 3 {
                return None;
            }
            let mut out = Vec::with_capacity(pixels * 4);
            for px in raw[..pixels * 3].as_chunks::<3>().0 {
                out.extend_from_slice(&[px[2], px[1], px[0], 255]);
            }
            out
        }
        ColorSpace::BGRA => {
            if raw.len() < pixels * 4 {
                return None;
            }
            let mut out = Vec::with_capacity(pixels * 4);
            for px in raw[..pixels * 4].as_chunks::<4>().0 {
                out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
            }
            out
        }
        ColorSpace::Luma => {
            if raw.len() < pixels {
                return None;
            }
            let mut out = Vec::with_capacity(pixels * 4);
            for &g in &raw[..pixels] {
                out.extend_from_slice(&[g, g, g, 255]);
            }
            out
        }
        ColorSpace::LumaA => {
            if raw.len() < pixels * 2 {
                return None;
            }
            let mut out = Vec::with_capacity(pixels * 4);
            for px in raw[..pixels * 2].as_chunks::<2>().0 {
                out.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
            out
        }
        _ => return None,
    };
    RgbaImage::from_raw(w as u32, h as u32, rgba)
}

/// Decodes many tiles in parallel. Missing/garbled tiles map to `None`.
pub fn decode_tiles_parallel(datas: &[Option<Vec<u8>>]) -> Vec<Option<RgbaImage>> {
    datas
        .par_iter()
        .map(|d| d.as_deref().and_then(decode_tile))
        .collect()
}

/// Composes the day-base frame: background fill + all tiles painted
/// source-over, mirroring QPainter's `CompositionMode_SourceOver`.
pub fn compose_base(
    region: &Region,
    bg_color: Rgba<u8>,
    tile_coords: &[TileCoord],
    decoded: &[Option<RgbaImage>],
) -> RgbaImage {
    let mut canvas = RgbaImage::from_pixel(
        region.width.max(1) as u32,
        region.height.max(1) as u32,
        bg_color,
    );
    for (coord, tile) in tile_coords.iter().zip(decoded) {
        if let Some(tile) = tile {
            overlay_rgba(&mut canvas, tile, coord.ox as i64, coord.oy as i64);
        }
    }
    canvas
}

/// Alpha-blits `src` onto `dst` at (`ox`, `oy`), reproducing
/// `image::imageops::overlay` **byte for byte** — including its clipping rules
/// and its alpha fast paths — but on raw row slices instead of the generic
/// per-pixel `get_pixel` / `put_pixel` trait calls.
///
/// Every composed frame runs this loop once per tile, so the trait-level
/// version was a real cost: each pixel went through a `Pixel` lookup and a
/// bounds check. Tiles are contiguous rows, so clipping is computed once per
/// tile and the inner loop walks `u8` quadruplets. Partial alpha still uses
/// [`blend_pixel`], which is already bit-exact with `Rgba<u8>::blend`
/// (the unchanged-frame linking relies on that parity).
pub fn overlay_rgba(dst: &mut RgbaImage, src: &RgbaImage, ox: i64, oy: i64) {
    let (dw, dh) = (dst.width() as i64, dst.height() as i64);
    let (sw, sh) = (src.width() as i64, src.height() as i64);
    // Same rejection test as `overlay_bounds_ext`.
    if ox > dw || oy > dh || ox + sw <= 0 || oy + sh <= 0 {
        return;
    }
    let dst_x = ox.clamp(0, dw) as usize;
    let x_end = (ox + sw).clamp(0, dw) as usize;
    let dst_y = oy.clamp(0, dh) as usize;
    let y_end = (oy + sh).clamp(0, dh) as usize;
    let width = x_end - dst_x;
    let height = y_end - dst_y;
    if width == 0 || height == 0 {
        return;
    }
    let src_x = ox.saturating_neg().clamp(0, sw) as usize;
    let src_y = oy.saturating_neg().clamp(0, sh) as usize;

    let dst_stride = dw as usize * 4;
    let src_stride = sw as usize * 4;
    let dst_bytes: &mut [u8] = &mut *dst;
    let src_bytes = src.as_raw();

    // Every pixel opaque: whole-row memcpy instead of a per-pixel alpha test.
    if src_bytes.as_chunks::<4>().0.iter().all(|px| px[3] == 255) {
        for row in 0..height {
            let d0 = (dst_y + row) * dst_stride + dst_x * 4;
            let s0 = (src_y + row) * src_stride + src_x * 4;
            dst_bytes[d0..d0 + width * 4].copy_from_slice(&src_bytes[s0..s0 + width * 4]);
        }
        return;
    }

    for row in 0..height {
        let d0 = (dst_y + row) * dst_stride + dst_x * 4;
        let s0 = (src_y + row) * src_stride + src_x * 4;
        let dst_row = &mut dst_bytes[d0..d0 + width * 4];
        let src_row = &src_bytes[s0..s0 + width * 4];
        for (d, s) in dst_row
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(src_row.as_chunks::<4>().0)
        {
            match s[3] {
                0 => {}                      // transparent: destination untouched
                255 => d.copy_from_slice(s), // opaque: straight replace
                _ => {
                    let mut px = [d[0], d[1], d[2], d[3]];
                    blend_pixel(&mut px, &[s[0], s[1], s[2], s[3]]);
                    d.copy_from_slice(&px);
                }
            }
        }
    }
}

/// Outcome of an in-place snapshot overlay (see
/// [`apply_cumulative_snapshot`] and [`paint_overlay_inplace`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayOutcome {
    /// True when at least one pixel of the canvas differs from what was
    /// there before — the cheap per-frame equality signal that lets the
    /// pipeline link unchanged frames instead of re-saving them.
    pub changed: bool,
    /// Number of tiles actually painted (tiles fully outside the frame or
    /// missing do not count).
    pub tiles_painted: usize,
}

/// Float src-over blend of one RGBA8 pixel, **bit-for-bit identical** to
/// `image`'s private `Blend for Rgba<u8>` (the blend behind
/// `imageops::overlay`, which mirrors QPainter's
/// `CompositionMode_SourceOver`). Verified exhaustively by the
/// `blend_parity` integration test (every source/destination alpha pair ×
/// a spread of channel values, plus a randomized sweep).
///
/// Rust-specific optimizations vs. the V1 port (which ran this blend
/// through QPainter per frame): the four alpha/color normalizations,
/// `alpha_final` and `(1 − fa)` are loop-invariant across the three color
/// channels, so they are hoisted and each channel costs exactly one
/// division (the un-premultiply) instead of re-running the whole unit-space
/// pipeline per channel. Every remaining operation keeps the reference's
/// *exact* float expression graph on purpose: substituting a reciprocal
/// multiply for the `/ 255` normalizations (`1/255` is not representable
/// in binary, so `c · (1/255)` ≠ `c / 255` for e.g. c = 3) or folding the
/// destination premultiply into one integer product (`(bg_c·bg_a)/255²`
/// rounds differently than `(bg_c/255)·(bg_a/255)`) flips truncated bytes
/// at boundary cases and silently corrupts the unchanged-frame hardlink
/// bookkeeping below. Bit-parity is not cosmetic; it is the contract —
/// exhaustively verified by the `blend_parity` integration test.
#[inline]
pub fn blend_pixel(dst: &mut [u8; 4], src: &[u8; 4]) {
    let a = src[3];
    if a == 0 {
        return; // fully transparent: destination untouched (like Blend)
    }
    if a == 255 {
        *dst = *src; // fully opaque replace (like Blend)
        return;
    }
    const MAX: f32 = 255.0;
    let fa = a as f32 / MAX; // reference: fg_a (hoisted, used by all channels)
    let ba = dst[3] as f32 / MAX; // reference: bg_a
                                  // alpha_final = bg_a + fg_a − bg_a·fg_a (same expression as reference)
    let alpha_final = ba + fa - ba * fa;
    if alpha_final == 0.0 {
        return; // reference returns the destination unchanged
    }
    let k = 1.0 - fa; // reference: (1.0 - fg_a), hoisted
    let mut out = [0u8; 4];
    for c in 0..3 {
        // Identical graph to the reference: fg_c_a = (fg_c/max)·fa,
        // bg_c_a = (bg_c/max)·ba, num = fg_c_a + bg_c_a·k,
        // out_c = trunc(max · (num / alpha_final)).
        let fg_c_a = (src[c] as f32 / MAX) * fa;
        let bg_c_a = (dst[c] as f32 / MAX) * ba;
        let num = fg_c_a + bg_c_a * k;
        out[c] = (MAX * (num / alpha_final)) as u8; // NumCast truncates
    }
    out[3] = (MAX * alpha_final) as u8;
    *dst = out;
}

/// Paints a snapshot frame's tiles over `base` **in place**, returning
/// whether anything changed.
///
/// This is the Rust-native replacement for the V1 port's clone-paint-compare
/// cycle: instead of allocating a second full-size buffer, copying it,
/// painting over the copy and then byte-comparing both frames, the tiles are
/// painted straight into the running canvas and every touched destination
/// pixel is compared *before* it is overwritten. Work drops from three
/// full-frame passes (copy + paint + compare) to a single pass over the
/// tiles' own footprint, and the peak memory halves (no second frame
/// buffer). Tiles that decode fully transparent — the overwhelming majority
/// of a quiet snapshot — cost only an alpha check per pixel.
///
/// The blend is exactly `image::imageops::overlay`'s src-over semantics
/// (which mirror QPainter's `CompositionMode_SourceOver`), verified by test.
/// When `changed` is false the canvas is guaranteed bit-identical to the
/// input, which keeps the caller's unchanged-frame bookkeeping sound.
pub fn paint_overlay_inplace(
    base: &mut RgbaImage,
    tile_coords: &[TileCoord],
    decoded: &[Option<RgbaImage>],
) -> OverlayOutcome {
    let mut outcome = OverlayOutcome {
        changed: false,
        tiles_painted: 0,
    };
    let (fw, fh) = (base.width(), base.height());
    for (coord, tile) in tile_coords.iter().zip(decoded) {
        let Some(tile) = tile else { continue };
        // Clip the tile rect to the frame, same bounds math as
        // image::imageops::overlay (overlay_bounds_ext).
        let ox = coord.ox.max(0).min(fw as i32) as u32;
        let oy = coord.oy.max(0).min(fh as i32) as u32;
        let max_x = (coord.ox as i64 + tile.width() as i64).clamp(0, fw as i64) as u32;
        let max_y = (coord.oy as i64 + tile.height() as i64).clamp(0, fh as i64) as u32;
        if ox >= max_x || oy >= max_y {
            continue;
        }
        outcome.tiles_painted += 1;
        let sx0 = ox as i64 - coord.ox as i64;
        let stride = tile.width() as usize;
        let row_len = (max_x - ox) as usize;
        let raw = base.as_mut();
        let kernel = crate::simd::kernel();
        for y in oy..max_y {
            let src_row = ((y as i64 - coord.oy as i64) * stride as i64 + sx0) as usize;
            let src = &tile.as_raw()[src_row * 4..][..row_len * 4];
            let dst_row = (y as usize * fw as usize + ox as usize) * 4;
            let dst = &mut raw[dst_row..][..row_len * 4];
            if kernel.blend_rect(dst, row_len, src, row_len, row_len, 1) {
                outcome.changed = true;
            }
        }
    }
    outcome
}

/// A compact per-tile mask of the pixels a cumulative snapshot painted.
///
/// History snapshots are cumulative diffs against the day base, not deltas
/// between consecutive snapshots. To render snapshot N correctly the pipeline
/// has to know which pixels snapshot N-1 painted, so it can first restore them
/// to the day base and then paint snapshot N. The mask stores one bit per tile
/// pixel (8 KiB for a 256×256 chunk) instead of a second copy of the decoded
/// tile, which keeps the running-canvas fast path affordable.
#[derive(Debug, Clone)]
pub(crate) struct TileMask {
    width: u32,
    height: u32,
    /// One row of mask bits per tile row (`row_words` = ceil(width / 64)), so
    /// the restore pass can walk rows without a division per set pixel.
    bits: Vec<u64>,
    /// Every pixel of the tile was painted, so `bits` is unused (and empty).
    /// Fully opaque chunks are common enough that special-casing them skips
    /// the per-pixel mask bookkeeping entirely.
    full: bool,
}

impl TileMask {
    fn row_words(width: u32) -> usize {
        (width as usize).div_ceil(64)
    }

    pub(crate) fn new(width: u32, height: u32) -> Self {
        let words = Self::row_words(width) * height as usize;
        Self {
            width,
            height,
            bits: vec![0; words],
            full: false,
        }
    }

    /// A mask that covers the whole tile, without per-pixel storage.
    fn full(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            bits: Vec::new(),
            full: true,
        }
    }

    /// Marks a pixel; callers guarantee `lx < width` and `ly < height`.
    pub(crate) fn set(&mut self, lx: usize, ly: usize) {
        if !self.full {
            let words = Self::row_words(self.width);
            self.bits[ly * words + lx / 64] |= 1u64 << (lx % 64);
        }
    }

    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn is_full(&self) -> bool {
        self.full
    }
}

/// Renders one history snapshot over the day base, restoring pixels that were
/// dropped from the previous cumulative snapshot.
///
/// `masks` must have one entry per tile and is updated in place. A pixel that
/// was painted by the previous snapshot but is absent (transparent or 404)
/// from the current one is restored from `day_base`; every pixel painted by
/// the current snapshot is blended over `day_base`, never over the previous
/// frame's value. This matches what the sites' own history viewer renders:
/// `day_base + current cumulative snapshot`.
///
/// `changed` is true only when the final canvas differs from the canvas before
/// this call, so an identical cumulative snapshot can still be linked to its
/// predecessor even though the implementation had to restore-and-repaint it
/// internally.
pub(crate) fn apply_cumulative_snapshot(
    canvas: &mut RgbaImage,
    day_base: &RgbaImage,
    tile_coords: &[TileCoord],
    decoded: &[Option<RgbaImage>],
    masks: &mut [Option<TileMask>],
) -> OverlayOutcome {
    let mut outcome = OverlayOutcome {
        changed: false,
        tiles_painted: 0,
    };
    debug_assert_eq!(canvas.dimensions(), day_base.dimensions());
    if canvas.dimensions() != day_base.dimensions() {
        // Should be impossible: the day base is always composed at the frame
        // size. Do not risk an out-of-bounds read if a caller bypasses the
        // pipeline.
        return outcome;
    }

    let fw = canvas.width() as usize;
    let fh = canvas.height() as usize;
    let stride = fw * 4;
    let base_raw = day_base.as_raw();
    let canvas_raw: &mut [u8] = canvas.as_mut();

    for (idx, coord) in tile_coords.iter().enumerate() {
        let current = decoded.get(idx).and_then(|tile| tile.as_ref());
        let prev_mask = masks.get_mut(idx).and_then(|mask| mask.take());
        if current.is_none() && prev_mask.is_none() {
            continue;
        }
        // Classify the whole tile once. Real pixel-art chunks are almost
        // always either fully transparent (a quiet snapshot), fully opaque or
        // a mix; the first two skip the float blend and the day-base read.
        let class = current.map(classify_tile);

        // 1) Restore whatever the previous cumulative snapshot painted that
        //    this snapshot does not paint again. A pixel the current tile
        //    covers is rewritten from the day base in step 2 anyway, so it
        //    does not need to be restored first. Cumulative snapshots only
        //    grow during a day, so the restore pass usually finds almost
        //    nothing to do.
        if let Some(mask) = prev_mask {
            let (mw, mh) = mask.dimensions();
            let (mw, mh) = (mw as usize, mh as usize);
            // Frame-space rect covered by the mask, clipped to the frame.
            let x0 = coord.ox.max(0) as usize;
            let y0 = coord.oy.max(0) as usize;
            let x1 = (coord.ox as i64 + mw as i64).clamp(0, fw as i64) as usize;
            let y1 = (coord.oy as i64 + mh as i64).clamp(0, fh as i64) as usize;
            if mw > 0 && mh > 0 && x0 < x1 && y0 < y1 {
                let covers_all = class == Some(TileAlpha::FullOpaque);
                if !covers_all {
                    let cur = current
                        .map(|tile| (tile.as_raw(), tile.width() as usize, tile.height() as usize));
                    // A dropped tile can be restored as whole rows.
                    if mask.is_full() && cur.is_none() {
                        for py in y0..y1 {
                            let off = py * stride + x0 * 4;
                            let len = (x1 - x0) * 4;
                            let src = &base_raw[off..off + len];
                            let dst = &mut canvas_raw[off..off + len];
                            if dst[..] != src[..] {
                                dst.copy_from_slice(src);
                                outcome.changed = true;
                            }
                        }
                    } else {
                        let sx0 = x0 as i64 - coord.ox as i64;
                        let visible = x1 - x0;
                        let row_words = TileMask::row_words(mw as u32);
                        for py in y0..y1 {
                            let ly = (py as i64 - coord.oy as i64) as usize;
                            let dst_off = py * stride + x0 * 4;
                            let cur_row =
                                cur.and_then(|(raw, tw, th)| (ly < th).then_some((raw, tw)));
                            if mask.is_full() {
                                for k in 0..visible {
                                    let lx = sx0 as usize + k;
                                    if let Some((raw, tw)) = cur_row {
                                        if lx < tw && raw[(ly * tw + lx) * 4 + 3] != 0 {
                                            continue;
                                        }
                                    }
                                    let off = dst_off + k * 4;
                                    if canvas_raw[off..off + 4] != base_raw[off..off + 4] {
                                        canvas_raw[off..off + 4]
                                            .copy_from_slice(&base_raw[off..off + 4]);
                                        outcome.changed = true;
                                    }
                                }
                            } else {
                                let words = &mask.bits[ly * row_words..(ly + 1) * row_words];
                                let mut base_lx = 0usize;
                                for &word in words {
                                    let mut bits = word;
                                    while bits != 0 {
                                        let lx = base_lx + bits.trailing_zeros() as usize;
                                        bits &= bits - 1;
                                        if lx < sx0 as usize || lx >= sx0 as usize + visible {
                                            continue;
                                        }
                                        if let Some((raw, tw)) = cur_row {
                                            if lx < tw && raw[(ly * tw + lx) * 4 + 3] != 0 {
                                                continue;
                                            }
                                        }
                                        let off = dst_off + (lx - sx0 as usize) * 4;
                                        if canvas_raw[off..off + 4] != base_raw[off..off + 4] {
                                            canvas_raw[off..off + 4]
                                                .copy_from_slice(&base_raw[off..off + 4]);
                                            outcome.changed = true;
                                        }
                                    }
                                    base_lx += 64;
                                }
                            }
                        }
                    }
                }
            }
        }

        // 2) Paint the current cumulative snapshot over the restored day base.
        let Some(tile) = current else {
            if idx < masks.len() {
                masks[idx] = None;
            }
            continue;
        };
        let (tw, th) = (tile.width() as usize, tile.height() as usize);
        if tw == 0 || th == 0 || class == Some(TileAlpha::Empty) {
            if idx < masks.len() {
                masks[idx] = None;
            }
            continue;
        }

        // Clip the tile rectangle to the frame once, then walk row slices.
        let cx0 = coord.ox as i64;
        let cy0 = coord.oy as i64;
        let ox0 = cx0.max(0) as usize;
        let oy0 = cy0.max(0) as usize;
        let ox1 = (cx0 + tw as i64).clamp(0, fw as i64) as usize;
        let oy1 = (cy0 + th as i64).clamp(0, fh as i64) as usize;
        if ox0 >= ox1 || oy0 >= oy1 {
            if idx < masks.len() {
                masks[idx] = None;
            }
            continue;
        }
        let sx0 = (ox0 as i64 - cx0) as usize;
        let visible = (ox1 - ox0) * 4;
        let tile_stride = tw * 4;
        let raw = tile.as_raw();

        let mut mask = if class == Some(TileAlpha::FullOpaque) {
            TileMask::full(tw as u32, th as u32)
        } else {
            TileMask::new(tw as u32, th as u32)
        };
        let mut painted = false;

        if class == Some(TileAlpha::FullOpaque) {
            // Whole rows are opaque: a memcpy beats a per-pixel blend check.
            for py in oy0..oy1 {
                let ly = (py as i64 - cy0) as usize;
                let s0 = ly * tile_stride + sx0 * 4;
                let src = &raw[s0..s0 + visible];
                let off = py * stride + ox0 * 4;
                let dst = &mut canvas_raw[off..off + visible];
                if dst[..] != src[..] {
                    dst.copy_from_slice(src);
                    outcome.changed = true;
                }
            }
            painted = true;
        } else {
            for py in oy0..oy1 {
                let ly = (py as i64 - cy0) as usize;
                let src_row = &raw[ly * tile_stride..(ly + 1) * tile_stride];
                let off = py * stride + ox0 * 4;
                let dst_row = &mut canvas_raw[off..off + visible];
                for (nx, (d, s)) in dst_row
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(src_row.as_chunks::<4>().0[sx0..].iter())
                    .enumerate()
                {
                    match s[3] {
                        0 => {}
                        255 => {
                            painted = true;
                            mask.set(sx0 + nx, ly);
                            if *d != *s {
                                *d = *s;
                                outcome.changed = true;
                            }
                        }
                        _ => {
                            painted = true;
                            mask.set(sx0 + nx, ly);
                            let base_off = py * stride + (ox0 + nx) * 4;
                            let mut target = [
                                base_raw[base_off],
                                base_raw[base_off + 1],
                                base_raw[base_off + 2],
                                base_raw[base_off + 3],
                            ];
                            blend_pixel(&mut target, s);
                            if *d != target {
                                *d = target;
                                outcome.changed = true;
                            }
                        }
                    }
                }
            }
        }

        if painted {
            outcome.tiles_painted += 1;
        }
        if idx < masks.len() {
            masks[idx] = painted.then_some(mask);
        }
    }
    outcome
}

/// The alpha shapes a decoded chunk can have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TileAlpha {
    /// No pixel carries any alpha: the chunk is a no-op.
    Empty,
    /// Every pixel is fully opaque: whole rows can be copied.
    FullOpaque,
    /// Every non-transparent pixel is fully opaque, but some are transparent:
    /// direct copy, no blending and no day-base read.
    OpaqueOnly,
    /// At least one partially transparent pixel: needs the full blend.
    Mixed,
}

fn classify_tile(tile: &RgbaImage) -> TileAlpha {
    let mut any = false;
    let mut all_opaque = true;
    let mut has_partial = false;
    for px in tile.as_raw().as_chunks::<4>().0 {
        match px[3] {
            0 => all_opaque = false,
            255 => any = true,
            _ => {
                any = true;
                all_opaque = false;
                has_partial = true;
                break;
            }
        }
    }
    if !any {
        TileAlpha::Empty
    } else if has_partial {
        TileAlpha::Mixed
    } else if all_opaque {
        TileAlpha::FullOpaque
    } else {
        TileAlpha::OpaqueOnly
    }
}

/// Source-over composites `src` onto `base` without touching `base`.
#[cfg(test)]
fn blend_over(base: Rgba<u8>, src: Rgba<u8>) -> Rgba<u8> {
    let mut out = [base[0], base[1], base[2], base[3]];
    blend_pixel(&mut out, &[src[0], src[1], src[2], src[3]]);
    Rgba(out)
}

/// Paints a snapshot frame's tiles over `base` (the accumulated image).
/// When no tile decoded, `base` is returned unchanged.
///
/// Kept for API compatibility with the front-ends; the pipeline uses
/// [`apply_cumulative_snapshot`] instead, because history snapshots are
/// cumulative diffs rather than deltas.
pub fn paint_overlay(
    base: &RgbaImage,
    tile_coords: &[TileCoord],
    decoded: &[Option<RgbaImage>],
) -> RgbaImage {
    let any = decoded.iter().any(|d| d.is_some());
    if !any {
        return base.clone();
    }
    let mut out = base.clone();
    for (coord, tile) in tile_coords.iter().zip(decoded) {
        if let Some(tile) = tile {
            overlay_rgba(&mut out, tile, coord.ox as i64, coord.oy as i64);
        }
    }
    out
}

/// Packs a `0xRRGGBB` value into an opaque RGBA color (canvas background).
pub fn rgba_from_rgb(rgb: u32) -> Rgba<u8> {
    Rgba([
        ((rgb >> 16) & 0xff) as u8,
        ((rgb >> 8) & 0xff) as u8,
        (rgb & 0xff) as u8,
        0xff,
    ])
}

/// True when the composed image carries no content at all: every pixel equals
/// the background colour.
///
/// Kept for tests and tooling. The pipeline deliberately does **not** use this
/// to detect a missing day base: a base tile that decoded to the background
/// colour is a real, blank day and must reset the canvas, whereas only a
/// completely missing base should be carried forward.
pub fn is_solid_background(img: &RgbaImage, bg: Rgba<u8>) -> bool {
    img.pixels().all(|p| *p == bg)
}

/// Raw byte equality between two frames.
///
/// The V1 original used a full-frame equality check as the unchanged-frame
/// test *after* clone-paint-compare; the Rust pipeline computes the same
/// signal while rendering each cumulative snapshot
/// ([`apply_cumulative_snapshot`]), so nothing on the hot path calls this
/// anymore. Kept public as the canonical full-frame equality check for tests
/// and tooling.
#[allow(dead_code)]
pub fn frames_equal(a: &RgbaImage, b: &RgbaImage) -> bool {
    a.dimensions() == b.dimensions() && a.as_raw() == b.as_raw()
}

/// Saves a frame as PNG. `quality` (-1 = default, 0..9) maps to the PNG
/// compression level, matching the V1 implementation's V1 PNG writer behavior.
///
/// `png` only writes the IEND chunk and flushes its `BufWriter` from `Drop`,
/// where any I/O error is discarded. A full disk would therefore silently
/// produce a truncated frame that `--resume` happily adopts. The writer below
/// records write/flush failures in a shared slot so they surface after the
/// encoder has been dropped (and has flushed for the last time).
pub fn save_png(img: &RgbaImage, path: &std::path::Path, quality: i32) -> image::ImageResult<()> {
    let file = std::fs::File::create(path)?;
    let comp = match quality.clamp(-1, 9) {
        -1 => image::codecs::png::CompressionType::Default,
        0 => image::codecs::png::CompressionType::Fast,
        n if n >= 9 => image::codecs::png::CompressionType::Best,
        n => image::codecs::png::CompressionType::Level(n.try_into().expect("clamped 1..=8")),
    };
    let error: Arc<Mutex<Option<std::io::Error>>> = Arc::new(Mutex::new(None));
    let writer = CheckedWriter {
        inner: file,
        error: Arc::clone(&error),
    };
    let encoder = image::codecs::png::PngEncoder::new_with_quality(
        std::io::BufWriter::new(writer),
        comp,
        image::codecs::png::FilterType::Adaptive,
    );
    image::ImageEncoder::write_image(
        encoder,
        img.as_raw(),
        img.width(),
        img.height(),
        image::ExtendedColorType::Rgba8,
    )?;
    // `write_image` consumed the encoder, so the `BufWriter` (and its final
    // flush) has already run; the slot now contains the first I/O failure.
    if let Some(e) = error.lock().unwrap_or_else(|p| p.into_inner()).take() {
        return Err(image::ImageError::IoError(e));
    }
    Ok(())
}

/// A `Write` adapter that remembers the first I/O error instead of letting it
/// vanish in `BufWriter`'s `Drop`.
struct CheckedWriter<W: std::io::Write> {
    inner: W,
    error: Arc<Mutex<Option<std::io::Error>>>,
}

impl<W: std::io::Write> std::io::Write for CheckedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let result = self.inner.write(buf);
        if let Err(e) = &result {
            let mut slot = self.error.lock().unwrap_or_else(|p| p.into_inner());
            if slot.is_none() {
                *slot = Some(std::io::Error::new(e.kind(), e.to_string()));
            }
        }
        result
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let result = self.inner.flush();
        if let Err(e) = &result {
            let mut slot = self.error.lock().unwrap_or_else(|p| p.into_inner());
            if slot.is_none() {
                *slot = Some(std::io::Error::new(e.kind(), e.to_string()));
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(x: i64, y: i64, w: i64, h: i64) -> Region {
        Region {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn tile_coords_cover_the_region() {
        // 65536-px canvas, standard -128..128 region like the GUI default.
        let coords = build_tile_coords(&region(-128, -128, 257, 257), 65536);
        // offset = -32768; the region spans -128..=128, so tiles 127 and 128
        // cover each axis (2 per axis = 4 tiles).
        assert_eq!(coords.len(), 4);
        let xs: Vec<i64> = coords.iter().map(|c| c.xi).collect();
        assert!(xs.contains(&127) && xs.contains(&128));
        // ox = xi*256 + offset - x = 127*256 - 32768 + 128 = -128.
        assert_eq!(coords[0].ox, -128);
        assert_eq!(coords[0].oy, -128);
    }

    #[test]
    fn tile_coords_clamp_to_canvas() {
        // A region hanging over the canvas edge must not request tiles that
        // cannot exist.
        let coords = build_tile_coords(&region(-70000, -70000, 200, 200), 65536);
        assert!(coords
            .iter()
            .all(|c| c.xi >= 0 && c.yi >= 0 && c.xi < 256 && c.yi < 256));
        let coords = build_tile_coords(&region(65400, 65400, 200, 200), 65536);
        assert!(coords.iter().all(|c| c.xi < 256 && c.yi < 256));
    }

    #[test]
    fn base_composition_fills_and_paints() {
        let r = region(0, 0, 300, 300);
        let coords = build_tile_coords(&r, 4096);
        assert_eq!(coords.len(), 4); // tiles (0,0),(1,0),(0,1),(1,1)

        // Two tiles: an opaque red 256x256 and a 50%-alpha green tile.
        let red = RgbaImage::from_pixel(256, 256, Rgba([255, 0, 0, 255]));
        let green = RgbaImage::from_pixel(256, 256, Rgba([0, 255, 0, 128]));

        let datas: Vec<Option<Vec<u8>>> = vec![Some(png_bytes(&red)), Some(png_bytes(&green))];
        let decoded = decode_tiles_parallel(&datas);
        assert_eq!(decoded.iter().filter(|d| d.is_some()).count(), 2);

        let bg = Rgba([10, 20, 30, 255]);
        let img = compose_base(&r, bg, &coords, &decoded);
        assert_eq!((img.width(), img.height()), (300, 300));
        // Tile 1 (opaque red) covers x<256.
        assert_eq!(img.get_pixel(0, 0), &Rgba([255, 0, 0, 255]));
        // Tile 2 (50% green) starts at x=256 and blends over the background
        // fill: 0*128/255 + 10*127/255 ≈ 5, 255*128/255 + 20*127/255 ≈ 138.
        let px = img.get_pixel(257, 0);
        assert!((px[0] as i32 - 5).abs() <= 2);
        assert!((px[1] as i32 - 138).abs() <= 2);
        assert!((px[2] as i32 - 15).abs() <= 2);
    }

    #[test]
    fn solid_background_detection() {
        let r = region(0, 0, 16, 16);
        let coords = build_tile_coords(&r, 4096);
        let bg = Rgba([255, 255, 255, 255]);
        // No tiles at all -> solid background.
        let img = compose_base(&r, bg, &coords, &[None, None, None, None]);
        assert!(is_solid_background(&img, bg));
        // A single opaque pixel breaks the check.
        let mut t = RgbaImage::new(256, 256);
        *t.get_pixel_mut(5, 5) = Rgba([1, 2, 3, 255]);
        let img = compose_base(&r, bg, &coords, &[Some(t), None, None, None]);
        assert!(!is_solid_background(&img, bg));
    }

    #[test]
    fn overlay_without_tiles_returns_base() {
        let base = RgbaImage::from_pixel(4, 4, Rgba([1, 2, 3, 255]));
        let coords = build_tile_coords(&region(0, 0, 4, 4), 4096);
        let out = paint_overlay(&base, &coords, &[None, None, None, None]);
        assert!(frames_equal(&base, &out));
    }

    #[test]
    fn blend_pixel_fast_paths() {
        // Transparent source: destination untouched.
        let mut dst = [7, 8, 9, 10];
        blend_pixel(&mut dst, &[1, 2, 3, 0]);
        assert_eq!(dst, [7, 8, 9, 10]);
        // Opaque source: full replace (alpha included).
        blend_pixel(&mut dst, &[1, 2, 3, 255]);
        assert_eq!(dst, [1, 2, 3, 255]);
        // Transparent destination + partial source: the blend is *almost*
        // an identity of the source, but image's Blend computes
        // out_c = trunc(255·((c/255)·fa / fa)) and the f32 round trip can
        // lose one LSB for some values (e.g. c = 9 → 8). Must match that
        // exactly; the blend_parity integration test proves it for every
        // alpha pair. With ba = 0 the algebra collapses to src unchanged
        // for these channel values.
        let mut clear = [0, 0, 0, 0];
        blend_pixel(&mut clear, &[9, 9, 9, 128]);
        assert_eq!(clear, [9, 9, 9, 128]);
        // A known partial-alpha result. Note the output alpha: image's
        // float Blend computes trunc(255·alpha_final) = 254 here, and we
        // must match it bit-for-bit (see tests/blend_parity.rs).
        let mut d = [10u8, 20, 30, 255];
        blend_pixel(&mut d, &[200, 50, 50, 128]);
        assert_eq!(d, [105, 35, 40, 254]);
        // Case where image's float rounding differs from exact rational
        // math (integer-only blends get this wrong): must match image.
        let mut d = [200, 0, 128, 2];
        blend_pixel(&mut d, &[200, 0, 55, 1]);
        assert_eq!(d, [199, 0, 103, 2]);
    }

    #[test]
    fn overlay_rgba_is_byte_identical_to_image_overlay() {
        // Deterministic pseudo-random pixels covering all alpha classes.
        let mut seed = 0x1234_5678u32;
        let mut next = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 16) as u8
        };
        let dst_src: Vec<u8> = (0..(9 * 7 * 4)).map(|_| next()).collect();
        let tile_src: Vec<u8> = (0..(4 * 3 * 4))
            .map(|i| {
                let v = next();
                if i % 4 == 3 {
                    // Rotate through the three interesting alpha classes.
                    match (i / 4) % 3 {
                        0 => 0,
                        1 => 255,
                        _ => 128,
                    }
                } else {
                    v
                }
            })
            .collect();
        let dst = RgbaImage::from_raw(9, 7, dst_src).unwrap();
        let tile = RgbaImage::from_raw(4, 3, tile_src).unwrap();

        // Inside, partially outside on every side, fully outside, and the
        // zero-width/zero-height edge cases of `overlay_bounds_ext`.
        let offsets = [
            (0, 0),
            (2, 1),
            (-2, -1),
            (-4, 5),
            (7, 4),
            (8, 6),
            (9, 7),
            (5, -3),
            (-9, 0),
        ];
        for (ox, oy) in offsets {
            let mut expected = dst.clone();
            image::imageops::overlay(&mut expected, &tile, ox, oy);
            let mut actual = dst.clone();
            overlay_rgba(&mut actual, &tile, ox, oy);
            assert_eq!(
                actual.as_raw(),
                expected.as_raw(),
                "mismatch at offset ({ox}, {oy})"
            );
        }
    }

    #[test]
    fn tile_count_matches_the_built_grid() {
        let cases = [
            (
                Region {
                    x: 0,
                    y: 0,
                    width: 512,
                    height: 256,
                },
                4096,
            ),
            (
                Region {
                    x: -128,
                    y: -128,
                    width: 257,
                    height: 257,
                },
                65536,
            ),
            // Fully outside the canvas: nothing to fetch.
            (
                Region {
                    x: 9000,
                    y: 9000,
                    width: 10,
                    height: 10,
                },
                4096,
            ),
            // Negative/unknown canvas size: no clamping at all.
            (
                Region {
                    x: -600,
                    y: -600,
                    width: 1200,
                    height: 1200,
                },
                0,
            ),
        ];
        for (region, canvas) in cases {
            assert_eq!(
                tile_count(&region, canvas),
                build_tile_coords(&region, canvas).len() as i64,
                "region {region:?} on a {canvas}px canvas"
            );
        }
        // An absurd canvas size plus a big region is reported as a count,
        // without building (or pre-allocating) the impossible grid.
        let huge = Region {
            x: 0,
            y: 0,
            width: 1_000_000,
            height: 1_000_000,
        };
        assert!(tile_count(&huge, 1_000_000_000) > 10_000_000);
    }

    #[test]
    fn paint_inplace_equals_clone_paint_and_detects_change() {
        let r = region(0, 0, 300, 300);
        let coords = build_tile_coords(&r, 4096);
        let bg = Rgba([10, 20, 30, 255]);
        let base = compose_base(&r, bg, &coords, &[None, None, None, None]);

        // A partially transparent tile with a hole of untouched pixels.
        let mut tile = RgbaImage::from_pixel(256, 256, Rgba([200, 50, 50, 128]));
        for y in 0..64 {
            for x in 0..64 {
                *tile.get_pixel_mut(x, y) = Rgba([0, 0, 0, 0]);
            }
        }
        let decoded = vec![Some(tile), None, None, None];

        // Reference path: clone + imageops overlay.
        let reference = paint_overlay(&base, &coords, &decoded);

        // In-place path must produce the exact same canvas...
        let mut inplace = base.clone();
        let outcome = paint_overlay_inplace(&mut inplace, &coords, &decoded);
        assert!(outcome.changed);
        assert_eq!(outcome.tiles_painted, 1);
        assert!(frames_equal(&reference, &inplace));

        // ...and painting the identical tiles again does NOT restore the
        // canvas (src-over is not idempotent for semi-transparent pixels:
        // 128 composites to 254, then 128 over 254 lands at 255), so the
        // second pass legitimately reports a change. What must hold — and
        // what the pipeline's link decision relies on — is that whenever
        // `changed` is false the canvas provably stayed bit-identical.
        let snapshot = inplace.clone();
        let outcome2 = paint_overlay_inplace(&mut inplace, &coords, &decoded);
        assert!(outcome2.changed);
        assert!(!frames_equal(&snapshot, &inplace));

        // A no-op pass (fully transparent tiles) leaves the canvas exactly
        // as it was and never reports a change. The snapshot must be taken
        // *after* the previous pass mutated the canvas — painting onto an
        // opaque destination with alpha-0 source pixels is provably inert.
        let snapshot2 = inplace.clone();
        let clear = RgbaImage::from_pixel(256, 256, Rgba([7, 7, 7, 0]));
        let outcome3 =
            paint_overlay_inplace(&mut inplace, &coords, &[Some(clear), None, None, None]);
        assert!(!outcome3.changed);
        assert!(frames_equal(&snapshot2, &inplace));
    }

    /// History snapshots are cumulative diffs against the day base. A pixel
    /// removed from a later cumulative snapshot must disappear from the frame,
    /// and an identical cumulative snapshot must still report "unchanged" so
    /// the pipeline can link it.
    #[test]
    fn cumulative_snapshot_restores_reverted_pixels() {
        let bg = Rgba([10, 20, 30, 255]);
        let base = RgbaImage::from_pixel(4, 4, bg);
        let coords = [TileCoord {
            xi: 0,
            yi: 0,
            ox: 0,
            oy: 0,
        }];
        let mut masks: Vec<Option<TileMask>> = vec![None];
        let mut canvas = base.clone();

        let mut tile = RgbaImage::from_pixel(4, 4, Rgba([0, 0, 0, 0]));
        *tile.get_pixel_mut(1, 1) = Rgba([200, 10, 10, 255]);
        *tile.get_pixel_mut(2, 2) = Rgba([10, 200, 10, 128]);

        let outcome = apply_cumulative_snapshot(
            &mut canvas,
            &base,
            &coords,
            &[Some(tile.clone())],
            &mut masks,
        );
        assert!(outcome.changed);
        assert_eq!(canvas.get_pixel(1, 1), &Rgba([200, 10, 10, 255]));
        assert_eq!(
            canvas.get_pixel(2, 2),
            &blend_over(bg, Rgba([10, 200, 10, 128]))
        );
        assert!(masks[0].is_some());

        // Re-applying the same cumulative snapshot leaves the final canvas
        // bit-identical, so the frame can be linked.
        let again =
            apply_cumulative_snapshot(&mut canvas, &base, &coords, &[Some(tile)], &mut masks);
        assert!(!again.changed);
        assert!(masks[0].is_some());

        // The pixel disappeared from the cumulative diff: it must revert to
        // the day base, not stay on the running canvas forever.
        let empty = RgbaImage::from_pixel(4, 4, Rgba([0, 0, 0, 0]));
        let reverted =
            apply_cumulative_snapshot(&mut canvas, &base, &coords, &[Some(empty)], &mut masks);
        assert!(reverted.changed);
        assert!(canvas.pixels().all(|p| *p == bg));
        assert!(masks[0].is_none());

        // An empty cumulative snapshot on an already-base canvas is a no-op.
        let empty_again = RgbaImage::from_pixel(4, 4, Rgba([0, 0, 0, 0]));
        let noop = apply_cumulative_snapshot(
            &mut canvas,
            &base,
            &coords,
            &[Some(empty_again)],
            &mut masks,
        );
        assert!(!noop.changed);
    }

    /// zune-png and the `image` fallback must agree for RGB and grayscale
    /// chunks too: the fast decoder is allowed to be faster, never different.
    #[test]
    fn fast_decoder_matches_image_for_rgb_and_grayscale() {
        let rgb = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            16,
            8,
            image::Rgb([12, 200, 45]),
        ));
        let luma = DynamicImage::ImageLuma8(image::GrayImage::from_pixel(8, 8, image::Luma([77])));
        let rgba = DynamicImage::ImageRgba8(RgbaImage::from_pixel(8, 8, Rgba([1, 2, 3, 128])));
        for img in [rgb, luma, rgba] {
            let mut buf = std::io::Cursor::new(Vec::new());
            img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
            let bytes = buf.into_inner();
            let expected = image::load_from_memory(&bytes).unwrap().to_rgba8();
            let actual = decode_tile(&bytes).expect("fast decode");
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn png_round_trip() {
        let img = RgbaImage::from_pixel(8, 8, Rgba([9, 8, 7, 255]));
        let bytes = png_bytes(&img);
        let back = decode_tile(&bytes).unwrap();
        assert!(frames_equal(&img, &back));
    }

    fn png_bytes(img: &RgbaImage) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img.clone())
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }
}
