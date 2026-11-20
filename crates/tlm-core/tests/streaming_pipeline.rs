//! End-to-end proof of the streaming path.
//!
//! Seeds the tile cache exactly like `pipeline_accumulation`, flips the run to
//! streaming mode, and checks that ffmpeg produced a real video file. Streaming
//! keeps the intermediate `t*.png` frames by default; the explicit opt-out is
//! verified with a second pass.

use std::collections::BTreeMap;

use chrono::NaiveDate;
use image::{Rgba, RgbaImage};
use tlm_core::compose;
use tlm_core::coord::Region;
use tlm_core::pipeline::{self, PipelineParams};
use tlm_core::settings::Settings;
use tokio_util::sync::CancellationToken;

const STORAGE: &str = "invalid.invalid/stream";
const CANVAS: &str = "0";
const DATE: &str = "20000104";
const CANVAS_PX: i64 = 4096;

fn ffmpeg_path() -> std::path::PathBuf {
    tlm_core::video::find_ffmpeg().expect("ffmpeg was found by the test guard")
}

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

fn seed_tile(x: i64, y: i64, ts: &str, img: &RgbaImage) {
    tlm_core::cache::save_tile(STORAGE, CANVAS, DATE, x, y, ts, &png(img));
}

#[tokio::test]
async fn streaming_run_writes_video_and_keeps_pngs_by_default() {
    if tlm_core::video::find_ffmpeg().is_none() {
        eprintln!("ffmpeg not installed; skipping streaming integration test");
        return;
    }
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let out_dir = tempfile::tempdir().expect("out tempdir");
    std::env::set_var(tlm_core::cache::CACHE_DIR_ENV, cache_dir.path());

    let region = Region {
        x: 0,
        y: 0,
        width: 512,
        height: 256,
    };
    let coords = compose::build_tile_coords(&region, CANVAS_PX);
    let (l, r) = (coords[0], coords[1]);
    let clear = solid([0, 0, 0, 0]);
    let content = solid([10, 200, 30, 255]);
    let changed = solid([220, 40, 40, 255]);
    seed_tile(l.xi, l.yi, "", &content);
    seed_tile(r.xi, r.yi, "", &clear);
    seed_tile(l.xi, l.yi, "0001", &content);
    seed_tile(r.xi, r.yi, "0001", &changed);

    let mut day_timestamps = BTreeMap::new();
    day_timestamps.insert(
        NaiveDate::from_ymd_opt(2000, 1, 4).unwrap(),
        vec!["0001".to_string()],
    );

    let settings = Settings {
        domain: "invalid.invalid".into(),
        storage_domain: STORAGE.into(),
        max_retries: 0,
        stream_video: true,
        fps: 10,
        ..Settings::default()
    };
    let params = PipelineParams {
        settings: settings.clone(),
        canvas_id: CANVAS.into(),
        region,
        start: NaiveDate::from_ymd_opt(2000, 1, 4).unwrap(),
        end: NaiveDate::from_ymd_opt(2000, 1, 4).unwrap(),
        out_dir: out_dir.path().to_path_buf(),
        resume: false,
        canvas_size_px: Some(CANVAS_PX),
        bg_color: Some(0),
        day_timestamps: Some(day_timestamps.clone()),
    };

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let result = pipeline::run_pipeline(params, tx, CancellationToken::new())
        .await
        .expect("streaming pipeline succeeds");

    assert_eq!(result.frames, 2, "day base + one snapshot");
    assert!(!result.cancelled);
    let video = result.video_path.expect("streaming produced a video path");
    assert_eq!(video, out_dir.path().join("timelapse.mp4"));
    let meta = std::fs::metadata(&video).expect("video exists");
    assert!(meta.len() > 0, "video file is empty");

    // The new default keeps the intermediate frames alongside the video.
    let pngs: Vec<_> = std::fs::read_dir(out_dir.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".png"))
        .collect();
    assert_eq!(pngs.len(), 2, "expected retained PNG frames: {pngs:?}");

    // The file is a real MP4 container rather than a truncated stream.
    let head = std::fs::read(&video).unwrap();
    assert_eq!(&head[4..8], b"ftyp", "missing MP4 ftyp box");

    // Opting out restores the old video-only streaming behaviour.
    let out_video_only = tempfile::tempdir().expect("video-only out tempdir");
    let mut video_only_settings = settings.clone();
    video_only_settings.save_png_sequence = false;
    let video_only_params = PipelineParams {
        settings: video_only_settings,
        canvas_id: CANVAS.into(),
        region,
        start: NaiveDate::from_ymd_opt(2000, 1, 4).unwrap(),
        end: NaiveDate::from_ymd_opt(2000, 1, 4).unwrap(),
        out_dir: out_video_only.path().to_path_buf(),
        resume: false,
        canvas_size_px: Some(CANVAS_PX),
        bg_color: Some(0),
        day_timestamps: Some(day_timestamps.clone()),
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let video_only_result = pipeline::run_pipeline(video_only_params, tx, CancellationToken::new())
        .await
        .expect("video-only streaming pipeline succeeds");
    assert!(video_only_result.video_path.is_some());
    let video_only_pngs = std::fs::read_dir(out_video_only.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".png"))
        .count();
    assert_eq!(
        video_only_pngs, 0,
        "save_png_sequence=false must write no PNGs"
    );

    // Second pass: a 9:16 export with every studio effect enabled. This pins
    // that the encoder is configured with the *preset* dimensions, not the
    // region dimensions (a mismatch makes ffmpeg reject the first frame).
    let out_social = tempfile::tempdir().expect("social out tempdir");
    let mut social_settings = settings.clone();
    social_settings.social_preset = tlm_core::settings::SocialPreset::Vertical916;
    social_settings.show_timestamp = true;
    social_settings.show_progress_bar = true;
    social_settings.watermark_text = "STREAM TEST".into();
    social_settings.camera_motion = tlm_core::settings::CameraMotion::ZoomOut;
    // Exercise the real audio muxer, not just argument construction.
    let audio_path = out_social.path().join("tone.wav");
    let audio_ok = std::process::Command::new(ffmpeg_path())
        .args([
            "-y",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
        ])
        .arg(&audio_path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if audio_ok {
        social_settings.audio_path = audio_path.to_string_lossy().into_owned();
        social_settings.audio_fade_ms = 200;
    }
    let social_params = PipelineParams {
        settings: social_settings,
        canvas_id: CANVAS.into(),
        region,
        start: NaiveDate::from_ymd_opt(2000, 1, 4).unwrap(),
        end: NaiveDate::from_ymd_opt(2000, 1, 4).unwrap(),
        out_dir: out_social.path().to_path_buf(),
        resume: false,
        canvas_size_px: Some(CANVAS_PX),
        bg_color: Some(0),
        day_timestamps: None,
    };
    // Reuse the seeded timestamps: a caller that already knows them skips the
    // network entirely.
    let mut social_params = social_params;
    social_params.day_timestamps = Some(
        [(
            NaiveDate::from_ymd_opt(2000, 1, 4).unwrap(),
            vec!["0001".to_string()],
        )]
        .into_iter()
        .collect(),
    );
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let social_result = pipeline::run_pipeline(social_params, tx, CancellationToken::new())
        .await
        .expect("social streaming pipeline succeeds");
    let social_video = social_result.video_path.expect("social video path");
    assert!(std::fs::metadata(&social_video).unwrap().len() > 0);
    let head = std::fs::read(&social_video).unwrap();
    assert_eq!(&head[4..8], b"ftyp");
}
