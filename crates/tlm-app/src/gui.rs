//! TimelapseMaker GUI — the Slint desktop front end.
//!
//! Layout mirrors the original design: header bar, left column (canvas, area,
//! dates, output folder, actions, console) and right column (live area
//! preview with day timeline, pipeline stages, progress). All heavy work
//! (canvas list, preview tiles, pipeline, ffmpeg) runs on background worker
//! threads with tokio runtimes; the Slint UI runs on the main thread and
//! is updated via event loop callbacks.

// The generated Slint types (AppWindow, LogItem, ...) live at the crate root.
use crate::*;

use std::cell::Cell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::{Datelike, NaiveDate};
use slint::{ComponentHandle, ModelRc, SharedPixelBuffer, SharedString, VecModel};

use tlm_core::compose::{self, TileCoord};
use tlm_core::coord::{self, Region};
use tlm_core::history::{self, CanvasInfo};
use tlm_core::i18n::{self, t, tf, LogLevel};
use tlm_core::link::LinkMode;
use tlm_core::pipeline::{self, Event, PipelineParams};
use tlm_core::settings::{
    self, CameraMotion, HardwareAcceleration, Language, OutputFormat, Settings, SocialPreset,
    VideoCodec, WatermarkPosition, PRESETS,
};
use tlm_core::template::Template;
use tlm_core::{net, video};
use tokio_util::sync::CancellationToken;

// ----------------------------------------------------------------------------
// Image conversion helper
// ----------------------------------------------------------------------------

pub fn image_to_slint_image(img: &image::RgbaImage) -> slint::Image {
    let width = img.width();
    let height = img.height();
    let buffer =
        SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(img.as_raw(), width, height);
    slint::Image::from_rgba8(buffer)
}

/// Longest edge of a preview frame handed to Slint. Full-resolution frames of
/// large regions (70 MP+) are reduced first: uploading a texture that big
/// stalls the renderer for seconds, and the panel never shows more than this.
const PREVIEW_MAX_DIM: u32 = 2048;

fn preview_upload_image(img: &image::RgbaImage) -> image::RgbaImage {
    let (w, h) = (img.width(), img.height());
    if w <= PREVIEW_MAX_DIM && h <= PREVIEW_MAX_DIM {
        return img.clone();
    }
    let scale = PREVIEW_MAX_DIM as f32 / w.max(h) as f32;
    let nw = ((w as f32 * scale).round() as u32).max(1);
    let nh = ((h as f32 * scale).round() as u32).max(1);
    image::imageops::thumbnail(img, nw, nh)
}

pub fn hex_to_slint_color(hex: &str) -> Option<slint::Color> {
    let s = hex.trim().strip_prefix('#').unwrap_or(hex.trim());
    if s.len() == 6 {
        let r = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let b = u8::from_str_radix(&s[4..6], 16).ok()?;
        Some(slint::Color::from_rgb_u8(r, g, b))
    } else {
        None
    }
}

pub fn preset_index_for(domain: &str, storage_domain: &str) -> i32 {
    let dom = domain.trim();
    let store = storage_domain.trim();
    for (idx, (_, d, s)) in PRESETS.iter().enumerate() {
        if dom.eq_ignore_ascii_case(d) && store.eq_ignore_ascii_case(s) {
            return idx as i32;
        }
    }
    // "Custom" index is after the presets
    PRESETS.len() as i32
}

pub fn is_coord_valid(s: &str) -> bool {
    s.parse::<tlm_core::coord::Coord>().is_ok()
}

/// Parses the exact `YYYY-MM-DD` text used by the date fields.
pub fn parse_date_text(s: &str) -> Option<NaiveDate> {
    let trimmed = s.trim();
    if trimmed.len() != 10 {
        return None;
    }
    NaiveDate::parse_from_str(trimmed, "%Y-%m-%d").ok()
}

fn duration_text(secs: u64) -> String {
    if secs >= 60 {
        let minutes = (secs / 60).to_string();
        let seconds = (secs % 60).to_string();
        tf("duration_min_sec", &[&minutes, &seconds])
    } else {
        let seconds = secs.to_string();
        tf("duration_sec", &[&seconds])
    }
}

pub fn is_input_ready(
    canvas_count: usize,
    coord1: &str,
    coord2: &str,
    start: NaiveDate,
    end: NaiveDate,
    out_folder: &str,
) -> bool {
    canvas_count > 0
        && is_coord_valid(coord1)
        && is_coord_valid(coord2)
        && start <= end
        && !out_folder.trim().is_empty()
}

// ----------------------------------------------------------------------------
// Worker plumbing
// ----------------------------------------------------------------------------

enum NetCmd {
    FetchCanvasList(Settings),
}

/// One preview fetch. Lives on its own channel (and its own worker) so a keystroke
/// burst cannot queue a stale preview behind the canvas list, and so in-flight
/// tile downloads can be cancelled as soon as a newer preview is requested.
struct PreviewCmd {
    id: u64,
    settings: Settings,
    canvas_id: String,
    canvas_size_px: i64,
    bg_color: u32,
    region: Region,
    date: NaiveDate,
}

/// Off-thread `cache::stats()` / `cache::prune()` / `cache::clear()`: directory
/// scans must not run on the Slint UI thread, where they stalled every progress
/// update, nor on the canvas-list worker, where they delayed the first preview.
enum CacheCmd {
    Stats,
    Prune(i32),
    Clear,
}

enum JobCmd {
    RunTimelapse(PipelineParams, CancellationToken),
    CompileOnly(Settings, PathBuf, CancellationToken),
}

enum WorkerEvent {
    CanvasList(Result<Vec<CanvasInfo>, String>),
    /// A preview update. Large previews are fetched, decoded and composed in
    /// tile chunks; `done` is false for the intermediate frames so the UI can
    /// paint the area progressively instead of freezing on the old image until
    /// every tile has arrived.
    PreviewFrame {
        id: u64,
        image: image::RgbaImage,
        done: bool,
    },
    Pipeline(Event),
    PipelineDone(Result<pipeline::PipelineResult, String>),
    Encode(video::EncodeEvent),
    EncodeDone(Result<video::CompileResult, String>),
    CacheStats {
        bytes: u64,
        count: usize,
    },
    /// The cache was wiped; the UI drops the current preview and refetches it.
    CacheCleared {
        bytes: u64,
        count: usize,
    },
}

fn spawn_net_worker(rx: Receiver<NetCmd>, events: Sender<WorkerEvent>) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        // Owned by this worker's runtime. Never hand these clients to the job
        // worker: pooled connections are only driven by the runtime that
        // created them.
        let clients = net::ClientCache::new();
        for cmd in rx {
            match cmd {
                NetCmd::FetchCanvasList(settings) => {
                    let result = rt.block_on(async {
                        let client = clients.get(&settings);
                        let cancel = CancellationToken::new();
                        history::fetch_canvas_list(&client, &settings, &cancel)
                            .await
                            .map_err(|e| e.to_string())
                    });
                    let _ = events.send(WorkerEvent::CanvasList(result));
                }
            }
        }
    });
}

fn spawn_cache_worker(rx: Receiver<CacheCmd>, events: Sender<WorkerEvent>) {
    std::thread::spawn(move || {
        for cmd in rx {
            let cleared = match cmd {
                CacheCmd::Stats => false,
                CacheCmd::Prune(max_age_days) => {
                    tlm_core::cache::prune(max_age_days, tlm_core::cache::DEFAULT_CACHE_MAX_BYTES);
                    false
                }
                CacheCmd::Clear => {
                    tlm_core::cache::clear();
                    true
                }
            };
            let (bytes, count) = tlm_core::cache::stats();
            let ev = if cleared {
                WorkerEvent::CacheCleared { bytes, count }
            } else {
                WorkerEvent::CacheStats { bytes, count }
            };
            let _ = events.send(ev);
        }
    });
}

/// Debounce window: typing in a coordinate field fires one preview per
/// keystroke; without a short settle time every intermediate region would hit
/// the network and the newest preview would arrive last but several seconds late.
const PREVIEW_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(120);

/// Tiles per preview chunk. A 512-tile preview used to allocate, decode and
/// upload its full bitmap only after the last tile arrived; chunking keeps the
/// network/decode bursts bounded and lets the first rows appear while the rest
/// is still downloading. Small previews stay a single round trip.
const PREVIEW_CHUNK_TILES: usize = 64;

/// At most this many progressive uploads per preview. Each partial upload
/// copies the whole bitmap into the UI, so a 4096x4096 preview must not send
/// dozens of full frames; four updates still make the first rows appear early.
const PREVIEW_MAX_CHUNKS: usize = 4;

/// Dedicated preview worker.
///
/// Three properties matter here, and none of them held when previews shared the
/// canvas-list worker:
/// * a burst of requests keeps only the newest (debounce + coalesce),
/// * a newer request cancels the in-flight one instead of queueing behind it
///   (dropping the future cancels the HTTP requests and frees the connections),
/// * the client cache belongs to this worker's runtime, so pooled sockets stay
///   driven and warm across previews.
fn spawn_preview_worker(
    rx: tokio::sync::mpsc::UnboundedReceiver<PreviewCmd>,
    events: Sender<WorkerEvent>,
) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let clients = net::ClientCache::new();
        rt.block_on(async move {
            let mut rx = rx;
            let mut pending = match rx.recv().await {
                Some(cmd) => Some(cmd),
                None => return,
            };
            while let Some(mut cmd) = pending.take() {
                // Coalesce: restart the window for every new request, so only the
                // last value of a typing/scrubbing burst is fetched.
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(PREVIEW_DEBOUNCE) => break,
                        got = rx.recv() => match got {
                            Some(new_cmd) => cmd = new_cmd,
                            None => return,
                        },
                    }
                }

                tokio::select! {
                    image = run_preview(&clients, &cmd, &events) => {
                        let _ = events.send(WorkerEvent::PreviewFrame {
                            id: cmd.id,
                            image: preview_upload_image(&image),
                            done: true,
                        });
                        pending = rx.recv().await;
                    }
                    got = rx.recv() => {
                        // Newer request: drop the in-flight fetch (and its
                        // sockets) and start over with the freshest one.
                        pending = got;
                    }
                }
            }
        });
    });
}

async fn run_preview(
    clients: &net::ClientCache,
    cmd: &PreviewCmd,
    events: &Sender<WorkerEvent>,
) -> image::RgbaImage {
    let settings = &cmd.settings;
    let client = clients.get(settings);
    let cancel = CancellationToken::new();
    let coords: Vec<TileCoord> = compose::build_tile_coords(&cmd.region, cmd.canvas_size_px);
    let sem = Arc::new(tlm_core::adaptive::AdaptiveLimiter::fixed(
        settings.max_concurrent_tiles.max(1) as usize,
    ));
    let date_str = cmd.date.format("%Y%m%d").to_string();
    let bg = compose::rgba_from_rgb(cmd.bg_color);

    // Start from the same background `compose_base` would use, then paint the
    // chunks into it as they arrive. Keeping one canvas (instead of composing
    // each chunk from scratch) preserves the source-over order and avoids
    // repeatedly allocating the full bitmap.
    let mut canvas = image::RgbaImage::from_pixel(
        cmd.region.width.max(1) as u32,
        cmd.region.height.max(1) as u32,
        bg,
    );
    let chunk_size = coords
        .len()
        .div_ceil(PREVIEW_MAX_CHUNKS)
        .max(PREVIEW_CHUNK_TILES);
    let chunk_count = coords.len().div_ceil(chunk_size);
    for (index, chunk) in coords.chunks(chunk_size).enumerate() {
        let datas = pipeline::fetch_frame_tiles(
            &client,
            settings,
            &sem,
            &settings.storage_domain,
            &cmd.canvas_id,
            &date_str,
            "",
            chunk,
            &cancel,
        )
        .await;
        let decoded = compose::decode_tiles_parallel(&datas);
        for (coord, tile) in chunk.iter().zip(decoded.iter()) {
            if let Some(tile) = tile {
                compose::overlay_rgba(&mut canvas, tile, coord.ox as i64, coord.oy as i64);
            }
        }

        // Publish a partial frame after every chunk except the last; the final
        // one is sent by the worker as `done: true`. The copy is the UI upload
        // copy — the worker keeps mutating its own canvas — and oversized
        // regions are reduced so Slint never uploads a 70 MP texture.
        if index + 1 < chunk_count {
            let _ = events.send(WorkerEvent::PreviewFrame {
                id: cmd.id,
                image: preview_upload_image(&canvas),
                done: false,
            });
        }
    }
    canvas
}

