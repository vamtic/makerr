//! The timelapse download pipeline.
//!
//! Frame semantics:
//! - every day starts with its own base frame (`0000`);
//! - history snapshot tiles are **cumulative diffs against the day base**,
//!   not deltas between consecutive snapshots: a snapshot frame is the day
//!   base with that snapshot painted over it. A pixel dropped from a later
//!   cumulative snapshot is restored to the day base, so erased pixels really
//!   disappear instead of ghosting through the rest of the video;
//! - a missing day base carries the previous frame forward; a day with no
//!   snapshots produces its day-base frame (or a warning when the base is
//!   missing too);
//! - unchanged snapshot frames are hardlinked to the previous frame file;
//! - `--resume` reuses existing `t*.png` frames and their numbering. Resume
//!   restarts the unfinished day at its base, because the per-snapshot pixel
//!   masks needed for correct cumulative composition are not stored on disk.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::NaiveDate;
use futures::StreamExt;
use image::{Rgba, RgbaImage};
use std::time::Duration;
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::adaptive::{AdaptiveLimiter, Outcome};
use crate::compose::{self, OverlayOutcome, TileCoord};
use crate::coord::{self, Region};
use crate::history::{self, CanvasInfo};
use crate::i18n::{tf, LogLevel};
use crate::link::{self, LinkKind, LinkMode};
use crate::net::{self, Client};
use crate::settings::Settings;
use crate::stream::{AsyncStreamEncoder, PipeEncoderConfig};
use crate::studio::camera::{apply_camera, CameraPlan};
use crate::studio::overlays::{apply_frame_overlays, OverlayConfig};

/// Events emitted while the pipeline runs. The CLI prints them; the GUI
/// feeds them into its state machine.
#[derive(Debug, Clone)]
pub enum Event {
    Log(LogLevel, String),
    Status(String),
    Progress {
        value: i64,
        max: i64,
    },
    Stats {
        bytes: u64,
        cached: u64,
        network: u64,
    },
    /// A freshly composed frame for the live rendering theater. Sent on a
    /// throttled schedule, not for every frame.
    PreviewFrame {
        image: Arc<RgbaImage>,
        index: i64,
        total: i64,
    },
}

pub fn emit_log(events: &mpsc::UnboundedSender<Event>, level: LogLevel, msg: String) {
    let _ = events.send(Event::Log(level, msg));
}

/// Ceiling on the number of tiles one frame may cover.
///
/// The biggest real canvas (PixMap Earth, 65536 px per side) is 256×256 = 65 536
/// chunks, so this leaves a 4× margin while keeping a bogus API `size` or a
/// mistyped region from asking for a frame buffer that cannot exist.
pub const MAX_FRAME_TILES: i64 = 262_144;

/// Whether snapshot `index` (0-based within a day) is kept for a frame-skip
/// setting.
///
/// `frame_skip = 0` keeps every snapshot; `frame_skip = N` keeps every
/// `(N + 1)`-th snapshot, starting with the first: `0, N+1, 2(N+1), …`.
/// Keeping the first snapshot matches the original application and the CLI's
/// "keep every Nth+1 snapshot" help text.
pub fn keep_snapshot(index: usize, frame_skip: i32) -> bool {
    let skip = (frame_skip.max(0) as usize).saturating_add(1);
    index.is_multiple_of(skip)
}

/// True when no day-base tile could be downloaded at all.
///
/// A tile that decoded successfully is a real base even if it is entirely the
/// background colour: the site returned the day's actual (blank) state, and the
/// canvas must be reset to it. Only a completely missing base is carried
/// forward from the previous frame.
fn base_tiles_missing(decoded: &[Option<RgbaImage>]) -> bool {
    decoded.iter().all(|tile| tile.is_none())
}

/// Everything the pipeline needs. `canvas_size_px` / `bg_color` can be
/// pre-supplied by a GUI that already has the canvas list; otherwise they
/// are fetched.
#[derive(Debug, Clone)]
pub struct PipelineParams {
    pub settings: Settings,
    pub canvas_id: String,
    pub region: Region,
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub out_dir: PathBuf,
    pub resume: bool,
    pub canvas_size_px: Option<i64>,
    pub bg_color: Option<u32>,
    /// Snapshot times per day, when the caller already knows them (an
    /// embedder, or a test that must not touch the network). Fetched from the
    /// site's history API when `None`.
    pub day_timestamps: Option<BTreeMap<NaiveDate, Vec<String>>>,
}

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("{0}")]
    Fatal(String),
    #[error("network error: {0}")]
    Network(#[from] net::FetchError),
}

#[derive(Debug, Clone)]
pub struct PipelineResult {
    pub frames: i64,
    pub cancelled: bool,
    /// Frames that were bit-identical to their predecessor and therefore
    /// linked instead of being re-encoded (disk-space savings).
    pub linked_frames: i64,
    /// The mechanism the filesystem actually gave us for those frames
    /// (`hardlink`, `symlink` or `copy` when neither is supported).
    pub link_kind: Option<LinkKind>,
    /// The rendered video, when the run streamed frames straight into ffmpeg
    /// instead of leaving a PNG sequence.
    pub video_path: Option<PathBuf>,
}

/// One unit of work: a day-base frame or one snapshot overlay.
#[derive(Debug, Clone)]
struct WorkItem {
    date: NaiveDate,
    /// HHMM snapshot, empty for the day base.
    ts: String,
    is_base: bool,
}

/// The running canvas that cumulative history snapshots are rendered onto.
///
/// History snapshot tiles are cumulative diffs against the day base, not
/// deltas between consecutive snapshots. The canvas therefore cannot simply be
/// overlaid with every snapshot: a pixel that is removed from a later snapshot
/// would stay on the canvas forever. This state keeps the fixed day base and a
/// compact alpha mask of the previous snapshot's pixels so
/// [`compose::apply_cumulative_snapshot`] can restore dropped pixels to the day
/// base before painting the next snapshot.
///
/// Keeping the canvas costs no copy: it is shared with the save pool through an
/// [`Arc`], and `Arc::make_mut` only clones the pixels while an encode of that
/// exact canvas is still in flight (at most `max_concurrent_saves` of them).
#[derive(Default)]
struct CanvasState {
    current: Option<Arc<RgbaImage>>,
    /// The fixed base of the current day. Every snapshot is composited over
    /// this image, never over the previous snapshot.
    base: Option<Arc<RgbaImage>>,
    /// Alpha mask of the previous cumulative snapshot, one entry per tile.
    masks: Vec<Option<compose::TileMask>>,
}

impl CanvasState {
    /// The running canvas, if any. The missing-day-base carry-forward uses it
    /// to link the new frame to the previous frame's file.
    fn peek(&self) -> Option<&Arc<RgbaImage>> {
        self.current.as_ref()
    }

    /// Makes a freshly composed day base the running canvas and starts a new
    /// snapshot-mask sequence for that day.
    fn install(&mut self, img: Arc<RgbaImage>, tile_count: usize) {
        self.base = Some(Arc::clone(&img));
        self.current = Some(img);
        self.masks = vec![None; tile_count];
    }

    /// Starts a new day whose base frame could not be downloaded.
    ///
    /// The previous frame is kept as the day base (the old carry-forward
    /// heuristic), but the previous day's snapshot masks are discarded because
    /// they belong to a different base.
    fn carry_forward_base(&mut self, tile_count: usize) {
        if let Some(current) = &self.current {
            self.base = Some(Arc::clone(current));
        }
        self.masks = vec![None; tile_count];
    }

    /// Renders one cumulative snapshot onto the running canvas — creating the
    /// background-filled canvas when the run has no base yet — and keeps the
    /// result as the running canvas.
    ///
    /// Returns the change signal (relied on by the unchanged-frame linking) and
    /// a handle to the painted canvas for the save pool. `changed` is false only
    /// when the final canvas is bit-identical to the previous frame.
    fn paint_snapshot(
        &mut self,
        region: &Region,
        bg: Rgba<u8>,
        tile_coords: &[TileCoord],
        decoded: &[Option<RgbaImage>],
    ) -> (OverlayOutcome, Arc<RgbaImage>) {
        let expected = (region.width.max(1) as u32, region.height.max(1) as u32);
        let blank = || Arc::new(compose::compose_base(region, bg, tile_coords, &[]));

        let mut canvas = match self.current.take() {
            Some(img) if img.dimensions() == expected => img,
            _ => blank(),
        };
        let base = match self.base.take() {
            Some(img) if img.dimensions() == expected => img,
            _ => blank(),
        };
        if self.masks.len() != tile_coords.len() {
            self.masks = vec![None; tile_coords.len()];
        }

        let outcome = compose::apply_cumulative_snapshot(
            Arc::make_mut(&mut canvas),
            base.as_ref(),
            tile_coords,
            decoded,
            &mut self.masks,
        );
        self.base = Some(base);
        self.current = Some(Arc::clone(&canvas));
        (outcome, canvas)
    }
}

/// One queued frame encode, tagged with the frame number it produces so a link
/// can wait for just its predecessor instead of draining the whole pool.
struct PendingSave {
    index: i64,
    handle: tokio::task::JoinHandle<Result<(), String>>,
}

