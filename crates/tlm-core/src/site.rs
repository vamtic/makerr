//! Per-site quirks that PixMap (and a few others) differ on vs. the
//! "normal" PixelPlanet-family layout.

use serde_json::Value;

/// Edge length in pixels of one chunk/tile on every supported site.
pub const CHUNK_PX: i64 = 256;

/// Known sites that expose the public canvas list at `/api/canvases` instead
/// of only the private `/api/me` snapshot.
///
/// `/api/me` shape (PixelYa, PXU, …):  `{ …, "canvases": { id: { "title", … } } }`
/// `/api/canvases` shape (PixMap):     `{ "canvases": { id: { "name", … } }, … }`
pub fn uses_canvases_api(domain: &str) -> bool {
    domain == "pixelplanet.fun" || domain == "pixmap.fun"
}

/// PixMap exposes snapshot history as a JSON object under `/history/snapshots`,
/// while the PixelPlanet-family sites expose a flat array of "hhmm" strings at
/// `/history`.
pub fn uses_snapshots_history_api(domain: &str) -> bool {
    domain == "pixmap.fun"
}

/// PixMap reports the canvas "size" as the number of 256-px chunks per side
/// (verified live: Earth -> 256, which is 256 chunks = 65536 px). The
/// PixelPlanet-family sites report it in pixels. This mirrors the reference
/// PixMap downloader's geometry rule: a raw value <= 1024 is a chunk count,
/// anything larger is already in pixels.
pub fn canvas_size_in_chunks(domain: &str) -> bool {
    domain == "pixmap.fun"
}

/// Canonical canvas edge length in pixels for a raw API "size" value.
pub fn canvas_size_pixels(domain: &str, raw_size: i64) -> i64 {
    if raw_size > 0 && raw_size <= 1024 && canvas_size_in_chunks(domain) {
        return raw_size * CHUNK_PX;
    }
    raw_size
}

/// Canvas edge length in chunks (tiles) for a raw API "size" value. Round up
/// so a canvas that is not an exact multiple of the tile size never loses its
/// partially covered edge tile.
pub fn canvas_size_chunks(domain: &str, raw_size: i64) -> i64 {
    let px = canvas_size_pixels(domain, raw_size);
    if px > 0 {
        (px + CHUNK_PX - 1) / CHUNK_PX
    } else {
        0
    }
}

/// URL that lists the snapshot times (HHMM) available for a canvas on a day.
/// `date` must be `yyyyMMdd`.
pub fn history_url(domain: &str, date: &str, canvas_id: &str) -> String {
    if uses_snapshots_history_api(domain) {
        format!("https://{domain}/history/snapshots/{canvas_id}?day={date}")
    } else {
        format!("https://{domain}/history?day={date}&id={canvas_id}")
    }
}