fn spawn_job_worker(rx: Receiver<JobCmd>, events: Sender<WorkerEvent>) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        for cmd in rx {
            match cmd {
                JobCmd::RunTimelapse(params, cancel) => {
                    let result = rt.block_on(async {
                        let (ptx, mut prx) = tokio::sync::mpsc::unbounded_channel();
                        let forwarder = {
                            let events = events.clone();
                            tokio::spawn(async move {
                                while let Some(ev) = prx.recv().await {
                                    if events.send(WorkerEvent::Pipeline(ev)).is_err() {
                                        break;
                                    }
                                }
                            })
                        };
                        let out = pipeline::run_pipeline(params, ptx, cancel).await;
                        let _ = forwarder.await;
                        out
                    });
                    let _ =
                        events.send(WorkerEvent::PipelineDone(result.map_err(|e| e.to_string())));
                }
                JobCmd::CompileOnly(settings, out_dir, cancel) => {
                    let result = rt.block_on(async {
                        let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel();
                        let forward = {
                            let events = events.clone();
                            tokio::spawn(async move {
                                while let Some(ev) = erx.recv().await {
                                    if events.send(WorkerEvent::Encode(ev)).is_err() {
                                        break;
                                    }
                                }
                            })
                        };
                        let out = video::compile(&settings, &out_dir, etx, &cancel).await;
                        let _ = forward.await;
                        out
                    });
                    let _ = events.send(WorkerEvent::EncodeDone(result.map_err(|e| e.to_string())));
                }
            }
        }
    });
}

// ----------------------------------------------------------------------------
// Application State
// ----------------------------------------------------------------------------

struct AppState {
    settings: Settings,
    settings_path: PathBuf,
    draft: Settings,

    canvases: Vec<CanvasInfo>,
    current_canvas: usize,
    canvas_list_failed: Option<String>,
    /// Canvas id from an imported template that must be selected once the
    /// matching canvas list has been fetched (the list is asynchronous).
    pending_canvas_id: Option<String>,

    coord1: String,
    coord2: String,
    start_date: NaiveDate,
    end_date: NaiveDate,
    /// Raw text of the date fields so invalid input can be reported instead of
    /// silently keeping the last valid date.
    start_date_text: String,
    end_date_text: String,
    output_folder: String,

    stage: i32,
    running: bool,
    progress_value: i64,
    progress_max: i64,
    status_text: String,
    eta_text: String,
    progress_detail: String,
    download_stats: String,
    /// True while the history/timestamp phase is still running and the exact
    /// frame total is not known yet. The progress card shows "Counting..." and
    /// an ellipsis instead of a misleading 0%.
    counting_frames: bool,
    process_start: Option<Instant>,
    last_video_path: Option<PathBuf>,
    completion_summary: String,

    log: VecDeque<(LogLevel, String)>,
    log_filter_errors: bool,
    /// Bumped whenever the filtered log view changes; `sync_ui_state` rebuilds
    /// the Slint model only when this differs from the applied revision.
    log_revision: u64,
    log_revision_applied: Cell<u64>,
    cache_stats_text: String,

    preview_id: u64,
    preview_day_offset: i64,
    preview_limit_exceeded: bool,
    last_preview_key: String,
    /// Pixels the UI should show: the last delivered day preview, or the live
    /// frame streamed by a running pipeline. Uploaded to Slint only when
    /// [`Self::preview_revision`] changes, instead of on every worker event.
    preview_image: Option<image::RgbaImage>,
    /// The last static day preview, kept so the panel can fall back to it when a
    /// live run ends (the streamed frame belongs to the run, not to the panel).
    preview_static: Option<image::RgbaImage>,
    preview_revision: u64,
    preview_revision_applied: Cell<u64>,
    /// A preview request is in flight (or debounced); drives the header pill.
    preview_loading: bool,
    live_preview_index: i64,
    live_preview_total: i64,

    net_tx: Sender<NetCmd>,
    preview_tx: tokio::sync::mpsc::UnboundedSender<PreviewCmd>,
    cache_tx: Sender<CacheCmd>,
    job_tx: Sender<JobCmd>,
    cancel: Arc<CancellationToken>,
}

impl AppState {
    fn new(
        net_tx: Sender<NetCmd>,
        preview_tx: tokio::sync::mpsc::UnboundedSender<PreviewCmd>,
        cache_tx: Sender<CacheCmd>,
        job_tx: Sender<JobCmd>,
    ) -> Self {
        let settings_path = settings::default_settings_path();
        // `studio_expanded` / `overlay_font_scale` / `audio_fade_ms` are still
        // part of the shared settings file (the CLI and studio pipeline honour
        // them), but the GUI no longer exposes a collapsible Studio panel.
        let settings = Settings::load_or_import(&settings_path);
        i18n::set_language(settings.language);

        let today = chrono::Local::now().date_naive();
        let start_date = today - chrono::Duration::days(7);
        let end_date = today;
        let output_folder = settings::default_output_folder()
            .to_string_lossy()
            .into_owned();

        let _ = net_tx.send(NetCmd::FetchCanvasList(settings.clone()));
        // Prune once at startup (off the UI thread) and fetch the size for the
        // settings panel.
        let _ = cache_tx.send(CacheCmd::Prune(settings.cache_ttl_days));

        AppState {
            settings_path,
            draft: settings.clone(),
            settings,
            canvases: Vec::new(),
            current_canvas: 0,
            canvas_list_failed: None,
            pending_canvas_id: None,
            coord1: "-128_-128".into(),
            coord2: "128_128".into(),
            start_date,
            end_date,
            start_date_text: start_date.format("%Y-%m-%d").to_string(),
            end_date_text: end_date.format("%Y-%m-%d").to_string(),
            output_folder,
            stage: -1,
            running: false,
            progress_value: 0,
            progress_max: 100,
            status_text: String::new(),
            eta_text: String::new(),
            progress_detail: String::new(),
            download_stats: String::new(),
            counting_frames: false,
            process_start: None,
            last_video_path: None,
            completion_summary: String::new(),
            log: VecDeque::new(),
            log_filter_errors: false,
            log_revision: 0,
            log_revision_applied: Cell::new(0),
            cache_stats_text: String::new(),
            preview_id: 0,
            preview_day_offset: 0,
            preview_limit_exceeded: false,
            last_preview_key: String::new(),
            preview_image: None,
            preview_static: None,
            preview_revision: 0,
            preview_revision_applied: Cell::new(0),
            preview_loading: false,
            live_preview_index: 0,
            live_preview_total: 0,
            net_tx,
            preview_tx,
            cache_tx,
            job_tx,
            cancel: Arc::new(CancellationToken::new()),
        }
    }

    fn selected_canvas(&self) -> Option<&CanvasInfo> {
        self.canvases.get(self.current_canvas)
    }

    fn region(&self) -> Option<Region> {
        coord::region_from_corners(&self.coord1, &self.coord2)
            .filter(|r| r.width > 0 && r.height > 0)
    }

    fn is_input_ready(&self) -> bool {
        let Some((start, end)) = self.parsed_dates() else {
            return false;
        };
        is_input_ready(
            self.canvases.len(),
            &self.coord1,
            &self.coord2,
            start,
            end,
            &self.output_folder,
        )
    }

    fn parsed_dates(&self) -> Option<(NaiveDate, NaiveDate)> {
        Some((
            parse_date_text(&self.start_date_text)?,
            parse_date_text(&self.end_date_text)?,
        ))
    }

    /// Inline field errors: (corner 1, corner 2, start date, end date, folder, canvas).
    fn validation_messages(&self) -> (String, String, String, String, String, String) {
        let coord1 = if !is_coord_valid(&self.coord1) {
            t("invalid_coord")
        } else {
            String::new()
        };
        let coord2 = if !is_coord_valid(&self.coord2) {
            t("invalid_coord")
        } else {
            String::new()
        };
        let start = parse_date_text(&self.start_date_text);
        let end = parse_date_text(&self.end_date_text);
        let start_err = match (start, end) {
            (None, _) => t("invalid_date"),
            (Some(s), Some(e)) if s > e => t("start_after_end"),
            _ => String::new(),
        };
        let end_err = if end.is_none() {
            t("invalid_date")
        } else {
            String::new()
        };
        let folder = if self.output_folder.trim().is_empty() {
            t("folder_required")
        } else {
            String::new()
        };
        let canvas = if self.canvases.is_empty() {
            t("no_canvas")
        } else {
            String::new()
        };
        (coord1, coord2, start_err, end_err, folder, canvas)
    }

    /// Human readable dimensions + aspect ratio of the clamped selection.
    fn region_dims_text(&self) -> String {
        let Some(region) = self.region() else {
            return String::new();
        };
        let region = match self.selected_canvas() {
            Some(canvas) => coord::clamp_to_canvas(region, canvas.size_px),
            None => region,
        };
        if region.width <= 0 || region.height <= 0 {
            return String::new();
        }
        let aspect = region.width as f64 / region.height as f64;
        tf(
            "region_dims",
            &[
                &region.width.to_string(),
                &region.height.to_string(),
                &format!("{aspect:.2}"),
            ],
        )
    }

    /// Expected peak RAM and disk footprint of a run with the current
    /// settings, region and date range.
    fn capacity_estimate(&self) -> crate::capacity::Estimate {
        let pixels = self
            .region()
            .map(|r| r.width.max(0) as u64 * r.height.max(0) as u64)
            .unwrap_or(0);
        let days = (self.end_date - self.start_date).num_days().max(0) as u64 + 1;
        let frames = days * crate::capacity::FRAMES_PER_DAY_ESTIMATE;
        crate::capacity::estimate(
            pixels,
            frames,
            self.settings.stream_video,
            self.settings.save_png_sequence,
        )
    }

    /// Empty string when the run fits, otherwise the pre-render capacity
    /// warning (missing RAM and/or disk) shown above the Create button.
    fn capacity_warning(&self) -> String {
        let est = self.capacity_estimate();
        if est.frame_bytes == 0 {
            return String::new();
        }
        let mut lines: Vec<String> = Vec::new();
        if let Some(ram) = crate::capacity::available_ram_bytes() {
            if est.ram_bytes > ram {
                lines.push(tf(
                    "capacity_low_ram",
                    &[
                        &crate::capacity::format_bytes(est.ram_bytes),
                        &crate::capacity::format_bytes(ram),
                    ],
                ));
            }
        }
        if est.disk_bytes > 0 {
            let dir = PathBuf::from(&self.output_folder);
            if let Some(free) = crate::capacity::free_disk_bytes(&dir) {
                if est.disk_bytes > free {
                    lines.push(tf(
                        "capacity_low_disk",
                        &[
                            &crate::capacity::format_bytes(est.disk_bytes),
                            &crate::capacity::format_bytes(free),
                            &dir.display().to_string(),
                        ],
                    ));
                }
            }
        }
        lines.join("\n")
    }

    fn log_line(&mut self, level: LogLevel, msg: String) {
        self.log.push_back((level, msg));
        while self.log.len() > 400 {
            self.log.pop_front();
        }
        self.log_revision = self.log_revision.wrapping_add(1);
    }

    fn preview_day_text(&self) -> String {
        let d = self.start_date + chrono::Duration::days(self.preview_day_offset);
        let total = (self.end_date - self.start_date).num_days() + 1;
        tf(
            "preview_day",
            &[
                &d.format("%Y-%m-%d").to_string(),
                &(self.preview_day_offset + 1).to_string(),
                &total.max(1).to_string(),
            ],
        )
    }