/// Runs the full download pipeline. Emits progress on `events` and honors
/// `cancel`. Frame files land in `params.out_dir` as `t1.png`, `t2.png`, …
pub async fn run_pipeline(
    params: PipelineParams,
    events: mpsc::UnboundedSender<Event>,
    cancel: CancellationToken,
) -> Result<PipelineResult, PipelineError> {
    let settings = params.settings.clone();
    // A run's download stats are per-run; without this the GUI's counters kept
    // growing across runs (and showed the previous run's numbers before the
    // first tile of the new one arrived).
    crate::cache::reset_stats();

    // One client per run, built on (and used only within) the runtime that
    // drives the pipeline. Sharing a client with another runtime leaves its
    // pooled connections driven by nobody, which turns every fetch into a
    // wait-until-timeout (see `net` module docs).
    let client = Arc::new(net::make_client(&settings));

    emit_log(&events, LogLevel::Info, tf("timelapse_started", &[]));

    // ---- canvas metadata -------------------------------------------------
    // A pre-supplied size is trusted (the GUI just refreshed the canvas list);
    // a missing background simply means "site did not publish a palette", and
    // the V1 app's white default applies.
    let (canvas_size_px, bg_rgb) = match params.canvas_size_px {
        Some(size) if size > 0 => (size, rgb_to_rgba(params.bg_color.unwrap_or(0xffffff))),
        _ => {
            events.send(Event::Status(tf("fetching_canvas", &[]))).ok();
            let canvases = history::fetch_canvas_list(&client, &settings, &cancel).await?;
            let info: &CanvasInfo = history::find_canvas(&canvases, &params.canvas_id)
                .ok_or_else(|| PipelineError::Fatal(tf("canvas_error", &[])))?;
            if info.size_px <= 0 {
                return Err(PipelineError::Fatal(tf("invalid_canvas_size_error", &[])));
            }
            (
                info.size_px,
                rgb_to_rgba(params.bg_color.or(info.bg_color).unwrap_or(0xffffff)),
            )
        }
    };
    emit_log(
        &events,
        LogLevel::Info,
        tf("canvas_size", &[&canvas_size_px.to_string()]),
    );

    // ---- tile grid --------------------------------------------------------
    // Crop to the canvas first: pixels outside `[-size/2, size/2)` can never
    // have tiles, and cropping here keeps a requested selection from turning
    // into background-only multi-gigabyte borders. Then refuse impossible
    // grids before allocating anything.
    let region = coord::clamp_to_canvas(params.region, canvas_size_px);
    let tiles = compose::tile_count(&region, canvas_size_px);
    if tiles > MAX_FRAME_TILES {
        return Err(PipelineError::Fatal(tf(
            "frame_too_large",
            &[&tiles.to_string(), &MAX_FRAME_TILES.to_string()],
        )));
    }
    if let Err(e) = coord::check_frame_size(&region) {
        let msg = match e {
            coord::RegionError::Empty => tf("region_empty", &[]),
            coord::RegionError::TooLarge {
                width,
                height,
                pixels,
            } => tf(
                "region_too_large",
                &[&width.to_string(), &height.to_string(), &pixels.to_string()],
            ),
        };
        return Err(PipelineError::Fatal(msg));
    }
    // Shared via Arc: the V1 port handed the tile list to every queued frame
    // by value (a fresh heap vector per prefetch task); in Rust one atomic
    // refcount clone covers all of them, and the hot painting loops read the
    // same contiguous buffer through the deref.
    let tile_coords: Arc<Vec<TileCoord>> =
        Arc::new(compose::build_tile_coords(&region, canvas_size_px));

    // ---- date range -------------------------------------------------------
    if params.end < params.start {
        return Err(PipelineError::Fatal(tf("invalid_date_range", &[])));
    }
    let dates: Vec<NaiveDate> = {
        let mut d = Vec::new();
        let mut cur = params.start;
        while cur <= params.end {
            d.push(cur);
            cur += chrono::Duration::days(1);
        }
        d
    };

    // ---- history timestamps ----------------------------------------------
    // A caller that already knows the snapshot times (an embedder, or a test
    // that must not touch the network) skips the history requests entirely.
    // Immutable strings shared with every prefetch task (one refcount bump
    // each instead of a fresh String allocation per queued frame).
    let canvas_id_shared = Arc::new(params.canvas_id.clone());
    let storage_shared = Arc::new(settings.storage_domain.clone());

    // The tile pool is created before the history phase so the cache warmer
    // can use it while the rest of the history is still downloading.
    let tile_limiter = Arc::new(AdaptiveLimiter::for_tiles(
        settings.adaptive_concurrency,
        settings.max_concurrent_tiles,
    ));

    let (timestamps, failed_days): (BTreeMap<NaiveDate, Vec<String>>, Vec<NaiveDate>) = match params
        .day_timestamps
        .clone()
    {
        Some(known) => (known, Vec::new()),
        None => {
            events
                .send(Event::Status(tf("fetching_timestamps", &[])))
                .ok();
            emit_log(&events, LogLevel::Info, tf("fetching_timestamps", &[]));
            emit_log(&events, LogLevel::Info, tf("warming_cache", &[]));

            // Warm the tile cache while history is still arriving. The
            // base frames need no history at all, and snapshot tiles are
            // queued as soon as their day settles. Tile downloads go to
            // `storage_domain` while history goes to `domain`, so the two
            // streams do not compete for the same host's rate limit.
            let (warm_tx, warm_rx) = mpsc::unbounded_channel();
            let warm_stop = CancellationToken::new();
            let mut warmer = tokio::spawn(warm_history_days(
                Arc::clone(&client),
                settings.clone(),
                Arc::clone(&tile_limiter),
                Arc::clone(&tile_coords),
                Arc::clone(&canvas_id_shared),
                Arc::clone(&storage_shared),
                warm_rx,
                warm_stop.clone(),
                cancel.clone(),
            ));

            // Day bases need no history at all, so queue them before the
            // first timestamp request returns. Snapshot jobs follow as
            // each day settles, keeping the tile pool busy the whole time.
            for date in &dates {
                let _ = warm_tx.send(WarmJob::Base(*date));
            }

            let progress_events = events.clone();
            let mut stream = history::stream_range_timestamps(
                (*client).clone(),
                settings.clone(),
                dates.clone(),
                params.canvas_id.clone(),
                cancel.clone(),
                Box::new(move |done: usize, total: usize| {
                    let _ = progress_events.send(Event::Status(tf(
                        "fetching_timestamps_progress",
                        &[&done.to_string(), &total.to_string()],
                    )));
                }),
            );

            let mut per_day = BTreeMap::new();
            let mut failed = Vec::new();
            while let Some((date, result)) = stream.next().await {
                match result {
                    Ok(ts) => {
                        enqueue_snapshot_jobs(&warm_tx, date, &ts, settings.frame_skip);
                        per_day.insert(date, ts);
                    }
                    Err(net::FetchError::Cancelled) => {
                        failed.push(date);
                        break;
                    }
                    Err(_) => failed.push(date),
                }
            }
            drop(stream);

            // A concurrent burst can exhaust the per-request retries even
            // when the server recovers moments later. Retry just the failed
            // days sequentially before building the work list, so a short
            // throttle cannot silently shorten the timeline.
            let (recovered, still_failed) =
                history::retry_failed_days(&client, &settings, failed, &params.canvas_id, &cancel)
                    .await;
            for (date, ts) in recovered {
                enqueue_snapshot_jobs(&warm_tx, date, &ts, settings.frame_skip);
                per_day.insert(date, ts);
            }
            failed = still_failed;
            drop(warm_tx);

            // Stop the warmer before the pipeline starts: its queued days
            // are about to be fetched anyway, and two consumers racing on
            // the same tiles would only duplicate work. Let the current
            // frame finish so its tiles reach the cache, but never block
            // the real run for long.
            warm_stop.cancel();
            if tokio::time::timeout(Duration::from_secs(2), &mut warmer)
                .await
                .is_err()
            {
                warmer.abort();
            }

            (per_day, failed)
        }
    };

    if !failed_days.is_empty() {
        emit_log(
            &events,
            LogLevel::Warning,
            tf(
                "history_failed_days",
                &[
                    &failed_days.len().to_string(),
                    &failed_days
                        .iter()
                        .map(|d| d.format("%Y-%m-%d").to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                ],
            ),
        );
    }

    if cancel.is_cancelled() {
        return Ok(PipelineResult {
            frames: 0,
            cancelled: true,
            linked_frames: 0,
            link_kind: None,
            video_path: None,
        });
    }

    // ---- work list --------------------------------------------------------
    // `frame_skip = N` keeps every (N+1)-th snapshot, starting with the first
    // one (indices 0, N+1, 2(N+1), …). N = 0 keeps every snapshot.
    let mut work: Vec<WorkItem> = Vec::new();
    for date in &dates {
        work.push(WorkItem {
            date: *date,
            ts: String::new(),
            is_base: true,
        });
        if let Some(ts_list) = timestamps.get(date) {
            for (ts_idx, ts) in ts_list.iter().enumerate() {
                if !keep_snapshot(ts_idx, settings.frame_skip) {
                    continue;
                }
                work.push(WorkItem {
                    date: *date,
                    ts: ts.clone(),
                    is_base: false,
                });
            }
        }
    }
    let total_frames = work.len() as i64;
    events
        .send(Event::Progress {
            value: 0,
            max: total_frames,
        })
        .ok();
    emit_log(
        &events,
        LogLevel::Info,
        tf("total_frames", &[&total_frames.to_string()]),
    );

    // ---- output folder prep ----------------------------------------------
    std::fs::create_dir_all(&params.out_dir)
        .map_err(|e| PipelineError::Fatal(format!("{}: {e}", params.out_dir.display())))?;
    if !params.resume || settings.stream_video {
        clean_frames(&params.out_dir);
        write_metadata(&params.out_dir, &params, &region)?;
    }

    // ---- rendering mode ---------------------------------------------------
    // Streaming writes raw RGBA frames straight into ffmpeg; nothing but the
    // final video ever reaches the disk. The legacy PNG sequence remains
    // available (and is the fallback when the packed output is requested).
    let streaming = settings.stream_video;
    let save_pngs = !streaming || settings.save_png_sequence;
    if params.resume && streaming {
        emit_log(&events, LogLevel::Warning, tf("stream_resume_ignored", &[]));
    }
    let overlay_config = OverlayConfig::from_settings(&settings);
    let camera_plan = CameraPlan::new(settings.camera_motion, settings.camera_strength);
    let social_preset = settings.social_preset;
    let social_active = social_preset != crate::settings::SocialPreset::Source;
    let (out_width, out_height) = crate::studio::social::target_dimensions(
        social_preset,
        region.width.max(1) as u32,
        region.height.max(1) as u32,
    );
    // Resuming frames is only sound while the frame size equals the selected
    // region; a framing preset changes it.
    let resume_enabled = params.resume && !streaming && !social_active;
    if params.resume && social_active {
        emit_log(&events, LogLevel::Warning, tf("stream_resume_ignored", &[]));
    }
    let visual_fx = !overlay_config.is_empty() || !camera_plan.is_static() || social_active;
    let preview_stride = ((total_frames / 180).max(1)) as usize;

    let mut stream_encoder: Option<AsyncStreamEncoder> = if streaming {
        let output_path = params
            .out_dir
            .join(format!("timelapse.{}", settings.output_format.extension()));
        let audio_track = crate::studio::audio::resolve_track(&settings.audio_path);
        if !settings.audio_path.trim().is_empty() && audio_track.is_none() {
            emit_log(
                &events,
                LogLevel::Warning,
                tf("audio_track_missing", &[&settings.audio_path]),
            );
        }
        let cfg = PipeEncoderConfig {
            width: out_width,
            height: out_height,
            fps: settings.fps.max(1) as u32,
            output_path,
            settings: settings.clone(),
            audio_track,
            total_frames,
        };
        Some(AsyncStreamEncoder::spawn(cfg).map_err(|e| PipelineError::Fatal(e.to_string()))?)
    } else {
        None
    };

    // ---- resume -----------------------------------------------------------
    // Adopt the contiguous prefix of existing frames without decoding the whole
    // set: only the last adopted frame is needed as the running canvas. Frames
    // past the first gap are stale leftovers from a longer previous run and are
    // removed so their numbering cannot collide with the frames written now.
    // If the next unfinished frame is in the middle of a day, `prepare_resume`
    // rewinds to that day's base: correct cumulative composition needs the day
    // base and the previous snapshot mask, neither of which is stored on disk.
    let mut current = CanvasState::default();
    let mut resume_skip = 0usize;
    if resume_enabled {
        let (skip, last_frame) = prepare_resume(&params.out_dir, &params, &region, &work, &events);
        resume_skip = skip;
        if let Some(img) = last_frame {
            current.install(img, tile_coords.len());
        }
        if resume_skip > 0 {
            events
                .send(Event::Progress {
                    value: resume_skip as i64,
                    max: total_frames,
                })
                .ok();
        }
    }

    // ---- shared download state -------------------------------------------
    // `tile_limiter` (created before the history phase for the cache warmer) is
    // the one global tile pool: the lookahead prefetch tasks, the warmer and
    // the current frame all draw from it, so network concurrency never exceeds
    // the adaptive tile limit regardless of how many frames are in flight.

    // Prefetch budget: 64 MiB worth of in-flight *decoded* tiles, clamped to
    // 4..64 frames. A 256×256 tile decodes to 256 KiB, so budget from that real
    // footprint instead of the compressed size — otherwise a wide region would
    // sit on the 64-frame cap with far more than 64 MiB of PNGs in flight.
    // The buffers kept per tile are the raw PNG payloads returned by
    // `fetch_frame_tiles`, which are a fraction of the decoded size, so the
    // estimate stays conservative on purpose.
    let est_bytes_per_frame = (tile_coords.len() as i64 * 8 * 8192).max(1);
    let prefetch_lookahead = ((64 * 1024 * 1024) / est_bytes_per_frame).clamp(4, 64) as usize;

    let mut prefetched: VecDeque<tokio::task::JoinHandle<Vec<Option<Vec<u8>>>>> = VecDeque::new();
    let mut next_prefetch = resume_skip;

    let mut frame_index: i64 = resume_skip as i64 + 1;
    let mut completed_frames: i64 = resume_skip as i64;
    let mut linked_frames: i64 = 0;
    let mut save_failures = 0usize;
    let mut link_kind: Option<LinkKind> = None;
    let link_mode: LinkMode = settings.link_mode;

    // Bounded PNG save pool. The permit is taken *before* the save task is
    // spawned, so a slow disk pushes back on the frame loop instead of piling
    // up thousands of finished frames (a full frame buffer each) in memory.
    let save_sem = Arc::new(Semaphore::new(settings.max_concurrent_saves.max(1) as usize));
    let mut save_handles: Vec<PendingSave> = Vec::new();

    let mut last_status_date = String::new();

    for (i, item) in work.iter().enumerate().skip(resume_skip) {
        if cancel.is_cancelled() {
            break;
        }

        let date_str = item.date.format("%Y-%m-%d").to_string();
        if date_str != last_status_date {
            last_status_date = date_str.clone();
            events
                .send(Event::Status(tf("processing_date", &[&date_str])))
                .ok();
        }

        let frame_path = params.out_dir.join(format!("t{frame_index}.png"));

        // Top up the prefetch window.
        let limit = work.len().min(i + 1 + prefetch_lookahead);
        while next_prefetch < limit {
            let idx = next_prefetch;
            next_prefetch += 1;
            let fp = params.out_dir.join(format!("t{}.png", idx as i64 + 1));
            if resume_enabled && fp.exists() {
                continue;
            }
            let w = work[idx].clone();
            let client = Arc::clone(&client);
            let sem = Arc::clone(&tile_limiter);
            let settings = settings.clone();
            let canvas_id = Arc::clone(&canvas_id_shared);
            let storage = Arc::clone(&storage_shared);
            let coords = Arc::clone(&tile_coords);
            let cancel = cancel.clone();
            let api_date = w.date.format("%Y%m%d").to_string();
            prefetched.push_back(tokio::spawn(async move {
                fetch_frame_tiles(
                    &client, &settings, &sem, &storage, &canvas_id, &api_date, &w.ts, &coords,
                    &cancel,
                )
                .await
            }));
        }

        // Consume this frame's tiles.
        let api_date = item.date.format("%Y%m%d").to_string();
        let datas = if let Some(handle) = prefetched.pop_front() {
            handle
                .await
                .unwrap_or_else(|_| vec![None; tile_coords.len()])
        } else {
            fetch_frame_tiles(
                &client,
                &settings,
                &tile_limiter,
                &settings.storage_domain,
                &params.canvas_id,
                &api_date,
                &item.ts,
                &tile_coords,
                &cancel,
            )
            .await
        };

        if cancel.is_cancelled() {
            break;
        }

        let decoded = compose::decode_tiles_parallel(&datas);
        let current_time = item_datetime(item.date, &item.ts);
        // The canvas this step contributes to the video. It is rendered at most
        // once and shared through `Arc` with the PNG pool, the encoder and the
        // preview channel.
        let mut encode_source: Option<Arc<RgbaImage>> = None;
        // Output already rendered for the PNG sequence; the encoder reuses it
        // instead of applying the overlays a second time.
        let mut rendered_output: Option<Arc<RgbaImage>> = None;
        let mut encode_duplicate = false;

        if item.is_base {
            let composed = Arc::new(compose::compose_base(
                &region,
                bg_rgb,
                &tile_coords,
                &decoded,
            ));
            if current.peek().is_some() && base_tiles_missing(&decoded) {
                // No base tile at all for this day. If the day has no
                // snapshots either, carry the previous frame forward via
                // linking/copy. Otherwise keep the previous frame as the base
                // the snapshots can paint onto.
                let day_no_snaps = timestamps.get(&item.date).is_none_or(|v| v.is_empty());
                if day_no_snaps {
                    emit_log(
                        &events,
                        LogLevel::Warning,
                        tf("day_no_snapshots", &[&date_str]),
                    );
                } else {
                    emit_log(
                        &events,
                        LogLevel::Warning,
                        tf("base_frame_missing", &[&date_str]),
                    );
                }
                if let Some(prev) = current.peek().map(Arc::clone) {
                    if visual_fx {
                        // The timestamp/progress overlay differs from the
                        // predecessor even though the canvas did not, so this
                        // frame must be re-rendered rather than linked.
                        if save_pngs {
                            let rendered = render_output_frame(
                                &prev,
                                &overlay_config,
                                &camera_plan,
                                &current_time,
                                frame_index,
                                total_frames,
                                social_preset,
                            );
                            let permit = next_save_permit(&save_sem).await;
                            save_handles.push(PendingSave {
                                index: frame_index,
                                handle: spawn_save(
                                    Arc::clone(&rendered),
                                    frame_path.clone(),
                                    settings.quality,
                                    permit,
                                ),
                            });
                            rendered_output = Some(rendered);
                        }
                        encode_source = Some(prev);
                    } else {
                        if save_pngs {
                            // Carry the previous frame forward: link to
                            // predecessor's file.
                            save_failures +=
                                drain_saves_until(&mut save_handles, frame_index - 1).await;
                            let dst = frame_path.clone();
                            let quality = settings.quality;
                            let prev_index = frame_index - 1;
                            save_handles.push(PendingSave {
                                index: frame_index,
                                handle: tokio::task::spawn_blocking(move || {
                                    save_frame_linked_or_copy(
                                        &prev, prev_index, &dst, quality, link_mode,
                                    )
                                    .map(|_| ())
                                    .map_err(|e| format!("{}: {e}", dst.display()))
                                }),
                            });
                        }
                        encode_duplicate = true;
                    }
                } else if streaming {
                    // Very first frame of the run and its base is missing:
                    // stream the background-filled frame so the video keeps
                    // its timeline.
                    encode_source = Some(Arc::new(compose::compose_base(
                        &region,
                        bg_rgb,
                        &tile_coords,
                        &[],
                    )));
                }
                current.carry_forward_base(tile_coords.len());
            } else {
                // A decoded base is authoritative even when it is entirely the
                // background colour: the day really is blank and must reset
                // the canvas. Sharing the composition with the save pool
                // (instead of dropping it, or copying it the way the V1
                // clone-paint-compare cycle did) keeps the frame semantics and
                // costs no copy — `Arc::make_mut` only clones while this encode
                // is still in flight.
                if save_pngs {
                    let output = render_output_frame(
                        &composed,
                        &overlay_config,
                        &camera_plan,
                        &current_time,
                        frame_index,
                        total_frames,
                        social_preset,
                    );
                    let permit = next_save_permit(&save_sem).await;
                    save_handles.push(PendingSave {
                        index: frame_index,
                        handle: spawn_save(
                            Arc::clone(&output),
                            frame_path.clone(),
                            settings.quality,
                            permit,
                        ),
                    });
                    rendered_output = Some(output);
                }
                if streaming {
                    encode_source = Some(Arc::clone(&composed));
                }
                current.install(composed, tile_coords.len());
            }
        } else {
            // History snapshots are cumulative diffs from the day base, so
            // render this frame as base + current snapshot. Pixels dropped
            // from the previous cumulative snapshot are restored to the day
            // base first; `changed` is only true when the final canvas really
            // differs, so identical snapshots remain linkable.
            let (outcome, painted) =
                current.paint_snapshot(&region, bg_rgb, &tile_coords, &decoded);

            // Overlays and camera motion make even a bit-identical canvas a
            // visually different frame, so linking is only safe without them.
            let unchanged = !outcome.changed && frame_index > 1 && !visual_fx;
            if unchanged {
                if save_pngs {
                    // Frame identical to its predecessor: link it to the
                    // previous file instead of re-encoding. `auto` uses a
                    // hardlink (like `ln`), falls back to a symlink (`ln -s`,
                    // e.g. on exFAT where hardlinks are unsupported, and on
                    // Windows with Developer Mode) and only then to a byte
                    // copy.
                    let prev_path = params.out_dir.join(format!("t{}.png", frame_index - 1));
                    // An unchanged frame is linked to the file of the frame it
                    // matches, which may still be encoding: wait for just that
                    // pending save (and any earlier ones already finished), not
                    // the whole pool.
                    save_failures += drain_saves_until(&mut save_handles, frame_index - 1).await;
                    if let Some(kind) = link_frame(&prev_path, &frame_path, link_mode) {
                        linked_frames += 1;
                        link_kind = Some(kind);
                        emit_log(
                            &events,
                            LogLevel::Success,
                            tf("frame_unchanged", &[&format!("t{frame_index}.png")]),
                        );
                        // The canvas provably stayed bit-identical, so the new
                        // link already exposes this exact frame's pixels — no
                        // PNG encode at all. The canvas stays the running
                        // canvas.
                    } else {
                        // Linking unavailable (cross-device / unsupported FS):
                        // fall back to writing a real copy of the frame.
                        let permit = next_save_permit(&save_sem).await;
                        save_handles.push(PendingSave {
                            index: frame_index,
                            handle: spawn_save(
                                Arc::clone(&painted),
                                frame_path.clone(),
                                settings.quality,
                                permit,
                            ),
                        });
                    }
                }
                encode_duplicate = true;
            } else {
                if save_pngs {
                    let output = render_output_frame(
                        &painted,
                        &overlay_config,
                        &camera_plan,
                        &current_time,
                        frame_index,
                        total_frames,
                        social_preset,
                    );
                    let permit = next_save_permit(&save_sem).await;
                    save_handles.push(PendingSave {
                        index: frame_index,
                        handle: spawn_save(
                            Arc::clone(&output),
                            frame_path.clone(),
                            settings.quality,
                            permit,
                        ),
                    });
                    rendered_output = Some(output);
                }
                if streaming {
                    encode_source = Some(Arc::clone(&painted));
                }
            }
        }

        // ---- push the finished frame into the encoder --------------------
        if let Some(encoder) = stream_encoder.as_mut() {
            if encode_duplicate && encoder.frames_sent() > 0 {
                encoder
                    .duplicate_frame()
                    .await
                    .map_err(|e| PipelineError::Fatal(format!("streaming encode failed: {e}")))?;
            } else {
                // Reuse the PNG render when there is one: overlays, camera
                // motion and social framing are then applied once per frame.
                let output = match rendered_output {
                    Some(output) => Some(output),
                    None => encode_source.as_ref().map(|canvas| {
                        render_output_frame(
                            canvas,
                            &overlay_config,
                            &camera_plan,
                            &current_time,
                            frame_index,
                            total_frames,
                            social_preset,
                        )
                    }),
                };
                if let Some(output) = output {
                    // `push_frame_image` hands the encoder the shared frame, so
                    // no multi-megabyte copy is needed to stream it.
                    encoder
                        .push_frame_image(Arc::clone(&output))
                        .await
                        .map_err(|e| {
                            PipelineError::Fatal(format!("streaming encode failed: {e}"))
                        })?;
                    if (i % preview_stride == 0) || frame_index == total_frames {
                        events
                            .send(Event::PreviewFrame {
                                image: output,
                                index: frame_index,
                                total: total_frames,
                            })
                            .ok();
                    }
                }
            }
        }

        completed_frames += 1;
        events
            .send(Event::Progress {
                value: completed_frames,
                max: total_frames,
            })
            .ok();
        events
            .send(Event::Stats {
                bytes: crate::cache::bytes_downloaded(),
                cached: crate::cache::cache_hit_count(),
                network: crate::cache::network_fetch_count(),
            })
            .ok();
        frame_index += 1;
    }

    // ---- drain ------------------------------------------------------------
    for handle in prefetched {
        let _ = handle.await;
    }
    save_failures += drain_saves(&mut save_handles).await;

    // Closing stdin lets ffmpeg flush the container and exit. Errors are
    // surfaced after the partial file has been removed by `finish`.
    let video_path = if let Some(encoder) = stream_encoder.take() {
        if cancel.is_cancelled() {
            let _ = encoder.cancel().await;
            None
        } else {
            match encoder.finish().await {
                Ok(result) => Some(result.output_path),
                Err(e) => return Err(PipelineError::Fatal(e.to_string())),
            }
        }
    } else {
        None
    };

    if cancel.is_cancelled() {
        emit_log(&events, LogLevel::Warning, tf("process_cancelled", &[]));
        events
            .send(Event::Status(tf("process_cancelled", &[])))
            .ok();
        return Ok(PipelineResult {
            frames: frame_index - 1,
            cancelled: true,
            linked_frames,
            link_kind,
            video_path: None,
        });
    }
    if save_failures > 0 {
        return Err(PipelineError::Fatal(tf(
            "save_failed_error",
            &[&save_failures.to_string()],
        )));
    }

    if streaming {
        if let Some(path) = &video_path {
            emit_log(
                &events,
                LogLevel::Success,
                tf("timelapse_created", &[&path.to_string_lossy()]),
            );
            events.send(Event::Status(tf("ready", &[]))).ok();
        }
    } else {
        emit_log(&events, LogLevel::Success, tf("images_ready", &[]));
        events.send(Event::Status(tf("images_ready", &[]))).ok();
    }
    emit_log(
        &events,
        LogLevel::Success,
        tf("total_frames", &[&(frame_index - 1).to_string()]),
    );
    if linked_frames > 0 {
        emit_log(
            &events,
            LogLevel::Success,
            tf("frames_linked_summary", &[&linked_frames.to_string()]),
        );
        if let Some(kind) = link_kind {
            emit_log(
                &events,
                LogLevel::Info,
                tf("frames_link_kind", &[kind.as_str()]),
            );
        }
    }
    Ok(PipelineResult {
        frames: frame_index - 1,
        cancelled: false,
        linked_frames,
        link_kind,
        video_path,
    })
}

/// Downloads (with disk cache) every tile of one frame. Also used by the
/// GUI preview and the CLI `preview` command, which pass a `yyyyMMdd`
/// date string directly.
#[allow(clippy::too_many_arguments)] // flat params mirror the V1 tile downloader call site
pub async fn fetch_frame_tiles(
    client: &Client,
    settings: &Settings,
    tile_limiter: &Arc<AdaptiveLimiter>,
    storage_base: &str,
    canvas_id: &str,
    date_str: &str,
    ts: &str,
    coords: &[TileCoord],
    cancel: &CancellationToken,
) -> Vec<Option<Vec<u8>>> {
    let coords_len = coords.len();
    if coords_len == 0 {
        return Vec::new();
    }

    let cache_ts = crate::cache::normalize_ts(ts).to_string();

    // Cache pass first: hits and negative markers cost no network slot. One
    // read transaction resolves every key of the frame, which also keeps the
    // async workers off the blocking pool for more than a single hop.
    let keys: Arc<Vec<crate::cache::CacheKey>> = Arc::new(
        coords
            .iter()
            .map(|tc| {
                crate::cache::cache_key(storage_base, canvas_id, date_str, tc.xi, tc.yi, &cache_ts)
            })
            .collect(),
    );

    let cached: Vec<crate::cache::CacheStatus> = {
        let keys = Arc::clone(&keys);
        tokio::task::spawn_blocking(move || {
            // One read transaction per frame: the packed cache resolves a whole
            // frame's keys from a single B-tree snapshot instead of opening a
            // transaction per tile.
            let statuses = crate::cache::load_entries_batch(&keys);
            let hits = statuses
                .iter()
                .filter(|s| matches!(s, crate::cache::CacheStatus::Data(_)))
                .count() as u64;
            crate::cache::record_cache_hits(hits);
            statuses
        })
        .await
        .unwrap_or_else(|_| vec![crate::cache::CacheStatus::Missing; coords_len])
    };

    // Every tile is already resolved (present or definitively absent): no
    // network pass at all.
    if cached
        .iter()
        .all(|s| !matches!(s, crate::cache::CacheStatus::Missing))
    {
        return cached
            .into_iter()
            .map(|s| match s {
                crate::cache::CacheStatus::Data(data) => Some(data),
                _ => None,
            })
            .collect();
    }

    // Network pass on the *caller's* runtime so the shared client keeps its
    // pooled TCP/TLS/HTTP2 connections alive across frames. (The previous
    // version stood up a throwaway runtime per frame; dropping it tore the
    // pool down, so every frame paid fresh handshakes.)
    let ts_for_url = if cache_ts == "base" {
        ""
    } else {
        cache_ts.as_str()
    };
    // Frame-constant work hoisted out of the per-tile futures: the URL prefix
    // (which used to be re-formatted, date-sliced and all, for every tile) and
    // the settings snapshot (which used to be deep-cloned per tile).
    let prefix: Arc<str> = Arc::from(crate::tileurl::prefix(
        storage_base,
        canvas_id,
        date_str,
        ts_for_url,
    ));
    let underscore_layout = crate::tileurl::pixmap_layout(storage_base);
    let settings = Arc::new(settings.clone());

    let pending: Vec<usize> = cached
        .iter()
        .enumerate()
        .filter_map(|(i, s)| matches!(s, crate::cache::CacheStatus::Missing).then_some(i))
        .collect();
    // Continuous refill with a dynamically re-read limit. `buffered()` froze
    // the window at the value the limiter had when the frame started, so AIMD
    // growth could never apply to the frame that produced it. Large frames
    // (thousands of tiles) are exactly where that matters: after the first
    // frame or two the limiter has grown towards its ceiling, and every later
    // frame now starts at the higher value while still refilling one finished
    // request at a time.
    let mut fetched: Vec<(usize, net::GetOutcome)> = Vec::with_capacity(pending.len());
    let mut inflight = futures::stream::FuturesUnordered::new();
    let mut pending_iter = pending.into_iter();
    let mut exhausted = false;
    loop {
        let limit = tile_limiter.limit().max(1);
        while inflight.len() < limit && !exhausted {
            let Some(i) = pending_iter.next() else {
                exhausted = true;
                break;
            };
            let tc = coords[i];
            let client = client.clone();
            let settings = Arc::clone(&settings);
            let sem = Arc::clone(tile_limiter);
            let prefix = Arc::clone(&prefix);
            let cancel = cancel.clone();
            inflight.push(async move {
                let _permit = sem.acquire().await;
                if cancel.is_cancelled() {
                    return (i, net::GetOutcome::Failed { throttled: false });
                }
                let mut url = String::with_capacity(prefix.len() + 24);
                url.push_str(&prefix);
                write_tile_suffix(&mut url, tc.xi, tc.yi, underscore_layout);
                let outcome = net::get_bytes_with_retries_checked_outcome(
                    &client,
                    &settings,
                    &url,
                    "image/*, */*",
                    &cancel,
                    net::is_png,
                )
                .await;
                sem.report(match &outcome {
                    net::GetOutcome::Failed { throttled: true } => Outcome::Throttled,
                    net::GetOutcome::Failed { throttled: false } => Outcome::Failed,
                    net::GetOutcome::Data(_) | net::GetOutcome::Absent => Outcome::Ok,
                });
                (i, outcome)
            });
        }
        match inflight.next().await {
            Some(item) => fetched.push(item),
            None => break,
        }
    }

    // Persist the results off the async workers, in parallel: hundreds of
    // small writes must never stall the network futures of other frames.
    let mut out: Vec<Option<Vec<u8>>> = vec![None; coords_len];
    for (i, status) in cached.into_iter().enumerate() {
        if let crate::cache::CacheStatus::Data(data) = status {
            out[i] = Some(data);
        }
    }
    let mut writes: Vec<(crate::cache::CacheKey, Vec<u8>)> = Vec::new();
    let mut absent: Vec<crate::cache::CacheKey> = Vec::new();
    for (i, outcome) in fetched {
        match outcome {
            net::GetOutcome::Data(data) if !data.is_empty() => {
                writes.push((keys[i], data.clone()));
                out[i] = Some(data);
            }
            net::GetOutcome::Absent => absent.push(keys[i]),
            // A 200 with an empty body is not a definitive absence; leave it
            // uncached so a later run can retry it.
            net::GetOutcome::Data(_) | net::GetOutcome::Failed { .. } => {}
        }
    }

    if !writes.is_empty() || !absent.is_empty() {
        // Negative markers are tiny, but a missing day over a huge area would
        // create one entry per tile (tens of thousands for a full Earth frame).
        // Persist them only for reasonably sized frames; the network savings
        // still cover the common preview/typical-region cases.
        if absent.len() > 8192 {
            absent.clear();
        }
        let _ = tokio::task::spawn_blocking(move || {
            crate::cache::save_entries_batch(&writes);
            for (_, data) in &writes {
                crate::cache::record_network_fetch(data.len() as u64);
            }
            crate::cache::mark_absent_entries_batch(&absent);
        })
        .await;
    }
    out
}

/// One frame the cache warmer should download.
#[derive(Debug)]
enum WarmJob {
    Base(NaiveDate),
    Snapshot(NaiveDate, String),
}

/// How many frames the warmer may fetch at once.
///
/// The warmer draws from the same adaptive tile pool as the render loop, and
/// each `fetch_frame_tiles` call uses up to `limit` requests for its own tiles.
/// Dividing the pool by the frame size therefore fills the connection budget
/// without letting wide regions spawn a large number of concurrent frames.
fn warm_slots(limit: usize, tile_count: usize) -> usize {
    (limit.max(1) / tile_count.max(1)).max(1)
}

/// Queues the snapshot frames a run keeps for one settled day.
fn enqueue_snapshot_jobs(
    tx: &mpsc::UnboundedSender<WarmJob>,
    date: NaiveDate,
    snapshots: &[String],
    frame_skip: i32,
) {
    for (idx, ts) in snapshots.iter().enumerate() {
        if keep_snapshot(idx, frame_skip) {
            let _ = tx.send(WarmJob::Snapshot(date, ts.clone()));
        }
    }
}

/// Pre-downloads frame tiles while the `/history` range is still arriving.
///
/// Runs while the rest of the history range is still being fetched. History
/// lives on `settings.domain`, tiles on `settings.storage_domain`, so the two
/// streams do not compete for the same host's rate limit. Everything fetched
/// here lands in the shared disk cache — exactly what the pipeline reads a
/// moment later — so the history wait hides a real part of the download time.
///
/// Jobs are scheduled per frame: the caller queues every day base up front
/// and adds kept snapshots as each day settles. A day no longer waits for its
/// own history before its base downloads, snapshot frames of the same day are
/// no longer serialized, and the tile pool does not sit on an arbitrary
/// 8-day cap. Each job is one `fetch_frame_tiles` call, so the shared adaptive
/// limiter still caps the real network concurrency.
#[allow(clippy::too_many_arguments)]
async fn warm_history_days(
    client: Arc<Client>,
    settings: Settings,
    limiter: Arc<AdaptiveLimiter>,
    coords: Arc<Vec<TileCoord>>,
    canvas_id: Arc<String>,
    storage: Arc<String>,
    mut jobs: mpsc::UnboundedReceiver<WarmJob>,
    stop: CancellationToken,
    cancel: CancellationToken,
) {
    let slots = warm_slots(limiter.limit(), coords.len());
    let mut running: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    while let Some(job) = jobs.recv().await {
        if stop.is_cancelled() || cancel.is_cancelled() {
            break;
        }
        while running.len() >= slots {
            let _ = running.join_next().await;
        }
        running.spawn(warm_job(
            Arc::clone(&client),
            settings.clone(),
            Arc::clone(&limiter),
            Arc::clone(&coords),
            Arc::clone(&canvas_id),
            Arc::clone(&storage),
            job,
            cancel.clone(),
        ));
    }

    // In-flight frames finish (so their tiles still reach the cache); the
    // caller bounds this wait.
    while running.join_next().await.is_some() {}
}

/// Downloads the tiles of one base or snapshot frame into the cache.
#[allow(clippy::too_many_arguments)]
async fn warm_job(
    client: Arc<Client>,
    settings: Settings,
    limiter: Arc<AdaptiveLimiter>,
    coords: Arc<Vec<TileCoord>>,
    canvas_id: Arc<String>,
    storage: Arc<String>,
    job: WarmJob,
    cancel: CancellationToken,
) {
    let (date, ts) = match job {
        WarmJob::Base(date) => (date, String::new()),
        WarmJob::Snapshot(date, ts) => (date, ts),
    };
    let api_date = date.format("%Y%m%d").to_string();
    drop(
        fetch_frame_tiles(
            &client, &settings, &limiter, &storage, &canvas_id, &api_date, &ts, &coords, &cancel,
        )
        .await,
    );
}

/// Links `dst` to `src`, replacing a stale `dst` if one exists (from a
/// resumed or overwritten run). Returns the mechanism the filesystem gave us,
/// or `None` when `src` is gone and nothing could be created.
fn link_frame(src: &Path, dst: &Path, mode: LinkMode) -> Option<LinkKind> {
    link::link_or_copy(src, dst, mode).ok()
}

/// Appends the per-tile part of a tile URL (`{x}/{y}.png`, or `{x}_{y}.png` on
/// the PixMap chunk layout) to an already-built prefix buffer, without the
/// intermediate `String` that [`crate::tileurl::suffix`] would allocate.
fn write_tile_suffix(out: &mut String, tile_x: i64, tile_y: i64, underscore_layout: bool) {
    use std::fmt::Write as _;
    let sep = if underscore_layout { '_' } else { '/' };
    let _ = write!(out, "{tile_x}{sep}{tile_y}.png");
}

/// Saves `canvas` to `dst` the cheap way: if the predecessor frame file
/// (`t{prev_index}.png` next to `dst`) already holds these exact pixels, link
/// to or copy it instead of re-encoding a PNG. Falls back to a real encode
/// when there is no predecessor frame.
fn save_frame_linked_or_copy(
    canvas: &RgbaImage,
    prev_index: i64,
    dst: &Path,
    quality: i32,
    mode: LinkMode,
) -> Result<Option<LinkKind>, std::io::Error> {
    let prev_path = dst
        .parent()
        .map(|p| p.join(format!("t{prev_index}.png")))
        .filter(|p| p.exists());
    if let Some(prev) = prev_path.as_deref() {
        if let Ok(kind) = link::link_or_copy(prev, dst, mode) {
            return Ok(Some(kind));
        }
    }
    compose::save_png(canvas, dst, quality).map_err(std::io::Error::other)?;
    Ok(None)
}

/// Waits for every pending frame encode and returns how many failed.
///
/// Used at the end of a run (and after a cancellation). A `JoinError` (a panic
/// in the encode task) counts as a failure too — the old code silently ignored
/// it, so a frame could be missing while the run reported success.
async fn drain_saves(handles: &mut Vec<PendingSave>) -> usize {
    let mut failures = 0usize;
    for pending in handles.drain(..) {
        if !matches!(pending.handle.await, Ok(Ok(()))) {
            failures += 1;
        }
    }
    failures
}

/// Waits only for the pending encode that produces `index`.
///
/// Frame saves are queued in increasing frame order, so a link only needs its
/// own predecessor's file; waiting for every earlier save would re-introduce
/// the full-pool barrier this bounds.
async fn drain_saves_until(handles: &mut Vec<PendingSave>, index: i64) -> usize {
    let mut failures = 0usize;
    if let Some(pos) = handles.iter().position(|p| p.index == index) {
        let pending = handles.remove(pos);
        if !matches!(pending.handle.await, Ok(Ok(()))) {
            failures += 1;
        }
    }
    failures
}

/// Takes one permit from the bounded save pool.
///
/// Acquired *before* the save task is spawned so a slow disk throttles the
/// frame loop (backpressure) instead of the tasks queueing finished frames —
/// each a full frame buffer — in memory until the pool drains at the end.
async fn next_save_permit(sem: &Arc<Semaphore>) -> tokio::sync::OwnedSemaphorePermit {
    Arc::clone(sem)
        .acquire_owned()
        .await
        .expect("the save semaphore is never closed")
}

/// Queues a PNG encode on the blocking pool, holding `permit` until the file is
/// on disk so at most `max_concurrent_saves` frames are encoded (and kept
/// alive) at a time.
///
/// The image is shared, so `spawn_save` never copies the frame: the caller
/// keeps the running canvas and `Arc::make_mut` copies it only when a frame is
/// still being encoded.
fn spawn_save(
    img: Arc<RgbaImage>,
    path: PathBuf,
    quality: i32,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> tokio::task::JoinHandle<Result<(), String>> {
    tokio::task::spawn_blocking(move || {
        let _permit = permit; // released when the encode finishes
                              // Write through a temp file so an interrupted
                              // run never leaves a half-written frame that
                              // resume would adopt.
        let tmp = path.with_extension("png.tmp");
        match compose::save_png(&img, &tmp, quality) {
            Ok(()) => match std::fs::rename(&tmp, &path) {
                Ok(()) => Ok(()),
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    Err(format!("{}: {e}", path.display()))
                }
            },
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(format!("{}: {e}", path.display()))
            }
        }
    })
}

