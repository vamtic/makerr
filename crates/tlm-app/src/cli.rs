//! TimelapseMaker headless CLI (the `timelapse` binary without arguments
//! starts the GUI instead — see `main.rs`).
//!
//! A superset of the V1 app's `--headless` mode: everything a template
//! carries can also be passed directly as flags, plus canvas listing,
//! single-day preview rendering and cache management subcommands.
//!
//! Exit codes: 0 success · 1 cancelled/failed · 2 invalid input.

use std::path::{Path, PathBuf};
use std::time::Instant;

use chrono::NaiveDate;
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;

use tlm_core::i18n::{set_language, LogLevel};
use tlm_core::pipeline::{self, Event, PipelineParams};
use tlm_core::settings::{
    HardwareAcceleration, OutputFormat, Settings, VideoCodec, MAX_TIMEOUT, MIN_TIMEOUT,
};
use tlm_core::{coord, history, link::LinkMode, net, template, video};

#[derive(Parser)]
#[command(
    name = "timelapse",
    version,
    about = "Turn pixel-art canvas history into timelapse videos (no subcommand opens the GUI)"
)]
struct Cli {
    /// Omitted for the GUI: any subcommand runs headless.
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to maker/settings.json (CLI commands only)
    #[arg(long, global = true)]
    settings: Option<PathBuf>,
}

#[derive(Subcommand)]
// One value per process: the argument enum is deliberately flat (clap reads
// much better that way) and its size is irrelevant at this call site.
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Download frames and encode a timelapse video
    Run {
        /// Template JSON (canvas_id, first_cord, second_cord, dates, output_folder)
        #[arg(long)]
        template: Option<PathBuf>,

        /// Canvas UUID/id (overrides template)
        #[arg(long)]
        canvas: Option<String>,

        /// First corner as x_y (overrides template)
        #[arg(long, allow_hyphen_values = true)]
        from: Option<String>,

        /// Second corner as x_y (overrides template)
        #[arg(long, allow_hyphen_values = true)]
        to: Option<String>,

        /// Start date yyyy-MM-dd, inclusive (overrides template)
        #[arg(long)]
        start: Option<String>,

        /// End date yyyy-MM-dd, inclusive (overrides template)
        #[arg(long)]
        end: Option<String>,

        /// Output folder (overrides template)
        #[arg(long)]
        out: Option<PathBuf>,

        /// Site domain override (e.g. pixmap.fun). Also selects the preset
        /// storage domain unless --storage is given.
        #[arg(long)]
        domain: Option<String>,

        /// Storage base override (e.g. pixmap.fun/history/chunk)
        #[arg(long)]
        storage: Option<String>,

        /// Output video fps
        #[arg(long)]
        fps: Option<i32>,

        /// Video codec [h264|h265|av1]
        #[arg(long)]
        codec: Option<String>,

        /// Container format [mp4|avi|mkv|mov|webm]
        #[arg(long)]
        format: Option<String>,

        /// Hardware acceleration [auto|none|nvenc|amf|qsv|vaapi]
        #[arg(long)]
        hw: Option<String>,

        /// Keep every Nth+1 snapshot (0 = all)
        #[arg(long)]
        frame_skip: Option<i32>,

        /// Parallel tile connections
        #[arg(long)]
        max_tiles: Option<i32>,

        /// Parallel history connections
        #[arg(long)]
        max_connections: Option<i32>,

        /// Per-request timeout in seconds
        #[arg(long)]
        timeout: Option<f64>,

        /// Retry attempts per request
        #[arg(long)]
        retries: Option<i32>,

        /// How unchanged frames are stored: auto (hardlink, then symlink,
        /// then copy) | hard (ln) | soft (ln -s) | copy
        #[arg(long)]
        link_mode: Option<String>,

        /// Resume an interrupted run (reuse existing t*.png frames; the
        /// unfinished day is re-rendered from its base)
        #[arg(long)]
        resume: bool,

        /// Download and save frames only, skip video encoding
        #[arg(long)]
        frames_only: bool,

        /// Keep the intermediate t*.png sequence while streaming the video
        /// (the default; accepted for backwards compatibility)
        #[arg(long)]
        save_frames: bool,

        /// Do not keep t*.png frames when streaming the video
        #[arg(long, conflicts_with_all = ["save_frames", "frames_only"])]
        no_save_frames: bool,

        /// Disable zero-disk streaming and write a PNG sequence first
        #[arg(long)]
        no_stream: bool,

        /// Burn an animated date/time clock into every frame
        #[arg(long)]
        timestamps: bool,

        /// Draw a progress line along the bottom of every frame
        #[arg(long)]
        progress_bar: bool,

        /// Custom watermark text burned into every frame
        #[arg(long)]
        watermark: Option<String>,

        /// Camera motion: none | zoom_out | zoom_in | pan_lr | pan_rl
        #[arg(long)]
        camera: Option<String>,

        /// Background soundtrack (mp3/wav/flac/ogg), looped and faded
        #[arg(long)]
        audio: Option<PathBuf>,

        /// Framing preset: source | vertical (9:16) | square (1:1) | wide (16:9)
        #[arg(long)]
        preset: Option<String>,

        /// Persist command-line overrides into the settings file
        #[arg(long)]
        save_settings: bool,

        /// Show what would be downloaded (canvas, days, frame count) and exit
        #[arg(long)]
        dry_run: bool,
    },
    /// List the canvases offered by a site
    ListCanvases {
        /// Site domain (defaults to the settings' domain)
        #[arg(long)]
        domain: Option<String>,
    },
    /// Render one day of a region to a PNG image
    Preview {
        /// Canvas id
        #[arg(long)]
        canvas: String,

        /// First corner x_y
        #[arg(long, allow_hyphen_values = true)]
        from: String,

        /// Second corner x_y
        #[arg(long, allow_hyphen_values = true)]
        to: String,

        /// Day to render, yyyy-MM-dd (defaults to today)
        #[arg(long)]
        day: Option<String>,

        /// Output PNG path (default: ./preview.png)
        #[arg(long, short)]
        output: Option<PathBuf>,

        /// Site domain override
        #[arg(long)]
        domain: Option<String>,

        /// Storage base override
        #[arg(long)]
        storage: Option<String>,
    },
    /// Replace identical frames in an existing output folder with links
    /// (hardlink like `ln`, symlink like `ln -s`, or copy)
    Relink {
        /// Folder holding t1.png, t2.png, …
        #[arg(long, short)]
        out: PathBuf,

        /// auto (hardlink, then symlink, then copy) | hard | soft | copy
        #[arg(long)]
        mode: Option<String>,
    },
    /// Print tile cache size and entry count
    CacheStats,
    /// Delete every cached tile
    ClearCache,
    /// Prune the tile cache by age and size
    PruneCache {
        /// Max age in days (0 = keep any age)
        #[arg(long, default_value_t = 7)]
        days: i32,

        /// Max total size in MiB (0 = unlimited)
        #[arg(long, default_value_t = 512)]
        max_mib: u64,
    },
}