    fn tile_info_text(&self) -> String {
        match (self.region(), self.selected_canvas()) {
            (Some(r), Some(c)) => {
                // Show the tile count of what will actually be rendered, not of
                // the raw (possibly off-canvas) selection.
                let r = coord::clamp_to_canvas(r, c.size_px);
                if coord::check_frame_size(&r).is_err() {
                    return String::new();
                }
                let tiles = compose::tile_count(&r, c.size_px);
                let days = ((self.end_date - self.start_date).num_days() + 1).max(1);
                tf("stats_tile_info", &[&days.to_string(), &tiles.to_string()])
            }
            _ => String::new(),
        }
    }

    fn request_preview(&mut self) {
        // Previews are a convenience while configuring a run; don't compete
        // with an in-flight download for the shared connection pool. The panel
        // keeps showing the live frame instead.
        if self.running {
            // Whatever static frame we kept no longer matches the (possibly
            // edited) settings, so it must not be restored when the run ends.
            self.preview_static = None;
            return;
        }
        let Some(canvas) = self.selected_canvas().cloned() else {
            self.clear_preview();
            return;
        };
        let Some(region) = self.region() else {
            self.clear_preview();
            return;
        };
        // Crop to the canvas exactly like the pipeline does, so the preview
        // shows the same pixels the rendered frames will contain.
        let region = coord::clamp_to_canvas(region, canvas.size_px);
        match coord::check_frame_size(&region) {
            Ok(()) => {}
            Err(coord::RegionError::Empty) => {
                self.preview_limit_exceeded = false;
                self.clear_preview();
                return;
            }
            Err(coord::RegionError::TooLarge { .. }) => {
                self.preview_limit_exceeded = true;
                self.clear_preview();
                return;
            }
        }
        let tiles = compose::tile_count(&region, canvas.size_px);
        let over_limit = tiles > self.settings.preview_limit as i64;

        if tiles == 0 || over_limit {
            self.preview_limit_exceeded = over_limit;
            self.clear_preview();
            return;
        }
        self.preview_limit_exceeded = false;
        self.preview_id += 1;

        let total_days = (self.end_date - self.start_date).num_days() + 1;
        self.preview_day_offset = self
            .preview_day_offset
            .clamp(0, total_days.saturating_sub(1).max(0));
        let date = self.start_date + chrono::Duration::days(self.preview_day_offset);

        let bg_color = canvas.bg_color.unwrap_or(0xffffff);
        let key = format!(
            "{}|{}|{}|{}|{}|{}|{}|{}|{:06x}",
            self.settings.domain,
            self.settings.storage_domain,
            canvas.id,
            region.x,
            region.y,
            region.width,
            region.height,
            date.format("%Y-%m-%d"),
            bg_color
        );
        if key == self.last_preview_key {
            return;
        }
        self.last_preview_key = key;
        self.preview_loading = true;
        let _ = self.preview_tx.send(PreviewCmd {
            id: self.preview_id,
            settings: self.settings.clone(),
            canvas_id: canvas.id.clone(),
            canvas_size_px: canvas.size_px,
            bg_color,
            region,
            date,
        });
    }

    /// Drops the visible preview because the current selection cannot produce
    /// one. Without this the panel kept showing an image from a different area
    /// (or from before a settings change), which read as "the preview never
    /// updates".
    fn clear_preview(&mut self) {
        self.preview_static = None;
        self.preview_loading = false;
        if self.preview_image.take().is_some() {
            self.preview_revision = self.preview_revision.wrapping_add(1);
        }
    }

    /// Ends a run and puts the panel back into editing mode: the last static
    /// day preview is restored immediately and fetched again, because the run
    /// just populated the tile cache and the streamed frame belonged to the run.
    fn refresh_preview_after_run(&mut self) {
        self.running = false;
        self.live_preview_index = 0;
        self.live_preview_total = 0;
        self.preview_image = self.preview_static.clone();
        self.preview_revision = self.preview_revision.wrapping_add(1);
        self.last_preview_key.clear();
        self.request_preview();
    }

    fn start_timelapse(&mut self) {
        if self.running || !self.is_input_ready() {
            return;
        }
        let Some(canvas) = self.selected_canvas().cloned() else {
            return;
        };
        let Some(region) = self.region() else { return };

        self.running = true;
        self.stage = 2;
        self.progress_value = 0;
        self.progress_max = 100;
        self.log.clear();
        self.log_revision = self.log_revision.wrapping_add(1);
        self.progress_detail.clear();
        self.download_stats.clear();
        self.counting_frames = true;
        self.progress_detail = t("counting_frames");
        self.last_video_path = None;
        self.completion_summary.clear();
        self.eta_text.clear();
        self.process_start = Some(Instant::now());

        // Warn in the console too: the banner above the button is easy to
        // miss once the run is underway.
        let capacity = self.capacity_warning();
        if !capacity.is_empty() {
            self.log_line(LogLevel::Warning, capacity.replace('\n', "  "));
        }

        let params = PipelineParams {
            settings: self.settings.clone(),
            canvas_id: canvas.id.clone(),
            region,
            start: self.start_date,
            end: self.end_date,
            out_dir: PathBuf::from(&self.output_folder),
            resume: false,
            canvas_size_px: Some(canvas.size_px),
            bg_color: Some(canvas.bg_color.unwrap_or(0xffffff)),
            day_timestamps: None,
        };
        self.cancel = Arc::new(CancellationToken::new());
        let _ = self
            .job_tx
            .send(JobCmd::RunTimelapse(params, (*self.cancel).clone()));
    }

    fn start_compile_only(&mut self) {
        if self.running {
            return;
        }
        let out_dir = PathBuf::from(&self.output_folder);
        if !out_dir.exists() {
            self.log_line(LogLevel::Error, t("no_frames_found"));
            return;
        }
        self.running = true;
        // Stage 4 corresponds to Encoding/Compiling video
        self.stage = 4;
        self.progress_value = 0;
        self.progress_max = 100;
        self.eta_text.clear();
        self.process_start = Some(Instant::now());
        self.cancel = Arc::new(CancellationToken::new());
        let _ = self.job_tx.send(JobCmd::CompileOnly(
            self.settings.clone(),
            out_dir,
            (*self.cancel).clone(),
        ));
    }

    fn cancel_process(&mut self) {
        self.cancel.cancel();
        self.log_line(LogLevel::Warning, t("process_cancelled"));
        self.status_text = t("process_cancelled");
        self.stage = -1;
        self.reset_progress();
    }

    /// Hides the progress card again. The UI keeps it visible while
    /// `is-running || progress-fraction > 0`, so a cancelled or finished run
    /// must zero the values instead of leaving the last frame on screen.
    fn reset_progress(&mut self) {
        self.progress_value = 0;
        self.progress_max = 100;
        self.eta_text.clear();
        self.counting_frames = false;
        self.process_start = None;
    }

    fn handle_pipeline_event(&mut self, ev: Event) {
        match ev {
            Event::Log(level, msg) => self.log_line(level, msg),
            Event::Status(msg) => self.status_text = msg,
            Event::Progress { value, max } => {
                self.counting_frames = false;
                self.progress_value = value;
                if max > 0 {
                    self.progress_max = max;
                    self.progress_detail =
                        tf("downloaded_frames", &[&value.to_string(), &max.to_string()]);
                }
                if let Some(start) = self.process_start {
                    let elapsed = start.elapsed().as_secs_f64();
                    if value > 0 && max > 0 && value < max {
                        let remaining = elapsed * (max - value) as f64 / value as f64;
                        self.eta_text = tf(
                            "remaining_time",
                            &[
                                &((remaining as i64) / 60).to_string(),
                                &((remaining as i64) % 60).to_string(),
                            ],
                        );
                    } else if value >= max {
                        self.eta_text.clear();
                    }
                }
            }
            Event::PreviewFrame {
                image,
                index,
                total,
            } => {
                // Live frames are full render resolution; shrink them before
                // they reach the UI so a 70 MP region does not stall the GPU.
                self.preview_image = Some(preview_upload_image(&image));
                self.preview_revision = self.preview_revision.wrapping_add(1);
                self.live_preview_index = index;
                self.live_preview_total = total;
            }
            Event::Stats {
                bytes,
                cached,
                network,
            } => {
                self.counting_frames = false;
                let secs = self
                    .process_start
                    .map(|s| s.elapsed().as_secs_f64())
                    .unwrap_or(1.0);
                let mbps = if secs > 0.5 {
                    (bytes as f64 / 1024.0 / 1024.0) / secs
                } else {
                    0.0
                };
                self.download_stats = tf(
                    "download_stats",
                    &[
                        &cached.to_string(),
                        &network.to_string(),
                        &format!("{mbps:.2}"),
                    ],
                );
                self.progress_detail = tf(
                    "downloaded_frames",
                    &[
                        &self.progress_value.to_string(),
                        &self.progress_max.to_string(),
                    ],
                );
            }
        }
    }

    fn finish_pipeline(&mut self, result: Result<pipeline::PipelineResult, String>) -> bool {
        self.running = false;
        let _ = self.cache_tx.send(CacheCmd::Stats);
        match result {
            Ok(r) if r.cancelled => {
                self.stage = -1;
                self.reset_progress();
                self.refresh_preview_after_run();
                false
            }
            Ok(r) => {
                if r.linked_frames > 0 {
                    self.log_line(
                        LogLevel::Success,
                        tf("frames_linked_summary", &[&r.linked_frames.to_string()]),
                    );
                    if let Some(kind) = r.link_kind {
                        self.log_line(LogLevel::Info, tf("frames_link_kind", &[kind.as_str()]));
                    }
                }
                if let Some(video) = r.video_path {
                    // Zero-disk mode: the pipeline already produced the final
                    // video, so there is no separate compile step.
                    self.stage = 5;
                    self.last_video_path = Some(video);
                    self.status_text = t("ready");
                    let elapsed = self
                        .process_start
                        .map(|s| s.elapsed().as_secs())
                        .unwrap_or(0);
                    let dims = self
                        .region()
                        .map(|r| {
                            let (w, h) = tlm_core::studio::social::target_dimensions(
                                self.settings.social_preset,
                                r.width.max(1) as u32,
                                r.height.max(1) as u32,
                            );
                            format!("{w}x{h}")
                        })
                        .unwrap_or_default();
                    self.completion_summary = tf(
                        "completion_summary_zero_disk",
                        &[&duration_text(elapsed), &r.frames.to_string(), &dims],
                    );
                    self.reset_progress();
                    self.refresh_preview_after_run();
                    notify_completion(&self.settings, &self.completion_summary);
                    return true;
                }
                self.running = true;
                self.stage = 4;
                self.progress_value = 0;
                self.progress_max = 100;
                self.process_start = Some(Instant::now());
                let out_dir = PathBuf::from(&self.output_folder);
                self.cancel = Arc::new(CancellationToken::new());
                let _ = self.job_tx.send(JobCmd::CompileOnly(
                    self.settings.clone(),
                    out_dir,
                    (*self.cancel).clone(),
                ));
                false
            }
            Err(e) => {
                self.log_line(LogLevel::Error, e);
                self.status_text = t("error_occurred");
                self.stage = -1;
                self.reset_progress();
                self.refresh_preview_after_run();
                false
            }
        }
    }

    fn handle_encode_event(&mut self, ev: video::EncodeEvent) {
        match ev {
            video::EncodeEvent::Log(level, msg) => self.log_line(level, msg),
            video::EncodeEvent::Status(msg) => self.status_text = msg,
            video::EncodeEvent::Progress { frame, total } => {
                self.progress_value = frame;
                self.progress_max = total.max(1);
                if let Some(start) = self.process_start {
                    let elapsed = start.elapsed().as_secs_f64();
                    if frame > 0 && frame < total {
                        let fps = frame as f64 / elapsed.max(0.5);
                        self.progress_detail = tf(
                            "encoding_speed",
                            &[&frame.to_string(), &total.to_string(), &format!("{fps:.1}")],
                        );
                        let remaining = elapsed * (total - frame) as f64 / frame as f64;
                        self.eta_text = tf(
                            "remaining_time",
                            &[
                                &((remaining as i64) / 60).to_string(),
                                &((remaining as i64) % 60).to_string(),
                            ],
                        );
                    }
                }
            }
        }
    }