/// The numeric frame index in `t<N>.png`, if `name` is a frame file.
fn frame_number(name: &str) -> Option<i64> {
    if !(name.starts_with('t') && name.ends_with(".png")) {
        return None;
    }
    let digits = &name[1..name.len() - 4];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Warns when `--resume` is pointed at a folder produced with a different
/// canvas, region, start date or frame-skip setting. Returns `true` when a
/// mismatch was found; the caller then starts a fresh run instead of mixing
/// incompatible frames into one video.
///
/// The start date is essential: adopting `t1.png..` from an older run with a
/// different start would silently reuse frames from the wrong dates (and even
/// the wrong days), because frame numbers alone do not identify a date range.
fn warn_if_metadata_mismatch(
    out_dir: &Path,
    params: &PipelineParams,
    region: &Region,
    events: &mpsc::UnboundedSender<Event>,
) -> bool {
    let Ok(raw) = std::fs::read(out_dir.join("metadata.json")) else {
        return false;
    };
    let Ok(meta) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return false;
    };
    // Without a canvas id there is nothing authoritative to compare against
    // (e.g. a hand-written metadata.json); stay quiet rather than warn falsely.
    let Some(meta_canvas) = meta.get("canvasId").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let start_date = params.start.format("%Y-%m-%d").to_string();
    let same = meta_canvas == params.canvas_id.as_str()
        && meta
            .get("startDate")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|v| v == start_date.as_str())
        && meta
            .get("x")
            .and_then(serde_json::Value::as_i64)
            .is_none_or(|v| v == region.x)
        && meta
            .get("y")
            .and_then(serde_json::Value::as_i64)
            .is_none_or(|v| v == region.y)
        && meta
            .get("width")
            .and_then(serde_json::Value::as_i64)
            .is_none_or(|v| v == region.width)
        && meta
            .get("height")
            .and_then(serde_json::Value::as_i64)
            .is_none_or(|v| v == region.height)
        && meta
            .get("frameSkip")
            .and_then(serde_json::Value::as_i64)
            .is_none_or(|v| v == params.settings.frame_skip as i64);
    if !same {
        emit_log(
            events,
            LogLevel::Warning,
            tf("resume_metadata_mismatch", &[]),
        );
    }
    !same
}