/// Parses the command line and runs a subcommand.
///
/// Returns `None` when no subcommand was given (the caller then opens the
/// GUI), or the process exit code otherwise.
pub fn dispatch() -> Option<i32> {
    let cli = Cli::parse();
    let command = cli.command?;
    let explicit_settings = cli.settings.clone();
    let settings_path = explicit_settings
        .clone()
        .unwrap_or_else(tlm_core::settings::default_settings_path);
    let mut settings = if explicit_settings.is_some() {
        Settings::load(&settings_path)
    } else {
        Settings::load_or_import(&settings_path)
    };
    set_language(settings.language);

    let code = match command {
        Command::Relink { out, mode } => cmd_relink(&out, mode.as_deref()),
        Command::CacheStats => cmd_cache_stats(),
        Command::ClearCache => cmd_clear_cache(),
        Command::PruneCache { days, max_mib } => cmd_prune_cache(days, max_mib),
        Command::ListCanvases { domain } => cmd_list_canvases(&mut settings, domain),
        Command::Preview {
            canvas,
            from,
            to,
            day,
            output,
            domain,
            storage,
        } => cmd_preview(settings, canvas, from, to, day, output, domain, storage),
        Command::Run {
            template,
            canvas,
            from,
            to,
            start,
            end,
            out,
            domain,
            storage,
            fps,
            codec,
            format,
            hw,
            frame_skip,
            max_tiles,
            max_connections,
            timeout,
            retries,
            link_mode,
            resume,
            frames_only,
            save_frames,
            no_save_frames,
            no_stream,
            timestamps,
            progress_bar,
            watermark,
            camera,
            audio,
            preset,
            save_settings,
            dry_run,
        } => cmd_run(
            settings,
            settings_path,
            template,
            canvas,
            from,
            to,
            start,
            end,
            out,
            domain,
            storage,
            fps,
            codec,
            format,
            hw,
            frame_skip,
            max_tiles,
            max_connections,
            timeout,
            retries,
            link_mode,
            resume,
            frames_only,
            save_frames,
            no_save_frames,
            no_stream,
            timestamps,
            progress_bar,
            watermark,
            camera,
            audio,
            preset,
            save_settings,
            dry_run,
        ),
    };
    Some(code)
}