    fn finish_encode(&mut self, result: Result<video::CompileResult, String>) -> bool {
        self.running = false;
        let _ = self.cache_tx.send(CacheCmd::Stats);
        match result {
            Ok(r) if r.cancelled => {
                self.stage = -1;
                self.reset_progress();
                self.refresh_preview_after_run();
                false
            }
            Ok(r) => {
                self.stage = 5;
                self.last_video_path = Some(r.output_path);
                let elapsed = self
                    .process_start
                    .map(|s| s.elapsed().as_secs())
                    .unwrap_or(0);
                let dims = self
                    .region()
                    .map(|r| format!("{}x{}", r.width, r.height))
                    .unwrap_or_default();
                let duration = duration_text(elapsed);
                self.completion_summary = tf(
                    "completion_summary",
                    &[&duration, &self.progress_max.to_string(), &dims],
                );
                self.reset_progress();
                self.refresh_preview_after_run();
                notify_completion(&self.settings, &self.completion_summary);
                if self.settings.cache_ttl_days > 0 {
                    let _ = self
                        .cache_tx
                        .send(CacheCmd::Prune(self.settings.cache_ttl_days));
                }
                true
            }
            Err(e) => {
                self.log_line(LogLevel::Error, e.clone());
                self.status_text = tf(
                    "compilation_error",
                    &[&e.chars().take(120).collect::<String>()],
                );
                self.stage = -1;
                self.reset_progress();
                self.refresh_preview_after_run();
                false
            }
        }
    }
}

// ----------------------------------------------------------------------------
// Sync state to UI helper
// ----------------------------------------------------------------------------

/// Copies the studio controls (always visible, not just inside the settings
/// dialog) into a `Settings` value.
fn apply_studio_draft(ui: &AppWindow, d: &mut Settings) {
    d.stream_video = ui.get_draft_stream_video();
    d.save_png_sequence = ui.get_draft_save_png_sequence();
    d.show_timestamp = ui.get_draft_show_timestamp();
    d.timestamp_format = ui.get_draft_timestamp_format().to_string();
    d.show_progress_bar = ui.get_draft_show_progress_bar();
    d.watermark_text = ui.get_draft_watermark_text().to_string();
    d.watermark_position = match ui.get_draft_watermark_position() {
        0 => WatermarkPosition::TopLeft,
        2 => WatermarkPosition::BottomLeft,
        3 => WatermarkPosition::BottomRight,
        _ => WatermarkPosition::TopRight,
    };
    d.camera_motion = match ui.get_draft_camera_motion() {
        1 => CameraMotion::ZoomOut,
        2 => CameraMotion::ZoomIn,
        3 => CameraMotion::PanLeftToRight,
        4 => CameraMotion::PanRightToLeft,
        _ => CameraMotion::None,
    };
    d.camera_strength = ui.get_draft_camera_strength();
    d.social_preset = match ui.get_draft_social_preset() {
        1 => SocialPreset::Vertical916,
        2 => SocialPreset::Square11,
        3 => SocialPreset::Wide169,
        _ => SocialPreset::Source,
    };
    d.audio_path = ui.get_draft_audio_path().to_string();
}

fn sync_ui_state(ui: &AppWindow, state: &AppState) {
    ui.set_onboarding_done(state.settings.onboarding_done);
    ui.set_console_expanded(state.settings.console_expanded);
    ui.set_current_canvas_index(state.current_canvas as i32);
    ui.set_coord1(state.coord1.clone().into());
    ui.set_coord2(state.coord2.clone().into());
    ui.set_start_date_str(state.start_date_text.clone().into());
    ui.set_end_date_str(state.end_date_text.clone().into());
    ui.set_output_folder(state.output_folder.clone().into());
    ui.set_is_running(state.running);
    let input_ready = state.is_input_ready();
    ui.set_input_ready(input_ready);
    ui.set_pipeline_stage(state.stage);

    // Inline validation and computed hints.
    let (coord1_err, coord2_err, start_err, end_err, folder_err, canvas_err) =
        state.validation_messages();
    ui.set_coord1_error(coord1_err.clone().into());
    ui.set_coord2_error(coord2_err.clone().into());
    ui.set_start_date_error(start_err.clone().into());
    ui.set_end_date_error(end_err.clone().into());
    ui.set_folder_error(folder_err.clone().into());
    ui.set_canvas_error_text(canvas_err.clone().into());
    let hint = if input_ready {
        String::new()
    } else {
        [
            coord1_err, coord2_err, start_err, end_err, folder_err, canvas_err,
        ]
        .into_iter()
        .find(|msg| !msg.is_empty())
        .unwrap_or_else(|| t("create_hint"))
    };
    ui.set_validate_hint(hint.into());
    ui.set_dims_text(state.region_dims_text().into());
    ui.set_capacity_warning(state.capacity_warning().into());

    let frac = if state.progress_max > 0 {
        (state.progress_value as f32 / state.progress_max as f32).clamp(0.0, 1.0)
    } else {
        0.0
    };
    ui.set_progress_fraction(frac);
    let p_text = if state.counting_frames {
        "\u{2026}".to_string()
    } else if state.progress_max > 0 && state.progress_max != 100 {
        format!("{} / {}", state.progress_value, state.progress_max)
    } else {
        String::new()
    };
    ui.set_progress_text(p_text.into());
    ui.set_progress_detail(state.progress_detail.clone().into());
    ui.set_download_stats(state.download_stats.clone().into());
    ui.set_status_text(state.status_text.clone().into());
    ui.set_eta_text(state.eta_text.clone().into());

    ui.set_preview_limit_exceeded(state.preview_limit_exceeded);
    ui.set_preview_loading(state.preview_loading && !state.running);
    ui.set_preview_day_text(state.preview_day_text().into());
    ui.set_preview_date_text(
        (state.start_date + chrono::Duration::days(state.preview_day_offset))
            .format("%Y-%m-%d")
            .to_string()
            .into(),
    );
    ui.set_preview_scrub_pos(state.preview_day_offset as f32);
    ui.set_tile_info_text(state.tile_info_text().into());
    let total_days = (state.end_date - state.start_date).num_days() + 1;
    ui.set_preview_day_count(total_days.max(1) as i32);
    ui.set_can_preview_back(state.preview_day_offset > 0);
    ui.set_can_preview_fwd(state.preview_day_offset < total_days - 1);
    ui.set_output_available(state.last_video_path.is_some());

    // Upload the preview only when the pixels actually changed. This used to
    // copy and push the whole frame on *every* worker event (hundreds per
    // second during a run) even when the image was identical.
    if state.preview_revision != state.preview_revision_applied.get() {
        match &state.preview_image {
            Some(img) => {
                ui.set_preview_image(image_to_slint_image(img));
                // The preview canvas is not necessarily square (arbitrary
                // region). Slint needs the intrinsic ratio to clamp panning
                // against the actually painted rectangle.
                ui.set_preview_aspect(img.width() as f32 / img.height().max(1) as f32);
                ui.set_has_preview(true);
            }
            None => {
                ui.set_preview_aspect(1.0);
                ui.set_has_preview(false);
            }
        }
        state.preview_revision_applied.set(state.preview_revision);
    }

    // Live rendering theater: the streamed frame is already in `preview_image`,
    // so only its label needs updating while a run is in progress.
    if state.running && state.live_preview_total > 0 {
        let pct = state.live_preview_index * 100 / state.live_preview_total;
        ui.set_preview_day_text(
            tf(
                "live_frame",
                &[&state.live_preview_index.to_string(), &pct.to_string()],
            )
            .into(),
        );
    }

    // Rebuild the log model only when its contents changed. This used to run on
    // *every* event — hundreds of times per second during a download — even
    // though progress messages never touch the log.
    if state.log_revision != state.log_revision_applied.get() {
        let log_vec: Vec<LogItem> = state
            .log
            .iter()
            .filter(|(lvl, _)| {
                !state.log_filter_errors || matches!(*lvl, LogLevel::Warning | LogLevel::Error)
            })
            .map(|(lvl, msg)| LogItem {
                level: *lvl as i32,
                message: msg.clone().into(),
            })
            .collect();
        ui.set_log_items(ModelRc::new(VecModel::from(log_vec)));
        state.log_revision_applied.set(state.log_revision);
    }
    // Must match the "Errors only" filter above, otherwise the badge promises
    // fewer lines than the filter actually shows.
    let error_count = state
        .log
        .iter()
        .filter(|(lvl, _)| matches!(*lvl, LogLevel::Warning | LogLevel::Error))
        .count();
    let error_text = if error_count > 0 {
        tf("log_error_count", &[&error_count.to_string()])
    } else {
        String::new()
    };
    ui.set_log_error_text(error_text.into());
    ui.set_log_errors_only(state.log_filter_errors);

    // Sync draft settings in UI. While the settings dialog is open the user
    // owns those fields; progress events must not reset sliders to the saved
    // values mid-edit.
    if !ui.get_show_settings() {
        ui.set_draft_domain(state.settings.domain.clone().into());
        ui.set_draft_storage_domain(state.settings.storage_domain.clone().into());
        ui.set_draft_lang_idx(match state.settings.language {
            Language::En => 0,
            Language::Hu => 1,
        });
        ui.set_draft_user_agent(state.settings.user_agent.clone().into());
        ui.set_draft_preview_limit(state.settings.preview_limit);
        ui.set_draft_cache_ttl_days(state.settings.cache_ttl_days);
        ui.set_draft_notify_on_completion(state.settings.notify_on_completion);
        ui.set_draft_max_connections(state.settings.max_connections);
        ui.set_draft_max_concurrent_tiles(state.settings.max_concurrent_tiles);
        ui.set_draft_max_concurrent_saves(state.settings.max_concurrent_saves);
        ui.set_draft_adaptive_concurrency(state.settings.adaptive_concurrency);
        ui.set_draft_timeout(state.settings.timeout.round() as i32);
        ui.set_draft_max_retries(state.settings.max_retries);
        ui.set_draft_quality(state.settings.quality);
        ui.set_draft_frame_skip(state.settings.frame_skip);
        ui.set_draft_link_mode(match state.settings.link_mode {
            LinkMode::Auto => 0,
            LinkMode::Hard => 1,
            LinkMode::Soft => 2,
            LinkMode::Copy => 3,
        });
        ui.set_draft_output_format(match state.settings.output_format {
            OutputFormat::Mp4 => 0,
            OutputFormat::Mkv => 1,
            OutputFormat::Webm => 2,
            OutputFormat::Mov => 3,
            OutputFormat::Avi => 4,
        });
        ui.set_draft_fps(state.settings.fps);
        ui.set_draft_video_codec(match state.settings.video_codec {
            VideoCodec::H264 => 0,
            VideoCodec::H265 => 1,
            VideoCodec::Av1 => 2,
        });
        ui.set_draft_hw_accel(match state.settings.hardware_acceleration {
            HardwareAcceleration::Auto => 0,
            HardwareAcceleration::None => 1,
            HardwareAcceleration::Nvenc => 2,
            HardwareAcceleration::Amf => 3,
            HardwareAcceleration::Qsv => 4,
            HardwareAcceleration::Vaapi => 5,
        });
        ui.set_draft_stream_video(state.settings.stream_video);
        ui.set_draft_save_png_sequence(state.settings.save_png_sequence);
        ui.set_draft_show_timestamp(state.settings.show_timestamp);
        ui.set_draft_show_progress_bar(state.settings.show_progress_bar);
        ui.set_draft_watermark_text(state.settings.watermark_text.clone().into());
        ui.set_draft_timestamp_format(state.settings.timestamp_format.clone().into());
        ui.set_draft_camera_motion(match state.settings.camera_motion {
            CameraMotion::None => 0,
            CameraMotion::ZoomOut => 1,
            CameraMotion::ZoomIn => 2,
            CameraMotion::PanLeftToRight => 3,
            CameraMotion::PanRightToLeft => 4,
        });
        ui.set_draft_camera_strength(state.settings.camera_strength);
        ui.set_draft_social_preset(match state.settings.social_preset {
            SocialPreset::Source => 0,
            SocialPreset::Vertical916 => 1,
            SocialPreset::Square11 => 2,
            SocialPreset::Wide169 => 3,
        });
        ui.set_draft_audio_path(state.settings.audio_path.clone().into());
        ui.set_font_size(state.settings.font_size);
        ui.set_draft_accent_color(state.settings.accent_color.clone().into());
        ui.set_draft_hover_color(state.settings.hover_color.clone().into());
        if let Some(c) = hex_to_slint_color(&state.settings.accent_color) {
            ui.set_col_accent(c);
            ui.set_accent_swatch(c);
        }
        if let Some(c) = hex_to_slint_color(&state.settings.hover_color) {
            ui.set_col_hover(c);
            ui.set_hover_swatch(c);
        }
        ui.set_draft_preset_idx(preset_index_for(
            &state.settings.domain,
            &state.settings.storage_domain,
        ));
    }

    ui.set_cache_stats_text(state.cache_stats_text.clone().into());
}

