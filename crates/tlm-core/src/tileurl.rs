//! Builds tile URLs for the supported storage layouts.
//!
//! Most sites use a plain slashed date layout:
//! `https://{storageBase}/{YYYY}/{MM}/{DD}/{canvasId}[/{ts}]/{x}/{y}.png`
//!
//! PixMap uses a compact "chunk" layout whose template is
//! `https://pixmap.fun/history/chunk` (i.e. the storage base already ends
//! with "/history/chunk"):
//! `https://{storageBase}/{canvasId}/{YYYYMMDD}/{HHMM}/{x}_{y}.png`
//! The base frame on PixMap is represented by the special timestamp "0000".
//!
//! `date` must be `yyyyMMdd`; `ts` is the HHMM snapshot, or empty for the
//! base frame.

/// True when the storage base uses the PixMap compact chunk layout.
pub fn pixmap_layout(storage_base: &str) -> bool {
    storage_base.contains("/history/chunk")
}

/// The constant part of the URL through the date/timestamp segment. Always
/// ends with '/' so a tile URL is simply [`prefix`] + [`suffix`].
pub fn prefix(storage_base: &str, canvas_id: &str, date: &str, ts: &str) -> String {
    if pixmap_layout(storage_base) {
        let ts_part = if ts.is_empty() { "0000" } else { ts };
        return format!("https://{storage_base}/{canvas_id}/{date}/{ts_part}/");
    }

    if ts.is_empty() {
        format!(
            "https://{}/{}/{}/{}/{}/tiles/",
            storage_base,
            &date[0..4],
            &date[4..6],
            &date[6..8],
            canvas_id
        )
    } else {
        format!(
            "https://{}/{}/{}/{}/{}/{}/",
            storage_base,
            &date[0..4],
            &date[4..6],
            &date[6..8],
            canvas_id,
            ts
        )
    }
}

/// The per-tile part appended to [`prefix`]. PixMap joins the tile
/// coordinates with '_', the PixelPlanet family uses '/'.
pub fn suffix(tile_x: i64, tile_y: i64, underscore_layout: bool) -> String {
    if underscore_layout {
        format!("{tile_x}_{tile_y}.png")
    } else {
        format!("{tile_x}/{tile_y}.png")
    }
}

/// Builds a single full tile URL.
pub fn build(
    storage_base: &str,
    canvas_id: &str,
    date: &str,
    ts: &str,
    tile_x: i64,
    tile_y: i64,
) -> String {
    prefix(storage_base, canvas_id, date, ts) + &suffix(tile_x, tile_y, pixmap_layout(storage_base))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ppf_base_frame() {
        assert_eq!(
            build("storage.gs-os.com", "0", "20000103", "", 131, 124),
            "https://storage.gs-os.com/2000/01/03/0/tiles/131/124.png"
        );
    }

    #[test]
    fn pxu_base_frame() {
        assert_eq!(
            build("pxuniverse.fun/backupdata", "0", "20000103", "", 132, 124),
            "https://pxuniverse.fun/backupdata/2000/01/03/0/tiles/132/124.png"
        );
    }

    #[test]
    fn pixelya_base_frame() {
        assert_eq!(
            build("storage.pixelya.fun/history", "5", "20000103", "", 132, 115),
            "https://storage.pixelya.fun/history/2000/01/03/5/tiles/132/115.png"
        );
    }

    #[test]
    fn pixelya_timestamped_frame() {
        assert_eq!(
            build(
                "storage.pixelya.fun/history",
                "5",
                "20000103",
                "1230",
                132,
                115
            ),
            "https://storage.pixelya.fun/history/2000/01/03/5/1230/132/115.png"
        );
    }

    #[test]
    fn pixmap_base_frame_uses_0000() {
        assert_eq!(
            build("pixmap.fun/history/chunk", "0", "20000103", "", 157, 73),
            "https://pixmap.fun/history/chunk/0/20000103/0000/157_73.png"
        );
    }

    #[test]
    fn pixmap_timestamped_frame() {
        assert_eq!(
            build("pixmap.fun/history/chunk", "0", "20000103", "1200", 157, 73),
            "https://pixmap.fun/history/chunk/0/20000103/1200/157_73.png"
        );
    }

    #[test]
    fn pixmap_custom_storage_host() {
        assert_eq!(
            build(
                "backup.pixmap.fun/history/chunk",
                "0",
                "20000103",
                "",
                157,
                73
            ),
            "https://backup.pixmap.fun/history/chunk/0/20000103/0000/157_73.png"
        );
    }

    // The hot download path hoists the frame-constant prefix() and appends the
    // short per-tile suffix(). Pin both down explicitly so the two-part
    // construction can never drift from the documented URL layouts.
    #[test]
    fn prefix_and_suffix() {
        // PixMap: compact chunk layout, the base frame uses the 0000 timestamp
        // and the tile coordinates are joined with '_'.
        assert_eq!(
            prefix("pixmap.fun/history/chunk", "0", "20000103", ""),
            "https://pixmap.fun/history/chunk/0/20000103/0000/"
        );
        assert_eq!(
            prefix("pixmap.fun/history/chunk", "0", "20000103", "1200"),
            "https://pixmap.fun/history/chunk/0/20000103/1200/"
        );
        assert_eq!(suffix(157, 73, true), "157_73.png");
        assert_eq!(suffix(-1, 0, true), "-1_0.png");
        assert_eq!(
            format!(
                "{}{}",
                prefix("pixmap.fun/history/chunk", "0", "20000103", "1200"),
                suffix(157, 73, true)
            ),
            "https://pixmap.fun/history/chunk/0/20000103/1200/157_73.png"
        );

        // PixelPlanet family: slashed date layout, the base frame lives under
        // .../tiles/ and the tile coordinates are joined with '/'.
        assert_eq!(
            prefix("storage.gs-os.com", "0", "20000103", ""),
            "https://storage.gs-os.com/2000/01/03/0/tiles/"
        );
        assert_eq!(
            prefix("storage.gs-os.com", "0", "20000103", "1230"),
            "https://storage.gs-os.com/2000/01/03/0/1230/"
        );
        assert_eq!(
            prefix("storage.pixelya.fun/history", "5", "20000103", ""),
            "https://storage.pixelya.fun/history/2000/01/03/5/tiles/"
        );
        assert_eq!(suffix(131, 124, false), "131/124.png");
    }

    #[test]
    fn pixmap_layout_detection() {
        assert!(pixmap_layout("pixmap.fun/history/chunk"));
        assert!(pixmap_layout("backup.pixmap.fun/history/chunk"));
        assert!(!pixmap_layout("storage.gs-os.com"));
        assert!(!pixmap_layout("storage.pixelya.fun/history"));
    }
}