/// `relink`: turn byte-identical frames in an existing folder into links, so
/// a run produced before linking (or one where the filesystem silently copied)
/// gets the same disk savings as a fresh run.
fn cmd_relink(out: &Path, mode: Option<&str>) -> i32 {
    if !out.is_dir() {
        eprintln!("Not a folder: {}", out.display());
        return 2;
    }
    let mode = mode.map(LinkMode::parse).unwrap_or(LinkMode::Auto);
    match tlm_core::link::relink_identical_frames(out, mode) {
        Ok(report) => {
            println!(
                "Relinked {} of {} frame(s) [{}] — {:.1} MB now shared",
                report.linked,
                report.scanned,
                report.kind.map(|k| k.as_str()).unwrap_or("nothing to link"),
                report.freed_bytes as f64 / 1024.0 / 1024.0
            );
            0
        }
        Err(e) => {
            eprintln!("[error] relink failed: {e}");
            1
        }
    }
}

fn cmd_cache_stats() -> i32 {
    let (bytes, count) = tlm_core::cache::stats();
    println!(
        "Cache: {:.1} MB / {} tiles",
        bytes as f64 / 1024.0 / 1024.0,
        count
    );
    0
}

fn cmd_clear_cache() -> i32 {
    let removed = tlm_core::cache::clear();
    println!("Cleared {removed} tile cache entries");
    0
}

fn cmd_prune_cache(days: i32, max_mib: u64) -> i32 {
    let removed = tlm_core::cache::prune(days, max_mib * 1024 * 1024);
    println!("Pruned {removed} tile cache entries");
    0
}