fn string_model(items: &[String]) -> ModelRc<SharedString> {
    let values: Vec<SharedString> = items
        .iter()
        .map(|s| SharedString::from(s.as_str()))
        .collect();
    ModelRc::new(VecModel::from(values))
}

fn sync_i18n(ui: &AppWindow) {
    ui.set_app_version(env!("CARGO_PKG_VERSION").into());
    ui.set_window_title(t("window_title").into());
    ui.set_str_settings(t("settings").into());
    ui.set_str_import_short(t("import_short").into());
    ui.set_str_export_short(t("export_short").into());
    ui.set_str_onboarding_title(t("onboarding_title").into());
    ui.set_str_onboarding_body(t("onboarding_body").into());
    ui.set_str_got_it(t("got_it").into());
    ui.set_str_select_canvas(t("select_canvas").into());
    ui.set_str_select_area(t("select_area").into());
    ui.set_str_first_cord(t("first_cord").into());
    ui.set_str_second_cord(t("second_cord").into());
    ui.set_str_date_range(t("date_range").into());
    ui.set_str_start_date(t("start_date").into());
    ui.set_str_end_date(t("end_date").into());
    ui.set_str_output_folder(t("output_folder").into());
    ui.set_str_browse(t("browse").into());
    ui.set_str_create_timelapse(t("create_timelapse").into());
    ui.set_str_render_only(t("render_only").into());
    ui.set_str_cancel(t("cancel").into());
    ui.set_str_console(t("console").into());
    ui.set_str_log_clear(t("log_clear").into());
    ui.set_str_log_copy(t("log_copy").into());
    ui.set_str_log_errors_only(t("log_errors_only").into());
    ui.set_str_area_preview(t("area_preview").into());
    ui.set_str_preview_limit_exceeded(t("preview_limit_exceeded").into());
    ui.set_str_online_canvas(t("online_canvas").into());
    ui.set_str_pipeline(t("pipeline").into());
    ui.set_str_progress(t("progress").into());
    ui.set_str_ok(t("ok").into());
    ui.set_str_close(t("close").into());
    ui.set_str_open_video(t("open_video").into());
    ui.set_str_open_folder(t("open_folder").into());
    ui.set_str_timelapse_created(t("timelapse_created_title").into());

    // Studio FX
    ui.set_str_studio_zero_disk(t("studio_zero_disk").into());
    ui.set_str_studio_keep_png(t("studio_keep_png").into());
    ui.set_str_studio_timestamp(t("studio_timestamp").into());
    ui.set_str_studio_progress_bar(t("studio_progress_bar").into());
    ui.set_str_studio_time_format(t("studio_time_format").into());
    ui.set_str_studio_watermark(t("studio_watermark").into());
    ui.set_str_studio_position(t("studio_position").into());
    ui.set_str_studio_camera(t("studio_camera").into());
    ui.set_str_studio_camera_strength(t("studio_camera_strength").into());
    ui.set_str_studio_frame(t("studio_frame").into());
    ui.set_str_studio_audio(t("studio_audio").into());
    ui.set_str_studio_audio_placeholder(t("studio_audio_placeholder").into());

    // Date presets and validation
    ui.set_str_date_last7(t("date_last7").into());
    ui.set_str_date_last30(t("date_last30").into());
    ui.set_str_date_this_month(t("date_this_month").into());
    ui.set_str_live(t("live_badge").into());
    ui.set_str_clear_field(t("clear_field").into());
    ui.set_str_preview_zoom_out(t("preview_zoom_out").into());
    ui.set_str_preview_zoom_in(t("preview_zoom_in").into());
    ui.set_str_preview_reset_view(t("preview_reset_view").into());
    ui.set_str_preview_prev_day(t("preview_prev_day").into());
    ui.set_str_preview_next_day(t("preview_next_day").into());
    ui.set_str_help(t("help").into());
    ui.set_str_about(t("about").into());
    ui.set_str_about_version(t("about_version").into());
    ui.set_str_about_app_license(t("about_app_license").into());
    ui.set_str_about_slint_license(t("about_slint_license").into());
    ui.set_str_about_third_party(t("about_third_party").into());
    ui.set_str_about_copyright(t("about_copyright").into());
    ui.set_str_about_links(t("about_links").into());
    ui.set_str_setup(t("setup").into());
    ui.set_str_setup_hint(t("setup_hint").into());
    ui.set_str_ready_to_render(t("ready_to_render").into());

    // Settings labels
    ui.set_str_tab_general(t("tab_general").into());
    ui.set_str_tab_server(t("tab_server").into());
    ui.set_str_tab_download(t("tab_download").into());
    ui.set_str_tab_video(t("tab_video").into());
    ui.set_str_tab_effects(t("tab_effects").into());
    ui.set_str_tab_appearance(t("tab_appearance").into());
    ui.set_str_language(t("language").into());
    ui.set_str_user_agent(t("user_agent").into());
    ui.set_str_preview_limit(t("preview_limit").into());
    ui.set_str_cache_retention(t("cache_ttl_days").into());
    ui.set_str_notify_on_completion(t("notify_on_completion").into());
    ui.set_str_preset(t("preset").into());
    ui.set_str_domain(t("domain").into());
    ui.set_str_storage_domain(t("storage_domain").into());
    ui.set_str_max_connections(t("max_connections").into());
    ui.set_str_max_concurrent_tiles(t("max_concurrent_tiles").into());
    ui.set_str_adaptive_ceiling(t("adaptive_ceiling").into());
    ui.set_str_max_concurrent_saves(t("max_concurrent_saves").into());
    ui.set_str_adaptive_concurrency(t("adaptive_concurrency").into());
    ui.set_str_timeout(t("timeout").into());
    ui.set_str_max_retries(t("max_retries").into());
    ui.set_str_quality(t("quality").into());
    ui.set_str_frame_skip(t("frame_skip").into());
    ui.set_str_link_mode(t("link_mode").into());
    ui.set_str_output_format(t("output_format").into());
    ui.set_str_fps(t("fps").into());
    ui.set_str_video_codec(t("video_codec").into());
    ui.set_str_hw_accel(t("hardware_acceleration").into());
    ui.set_str_font_size(t("font_size").into());
    ui.set_str_accent_color(t("accent_color").into());
    ui.set_str_hover_color(t("hover_color").into());
    ui.set_str_cache_size(t("cache_size").into());
    ui.set_str_clear_cache(t("cache_clear").into());
    ui.set_str_clear_cache_confirm(t("clear_cache_confirm").into());
    ui.set_str_preview_loading(t("preview_loading").into());
    ui.set_str_preview_empty_title(t("preview_empty_title").into());
    ui.set_str_preview_empty_body(t("preview_empty_body").into());
    ui.set_str_console_empty_title(t("console_empty_title").into());
    ui.set_str_console_empty_body(t("console_empty_body").into());
    ui.set_str_applies_next_run(t("applies_next_run").into());
    ui.set_str_general_hint(t("general_hint").into());
    ui.set_str_appearance_hint(t("appearance_hint").into());
    ui.set_str_effects_output(t("effects_output").into());
    ui.set_str_effects_overlays(t("effects_overlays").into());
    ui.set_str_effects_motion(t("effects_motion").into());
    ui.set_setting_desc(t("setting_desc_placeholder").into());

    // Setting descriptions
    ui.set_desc_language(t("desc_language").into());
    ui.set_desc_user_agent(t("desc_user_agent").into());
    ui.set_desc_preview_limit(t("desc_preview_limit").into());
    ui.set_desc_cache_ttl_days(t("desc_cache_ttl_days").into());
    ui.set_desc_notify_on_completion(t("desc_notify_on_completion").into());
    ui.set_desc_preset(t("desc_preset").into());
    ui.set_desc_domain(t("desc_domain").into());
    ui.set_desc_storage_domain(t("desc_storage_domain").into());
    ui.set_desc_max_connections(t("desc_max_connections").into());
    ui.set_desc_max_concurrent_tiles(t("desc_max_concurrent_tiles").into());
    ui.set_desc_max_concurrent_saves(t("desc_max_concurrent_saves").into());
    ui.set_desc_adaptive_concurrency(t("desc_adaptive_concurrency").into());
    ui.set_desc_timeout(t("desc_timeout").into());
    ui.set_desc_max_retries(t("desc_max_retries").into());
    ui.set_desc_quality(t("desc_quality").into());
    ui.set_desc_frame_skip(t("desc_frame_skip").into());
    ui.set_desc_link_mode(t("desc_link_mode").into());
    ui.set_desc_output_format(t("desc_output_format").into());
    ui.set_desc_fps(t("desc_fps").into());
    ui.set_desc_video_codec(t("desc_video_codec").into());
    ui.set_desc_hw_accel(t("desc_hardware_acceleration").into());
    ui.set_desc_font_size(t("desc_font_size").into());
    ui.set_desc_accent_color(t("desc_accent_color").into());
    ui.set_desc_hover_color(t("desc_hover_color").into());
    ui.set_desc_effects_stream(t("desc_effects_stream").into());
    ui.set_desc_effects_png(t("desc_effects_png").into());
    ui.set_desc_effects_timestamp(t("desc_effects_timestamp").into());
    ui.set_desc_effects_progress(t("desc_effects_progress").into());
    ui.set_desc_effects_time_format(t("desc_effects_time_format").into());
    ui.set_desc_effects_watermark(t("desc_effects_watermark").into());
    ui.set_desc_effects_position(t("desc_effects_position").into());
    ui.set_desc_effects_camera(t("desc_effects_camera").into());
    ui.set_desc_effects_camera_strength(t("desc_effects_camera_strength").into());
    ui.set_desc_effects_frame(t("desc_effects_frame").into());
    ui.set_desc_effects_audio(t("desc_effects_audio").into());

    // Localized option lists
    ui.set_stage_names(string_model(&[
        t("stage_area_selected"),
        t("stage_tiles_found"),
        t("stage_downloading"),
        t("stage_rendering"),
        t("stage_encoding"),
    ]));
    ui.set_watermark_options(string_model(&[
        t("watermark_top_left"),
        t("watermark_top_right"),
        t("watermark_bottom_left"),
        t("watermark_bottom_right"),
    ]));
    ui.set_camera_options(string_model(&[
        t("camera_static"),
        t("camera_zoom_out"),
        t("camera_zoom_in"),
        t("camera_pan_lr"),
        t("camera_pan_rl"),
    ]));
    ui.set_frame_options(string_model(&[
        t("frame_source"),
        t("frame_vertical"),
        t("frame_square"),
        t("frame_wide"),
    ]));
}

// ----------------------------------------------------------------------------
// Desktop notification
// ----------------------------------------------------------------------------