/// Adopts the contiguous `t1.png..tN.png` prefix of a resumed run.
///
/// Returns the number of adopted frames and the last adopted image (the running
/// canvas), and removes any stale frames past the first gap. Only the last
/// frame is decoded: decoding thousands of unchanged frames just to throw them
/// away was the dominant cost of resuming a long run.
///
/// If the resume point is in the middle of a day, the prefix is trimmed so the
/// next frame is that day's base frame. Cumulative snapshot composition needs
/// the day base plus the alpha mask of the previous snapshot, and only the
/// pixels of the last frame are on disk. Redoing at most one day is cheap and
/// guarantees the rest of the video is not composed from a stale mask.
fn prepare_resume(
    out_dir: &Path,
    params: &PipelineParams,
    region: &Region,
    work: &[WorkItem],
    events: &mpsc::UnboundedSender<Event>,
) -> (usize, Option<Arc<RgbaImage>>) {
    let mismatch = warn_if_metadata_mismatch(out_dir, params, region, events);
    if mismatch {
        // The existing frames belong to a different selection. Discarding them
        // and rewriting the metadata is safer than mixing incompatible frame
        // sizes in the final video.
        clean_frames(out_dir);
        if let Err(e) = write_metadata(out_dir, params, region) {
            emit_log(events, LogLevel::Warning, e.to_string());
        }
        return (0, None);
    }

    let mut count = 0usize;
    while count < work.len() && out_dir.join(format!("t{}.png", count + 1)).exists() {
        count += 1;
    }

    // `count` is the number of existing frames, so `work[count]` is the next
    // frame to produce. If it is a snapshot, rewind to the last day base at or
    // before it; the adopted prefix then ends just before that base.
    if count > 0 && count < work.len() && !work[count].is_base {
        let mut rewrite_from = count;
        while rewrite_from > 0 && !work[rewrite_from].is_base {
            rewrite_from -= 1;
        }
        count = rewrite_from;
    }

    // Frames beyond the adopted prefix are leftovers from a longer previous
    // run; removing them keeps the numbering from colliding with what this run
    // writes next.
    if let Ok(read) = std::fs::read_dir(out_dir) {
        for entry in read.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let stale_frame = frame_number(&name).is_some_and(|num| num > count as i64);
            if stale_frame || name.ends_with(".png.tmp") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    let matches_region = |img: &image::DynamicImage| {
        img.width() as i64 == region.width && img.height() as i64 == region.height
    };
    while count > 0 {
        let last = out_dir.join(format!("t{count}.png"));
        match image::open(&last) {
            Ok(img) if matches_region(&img) => {
                return (count, Some(Arc::new(img.to_rgba8())));
            }
            Ok(_) => {
                emit_log(
                    events,
                    LogLevel::Warning,
                    format!("{}: unexpected frame size", last.display()),
                );
                // A frame from another selection cannot be used as the running
                // canvas; drop it and try the one before it so the prefix stays
                // contiguous and correctly sized.
                let _ = std::fs::remove_file(&last);
                count -= 1;
            }
            Err(e) => {
                emit_log(
                    events,
                    LogLevel::Warning,
                    format!("{}: {e}", last.display()),
                );
                // A corrupt tail frame cannot be adopted; start from the one
                // before it so the prefix stays contiguous.
                let _ = std::fs::remove_file(&last);
                count -= 1;
            }
        }
    }
    (0, None)
}

fn clean_frames(out_dir: &std::path::Path) {
    let Ok(read) = std::fs::read_dir(out_dir) else {
        return;
    };
    for entry in read.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if is_frame_file(&name) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// `t*.png` plus any leftover temp files.
fn is_frame_file(name: &str) -> bool {
    // `t.png` has no numeric part, so it is not a frame (and must not be
    // deleted by `clean_frames`).
    frame_number(name).is_some() || name.ends_with(".png.tmp")
}

fn write_metadata(
    out_dir: &std::path::Path,
    params: &PipelineParams,
    region: &Region,
) -> Result<(), PipelineError> {
    let meta = serde_json::json!({
        "canvasId": params.canvas_id,
        "x": region.x,
        "y": region.y,
        "width": region.width,
        "height": region.height,
        "startDate": params.start.format("%Y-%m-%d").to_string(),
        "endDate": params.end.format("%Y-%m-%d").to_string(),
        "frameSkip": params.settings.frame_skip,
    });
    std::fs::write(
        out_dir.join("metadata.json"),
        serde_json::to_vec_pretty(&meta).expect("metadata serialize"),
    )
    .map_err(|e| PipelineError::Fatal(format!("metadata: {e}")))
}

fn rgb_to_rgba(rgb: u32) -> image::Rgba<u8> {
    compose::rgba_from_rgb(rgb)
}

/// The wall-clock time a work item represents: the day base is midnight, a
/// snapshot uses its `HHMM` timestamp.
fn item_datetime(date: NaiveDate, ts: &str) -> chrono::NaiveDateTime {
    let time = if ts.is_empty() {
        chrono::NaiveTime::MIN
    } else {
        chrono::NaiveTime::parse_from_str(ts, "%H%M").unwrap_or(chrono::NaiveTime::MIN)
    };
    date.and_time(time)
}

/// Applies the studio pipeline (camera motion, then burned-in overlays) to a
/// composed frame. Without any enabled effect the shared frame is returned as
///-is, so the default path costs one `Arc` bump.
fn render_output_frame(
    frame: &Arc<RgbaImage>,
    overlay: &OverlayConfig,
    camera: &CameraPlan,
    current_time: &chrono::NaiveDateTime,
    frame_index: i64,
    total_frames: i64,
    social: crate::settings::SocialPreset,
) -> Arc<RgbaImage> {
    let camera_active = !camera.is_static();
    let overlay_active = !overlay.is_empty();
    let social_active = social != crate::settings::SocialPreset::Source;
    if !camera_active && !overlay_active && !social_active {
        return Arc::clone(frame);
    }
    let progress = if total_frames > 1 {
        ((frame_index - 1) as f32 / (total_frames - 1) as f32).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let mut img = if camera_active {
        apply_camera(frame, camera, progress)
    } else {
        frame.as_ref().clone()
    };
    if social_active {
        img = crate::studio::social::apply_social(&img, social);
    }
    if overlay_active {
        apply_frame_overlays(
            &mut img,
            overlay,
            current_time,
            frame_index.saturating_sub(1) as usize,
            total_frames.max(1) as usize,
        );
    }
    Arc::new(img)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A solid 256×256 chunk, the size every supported site serves.
    fn chunk(rgb: [u8; 3]) -> RgbaImage {
        RgbaImage::from_pixel(256, 256, image::Rgba([rgb[0], rgb[1], rgb[2], 255]))
    }

    /// The frame-constant `prefix` + `write_tile_suffix` fast path used by
    /// `fetch_frame_tiles` must produce byte-identical URLs to
    /// [`crate::tileurl::build`], on both storage layouts.
    #[test]
    fn hoisted_url_prefix_matches_build() {
        /// (storage, canvas, date, timestamp, tiles)
        type UrlCase = (
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            [(i64, i64); 2],
        );
        let cases: [UrlCase; 4] = [
            (
                "storage.gs-os.com",
                "0",
                "20000103",
                "",
                [(131, 124), (-1, 0)],
            ),
            (
                "storage.gs-os.com",
                "0",
                "20000103",
                "1230",
                [(0, 0), (7, 9)],
            ),
            (
                "pixmap.fun/history/chunk",
                "0",
                "20000103",
                "",
                [(157, 73), (-1, 0)],
            ),
            (
                "pixmap.fun/history/chunk",
                "17",
                "20000103",
                "1200",
                [(3, 4), (0, 0)],
            ),
        ];
        for (storage, canvas, date, ts, tiles) in cases {
            let prefix = crate::tileurl::prefix(storage, canvas, date, ts);
            let underscore = crate::tileurl::pixmap_layout(storage);
            for (x, y) in tiles {
                let mut fast = String::with_capacity(prefix.len() + 24);
                fast.push_str(&prefix);
                write_tile_suffix(&mut fast, x, y, underscore);
                assert_eq!(
                    fast,
                    crate::tileurl::build(storage, canvas, date, ts, x, y),
                    "storage={storage} ts={ts} tile={x},{y}"
                );
            }
        }
    }

    /// History snapshots are cumulative diffs against the day base. A frame is
    /// the day base plus the current snapshot; a pixel dropped from the current
    /// snapshot must be restored to the day base, while an identical snapshot
    /// must still report "unchanged" so it can be linked.
    #[test]
    fn running_canvas_keeps_day_base_and_restores_reverted_pixels() {
        let region = Region {
            x: 0,
            y: 0,
            width: 512,
            height: 256,
        };
        // 512×256 at (0,0) on a 4096px canvas covers exactly two chunks.
        let coords = compose::build_tile_coords(&region, 4096);
        assert_eq!(coords.len(), 2);
        let bg = image::Rgba([0, 0, 0, 255]);

        let mut canvas = CanvasState::default();

        // Day base: the first chunk has content, the second is still empty.
        canvas.install(
            Arc::new(compose::compose_base(
                &region,
                bg,
                &coords,
                &[Some(chunk([1, 2, 3])), None],
            )),
            coords.len(),
        );
        let base = canvas.peek().expect("the day base became the canvas");
        assert_eq!(base.get_pixel(0, 0), &image::Rgba([1, 2, 3, 255]));
        assert_eq!(base.get_pixel(300, 0), &bg);

        // Snapshot 1 is the cumulative diff: second chunk changed, first
        // unchanged (no tile for it).
        let (outcome, _) =
            canvas.paint_snapshot(&region, bg, &coords, &[None, Some(chunk([9, 9, 9]))]);
        assert!(outcome.changed);
        let frame = canvas.peek().unwrap();
        assert_eq!(frame.get_pixel(300, 0), &image::Rgba([9, 9, 9, 255]));
        // ...and the base's content is still there: frame = base + snapshot.
        assert_eq!(
            frame.get_pixel(0, 0),
            &image::Rgba([1, 2, 3, 255]),
            "the running canvas must keep the day base's pixels"
        );

        // Repeating the same cumulative snapshot is a bit-identical frame, so
        // it can be linked to its predecessor instead of re-encoded.
        let (same, _) =
            canvas.paint_snapshot(&region, bg, &coords, &[None, Some(chunk([9, 9, 9]))]);
        assert!(!same.changed, "an unchanged snapshot must be linkable");

        // The second chunk disappeared from the cumulative diff: it must
        // revert to the day base instead of ghosting forever.
        let (reverted, _) = canvas.paint_snapshot(&region, bg, &coords, &[None, None]);
        assert!(reverted.changed);
        let frame = canvas.peek().unwrap();
        assert_eq!(
            frame.get_pixel(300, 0),
            &bg,
            "a pixel removed from a cumulative snapshot must return to the day base"
        );
        assert_eq!(
            frame.get_pixel(0, 0),
            &image::Rgba([1, 2, 3, 255]),
            "the untouched part of the day base must remain"
        );

        // Applying the empty cumulative snapshot again changes nothing.
        let (same_again, _) = canvas.paint_snapshot(&region, bg, &coords, &[None, None]);
        assert!(!same_again.changed);
    }

    #[test]
    fn warm_slots_fill_the_tile_pool() {
        assert_eq!(warm_slots(64, 1), 64);
        assert_eq!(warm_slots(64, 4), 16);
        assert_eq!(warm_slots(64, 64), 1);
        assert_eq!(warm_slots(2, 64), 1);
        assert_eq!(warm_slots(0, 0), 1);
    }

    #[test]
    fn warm_jobs_queue_one_base_and_only_kept_snapshots() {
        let date = NaiveDate::from_ymd_opt(2026, 1, 2).unwrap();
        let snapshots: Vec<String> = ["a", "b", "c", "d"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(WarmJob::Base(date)).unwrap();
        enqueue_snapshot_jobs(&tx, date, &snapshots, 1);
        drop(tx);

        let mut jobs = Vec::new();
        while let Ok(job) = rx.try_recv() {
            jobs.push(job);
        }
        assert_eq!(jobs.len(), 3);
        assert!(matches!(jobs[0], WarmJob::Base(d) if d == date));
        assert!(matches!(&jobs[1], WarmJob::Snapshot(d, ts) if *d == date && ts == "a"));
        assert!(matches!(&jobs[2], WarmJob::Snapshot(d, ts) if *d == date && ts == "c"));
    }

    #[test]
    fn frame_skip_keeps_the_first_snapshot_and_every_nth_after_it() {
        // frame_skip = 0 keeps every snapshot.
        assert!(keep_snapshot(0, 0));
        assert!(keep_snapshot(7, 0));

        // frame_skip = 1 keeps indices 0, 2, 4, … (every second snapshot).
        assert!(keep_snapshot(0, 1));
        assert!(!keep_snapshot(1, 1));
        assert!(keep_snapshot(2, 1));

        // frame_skip = 2 keeps indices 0, 3, 6, … (every third snapshot).
        assert!(keep_snapshot(0, 2));
        assert!(!keep_snapshot(1, 2));
        assert!(!keep_snapshot(2, 2));
        assert!(keep_snapshot(3, 2));

        // Negative values clamp to 0.
        assert!(keep_snapshot(5, -10));
    }

    #[test]
    fn a_decoded_blank_base_is_not_a_missing_base() {
        assert!(base_tiles_missing(&[]));
        assert!(base_tiles_missing(&[None, None]));

        let blank = RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255]));
        assert!(
            !base_tiles_missing(&[None, Some(blank)]),
            "a decoded background-coloured tile is a real day base and must reset the canvas"
        );
    }

    #[test]
    fn resume_rejects_a_different_start_date() {
        let dir = tempfile::tempdir().unwrap();
        let region = Region {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        let params = PipelineParams {
            settings: Settings::default(),
            canvas_id: "c".into(),
            region,
            start: NaiveDate::from_ymd_opt(2000, 1, 2).unwrap(),
            end: NaiveDate::from_ymd_opt(2000, 1, 3).unwrap(),
            out_dir: dir.path().to_path_buf(),
            resume: true,
            canvas_size_px: Some(4096),
            bg_color: Some(0),
            day_timestamps: None,
        };
        let meta = serde_json::json!({
            "canvasId": "c",
            "x": 0,
            "y": 0,
            "width": 10,
            "height": 10,
            "startDate": "2000-01-01",
            "endDate": "2000-01-03",
            "frameSkip": 0,
        });
        std::fs::write(
            dir.path().join("metadata.json"),
            serde_json::to_vec(&meta).unwrap(),
        )
        .unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        assert!(
            warn_if_metadata_mismatch(dir.path(), &params, &region, &tx),
            "resuming with a different start date must be refused"
        );

        let matching = serde_json::json!({
            "canvasId": "c",
            "x": 0,
            "y": 0,
            "width": 10,
            "height": 10,
            "startDate": "2000-01-02",
            "endDate": "2000-01-03",
            "frameSkip": 0,
        });
        std::fs::write(
            dir.path().join("metadata.json"),
            serde_json::to_vec(&matching).unwrap(),
        )
        .unwrap();
        assert!(!warn_if_metadata_mismatch(
            dir.path(),
            &params,
            &region,
            &tx
        ));
    }

    #[test]
    fn resume_rewinds_to_the_unfinished_days_base() {
        let dir = tempfile::tempdir().unwrap();
        let region = Region {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        };
        let date = NaiveDate::from_ymd_opt(2000, 1, 1).unwrap();
        let params = PipelineParams {
            settings: Settings::default(),
            canvas_id: "c".into(),
            region,
            start: date,
            end: date + chrono::Duration::days(1),
            out_dir: dir.path().to_path_buf(),
            resume: true,
            canvas_size_px: Some(4096),
            bg_color: Some(0),
            day_timestamps: None,
        };
        let work = vec![
            WorkItem {
                date,
                ts: String::new(),
                is_base: true,
            },
            WorkItem {
                date,
                ts: "0100".into(),
                is_base: false,
            },
            WorkItem {
                date,
                ts: "0200".into(),
                is_base: false,
            },
            WorkItem {
                date,
                ts: String::new(),
                is_base: true,
            },
            WorkItem {
                date,
                ts: "0300".into(),
                is_base: false,
            },
        ];
        let img = RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255]));
        for n in 1..=4 {
            compose::save_png(&img, &dir.path().join(format!("t{n}.png")), -1).unwrap();
        }

        let (tx, _rx) = mpsc::unbounded_channel();
        let (count, last) = prepare_resume(dir.path(), &params, &region, &work, &tx);
        assert_eq!(
            count, 3,
            "the unfinished day must restart from its base (frame 4), not mid-day"
        );
        assert!(last.is_some());
        assert!(
            !dir.path().join("t4.png").exists(),
            "frames from the unfinished day must be removed so numbering stays aligned"
        );
        assert!(dir.path().join("t3.png").exists());
    }

    /// A corrupt API canvas size must not make the pipeline allocate an
    /// impossible frame grid (the old code pre-allocated it with
    /// `Vec::with_capacity` and aborted the process).
    #[tokio::test]
    async fn absurd_tile_grids_are_rejected_before_allocating() {
        let dir = tempfile::tempdir().unwrap();
        let params = PipelineParams {
            settings: Settings::default(),
            canvas_id: "0".into(),
            region: Region {
                x: 0,
                y: 0,
                width: 1_000_000,
                height: 1_000_000,
            },
            start: NaiveDate::from_ymd_opt(2000, 1, 1).unwrap(),
            end: NaiveDate::from_ymd_opt(2000, 1, 1).unwrap(),
            out_dir: dir.path().to_path_buf(),
            resume: false,
            canvas_size_px: Some(1_000_000_000),
            bg_color: Some(0),
            day_timestamps: Some(BTreeMap::new()),
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let err = run_pipeline(params, tx, CancellationToken::new())
            .await
            .expect_err("an impossible grid must be refused");
        match err {
            PipelineError::Fatal(msg) => assert!(
                msg.contains(&MAX_FRAME_TILES.to_string()),
                "unexpected message: {msg}"
            ),
            other => panic!("expected a fatal error, got {other:?}"),
        }
    }

    /// The restructured fetcher runs on the caller's runtime and must keep the
    /// tile order intact (a miss is `None`, never a shifted result) even when
    /// every request fails.
    #[tokio::test]
    async fn fetch_frame_tiles_preserves_order_on_failure() {
        // Fail fast: no retry backoff for unreachable hosts.
        let settings = Settings {
            max_retries: 0,
            ..Settings::default()
        };
        let client = net::make_client(&settings);
        let sem = Arc::new(AdaptiveLimiter::fixed(4));
        let cancel = CancellationToken::new();
        // A 600x300 region on a 4096px canvas covers 3x2 chunks.
        let region = Region {
            x: 0,
            y: 0,
            width: 600,
            height: 300,
        };
        let coords = compose::build_tile_coords(&region, 4096);
        assert_eq!(coords.len(), 6);

        let datas = fetch_frame_tiles(
            &client,
            &settings,
            &sem,
            "invalid.invalid/history",
            "1",
            "20000101",
            "",
            &coords,
            &cancel,
        )
        .await;
        assert_eq!(datas.len(), coords.len());

        // Cancelled fetches must not panic and must still line up with tiles.
        cancel.cancel();
        let cancelled = fetch_frame_tiles(
            &client,
            &settings,
            &sem,
            "invalid.invalid/history",
            "1",
            "20000101",
            "",
            &coords,
            &cancel,
        )
        .await;
        assert_eq!(cancelled.len(), coords.len());
        assert!(cancelled.iter().all(Option::is_none));
    }

    #[tokio::test]
    async fn fetch_frame_tiles_handles_empty_coords() {
        let settings = Settings::default();
        let client = net::make_client(&settings);
        let sem = Arc::new(AdaptiveLimiter::fixed(1));
        let datas = fetch_frame_tiles(
            &client,
            &settings,
            &sem,
            "invalid.invalid/history",
            "1",
            "20000101",
            "",
            &[],
            &CancellationToken::new(),
        )
        .await;
        assert!(datas.is_empty());
    }

    #[test]
    fn frame_file_detection() {
        assert!(is_frame_file("t1.png"));
        assert!(is_frame_file("t123.png"));
        assert!(is_frame_file("t9.png.tmp"));
        assert!(!is_frame_file("timelapse.mp4"));
        assert!(!is_frame_file("tX.png"));
        assert!(!is_frame_file("metadata.json"));
        assert!(!is_frame_file("tile.png"));
        // A file literally named "t.png" is not a frame (it has no number) and
        // must never be deleted as one.
        assert!(!is_frame_file("t.png"));
        assert!(is_frame_file("t0.png"));
    }

    #[test]
    fn metadata_written_with_v1_field_names() {
        let dir = tempfile::tempdir().unwrap();
        let params = PipelineParams {
            settings: Settings::default(),
            canvas_id: "abc".into(),
            region: Region {
                x: -10,
                y: 5,
                width: 20,
                height: 15,
            },
            start: NaiveDate::from_ymd_opt(2000, 1, 1).unwrap(),
            end: NaiveDate::from_ymd_opt(2000, 1, 2).unwrap(),
            out_dir: dir.path().to_path_buf(),
            resume: false,
            canvas_size_px: Some(65536),
            bg_color: Some(0x102030),
            day_timestamps: None,
        };
        write_metadata(dir.path(), &params, &params.region).unwrap();
        let raw = std::fs::read_to_string(dir.path().join("metadata.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["canvasId"], "abc");
        assert_eq!(v["x"], -10);
        assert_eq!(v["width"], 20);
        assert_eq!(v["startDate"], "2000-01-01");
        assert_eq!(v["frameSkip"], 0);
    }

    /// The unchanged-frame link helper: when the predecessor file already
    /// holds the same pixels, `t{N}.png` becomes a hardlink to `t{N-1}.png`
    /// (same inode, zero extra disk) instead of a re-encoded copy.
    #[tokio::test]
    async fn save_frame_linked_or_copy_hardlinks_identical_predecessor() {
        let dir = tempfile::tempdir().unwrap();
        let img = RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255]));
        let prev = dir.path().join("t1.png");
        let dst = dir.path().join("t2.png");
        compose::save_png(&img, &prev, -1).unwrap();

        // Stale target from a previous run must be replaced by the link.
        std::fs::write(&dst, b"stale").unwrap();
        let kind = save_frame_linked_or_copy(&img, 1, &dst, -1, LinkMode::Auto)
            .unwrap()
            .expect("predecessor exists, so it must be linked");
        assert_eq!(kind, LinkKind::HardLink);

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta_prev = std::fs::metadata(&prev).unwrap();
            let meta_dst = std::fs::metadata(&dst).unwrap();
            assert_eq!(
                meta_prev.ino(),
                meta_dst.ino(),
                "unchanged frame must share the predecessor's inode"
            );
            assert_eq!(meta_dst.nlink(), 2);
        }
        #[cfg(not(unix))]
        assert_eq!(std::fs::read(&dst).unwrap(), std::fs::read(&prev).unwrap());

        // Missing predecessor: falls back to a real PNG encode.
        let dst3 = dir.path().join("t3.png");
        assert!(
            save_frame_linked_or_copy(&img, 999, &dst3, -1, LinkMode::Auto)
                .unwrap()
                .is_none()
        );
        assert!(dst3.exists());

        // Explicit symlink mode produces a symlink, not a copy.
        let dst4 = dir.path().join("t4.png");
        assert_eq!(
            save_frame_linked_or_copy(&img, 1, &dst4, -1, LinkMode::Soft)
                .unwrap()
                .unwrap(),
            LinkKind::SymLink
        );
        assert_eq!(std::fs::read(&dst4).unwrap(), std::fs::read(&prev).unwrap());
    }

    #[tokio::test]
    async fn test_multi_day_base_frame_planning_and_linking() {
        let dir = tempfile::tempdir().unwrap();
        let d1 = NaiveDate::from_ymd_opt(2000, 9, 1).unwrap();
        let d2 = NaiveDate::from_ymd_opt(2000, 9, 2).unwrap();
        let d3 = NaiveDate::from_ymd_opt(2000, 9, 3).unwrap();

        let mut day_timestamps = std::collections::BTreeMap::new();
        // Day 1 has 1 snapshot
        day_timestamps.insert(d1, vec!["0100".to_string()]);
        // Day 2 has 0 snapshots
        day_timestamps.insert(d2, vec![]);
        // Day 3 has 2 snapshots
        day_timestamps.insert(d3, vec!["0200".to_string(), "0300".to_string()]);

        let params = PipelineParams {
            settings: Settings {
                domain: "pixelya.fun".into(),
                storage_domain: "storage.pixelya.fun/history".into(),
                max_retries: 0,
                // This test covers frame planning/linking, not encoding. The
                // PNG-sequence path keeps it independent of an installed ffmpeg.
                stream_video: false,
                ..Settings::default()
            },
            canvas_id: "0".into(),
            region: Region {
                x: 0,
                y: 0,
                width: 256,
                height: 256,
            },
            start: d1,
            end: d3,
            out_dir: dir.path().to_path_buf(),
            resume: false,
            canvas_size_px: Some(4096),
            bg_color: Some(0),
            day_timestamps: Some(day_timestamps),
        };

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let res = run_pipeline(params, tx, CancellationToken::new())
            .await
            .unwrap();

        // 3 days:
        // Day 1: base + 1 snap = 2 frames
        // Day 2: base + 0 snap = 1 frame
        // Day 3: base + 2 snaps = 3 frames
        // Total = 6 frames
        assert_eq!(res.frames, 6);
        assert!(!res.cancelled);
    }
}