/// Display name of a canvas: `/api/canvases` responses use "name", `/api/me`
/// responses use "title".
pub fn canvas_name(canvas: &Value) -> Option<String> {
    let obj = canvas.as_object()?;
    for key in ["name", "title"] {
        if let Some(v) = obj.get(key).and_then(Value::as_str) {
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// The raw API "size" value of a canvas object, or `None` when absent.
pub fn canvas_raw_size(canvas: &Value) -> Option<i64> {
    canvas.get("size").and_then(Value::as_i64)
}

/// The palette of a canvas object as packed `0xRRGGBB` values.
///
/// The API carries them as arrays of `[r, g, b]` triples.
pub fn canvas_palette(canvas: &Value) -> Vec<[u8; 3]> {
    canvas
        .get("colors")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    let c = c.as_array()?;
                    let r = c.first()?.as_i64()? as u8;
                    let g = c.get(1)?.as_i64()? as u8;
                    let b = c.get(2)?.as_i64()? as u8;
                    Some([r, g, b])
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canvases_api_endpoint_selection() {
        assert!(uses_canvases_api("pixelplanet.fun"));
        assert!(uses_canvases_api("pixmap.fun"));
        assert!(!uses_canvases_api("pixelya.fun"));
        assert!(!uses_canvases_api("pixuniverse.fun"));
        assert!(!uses_canvases_api("custom.example.com"));
    }

    #[test]
    fn canvas_name_prefers_name() {
        let c = json!({"name": "Earth", "title": "ignored"});
        assert_eq!(canvas_name(&c).as_deref(), Some("Earth"));
    }

    #[test]
    fn canvas_name_falls_back_to_title() {
        let c = json!({"title": "World"});
        assert_eq!(canvas_name(&c).as_deref(), Some("World"));
    }

    #[test]
    fn canvas_name_from_real_pixmap_payload() {
        let doc = json!({
            "canvases": {
                "0": { "id": 0, "name": "Earth", "indent": "d", "size": 256 },
                "17": { "id": 17, "name": "Minimap", "indent": "b", "size": 16 }
            },
            "default": 0
        });
        let canvases = doc.get("canvases").unwrap();
        assert_eq!(canvas_name(&canvases["0"]).as_deref(), Some("Earth"));
        assert_eq!(canvas_name(&canvases["17"]).as_deref(), Some("Minimap"));
    }

    #[test]
    fn history_endpoint_selection() {
        assert!(uses_snapshots_history_api("pixmap.fun"));
        assert!(!uses_snapshots_history_api("pixelplanet.fun"));
        assert!(!uses_snapshots_history_api("pixelya.fun"));

        assert_eq!(
            history_url("pixmap.fun", "20000103", "0"),
            "https://pixmap.fun/history/snapshots/0?day=20000103"
        );
        assert_eq!(
            history_url("pixelplanet.fun", "20000103", "abc"),
            "https://pixelplanet.fun/history?day=20000103&id=abc"
        );
    }

    #[test]
    fn pixmap_canvas_size_is_reported_in_chunks() {
        assert!(canvas_size_in_chunks("pixmap.fun"));
        assert!(!canvas_size_in_chunks("pixelplanet.fun"));
        assert!(!canvas_size_in_chunks("pixelya.fun"));
        assert!(!canvas_size_in_chunks("pixuniverse.fun"));
    }

    #[test]
    fn pixmap_canvas_size_geometry_from_real_payload() {
        // PixMap Earth: 256 chunks -> 65536 px per side.
        assert_eq!(canvas_size_pixels("pixmap.fun", 256), 65536);
        // PixMap Moon: 64 chunks -> 16384 px.
        assert_eq!(canvas_size_pixels("pixmap.fun", 64), 16384);
        // Minimap-sized canvases: 16 chunks -> 4096 px.
        assert_eq!(canvas_size_pixels("pixmap.fun", 16), 4096);
        // A raw value above 1024 on PixMap is already in pixels.
        assert_eq!(canvas_size_pixels("pixmap.fun", 2048), 2048);
    }

    #[test]
    fn non_pixmap_canvas_size_stays_in_pixels() {
        assert_eq!(canvas_size_pixels("pixelplanet.fun", 256), 256);
        assert_eq!(canvas_size_pixels("pixelya.fun", 1024), 1024);
        assert_eq!(canvas_size_pixels("pixuniverse.fun", 8192), 8192);
        assert_eq!(canvas_size_pixels("custom.example.com", 64), 64);
    }

    #[test]
    fn invalid_canvas_size_stays_invalid() {
        assert_eq!(canvas_size_pixels("pixmap.fun", 0), 0);
        assert_eq!(canvas_size_pixels("pixmap.fun", -5), -5);
        assert_eq!(canvas_size_pixels("pixelplanet.fun", 0), 0);
    }

    #[test]
    fn canvas_size_chunks_conversion() {
        assert_eq!(canvas_size_chunks("pixmap.fun", 256), 256);
        assert_eq!(canvas_size_chunks("pixmap.fun", 64), 64);
        assert_eq!(canvas_size_chunks("pixelplanet.fun", 2048), 8);
        assert_eq!(canvas_size_chunks("pixelplanet.fun", 65536), 256);
        assert_eq!(canvas_size_chunks("pixmap.fun", 0), 0);
        assert_eq!(CHUNK_PX, 256);
    }

    #[test]
    fn palette_parsing() {
        let c = json!({"colors": [[255, 255, 255], [0, 0, 0]]});
        assert_eq!(canvas_palette(&c), vec![[255u8, 255, 255], [0, 0, 0]]);
        assert!(canvas_palette(&json!({})).is_empty());
    }
}