/// Best-effort "video is ready" notification. The setting was persisted by the
/// V1 app (and shown in the settings dialog) but the Rust GUI never used it.
fn notify_completion(settings: &Settings, body: &str) {
    if !settings.notify_on_completion {
        return;
    }
    let title = t("timelapse_created_title");
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("notify-send")
            .arg(&title)
            .arg(body)
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        // AppleScript string literals need backslashes and quotes escaped.
        let escape = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            escape(body),
            escape(&title)
        );
        let _ = std::process::Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .spawn();
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        // No portable notification API without extra dependencies.
        let _ = (title, body);
    }
}

// ----------------------------------------------------------------------------
// Main function
// ----------------------------------------------------------------------------

/// Desktop-entry id shared with the Linux packaging (`packaging/linux`).
///
/// Wayland compositors ignore the icon a client sets on its window: the title
/// bar and the taskbar take it from the `.desktop` file whose name matches the
/// window's app id. Publishing the id here lets an installed entry (and the
/// icon it names) win over the generic fallback. On X11 the same value becomes
/// `WM_CLASS`, which is what window grouping and `StartupWMClass` match on.
const XDG_APP_ID: &str = "timelapsemaker";

/// Starts the Slint GUI on the calling (main) thread; returns once the window
/// is closed.
pub fn run() -> Result<(), slint::PlatformError> {
    // The id can only be stored once a platform exists, and it has to be set
    // before the first window: the backend hands it to the compositor while it
    // creates the surface. A missing platform only means there is no XDG
    // desktop to talk to, so a failure here stays non-fatal.
    slint::BackendSelector::new().select()?;
    if let Err(err) = slint::set_xdg_app_id(XDG_APP_ID) {
        eprintln!("timelapse: could not set the XDG application id: {err}");
    }

    let (net_tx, net_rx) = std::sync::mpsc::channel::<NetCmd>();
    let (preview_tx, preview_rx) = tokio::sync::mpsc::unbounded_channel::<PreviewCmd>();
    let (cache_tx, cache_rx) = std::sync::mpsc::channel::<CacheCmd>();
    let (job_tx, job_rx) = std::sync::mpsc::channel::<JobCmd>();
    let (events_tx, events_rx) = std::sync::mpsc::channel::<WorkerEvent>();

    spawn_net_worker(net_rx, events_tx.clone());
    spawn_preview_worker(preview_rx, events_tx.clone());
    spawn_cache_worker(cache_rx, events_tx.clone());
    spawn_job_worker(job_rx, events_tx);

    let state = Arc::new(Mutex::new(AppState::new(
        net_tx, preview_tx, cache_tx, job_tx,
    )));
    let app = AppWindow::new()?;
    let app_weak = app.as_weak();

    // Restore the last window size (0 = keep the .slint preferred size).
    {
        let st = state.lock().unwrap();
        if st.settings.window_width > 0 && st.settings.window_height > 0 {
            app.window().set_size(slint::PhysicalSize::new(
                st.settings.window_width as u32,
                st.settings.window_height as u32,
            ));
        }
    }

    // Initial UI setup
    sync_i18n(&app);
    {
        let st = state.lock().unwrap();
        sync_ui_state(&app, &st);
    }

    // Spawn event loop forwarder to receive events from workers and dispatch to Slint UI
    {
        let state_ev = state.clone();
        let app_weak_ev = app_weak.clone();
        std::thread::spawn(move || {
            while let Ok(ev) = events_rx.recv() {
                let state_clone = state_ev.clone();
                let app_weak_clone = app_weak_ev.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = app_weak_clone.upgrade() else {
                        return;
                    };
                    let mut st = state_clone.lock().unwrap();
                    let mut show_comp = false;
                    match ev {
                        WorkerEvent::CanvasList(result) => match result {
                            Ok(list) => {
                                // An imported template's canvas id wins over the
                                // previous selection; both are resolved only now
                                // because the list arrives asynchronously.
                                let previous_id = st.selected_canvas().map(|c| c.id.clone());
                                let wanted_id = st.pending_canvas_id.take().or(previous_id);
                                st.canvas_list_failed = None;
                                st.canvases = list;
                                st.current_canvas = wanted_id
                                    .and_then(|id| st.canvases.iter().position(|c| c.id == id))
                                    .unwrap_or(0);
                                let names: Vec<SharedString> =
                                    st.canvases.iter().map(|c| c.name.clone().into()).collect();
                                ui.set_canvas_names(ModelRc::new(VecModel::from(names)));
                                ui.set_current_canvas_index(st.current_canvas as i32);
                                ui.set_canvas_list_error("".into());
                                st.request_preview();
                            }
                            Err(e) => {
                                st.canvas_list_failed = Some(e.clone());
                                st.log_line(LogLevel::Error, tf("canvas_list_error", &[&e]));
                                ui.set_canvas_list_error(e.into());
                            }
                        },
                        WorkerEvent::PreviewFrame { id, image, done } => {
                            // Anything but the newest request is stale by
                            // definition (the worker coalesces, but a slow
                            // response may still land after a newer request).
                            // A preview that was in flight when a run started
                            // must not replace the live theater either.
                            if id == st.preview_id && !st.running {
                                if done {
                                    st.preview_loading = false;
                                    st.preview_static = Some(image.clone());
                                }
                                st.preview_image = Some(image);
                                st.preview_revision = st.preview_revision.wrapping_add(1);
                            } else if id == st.preview_id {
                                st.preview_loading = false;
                            }
                        }
                        WorkerEvent::Pipeline(event) => st.handle_pipeline_event(event),
                        WorkerEvent::CacheStats { bytes, count } => {
                            st.cache_stats_text = tf(
                                "cache_stats_short",
                                &[
                                    &format!("{:.1}", bytes as f64 / 1024.0 / 1024.0),
                                    &count.to_string(),
                                ],
                            );
                        }
                        WorkerEvent::CacheCleared { bytes, count } => {
                            st.cache_stats_text = tf(
                                "cache_stats_short",
                                &[
                                    &format!("{:.1}", bytes as f64 / 1024.0 / 1024.0),
                                    &count.to_string(),
                                ],
                            );
                            // Every cached tile is gone; the preview on screen
                            // came from it, so it must be refetched.
                            st.last_preview_key.clear();
                            st.request_preview();
                        }
                        WorkerEvent::PipelineDone(result) => {
                            show_comp = st.finish_pipeline(result);
                        }
                        WorkerEvent::Encode(event) => st.handle_encode_event(event),
                        WorkerEvent::EncodeDone(result) => {
                            show_comp = st.finish_encode(result);
                        }
                    }
                    sync_ui_state(&ui, &st);
                    if show_comp {
                        ui.set_completion_summary(st.completion_summary.clone().into());
                        ui.set_show_completion(true);
                    }
                });
            }
        });
    }

    // Callbacks setup
    {
        let state_cb = state.clone();
        let app_weak_cb = app_weak.clone();
        app.on_dismiss_onboarding(move || {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let mut st = state_cb.lock().unwrap();
            st.settings.onboarding_done = true;
            let path = st.settings_path.clone();
            let _ = st.settings.save(&path);
            ui.set_onboarding_done(true);
        });
    }

    {
        let state_cb = state.clone();
        let app_weak_cb = app_weak.clone();
        app.on_canvas_selected(move |idx| {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let mut st = state_cb.lock().unwrap();
            if idx >= 0 && (idx as usize) < st.canvases.len() {
                st.current_canvas = idx as usize;
                st.pending_canvas_id = None;
                st.last_preview_key.clear();
                ui.set_preview_zoom(1.0);
                ui.set_preview_pan_x(0.0);
                ui.set_preview_pan_y(0.0);
                st.request_preview();
                sync_ui_state(&ui, &st);
            }
        });
    }

    {
        let state_cb = state.clone();
        let app_weak_cb = app_weak.clone();
        app.on_coord1_changed(move |text: SharedString| {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let mut st = state_cb.lock().unwrap();
            st.coord1 = text.to_string();
            st.last_preview_key.clear();
            ui.set_preview_zoom(1.0);
            ui.set_preview_pan_x(0.0);
            ui.set_preview_pan_y(0.0);
            // The dates came from the template, so no quick-range chip is active.
            ui.set_date_preset_active(-1);
            st.request_preview();
            sync_ui_state(&ui, &st);
        });
    }

    {
        let state_cb = state.clone();
        let app_weak_cb = app_weak.clone();
        app.on_coord2_changed(move |text: SharedString| {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let mut st = state_cb.lock().unwrap();
            st.coord2 = text.to_string();
            st.last_preview_key.clear();
            ui.set_preview_zoom(1.0);
            ui.set_preview_pan_x(0.0);
            ui.set_preview_pan_y(0.0);
            st.request_preview();
            sync_ui_state(&ui, &st);
        });
    }

    {
        let state_cb = state.clone();
        let app_weak_cb = app_weak.clone();
        app.on_start_date_changed(move |text: SharedString| {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let mut st = state_cb.lock().unwrap();
            st.start_date_text = text.to_string();
            // A hand-edited date no longer matches any preset chip.
            ui.set_date_preset_active(-1);
            if let Some(d) = parse_date_text(&text) {
                if d != st.start_date {
                    st.start_date = d;
                    st.preview_day_offset = 0;
                    st.last_preview_key.clear();
                    ui.set_preview_zoom(1.0);
                    ui.set_preview_pan_x(0.0);
                    ui.set_preview_pan_y(0.0);
                }
            }
            if st.parsed_dates().is_some_and(|(s, e)| s <= e) {
                st.request_preview();
            }
            sync_ui_state(&ui, &st);
        });
    }

    {
        let state_cb = state.clone();
        let app_weak_cb = app_weak.clone();
        app.on_end_date_changed(move |text: SharedString| {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let mut st = state_cb.lock().unwrap();
            st.end_date_text = text.to_string();
            // A hand-edited date no longer matches any preset chip.
            ui.set_date_preset_active(-1);
            if let Some(d) = parse_date_text(&text) {
                if d != st.end_date {
                    st.end_date = d;
                    st.last_preview_key.clear();
                    ui.set_preview_zoom(1.0);
                    ui.set_preview_pan_x(0.0);
                    ui.set_preview_pan_y(0.0);
                }
            }
            if st.parsed_dates().is_some_and(|(s, e)| s <= e) {
                st.request_preview();
            }
            sync_ui_state(&ui, &st);
        });
    }

    {
        let state_cb = state.clone();
        let app_weak_cb = app_weak.clone();
        app.on_date_preset(move |preset| {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let mut st = state_cb.lock().unwrap();
            let today = chrono::Local::now().date_naive();
            let range = match preset {
                0 => Some((today - chrono::Duration::days(6), today)),
                1 => Some((today - chrono::Duration::days(29), today)),
                2 => Some((
                    NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap_or(today),
                    today,
                )),
                _ => None,
            };
            let Some((start, end)) = range else { return };
            st.start_date = start;
            st.end_date = end;
            st.start_date_text = start.format("%Y-%m-%d").to_string();
            st.end_date_text = end.format("%Y-%m-%d").to_string();
            st.preview_day_offset = 0;
            st.last_preview_key.clear();
            ui.set_preview_zoom(1.0);
            ui.set_preview_pan_x(0.0);
            ui.set_preview_pan_y(0.0);
            st.request_preview();
            sync_ui_state(&ui, &st);
        });
    }

    {
        let state_cb = state.clone();
        let app_weak_cb = app_weak.clone();
        app.on_output_folder_changed(move |text: SharedString| {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let mut st = state_cb.lock().unwrap();
            st.output_folder = text.to_string();
            sync_ui_state(&ui, &st);
        });
    }

    {
        let state_cb = state.clone();
        let app_weak_cb = app_weak.clone();
        app.on_browse_output_folder(move || {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let mut st = state_cb.lock().unwrap();
            let mut dialog = rfd::FileDialog::new().set_title(t("select_output_folder"));
            let current = std::path::Path::new(&st.output_folder);
            if current.is_dir() {
                dialog = dialog.set_directory(current);
            }
            if let Some(dir) = dialog.pick_folder() {
                st.output_folder = dir.to_string_lossy().into_owned();
                sync_ui_state(&ui, &st);
            }
        });
    }

    {
        let state_cb = state.clone();
        let app_weak_cb = app_weak.clone();
        app.on_browse_audio(move || {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let mut st = state_cb.lock().unwrap();
            let mut dialog = rfd::FileDialog::new()
                .set_title(t("select_audio"))
                .add_filter("Audio", &["mp3", "wav", "flac", "aac", "ogg", "m4a"]);
            if let Some(parent) = std::path::Path::new(&st.settings.audio_path).parent() {
                if parent.is_dir() {
                    dialog = dialog.set_directory(parent);
                }
            }
            let Some(file) = dialog.pick_file() else {
                return;
            };
            ui.set_draft_audio_path(file.to_string_lossy().into_owned().into());
            let mut draft = st.settings.clone();
            apply_studio_draft(&ui, &mut draft);
            draft.clamp();
            st.settings = draft;
            let path = st.settings_path.clone();
            let _ = st.settings.save(&path);
            sync_ui_state(&ui, &st);
        });
    }

    {
        let app_weak_cb = app_weak.clone();
        app.on_accent_color_changed(move |text: SharedString| {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            match hex_to_slint_color(&text) {
                Some(color) => {
                    // Live preview: the swatch and every themed accent surface
                    // follow the text field, matching the hint in the dialog.
                    ui.set_accent_swatch(color);
                    ui.set_col_accent(color);
                    ui.set_accent_invalid(false);
                }
                None => ui.set_accent_invalid(!text.trim().is_empty()),
            }
        });
    }

    {
        let app_weak_cb = app_weak.clone();
        app.on_hover_color_changed(move |text: SharedString| {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            match hex_to_slint_color(&text) {
                Some(color) => {
                    ui.set_hover_swatch(color);
                    ui.set_col_hover(color);
                    ui.set_hover_invalid(false);
                }
                None => ui.set_hover_invalid(!text.trim().is_empty()),
            }
        });
    }

    {
        let state_cb = state.clone();
        let app_weak_cb = app_weak.clone();
        app.on_start_timelapse(move || {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let mut st = state_cb.lock().unwrap();
            st.start_timelapse();
            sync_ui_state(&ui, &st);
        });
    }

    {
        let state_cb = state.clone();
        let app_weak_cb = app_weak.clone();
        app.on_start_render_only(move || {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let mut st = state_cb.lock().unwrap();
            st.start_compile_only();
            sync_ui_state(&ui, &st);
        });
    }

    {
        let state_cb = state.clone();
        let app_weak_cb = app_weak.clone();
        app.on_cancel_process(move || {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let mut st = state_cb.lock().unwrap();
            st.cancel_process();
            sync_ui_state(&ui, &st);
        });
    }

    {
        let state_cb = state.clone();
        let app_weak_cb = app_weak.clone();
        app.on_log_clear(move || {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let mut st = state_cb.lock().unwrap();
            if !st.running {
                st.log.clear();
                st.log_revision = st.log_revision.wrapping_add(1);
                sync_ui_state(&ui, &st);
            }
        });
    }

    {
        let state_cb = state.clone();
        app.on_log_copy(move || {
            let st = state_cb.lock().unwrap();
            let text: String = st
                .log
                .iter()
                .filter(|(l, _)| {
                    !st.log_filter_errors || matches!(*l, LogLevel::Warning | LogLevel::Error)
                })
                .map(|(_, m)| format!("{m}\n"))
                .collect();
            if let Ok(mut board) = arboard::Clipboard::new() {
                let _ = board.set_text(text);
            }
        });
    }

    {
        let state_cb = state.clone();
        let app_weak_cb = app_weak.clone();
        app.on_toggle_log_errors_only(move || {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let mut st = state_cb.lock().unwrap();
            st.log_filter_errors = !st.log_filter_errors;
            st.log_revision = st.log_revision.wrapping_add(1);
            sync_ui_state(&ui, &st);
        });
    }

    {
        let state_cb = state.clone();
        let app_weak_cb = app_weak.clone();
        app.on_preview_step_day(move |step| {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let mut st = state_cb.lock().unwrap();
            let total = (st.end_date - st.start_date).num_days() + 1;
            let next_offset = st.preview_day_offset + step as i64;
            if next_offset >= 0 && next_offset < total {
                st.preview_day_offset = next_offset;
                st.last_preview_key.clear();
                st.request_preview();
                sync_ui_state(&ui, &st);
            }
        });
    }

    {
        let state_cb = state.clone();
        let app_weak_cb = app_weak.clone();
        app.on_preview_scrub(move |index| {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let mut st = state_cb.lock().unwrap();
            let total = (st.end_date - st.start_date).num_days() + 1;
            if total <= 0 {
                return;
            }
            let next = (index as i64).clamp(0, total - 1);
            if next != st.preview_day_offset {
                st.preview_day_offset = next;
                st.last_preview_key.clear();
                st.request_preview();
                sync_ui_state(&ui, &st);
            }
        });
    }

    {
        let state_cb = state.clone();
        let app_weak_cb = app_weak.clone();
        app.on_import_template(move || {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let path = rfd::FileDialog::new()
                .set_title(t("import_template"))
                .add_filter("JSON", &["json"])
                .pick_file();
            let Some(path) = path else { return };
            let tmpl = match Template::load(&path) {
                Ok(t) => t,
                Err(e) => {
                    let mut st = state_cb.lock().unwrap();
                    st.log_line(LogLevel::Error, e.to_string());
                    sync_ui_state(&ui, &st);
                    return;
                }
            };
            let mut st = state_cb.lock().unwrap();
            let mut domain_changed = false;
            if let Some(d) = &tmpl.domain {
                if *d != st.settings.domain {
                    st.settings.domain = d.clone();
                    domain_changed = true;
                }
            }
            if let Some(s) = &tmpl.storage_domain {
                if *s != st.settings.storage_domain {
                    st.settings.storage_domain = s.clone();
                    domain_changed = true;
                }
            }
            let canvas_id = tmpl.canvas_id.clone();
            if domain_changed {
                // The target canvas list belongs to the new site and arrives
                // asynchronously; remember which canvas the template selected.
                st.pending_canvas_id = Some(canvas_id);
                let _ = st.net_tx.send(NetCmd::FetchCanvasList(st.settings.clone()));
            } else if let Some(idx) = st.canvases.iter().position(|c| c.id == canvas_id) {
                st.current_canvas = idx;
                st.pending_canvas_id = None;
            } else {
                // The list may still be loading (for example at startup).
                st.pending_canvas_id = Some(canvas_id);
            }
            st.coord1 = tmpl.first_cord.trim().to_string();
            st.coord2 = tmpl.second_cord.trim().to_string();
            if let Ok(d) = tmpl.start() {
                st.start_date = d;
                st.start_date_text = d.format("%Y-%m-%d").to_string();
                st.preview_day_offset = 0;
            }
            if let Ok(d) = tmpl.end() {
                st.end_date = d;
                st.end_date_text = d.format("%Y-%m-%d").to_string();
            }
            st.output_folder = tmpl.output_folder.trim().to_string();
            st.last_preview_key.clear();
            ui.set_preview_zoom(1.0);
            ui.set_preview_pan_x(0.0);
            ui.set_preview_pan_y(0.0);
            st.log_line(LogLevel::Success, t("settings_loaded"));
            st.request_preview();
            sync_ui_state(&ui, &st);
        });
    }

    {
        let state_cb = state.clone();
        let app_weak_cb = app_weak.clone();
        app.on_export_template(move || {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let mut st = state_cb.lock().unwrap();
            let Some(canvas) = st.selected_canvas() else {
                return;
            };
            let path = rfd::FileDialog::new()
                .set_title(t("export_template"))
                .add_filter("JSON", &["json"])
                .set_file_name("template.json")
                .save_file();
            let Some(path) = path else { return };
            let tmpl = Template {
                canvas_id: canvas.id.clone(),
                first_cord: st.coord1.clone(),
                second_cord: st.coord2.clone(),
                start_date: st.start_date.format("%Y-%m-%d").to_string(),
                end_date: st.end_date.format("%Y-%m-%d").to_string(),
                output_folder: st.output_folder.clone(),
                domain: Some(st.settings.domain.clone()),
                storage_domain: Some(st.settings.storage_domain.clone()),
            };
            match tmpl.save(&path) {
                Ok(()) => st.log_line(LogLevel::Success, t("settings_exported")),
                Err(e) => st.log_line(LogLevel::Error, e.to_string()),
            }
            sync_ui_state(&ui, &st);
        });
    }

    {
        let state_cb = state.clone();
        let app_weak_cb = app_weak.clone();
        app.on_open_settings(move || {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let mut st = state_cb.lock().unwrap();
            st.draft = st.settings.clone();
            sync_ui_state(&ui, &st);
            ui.set_setting_desc(t("setting_desc_placeholder").into());
            ui.set_clear_cache_armed(false);
            ui.set_show_settings(true);
        });
    }

    {
        let app_weak_cb = app_weak.clone();
        app.on_preset_selected(move |idx| {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let idx = idx as usize;
            if idx < PRESETS.len() {
                let (_, dom, store) = PRESETS[idx];
                ui.set_draft_domain(dom.into());
                ui.set_draft_storage_domain(store.into());
                ui.set_draft_preset_idx(idx as i32);
            } else {
                ui.set_draft_preset_idx(PRESETS.len() as i32);
            }
        });
    }

    {
        let state_cb = state.clone();
        let app_weak_cb = app_weak.clone();
        app.on_save_settings(move || {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let mut st = state_cb.lock().unwrap();
            let mut d = st.draft.clone();
            d.domain = ui.get_draft_domain().to_string();
            d.storage_domain = ui.get_draft_storage_domain().to_string();
            d.language = match ui.get_draft_lang_idx() {
                1 => Language::Hu,
                _ => Language::En,
            };
            d.user_agent = ui.get_draft_user_agent().to_string();
            d.preview_limit = ui.get_draft_preview_limit();
            d.cache_ttl_days = ui.get_draft_cache_ttl_days();
            d.notify_on_completion = ui.get_draft_notify_on_completion();
            d.max_connections = ui.get_draft_max_connections();
            d.max_concurrent_tiles = ui.get_draft_max_concurrent_tiles();
            d.max_concurrent_saves = ui.get_draft_max_concurrent_saves();
            d.adaptive_concurrency = ui.get_draft_adaptive_concurrency();
            d.timeout = ui.get_draft_timeout() as f64;
            d.max_retries = ui.get_draft_max_retries();
            d.quality = ui.get_draft_quality();
            d.frame_skip = ui.get_draft_frame_skip();
            d.link_mode = match ui.get_draft_link_mode() {
                1 => LinkMode::Hard,
                2 => LinkMode::Soft,
                3 => LinkMode::Copy,
                _ => LinkMode::Auto,
            };
            d.output_format = match ui.get_draft_output_format() {
                1 => OutputFormat::Mkv,
                2 => OutputFormat::Webm,
                3 => OutputFormat::Mov,
                4 => OutputFormat::Avi,
                _ => OutputFormat::Mp4,
            };
            d.fps = ui.get_draft_fps();
            d.video_codec = match ui.get_draft_video_codec() {
                1 => VideoCodec::H265,
                2 => VideoCodec::Av1,
                _ => VideoCodec::H264,
            };
            d.hardware_acceleration = match ui.get_draft_hw_accel() {
                1 => HardwareAcceleration::None,
                2 => HardwareAcceleration::Nvenc,
                3 => HardwareAcceleration::Amf,
                4 => HardwareAcceleration::Qsv,
                5 => HardwareAcceleration::Vaapi,
                _ => HardwareAcceleration::Auto,
            };
            d.font_size = ui.get_font_size();
            d.accent_color = ui.get_draft_accent_color().to_string();
            d.hover_color = ui.get_draft_hover_color().to_string();
            apply_studio_draft(&ui, &mut d);

            let domain_changed =
                d.domain != st.settings.domain || d.storage_domain != st.settings.storage_domain;
            let lang_changed = d.language != st.settings.language;
            d.clamp();
            st.settings = d;
            let path = st.settings_path.clone();
            let _ = st.settings.save(&path);
            if lang_changed {
                i18n::set_language(st.settings.language);
                sync_i18n(&ui);
            }
            if domain_changed {
                st.canvases.clear();
                st.canvas_list_failed = None;
                st.pending_canvas_id = None;
                let _ = st.net_tx.send(NetCmd::FetchCanvasList(st.settings.clone()));
            } else {
                // Everything that shapes a tile URL (user agent, timeouts,
                // concurrency, background, limits) just changed, so the visible
                // preview may be stale even though the key would match.
                st.last_preview_key.clear();
                st.request_preview();
            }
            ui.set_show_settings(false);
            sync_ui_state(&ui, &st);
        });
    }

    {
        let state_cb = state.clone();
        let app_weak_cb = app_weak.clone();
        app.on_console_toggled(move || {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let mut st = state_cb.lock().unwrap();
            st.settings.console_expanded = ui.get_console_expanded();
            let path = st.settings_path.clone();
            let _ = st.settings.save(&path);
        });
    }

    {
        let state_cb = state.clone();
        let app_weak_cb = app_weak.clone();
        app.on_cancel_settings(move || {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let mut st = state_cb.lock().unwrap();
            // Discard every draft edit (including the live font-size, accent and
            // hover values that the UI applies while the dialog is open) and
            // re-sync so the window snaps back to the saved settings.
            st.draft = st.settings.clone();
            ui.set_show_settings(false);
            ui.set_accent_invalid(false);
            ui.set_hover_invalid(false);
            sync_ui_state(&ui, &st);
        });
    }

    {
        let state_cb = state.clone();
        let app_weak_cb = app_weak.clone();
        app.on_clear_cache(move || {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            let st = state_cb.lock().unwrap();
            let _ = st.cache_tx.send(CacheCmd::Clear);
            sync_ui_state(&ui, &st);
        });
    }

    {
        let state_cb = state.clone();
        app.on_open_video(move || {
            let st = state_cb.lock().unwrap();
            if let Some(p) = &st.last_video_path {
                let _ = open::that_detached(p);
            }
        });
    }

    {
        let state_cb = state.clone();
        app.on_open_folder(move || {
            let st = state_cb.lock().unwrap();
            // Prefer the folder of the finished video, but always fall back to
            // the configured output folder so the button is never a no-op.
            if let Some(dir) = st.last_video_path.as_ref().and_then(|p| p.parent()) {
                let _ = open::that_detached(dir);
            } else if !st.output_folder.trim().is_empty() {
                let _ = open::that_detached(PathBuf::from(st.output_folder.trim()));
            }
        });
    }

    {
        let app_weak_cb = app_weak.clone();
        app.on_close_completion(move || {
            let Some(ui) = app_weak_cb.upgrade() else {
                return;
            };
            ui.set_show_completion(false);
        });
    }

    // Persist the window size when the user closes the window.
    {
        let state_close = state.clone();
        let app_weak_close = app_weak.clone();
        app.window().on_close_requested(move || {
            if let Some(ui) = app_weak_close.upgrade() {
                let mut st = state_close.lock().unwrap();
                // Fullscreen is a transient viewing mode: keep the last
                // windowed size so the next launch does not open as a
                // borderless, un-resizable fullscreen window.
                if !ui.window().is_fullscreen() {
                    let size = ui.window().size();
                    st.settings.window_width = size.width as i32;
                    st.settings.window_height = size.height as i32;
                }
                let path = st.settings_path.clone();
                let _ = st.settings.save(&path);
            }
            slint::CloseRequestResponse::HideWindow
        });
    }

    app.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_coord_valid() {
        assert!(is_coord_valid("0_0"));
        assert!(is_coord_valid("-128_-128"));
        assert!(is_coord_valid("128_128"));
        assert!(is_coord_valid("  -128_-128  "));
        assert!(!is_coord_valid("  -128 _ 128  "));
        assert!(!is_coord_valid("abc"));
        assert!(!is_coord_valid("128_"));
        assert!(!is_coord_valid("_128"));
        assert!(!is_coord_valid(""));
    }

    #[test]
    fn test_hex_to_slint_color() {
        let c1 = hex_to_slint_color("#2b9ed9").expect("valid hex");
        assert_eq!(c1.red(), 0x2b);
        assert_eq!(c1.green(), 0x9e);
        assert_eq!(c1.blue(), 0xd9);

        let c2 = hex_to_slint_color("144870").expect("valid hex without #");
        assert_eq!(c2.red(), 0x14);
        assert_eq!(c2.green(), 0x48);
        assert_eq!(c2.blue(), 0x70);

        assert!(hex_to_slint_color("invalid").is_none());
        assert!(hex_to_slint_color("#12345").is_none());
        assert!(hex_to_slint_color("").is_none());
    }

    #[test]
    fn test_preset_index_for() {
        assert_eq!(preset_index_for("pixelplanet.fun", "storage.gs-os.com"), 0);
        assert_eq!(preset_index_for("PIXELPLANET.FUN", "STORAGE.GS-OS.COM"), 0);
        assert_eq!(
            preset_index_for("pixelya.fun", "storage.pixelya.fun/history"),
            1
        );
        assert_eq!(
            preset_index_for("pixmap.fun", "pixmap.fun/history/chunk"),
            2
        );
        assert_eq!(
            preset_index_for("pxuniverse.fun", "pxuniverse.fun/backupdata"),
            3
        );
        // Custom
        assert_eq!(preset_index_for("mycustom.domain", "custom.storage"), 4);
    }

    #[test]
    fn test_parse_date_text() {
        assert_eq!(
            parse_date_text("2000-02-29"),
            NaiveDate::from_ymd_opt(2000, 2, 29)
        );
        assert_eq!(
            parse_date_text("  2000-01-07  "),
            NaiveDate::from_ymd_opt(2000, 1, 7)
        );
        assert_eq!(parse_date_text("2000-1-7"), None);
        assert_eq!(parse_date_text("2000-13-01"), None);
        assert_eq!(parse_date_text(""), None);
    }

    #[test]
    fn test_is_input_ready() {
        let d1 = NaiveDate::from_ymd_opt(2000, 1, 1).unwrap();
        let d2 = NaiveDate::from_ymd_opt(2000, 1, 7).unwrap();
        assert!(is_input_ready(1, "-10_-10", "10_10", d1, d2, "out/"));
        assert!(!is_input_ready(0, "-10_-10", "10_10", d1, d2, "out/"));
        assert!(!is_input_ready(1, "bad", "10_10", d1, d2, "out/"));
        assert!(!is_input_ready(1, "-10_-10", "10_10", d2, d1, "out/"));
        assert!(!is_input_ready(1, "-10_-10", "10_10", d1, d2, "   "));
    }

    #[test]
    fn test_image_to_slint_image() {
        let img = image::RgbaImage::new(10, 10);
        let slint_img = image_to_slint_image(&img);
        let size = slint_img.size();
        assert_eq!(size.width, 10);
        assert_eq!(size.height, 10);
    }

    #[test]
    fn test_app_state_region_and_preview_key() {
        let (net_tx, _net_rx) = std::sync::mpsc::channel();
        let (preview_tx, _preview_rx) = tokio::sync::mpsc::unbounded_channel();
        let (cache_tx, _cache_rx) = std::sync::mpsc::channel();
        let (job_tx, _job_rx) = std::sync::mpsc::channel();
        let mut state = AppState::new(net_tx, preview_tx, cache_tx, job_tx);

        state.coord1 = "-64_-64".into();
        state.coord2 = "64_64".into();
        let reg = state.region().expect("valid region");
        assert_eq!(reg.width, 129);
        assert_eq!(reg.height, 129);

        state.canvases = vec![CanvasInfo {
            id: "canvas-123".into(),
            name: "Test Canvas".into(),
            size_px: 1024,
            bg_color: Some(0xffffff),
        }];
        state.current_canvas = 0;
        state.preview_day_offset = 2;
        let day_text = state.preview_day_text();
        assert!(!day_text.is_empty());
        let tile_info = state.tile_info_text();
        assert!(!tile_info.is_empty());

        // Test preview key changes when region height changes
        state.coord1 = "0_0".into();
        state.coord2 = "10_10".into();
        state.request_preview();
        let key1 = state.last_preview_key.clone();
        assert!(!key1.is_empty());

        // Change height only
        state.coord2 = "10_20".into();
        state.request_preview();
        let key2 = state.last_preview_key.clone();
        assert_ne!(key1, key2);
    }

    #[test]
    fn test_request_preview_clears_stale_image() {
        let (net_tx, _net_rx) = std::sync::mpsc::channel();
        let (preview_tx, _preview_rx) = tokio::sync::mpsc::unbounded_channel();
        let (cache_tx, _cache_rx) = std::sync::mpsc::channel();
        let (job_tx, _job_rx) = std::sync::mpsc::channel();
        let mut state = AppState::new(net_tx, preview_tx, cache_tx, job_tx);
        state.preview_image = Some(image::RgbaImage::new(2, 2));
        state.preview_revision = 1;
        state.preview_loading = true;

        // An unparsable corner means there is no region at all: the panel must
        // fall back to its empty state instead of keeping an unrelated frame.
        state.coord1 = "nope".into();
        state.coord2 = "10_10".into();
        state.request_preview();
        assert!(state.preview_image.is_none());
        assert!(!state.preview_loading);
        assert!(state.preview_revision > 1);
    }

    #[test]
    fn test_refresh_preview_after_run_restores_static_frame() {
        let (net_tx, _net_rx) = std::sync::mpsc::channel();
        let (preview_tx, _preview_rx) = tokio::sync::mpsc::unbounded_channel();
        let (cache_tx, _cache_rx) = std::sync::mpsc::channel();
        let (job_tx, _job_rx) = std::sync::mpsc::channel();
        let mut state = AppState::new(net_tx, preview_tx, cache_tx, job_tx);
        state.canvases = vec![CanvasInfo {
            id: "canvas-123".into(),
            name: "Test Canvas".into(),
            size_px: 1024,
            bg_color: Some(0xffffff),
        }];
        state.current_canvas = 0;
        state.coord1 = "-64_-64".into();
        state.coord2 = "64_64".into();
        state.preview_static = Some(image::RgbaImage::new(4, 4));
        state.preview_image = None;
        state.preview_revision = 3;
        state.live_preview_index = 7;
        state.live_preview_total = 8;

        state.running = true;
        let (preview_tx, _preview_rx) = tokio::sync::mpsc::unbounded_channel();
        state.preview_tx = preview_tx;

        state.refresh_preview_after_run();

        // The last rendered live frame is replaced by the day preview, the run
        // state is cleared and a fresh preview is requested.
        assert!(!state.running);
        assert_eq!(state.live_preview_index, 0);
        assert_eq!(state.live_preview_total, 0);
        assert!(state.preview_image.is_some());
        assert!(state.preview_revision > 3);
        assert!(state.preview_loading);
        assert!(!state.last_preview_key.is_empty());
    }

    #[test]
    fn test_app_state_log_line_buffering() {
        let (net_tx, _net_rx) = std::sync::mpsc::channel();
        let (preview_tx, _preview_rx) = tokio::sync::mpsc::unbounded_channel();
        let (cache_tx, _cache_rx) = std::sync::mpsc::channel();
        let (job_tx, _job_rx) = std::sync::mpsc::channel();
        let mut state = AppState::new(net_tx, preview_tx, cache_tx, job_tx);

        for i in 0..450 {
            state.log_line(LogLevel::Info, format!("Log line {i}"));
        }
        // Max capacity is capped at 400
        assert_eq!(state.log.len(), 400);
        assert_eq!(state.log.front().unwrap().1, "Log line 50");
        assert_eq!(state.log.back().unwrap().1, "Log line 449");
    }
}
