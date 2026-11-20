//! End-to-end regression test for the frame pipeline: snapshot tiles are
//! deltas, so every frame has to be the *accumulated* image, and a frame whose
//! tiles change nothing must be linked to its predecessor instead of re-encoded.
//!
//! Runs without any network: the tile bytes are seeded into the disk cache and
//! the snapshot times are supplied through `PipelineParams::day_timestamps`.
//! (This is a separate test binary on purpose — it sets the process-wide
//! `TIMELAPSE_CACHE_DIR`.)

use std::collections::BTreeMap;
use std::path::Path;

use chrono::NaiveDate;
use image::{Rgba, RgbaImage};
use tlm_core::compose;
use tlm_core::coord::Region;
use tlm_core::link::LinkKind;
use tlm_core::pipeline::{self, PipelineParams};
use tlm_core::settings::Settings;
use tokio_util::sync::CancellationToken;

const STORAGE: &str = "invalid.invalid/storage";
const CANVAS: &str = "0";
const DATE: &str = "20000103";
/// A 4096px canvas: the region below covers exactly two chunks.
const CANVAS_PX: i64 = 4096;

fn png(img: &RgbaImage) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img.clone())
        .write_to(&mut buf, image::ImageFormat::Png)
        .expect("encode");
    buf.into_inner()
}

fn solid(rgba: [u8; 4]) -> RgbaImage {
    RgbaImage::from_pixel(256, 256, Rgba(rgba))
}

/// Seeds one tile into the disk cache for the given snapshot timestamp
/// (`""` = the day base).
fn seed_tile(x: i64, y: i64, ts: &str, img: &RgbaImage) {
    tlm_core::cache::save_tile(STORAGE, CANVAS, DATE, x, y, ts, &png(img));
}

fn pixel(path: &Path, x: u32, y: u32) -> Rgba<u8> {
    *image::open(path)
        .expect("frame decodes")
        .to_rgba8()
        .get_pixel(x, y)
}

#[tokio::test]
async fn snapshot_frames_accumulate_and_unchanged_frames_are_linked() {
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let out_dir = tempfile::tempdir().expect("out tempdir");
    // Safe here: this test binary contains no other test that touches the cache
    // root.
    std::env::set_var(tlm_core::cache::CACHE_DIR_ENV, cache_dir.path());

    let region = Region {
        x: 0,
        y: 0,
        width: 512,
        height: 256,
    };
    let coords = compose::build_tile_coords(&region, CANVAS_PX);
    assert_eq!(coords.len(), 2, "two 256px chunks side by side");
    let (l, r) = (coords[0], coords[1]);
    assert!(l.ox < r.ox, "left chunk first");

    let content = solid([1, 2, 3, 255]);
    let clear = solid([0, 0, 0, 0]);
    let changed = solid([9, 9, 9, 255]);

    // Day base: the left chunk has content, the right one is empty.
    seed_tile(l.xi, l.yi, "", &content);
    seed_tile(r.xi, r.yi, "", &clear);
    // 00:01 changes only the right chunk.
    seed_tile(l.xi, l.yi, "0001", &clear);
    seed_tile(r.xi, r.yi, "0001", &changed);
    // 00:02 repeats 00:01 byte for byte: nothing changes.
    seed_tile(l.xi, l.yi, "0002", &clear);
    seed_tile(r.xi, r.yi, "0002", &changed);

    let mut day_timestamps = BTreeMap::new();
    day_timestamps.insert(
        NaiveDate::from_ymd_opt(2000, 1, 3).unwrap(),
        vec!["0001".to_string(), "0002".to_string()],
    );

    let settings = Settings {
        domain: "invalid.invalid".into(),
        storage_domain: STORAGE.into(),
        max_retries: 0,
        // This test inspects the PNG sequence; streaming is covered separately.
        stream_video: false,
        ..Settings::default()
    };
    let params = PipelineParams {
        settings,
        canvas_id: CANVAS.into(),
        region,
        start: NaiveDate::from_ymd_opt(2000, 1, 3).unwrap(),
        end: NaiveDate::from_ymd_opt(2000, 1, 3).unwrap(),
        out_dir: out_dir.path().to_path_buf(),
        resume: false,
        canvas_size_px: Some(CANVAS_PX),
        bg_color: Some(0),
        day_timestamps: Some(day_timestamps.clone()),
    };

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let result = pipeline::run_pipeline(params, tx, CancellationToken::new())
        .await
        .expect("pipeline succeeds");

    assert_eq!(result.frames, 3, "day base + two snapshots");
    assert!(!result.cancelled);

    let t1 = out_dir.path().join("t1.png");
    let t2 = out_dir.path().join("t2.png");
    let t3 = out_dir.path().join("t3.png");

    // Frame 1: the day base.
    assert_eq!(pixel(&t1, 0, 0), Rgba([1, 2, 3, 255]));
    assert_eq!(pixel(&t1, 300, 0), Rgba([0, 0, 0, 255]));

    // Frame 2: the delta painted onto the base — the left chunk must survive.
    assert_eq!(
        pixel(&t2, 0, 0),
        Rgba([1, 2, 3, 255]),
        "the accumulated canvas must carry the base frame's pixels into the snapshot"
    );
    assert_eq!(pixel(&t2, 300, 0), Rgba([9, 9, 9, 255]));

    // Frame 3: byte-identical to frame 2, so it is linked, not re-encoded.
    assert_eq!(
        result.linked_frames, 1,
        "the repeated snapshot is unchanged"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(result.link_kind, Some(LinkKind::HardLink));
        assert_eq!(
            std::fs::metadata(&t2).unwrap().ino(),
            std::fs::metadata(&t3).unwrap().ino(),
            "an unchanged frame must share its predecessor's inode"
        );
    }
    #[cfg(not(unix))]
    assert_eq!(std::fs::read(&t3).unwrap(), std::fs::read(&t2).unwrap());

    // A resumed run adopts the existing frames (and keeps their links) instead
    // of rewriting or renumbering them.
    #[cfg(unix)]
    let t2_inode = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(&t2).unwrap().ino()
    };
    let resume_params = PipelineParams {
        settings: Settings {
            domain: "invalid.invalid".into(),
            storage_domain: STORAGE.into(),
            max_retries: 0,
            stream_video: false,
            ..Settings::default()
        },
        canvas_id: CANVAS.into(),
        region,
        start: NaiveDate::from_ymd_opt(2000, 1, 3).unwrap(),
        end: NaiveDate::from_ymd_opt(2000, 1, 3).unwrap(),
        out_dir: out_dir.path().to_path_buf(),
        resume: true,
        canvas_size_px: Some(CANVAS_PX),
        bg_color: Some(0),
        day_timestamps: Some(day_timestamps),
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let resumed = pipeline::run_pipeline(resume_params, tx, CancellationToken::new())
        .await
        .expect("resumed pipeline succeeds");
    assert_eq!(resumed.frames, 3, "all three frames were adopted");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(&t2).unwrap().ino(),
            t2_inode,
            "resume must not rewrite an adopted frame"
        );
        assert_eq!(
            std::fs::metadata(&t3).unwrap().ino(),
            t2_inode,
            "resume must keep the adopted frame's hardlink"
        );
    }
    assert_eq!(pixel(&t2, 0, 0), Rgba([1, 2, 3, 255]));
}