fn cmd_list_canvases(settings: &mut Settings, domain: Option<String>) -> i32 {
    if let Some(d) = domain {
        settings.domain = d;
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let result: Result<Vec<history::CanvasInfo>, String> = rt.block_on(async {
        let client = net::make_client(settings);
        let cancel = CancellationToken::new();
        history::fetch_canvas_list(&client, settings, &cancel)
            .await
            .map_err(|e| e.to_string())
    });
    match result {
        Ok(canvases) => {
            println!("{:<14} {:<28} size(px)", "id", "name");
            for c in &canvases {
                println!("{:<14} {:<28} {}", c.id, c.name, c.size_px);
            }
            0
        }
        Err(e) => {
            // A failed fetch is a runtime failure, not invalid input.
            eprintln!("Failed to load canvas list: {e}");
            1
        }
    }
}

/// Preview failures split so invalid input keeps the documented exit code 2.
#[derive(Debug)]
enum PreviewError {
    Invalid(String),
    Failed(String),
}

impl From<String> for PreviewError {
    fn from(msg: String) -> Self {
        PreviewError::Failed(msg)
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_preview(
    mut settings: Settings,
    canvas: String,
    from: String,
    to: String,
    day: Option<String>,
    output: Option<PathBuf>,
    domain: Option<String>,
    storage: Option<String>,
) -> i32 {
    let domain_explicit = domain.is_some();
    let storage_explicit = storage.is_some();
    if let Some(d) = domain {
        settings.domain = d;
    }
    if let Some(s) = storage {
        settings.storage_domain = s;
    }
    if domain_explicit && !storage_explicit {
        if let Some(preset) = tlm_core::settings::preset_storage_for_domain(&settings.domain) {
            settings.storage_domain = preset.to_string();
        }
    }
    let region = match coord::region_from_corners(&from, &to) {
        Some(r) if r.width > 0 && r.height > 0 => r,
        _ => {
            eprintln!("Invalid coordinates: '{from}', '{to}' (expected x_y pairs)");
            return 2;
        }
    };
    let day = match day {
        Some(d) => match NaiveDate::parse_from_str(&d, "%Y-%m-%d") {
            Ok(v) => v,
            Err(_) => {
                eprintln!("Invalid date '{d}' (expected yyyy-MM-dd)");
                return 2;
            }
        },
        None => chrono::Local::now().date_naive(),
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let result: Result<PathBuf, PreviewError> = rt.block_on(async {
        let cancel = CancellationToken::new();
        let client = net::make_client(&settings);
        let canvases = history::fetch_canvas_list(&client, &settings, &cancel)
            .await
            .map_err(|e| e.to_string())?;
        let info = history::find_canvas(&canvases, &canvas)
            .ok_or_else(|| PreviewError::Invalid(format!("Canvas '{canvas}' not found")))?;
        if info.size_px <= 0 {
            return Err(PreviewError::Invalid("Invalid canvas size".to_string()));
        }
        let region = tlm_core::coord::clamp_to_canvas(region, info.size_px);
        if let Err(e) = tlm_core::coord::check_frame_size(&region) {
            return Err(PreviewError::Invalid(e.to_string()));
        }
        let tiles = tlm_core::compose::tile_count(&region, info.size_px);
        if tiles > tlm_core::pipeline::MAX_FRAME_TILES {
            return Err(PreviewError::Invalid(format!(
                "The selected area is too large ({tiles} tiles, {} allowed)",
                tlm_core::pipeline::MAX_FRAME_TILES
            )));
        }
        let coords = tlm_core::compose::build_tile_coords(&region, info.size_px);
        let date_str = day.format("%Y%m%d").to_string();
        eprintln!(
            "Previewing {} tiles of canvas {} on {}...",
            coords.len(),
            canvas,
            day.format("%Y-%m-%d")
        );

        // Fetch the day base tiles with the same bounded concurrency the
        // pipeline uses.
        let sem = std::sync::Arc::new(tlm_core::adaptive::AdaptiveLimiter::fixed(
            settings.max_concurrent_tiles.max(1) as usize,
        ));
        let datas: Vec<Option<Vec<u8>>> = tlm_core::pipeline::fetch_frame_tiles(
            &client,
            &settings,
            &sem,
            &settings.storage_domain,
            &canvas,
            &date_str,
            "",
            &coords,
            &cancel,
        )
        .await;
        let decoded = tlm_core::compose::decode_tiles_parallel(&datas);
        let bg = tlm_core::compose::rgba_from_rgb(info.bg_color.unwrap_or(0xffffff));
        let img = tlm_core::compose::compose_base(&region, bg, &coords, &decoded);
        let out_path = output.unwrap_or_else(|| PathBuf::from("preview.png"));
        tlm_core::compose::save_png(&img, &out_path, settings.quality)
            .map_err(|e| PreviewError::Failed(format!("save: {e}")))?;
        Ok(out_path)
    });
    match result {
        Ok(path) => {
            println!("Preview written: {}", path.display());
            0
        }
        Err(PreviewError::Invalid(e)) => {
            eprintln!("Preview failed: {e}");
            2
        }
        Err(PreviewError::Failed(e)) => {
            eprintln!("Preview failed: {e}");
            1
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_run(
    mut settings: Settings,
    settings_path: PathBuf,
    template_path: Option<PathBuf>,
    canvas: Option<String>,
    from: Option<String>,
    to: Option<String>,
    start: Option<String>,
    end: Option<String>,
    out: Option<PathBuf>,
    domain: Option<String>,
    storage: Option<String>,
    fps: Option<i32>,
    codec: Option<String>,
    format: Option<String>,
    hw: Option<String>,
    frame_skip: Option<i32>,
    max_tiles: Option<i32>,
    max_connections: Option<i32>,
    timeout: Option<f64>,
    retries: Option<i32>,
    link_mode: Option<String>,
    resume: bool,
    frames_only: bool,
    save_frames: bool,
    no_save_frames: bool,
    no_stream: bool,
    timestamps: bool,
    progress_bar: bool,
    watermark: Option<String>,
    camera: Option<String>,
    audio: Option<PathBuf>,
    preset: Option<String>,
    save_settings: bool,
    dry_run: bool,
) -> i32 {
    // ---- resolve job parameters (template merged with flag overrides) ----
    let mut canvas_id: Option<String> = canvas;
    let mut first_cord: Option<String> = from;
    let mut second_cord: Option<String> = to;
    let mut start_date: Option<NaiveDate> = None;
    let mut end_date: Option<NaiveDate> = None;
    let mut output_folder: Option<PathBuf> = out;
    let mut domain_explicit = domain.is_some();
    let mut storage_explicit = storage.is_some();
    let domain_flag_explicit = domain_explicit;
    let storage_flag_explicit = storage_explicit;

    if let Some(path) = &template_path {
        let tmpl = match template::Template::load(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("Cannot open template: {e}");
                return 2;
            }
        };
        if tmpl.canvas_id.is_empty() && canvas_id.is_none() {
            eprintln!("Template is missing 'canvas_id'");
            return 2;
        }
        if canvas_id.is_none() {
            canvas_id = Some(tmpl.canvas_id.clone());
        }
        if first_cord.is_none() {
            first_cord = Some(tmpl.first_cord.clone());
        }
        if second_cord.is_none() {
            second_cord = Some(tmpl.second_cord.clone());
        }
        if start.is_none() {
            start_date = tmpl.start().ok().or_else(|| {
                eprintln!("Invalid start_date '{}'", tmpl.start_date);
                None
            });
        }
        if end.is_none() {
            end_date = tmpl.end().ok().or_else(|| {
                eprintln!("Invalid end_date '{}'", tmpl.end_date);
                None
            });
        }
        if output_folder.is_none() {
            output_folder = Some(PathBuf::from(tmpl.output_folder.clone()));
        }
        if domain.is_none() {
            if let Some(d) = tmpl.domain {
                settings.domain = d;
                domain_explicit = true;
            }
        }
        if storage.is_none() {
            if let Some(s) = tmpl.storage_domain {
                settings.storage_domain = s;
                storage_explicit = true;
            }
        }
    }

    // Flag overrides applied last (start/end given as flags).
    if let Some(s) = &start {
        match NaiveDate::parse_from_str(s, "%Y-%m-%d") {
            Ok(d) => start_date = Some(d),
            Err(_) => {
                eprintln!("Invalid --start '{s}' (expected yyyy-MM-dd)");
                return 2;
            }
        }
    }
    if let Some(e) = &end {
        match NaiveDate::parse_from_str(e, "%Y-%m-%d") {
            Ok(d) => end_date = Some(d),
            Err(_) => {
                eprintln!("Invalid --end '{e}' (expected yyyy-MM-dd)");
                return 2;
            }
        }
    }
    if let Some(v) = domain {
        settings.domain = v;
        domain_explicit = true;
    }
    if let Some(v) = storage {
        settings.storage_domain = v;
        storage_explicit = true;
    }
    // A site domain alone identifies the matching storage layout. Without this
    // a `--domain pixmap.fun` run silently requested PixelPlanet storage URLs,
    // got 404s for every tile and produced a blank video with exit code 0.
    // A command-line --domain also wins over a template's storage: otherwise
    // overriding only the domain of an imported template would pair the new
    // site with the old site's storage layout.
    if !storage_flag_explicit && (domain_flag_explicit || (domain_explicit && !storage_explicit)) {
        if let Some(preset) = tlm_core::settings::preset_storage_for_domain(&settings.domain) {
            settings.storage_domain = preset.to_string();
        }
    }
    if let Some(v) = fps {
        settings.fps = v;
    }
    if let Some(v) = codec {
        settings.video_codec = VideoCodec::parse(&v);
    }
    if let Some(v) = format {
        settings.output_format = OutputFormat::parse(&v);
    }
    if let Some(v) = hw {
        settings.hardware_acceleration = HardwareAcceleration::parse(&v);
    }
    if let Some(v) = frame_skip {
        settings.frame_skip = v;
    }
    if let Some(v) = max_tiles {
        settings.max_concurrent_tiles = v;
    }
    if let Some(v) = max_connections {
        settings.max_connections = v;
    }
    if let Some(v) = timeout {
        settings.timeout = v.clamp(MIN_TIMEOUT, MAX_TIMEOUT);
    }
    if let Some(v) = retries {
        settings.max_retries = v;
    }
    if let Some(v) = &link_mode {
        settings.link_mode = LinkMode::parse(v);
    }
    if no_stream || frames_only || resume {
        // A PNG sequence is the resumable artefact; the two modes cannot be
        // combined meaningfully.
        settings.stream_video = false;
    }
    if save_frames || frames_only {
        settings.save_png_sequence = true;
    } else if no_save_frames {
        settings.save_png_sequence = false;
    }
    if timestamps {
        settings.show_timestamp = true;
    }
    if progress_bar {
        settings.show_progress_bar = true;
    }
    if let Some(text) = watermark {
        settings.watermark_text = text;
    }
    if let Some(motion) = camera {
        settings.camera_motion = tlm_core::settings::CameraMotion::parse(&motion);
    }
    if let Some(path) = audio {
        settings.audio_path = path.to_string_lossy().into_owned();
    }
    if let Some(p) = preset {
        settings.social_preset = tlm_core::settings::SocialPreset::parse(&p);
    }
    settings.clamp();

    let canvas_id = match canvas_id {
        Some(c) if !c.is_empty() => c,
        _ => {
            eprintln!("No canvas specified (use --canvas or --template)");
            return 2;
        }
    };
    let region = match (first_cord, second_cord) {
        (Some(a), Some(b)) => match coord::region_from_corners(&a, &b) {
            Some(r) if r.width > 0 && r.height > 0 => r,
            _ => {
                eprintln!("Invalid coordinates: '{a}', '{b}' (expected x_y pairs)");
                return 2;
            }
        },
        _ => {
            eprintln!("Coordinates missing (use --from/--to or --template)");
            return 2;
        }
    };
    let start = match start_date {
        Some(d) => d,
        None => {
            eprintln!("Start date missing (use --start or --template)");
            return 2;
        }
    };
    let end = match end_date {
        Some(d) => d,
        None => {
            eprintln!("End date missing (use --end or --template)");
            return 2;
        }
    };
    if end < start {
        eprintln!("End date is before start date");
        return 2;
    }
    let output_folder = match output_folder {
        Some(o) if !o.as_os_str().is_empty() => o,
        _ => {
            eprintln!("Output folder missing (use --out or --template)");
            return 2;
        }
    };

    // Persist effective settings only when the user explicitly asks. Ad-hoc
    // runs used to overwrite `maker/settings.json` with every command-line
    // override, so a one-off `--fps 60` quietly changed future GUI runs.
    if save_settings {
        if let Err(e) = settings.save(&settings_path) {
            eprintln!("[warning] could not save settings: {e}");
        }
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async move {
        let cancel = CancellationToken::new();
        tokio::spawn({
            let cancel = cancel.clone();
            async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    eprintln!("\nCancelling...");
                    cancel.cancel();
                }
            }
        });

        let params = PipelineParams {
            settings: settings.clone(),
            canvas_id: canvas_id.clone(),
            region,
            start,
            end,
            out_dir: output_folder.clone(),
            resume,
            canvas_size_px: None,
            bg_color: None,
            day_timestamps: None,
        };

        if dry_run {
            return dry_run_plan(&settings, &params, &cancel).await;
        }

        let started = Instant::now();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let drain_task = tokio::spawn(async move {
            drain_events(&mut rx).await;
        });

        let result = pipeline::run_pipeline(params, tx, cancel.clone()).await;
        let _ = drain_task.await;

        let result = match result {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[error] {e}");
                return 1;
            }
        };
        if result.cancelled {
            eprintln!("Timelapse failed or was cancelled");
            return 1;
        }

        if let Some(video) = result.video_path.clone() {
            let secs = started.elapsed().as_secs();
            println!(
                "Done: {} ({} frames, {}m{:02}s total)",
                video.display(),
                result.frames,
                secs / 60,
                secs % 60
            );
            return 0;
        }

        if frames_only {
            let secs = started.elapsed().as_secs();
            let linked = match (result.linked_frames, result.link_kind) {
                (0, _) => String::new(),
                (n, Some(kind)) => format!(" ({n} unchanged, {} via {})", n, kind.as_str()),
                (n, None) => format!(" ({n} unchanged)"),
            };
            println!(
                "Done: {} frames{} in {}m{:02}s -> {}",
                result.frames,
                linked,
                secs / 60,
                secs % 60,
                output_folder.display()
            );
            return 0;
        }

        // ---- video encoding ------------------------------------------------
        let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel();
        let encode_drain_task = tokio::spawn(async move {
            drain_encode_events(&mut erx).await;
        });
        let compile = video::compile(&settings, &output_folder, etx, &cancel).await;
        let _ = encode_drain_task.await;

        match compile {
            Ok(r) => {
                if r.cancelled {
                    eprintln!("Timelapse failed or was cancelled");
                    return 1;
                }
                let secs = started.elapsed().as_secs();
                let (bytes, cached, network) = (
                    tlm_core::cache::bytes_downloaded(),
                    tlm_core::cache::cache_hit_count(),
                    tlm_core::cache::network_fetch_count(),
                );
                eprintln!(
                    "[stats] {} cached · {} network · {:.1} MB downloaded",
                    cached,
                    network,
                    bytes as f64 / 1024.0 / 1024.0
                );
                println!(
                    "Done: {} ({} frames, {}m{:02}s total)",
                    r.output_path.display(),
                    result.frames,
                    secs / 60,
                    secs % 60
                );
                0
            }
            Err(e) => {
                eprintln!("[error] {e}");
                1
            }
        }
    })
}

/// Prints the would-be work list without downloading tiles.
async fn dry_run_plan(
    settings: &Settings,
    params: &PipelineParams,
    cancel: &CancellationToken,
) -> i32 {
    let client = net::make_client(settings);
    let canvases = match history::fetch_canvas_list(&client, settings, cancel).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[error] {e}");
            return 1;
        }
    };
    let info = match history::find_canvas(&canvases, &params.canvas_id) {
        Some(i) => i,
        None => {
            eprintln!("Canvas '{}' not found", params.canvas_id);
            return 2;
        }
    };

    let region = tlm_core::coord::clamp_to_canvas(params.region, info.size_px);
    if let Err(e) = tlm_core::coord::check_frame_size(&region) {
        eprintln!("[plan] invalid area: {e}");
        return 2;
    }
    let tile_total = tlm_core::compose::tile_count(&region, info.size_px);
    let mut dates = Vec::new();
    let mut cur = params.start;
    while cur <= params.end {
        dates.push(cur);
        cur += chrono::Duration::days(1);
    }

    eprintln!(
        "[plan] domain={} storage={}",
        settings.domain, settings.storage_domain
    );
    eprintln!("[plan] canvas {} ({} px)", info.name, info.size_px);
    eprintln!(
        "[plan] region {}x{} at ({},{}), {} tiles per frame",
        region.width, region.height, region.x, region.y, tile_total
    );

    let timestamps = history::fetch_range_timestamps(
        &client,
        settings,
        &dates,
        &params.canvas_id,
        cancel,
        |done, total| {
            if total > 1 {
                eprintln!("[plan] timestamps {done}/{total} days");
            }
        },
    )
    .await;
    for date in &timestamps.failed_days {
        eprintln!(
            "[plan] warning: no timestamps for {} (history request failed)",
            date.format("%Y-%m-%d")
        );
    }
    let mut total_frames = 0i64;
    for date in &dates {
        let ts_count = timestamps.get(date).map_or(0, |v| v.len());
        // Same selection as the pipeline: the first snapshot and every
        // (frame_skip + 1)-th one after it.
        let snapshots = (0..ts_count)
            .filter(|i| pipeline::keep_snapshot(*i, settings.frame_skip))
            .count() as i64;
        let day_frames = snapshots + 1;
        total_frames += day_frames;
        println!(
            "{}: {} snapshots -> {} frame(s)",
            date.format("%Y-%m-%d"),
            ts_count,
            day_frames
        );
    }
    println!("Total: ~{total_frames} frames, {tile_total} tiles");
    if total_frames == 0 {
        return 1;
    }
    0
}

async fn drain_events(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) {
    while let Some(ev) = rx.recv().await {
        match ev {
            Event::Log(level, msg) => {
                if matches!(level, LogLevel::Warning | LogLevel::Error) {
                    eprintln!("[{}] {}", log_tag(level), msg);
                }
            }
            Event::Status(msg) => println!("[status] {msg}"),
            Event::Progress { value, max } => {
                if max > 0 {
                    println!("[progress] {value}/{max}");
                }
            }
            Event::Stats { .. } => {}
            Event::PreviewFrame { .. } => {}
        }
    }
}

async fn drain_encode_events(rx: &mut tokio::sync::mpsc::UnboundedReceiver<video::EncodeEvent>) {
    while let Some(ev) = rx.recv().await {
        match ev {
            video::EncodeEvent::Log(level, msg) => {
                if matches!(level, LogLevel::Warning | LogLevel::Error) {
                    eprintln!("[{}] {}", log_tag(level), msg);
                }
            }
            video::EncodeEvent::Status(msg) => println!("[status] {msg}"),
            video::EncodeEvent::Progress { frame, total } => {
                if total > 0 {
                    println!("[progress] {frame}/{total}");
                }
            }
        }
    }
}

fn log_tag(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Info => "info",
        LogLevel::Success => "ok",
        LogLevel::Warning => "warn",
        LogLevel::Error => "error",
        LogLevel::Processing => "info",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_negative_corners_for_run_without_equals() {
        let cli = Cli::try_parse_from([
            "timelapse",
            "run",
            "--canvas",
            "17",
            "--from",
            "-128_-128",
            "--to",
            "-1_-1",
            "--start",
            "2000-01-01",
            "--end",
            "2000-01-01",
            "--out",
            "out",
        ])
        .expect("negative corner values must be accepted");

        let Some(Command::Run { from, to, .. }) = cli.command else {
            panic!("expected the run command");
        };
        assert_eq!(from.as_deref(), Some("-128_-128"));
        assert_eq!(to.as_deref(), Some("-1_-1"));
    }

    #[test]
    fn parses_studio_and_streaming_flags() {
        let cli = Cli::try_parse_from([
            "timelapse",
            "run",
            "--canvas",
            "17",
            "--from",
            "0_0",
            "--to",
            "255_255",
            "--start",
            "2000-01-01",
            "--end",
            "2000-01-01",
            "--out",
            "out",
            "--no-stream",
            "--save-frames",
            "--timestamps",
            "--progress-bar",
            "--watermark",
            "MY FACTION",
            "--camera",
            "zoom_out",
            "--preset",
            "vertical",
            "--audio",
            "song.mp3",
        ])
        .expect("studio flags parse");

        let Some(Command::Run {
            no_stream,
            save_frames,
            timestamps,
            progress_bar,
            watermark,
            camera,
            preset,
            audio,
            ..
        }) = cli.command
        else {
            panic!("expected the run command");
        };
        assert!(no_stream && save_frames && timestamps && progress_bar);
        assert_eq!(watermark.as_deref(), Some("MY FACTION"));
        assert_eq!(camera.as_deref(), Some("zoom_out"));
        assert_eq!(preset.as_deref(), Some("vertical"));
        assert_eq!(audio.unwrap().to_string_lossy(), "song.mp3");
    }

    #[test]
    fn no_save_frames_opts_out_of_retained_pngs() {
        let base = [
            "timelapse",
            "run",
            "--canvas",
            "17",
            "--from",
            "0_0",
            "--to",
            "255_255",
            "--start",
            "2000-01-01",
            "--end",
            "2000-01-01",
            "--out",
            "out",
        ];
        let cli = Cli::try_parse_from(base.into_iter().chain(["--no-save-frames"])).expect("parse");
        let Some(Command::Run {
            no_save_frames,
            save_frames,
            ..
        }) = cli.command
        else {
            panic!("expected the run command");
        };
        assert!(no_save_frames && !save_frames);

        let conflict = Cli::try_parse_from(
            base.into_iter()
                .chain(["--no-save-frames", "--save-frames"]),
        );
        assert!(conflict.is_err(), "the two flags must conflict");
    }

    #[test]
    fn parses_negative_corners_for_preview_without_equals() {
        let cli = Cli::try_parse_from([
            "timelapse",
            "preview",
            "--canvas",
            "17",
            "--from",
            "-256_-768",
            "--to",
            "-1_-513",
        ])
        .expect("negative preview corners must be accepted");

        let Some(Command::Preview { from, to, .. }) = cli.command else {
            panic!("expected the preview command");
        };
        assert_eq!(from.as_str(), "-256_-768");
        assert_eq!(to.as_str(), "-1_-513");
    }
}
