//! ffmpeg video compilation.
//!
//! Codec selection, quality flags and the pad filter match the V1 app so
//! both produce comparable output. Progress is parsed from ffmpeg's
//! `-progress pipe:1` stream and reported as `Event`s.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::i18n::{tf, LogLevel};
use crate::settings::{HardwareAcceleration, OutputFormat, Settings, VideoCodec};

/// Progress events for encoding.
#[derive(Debug, Clone)]
pub enum EncodeEvent {
    Log(LogLevel, String),
    Status(String),
    Progress { frame: i64, total: i64 },
}

/// Result of a compile run.
#[derive(Debug)]
pub struct CompileResult {
    pub output_path: PathBuf,
    pub cancelled: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum CompileError {
    #[error("{0}")]
    Fatal(String),
    #[error("ffmpeg failed: {0}")]
    FFmpeg(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Locates ffmpeg: next to the executable, then the usual install dirs,
/// then `PATH`.
pub fn find_ffmpeg() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let name = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    if let Some(dir) = exe_dir {
        let exe_candidate = dir.join(name);
        if exe_candidate.is_file() {
            return Some(exe_candidate);
        }
    }
    if let Some(found) = which("ffmpeg") {
        return Some(found);
    }
    let candidates = [
        PathBuf::from("/usr/bin/ffmpeg"),
        PathBuf::from("/usr/local/bin/ffmpeg"),
        PathBuf::from("/opt/homebrew/bin/ffmpeg"),
        PathBuf::from("/opt/local/bin/ffmpeg"),
    ];
    for c in &candidates {
        if c.is_file() {
            return Some(c.clone());
        }
    }
    None
}

/// Minimal PATH lookup.
///
/// On Windows the executable is `ffmpeg.exe` while callers pass `"ffmpeg"`, so
/// the `.exe` spelling is tried too — without it a perfectly installed ffmpeg
/// on `PATH` was never found (only the copy next to the app was).
pub fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let names = path_program_names(program);
    for dir in std::env::split_paths(&path) {
        for name in &names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// The file names to look for in each `PATH` entry: the program as given, plus
/// its `.exe` spelling on Windows.
fn path_program_names(program: &str) -> Vec<String> {
    let mut names = vec![program.to_string()];
    if cfg!(windows) && !program.to_ascii_lowercase().ends_with(".exe") {
        names.push(format!("{program}.exe"));
    }
    names
}

/// First `/dev/dri/renderD*` node with a positive number, or `None`.
#[cfg(target_os = "linux")]
pub fn detect_vaapi_device() -> Option<PathBuf> {
    let mut best: Option<(i32, PathBuf)> = None;
    if let Ok(read) = std::fs::read_dir("/dev/dri") {
        for entry in read.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(num) = name.strip_prefix("renderD") {
                if let Ok(n) = num.parse::<i32>() {
                    if n > 0 && best.as_ref().is_none_or(|(b, _)| n < *b) {
                        best = Some((n, entry.path()));
                    }
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

#[cfg(not(target_os = "linux"))]
pub fn detect_vaapi_device() -> Option<PathBuf> {
    None
}

/// Best-effort hardware encoder detection (Linux + Windows), mirroring the
/// V1 implementation. The result is cached for the process lifetime.
pub fn detect_hardware_encoder() -> &'static str {
    static CACHED: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    CACHED.get_or_init(detect_hardware_encoder_uncached)
}

fn detect_hardware_encoder_uncached() -> &'static str {
    if run_probe("nvidia-smi", &[], 1000) {
        return "nvenc";
    }

    #[cfg(target_os = "linux")]
    {
        if run_probe("vainfo", &[], 1000) {
            // vainfo succeeding on a machine with an AMD GPU plus a render
            // node is the same heuristic the V1 app used.
            let out = probe_output("vainfo", &[], 1000);
            let amd = out
                .as_deref()
                .map(|o| {
                    o.to_ascii_uppercase().contains("AMD")
                        || o.to_ascii_uppercase().contains("RADEON")
                })
                .unwrap_or(false);
            if amd && detect_vaapi_device().is_some() {
                return "vaapi";
            }
        }
        if detect_vaapi_device().is_some() {
            return "vaapi";
        }
    }

    #[cfg(target_os = "windows")]
    {
        // dxdiag into a temp file: AMD -> amf, Intel -> qsv.
        if let Some(hint) = dxdiag_encoder_hint(&std::env::temp_dir()) {
            return hint;
        }
    }

    "none"
}

/// The Windows dxdiag probe: AMD GPUs map to AMF, Intel ones to QSV.
///
/// Kept platform-independent on purpose. The body used to live inside a
/// `#[cfg(target_os = "windows")]` block and called a `PathBuf::pipe` method
/// that does not exist, so the workspace did not compile on windows-latest
/// while every Linux/macOS build was fine. Compiling it everywhere (it only
/// touches portable std APIs) makes that class of bug impossible; on other
/// platforms the `dxdiag` binary simply does not exist and the probe returns
/// `None`.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn dxdiag_encoder_hint(dir: &Path) -> Option<&'static str> {
    let _ = std::fs::create_dir_all(dir);
    let out_file = dir.join(format!("dxdiag_tlm_{}.txt", std::process::id()));
    let _ = std::process::Command::new("dxdiag")
        .args(["/t"])
        .arg(&out_file)
        .status();
    let text = std::fs::read_to_string(&out_file).ok();
    let _ = std::fs::remove_file(&out_file);

    encoder_hint_from_dxdiag(&text?)
}

/// Maps a dxdiag report to a hardware encoder family: AMD/Radeon → AMF,
/// Intel → QSV. Split from the probe so the mapping is unit-tested on every
/// platform.
fn encoder_hint_from_dxdiag(report: &str) -> Option<&'static str> {
    let upper = report.to_ascii_uppercase();
    if upper.contains("AMD") || upper.contains("RADEON") {
        return Some("amf");
    }
    if upper.contains("INTEL") {
        return Some("qsv");
    }
    None
}

/// Runs a probe program and reports whether it exited successfully.
/// A background thread + channel implements the 1s timeout the V1 app used
/// (std has no timeout on `status()`), so a hung nvidia-smi can't stall us.
fn run_probe(program: &str, args: &[&str], timeout_ms: u64) -> bool {
    let program = program.to_string();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let ok = std::process::Command::new(&program)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let _ = tx.send(ok);
    });
    rx.recv_timeout(std::time::Duration::from_millis(timeout_ms))
        .unwrap_or(false)
}

fn probe_output(program: &str, args: &[&str], timeout_ms: u64) -> Option<String> {
    let program = program.to_string();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = std::process::Command::new(&program)
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string());
        let _ = tx.send(out);
    });
    rx.recv_timeout(std::time::Duration::from_millis(timeout_ms))
        .unwrap_or(None)
}

/// The ffmpeg encoder name and quality flags for the current settings,
/// including the hardware-encoder fallback logic.
pub fn select_encoder(settings: &Settings) -> EncoderPlan {
    let format = settings.output_format;
    let vcodec = settings.video_codec;

    let mut codec: String = match format {
        OutputFormat::Avi => "mpeg4".into(),
        OutputFormat::Webm => "libvpx-vp9".into(),
        _ => match vcodec {
            VideoCodec::H265 => "libx265".into(),
            VideoCodec::Av1 => "libsvtav1".into(),
            VideoCodec::H264 => "libx264".into(),
        },
    };

    let mut pix_fmt: String = "yuv420p".into();

    let mut vf = "pad=ceil(iw/2)*2:ceil(ih/2)*2:(ow-iw)/2:(oh-ih)/2".to_string();
    let mut hw_init: Vec<String> = Vec::new();

    if !matches!(format, OutputFormat::Avi | OutputFormat::Webm) && vcodec != VideoCodec::Av1 {
        let family = if vcodec == VideoCodec::H265 {
            "hevc"
        } else {
            "h264"
        };
        let hw_suffix = match settings.hardware_acceleration {
            HardwareAcceleration::Auto => detect_hardware_encoder().to_string(),
            HardwareAcceleration::None => String::new(),
            other => other.as_setting_str().to_string(),
        };

        let mut hw_suffix = hw_suffix;
        if hw_suffix == "vaapi" && detect_vaapi_device().is_none() {
            hw_suffix.clear();
        }
        if !hw_suffix.is_empty() && hw_suffix != "none" {
            codec = format!("{family}_{hw_suffix}");
            if codec == "hevc_vaapi" || codec == "h264_vaapi" {
                pix_fmt = "nv12".into();
                if let Some(dev) = detect_vaapi_device() {
                    hw_init.push("-vaapi_device".into());
                    hw_init.push(dev.to_string_lossy().into_owned());
                    vf.push_str(",format=nv12,hwupload");
                } else {
                    codec = if codec == "hevc_vaapi" {
                        "libx265".into()
                    } else {
                        "libx264".into()
                    };
                    pix_fmt = "yuv420p".into();
                }
            }
        }
    }

    let quality: Vec<String> = if codec.starts_with("libx264") || codec.starts_with("libx265") {
        vec![
            "-crf".into(),
            "18".into(),
            "-preset".into(),
            "ultrafast".into(),
        ]
    } else if codec == "libsvtav1" {
        vec!["-preset".into(), "8".into(), "-crf".into(), "30".into()]
    } else if codec == "libvpx-vp9" {
        vec!["-crf".into(), "32".into(), "-b:v".into(), "0".into()]
    } else if codec == "mpeg4" {
        vec!["-q:v".into(), "3".into()]
    } else if codec.ends_with("_nvenc") {
        vec!["-cq".into(), "18".into(), "-preset".into(), "p1".into()]
    } else if codec.ends_with("_amf") {
        vec![
            "-qp_i".into(),
            "18".into(),
            "-qp_p".into(),
            "18".into(),
            "-quality".into(),
            "quality".into(),
        ]
    } else if codec.ends_with("_qsv") {
        vec![
            "-global_quality".into(),
            "18".into(),
            "-preset".into(),
            "veryfast".into(),
        ]
    } else if codec.ends_with("_vaapi") {
        vec!["-global_quality".into(), "18".into()]
    } else {
        Vec::new()
    };

    EncoderPlan {
        codec,
        pix_fmt,
        vf,
        hw_init,
        quality,
    }
}

/// A resolved encoder configuration (testable without running ffmpeg).
#[derive(Debug, Clone, PartialEq)]
pub struct EncoderPlan {
    pub codec: String,
    pub pix_fmt: String,
    pub vf: String,
    pub hw_init: Vec<String>,
    pub quality: Vec<String>,
}

impl EncoderPlan {
    /// True when the selected encoder depends on a hardware backend that may
    /// be missing/broken at runtime. Such a plan is worth one software retry.
    pub fn is_hardware(&self) -> bool {
        ["_nvenc", "_amf", "_qsv", "_vaapi"]
            .iter()
            .any(|suffix| self.codec.ends_with(suffix))
    }
}

/// Plans the encoder for a concrete frame size.
///
/// Hardware encoders are picked by *probe* of the machine, not of the frame
/// size, and GPUs have hard resolution limits (a Turing GTX 1650 caps H.264
/// at 4096×4096). ffmpeg then fails on the zero-th frame with the misleading
/// "No capable devices found", after the pipeline already spent minutes
/// downloading tiles. So a hardware plan is verified once per (codec, size)
/// and silently replaced by the software encoder when the GPU cannot encode
/// that resolution.
pub fn plan_for_frame(settings: &Settings, width: u32, height: u32) -> EncoderPlan {
    let plan = select_encoder(settings);
    if !plan.is_hardware() || hardware_handles_size(&plan, width, height) {
        return plan;
    }
    let mut software = settings.clone();
    software.hardware_acceleration = HardwareAcceleration::None;
    select_encoder(&software)
}

/// Cached `ffmpeg` probe: can `plan.codec` open a session for this size?
fn hardware_handles_size(plan: &EncoderPlan, width: u32, height: u32) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    type SizeCache = Mutex<HashMap<(String, u32, u32), bool>>;

    static CACHE: OnceLock<SizeCache> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (plan.codec.clone(), width, height);
    if let Some(hit) = cache.lock().ok().and_then(|c| c.get(&key).copied()) {
        return hit;
    }

    let ok = run_size_probe(plan, width, height);
    if let Ok(mut guard) = cache.lock() {
        guard.insert(key, ok);
    }
    ok
}

fn run_size_probe(plan: &EncoderPlan, width: u32, height: u32) -> bool {
    let Some(ffmpeg) = find_ffmpeg() else {
        return false;
    };
    let mut cmd = std::process::Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    cmd.args(&plan.hw_init);
    cmd.args([
        "-f",
        "lavfi",
        "-i",
        &format!("color=black:s={width}x{height}:d=0.1"),
    ]);
    if plan.pix_fmt == "nv12" {
        cmd.args(["-vf", "format=nv12,hwupload"]);
    }
    cmd.args(["-frames:v", "1", "-c:v", &plan.codec]);
    cmd.args(&plan.quality);
    cmd.args(["-f", "null", "-"]);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    // A broken backend usually fails within a second; cap the hang case so a
    // stuck driver cannot freeze the render start.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Builds the complete ffmpeg argument vector. `frame_count` enables the
/// strict gap check; see [`collect_frames`].
pub fn build_ffmpeg_args(plan: &EncoderPlan, settings: &Settings, out_path: &Path) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-y".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostats".into(),
        "-progress".into(),
        "pipe:1".into(),
        "-start_number".into(),
        "1".into(),
    ];
    args.extend(plan.hw_init.iter().cloned());
    args.extend([
        "-framerate".into(),
        settings.fps.to_string(),
        "-i".into(),
        "t%d.png".into(),
        "-vf".into(),
        plan.vf.clone(),
        "-c:v".into(),
        plan.codec.clone(),
        "-pix_fmt".into(),
        plan.pix_fmt.clone(),
    ]);
    args.extend(plan.quality.iter().cloned());
    args.extend(["-threads".into(), "0".into()]);
    if matches!(
        settings.output_format,
        OutputFormat::Mp4 | OutputFormat::Mov
    ) {
        args.extend(["-movflags".into(), "+faststart".into()]);
    }
    args.push(out_path.to_string_lossy().into_owned());
    args
}

/// Scans `out_dir` for `t*.png` frames and validates the numbering.
///
/// Returns the sorted paths plus the max frame number. Missing numbers are
/// an error (the V1 app refused to render gap-y frame sets because ffmpeg's
/// `t%d.png` input would silently stop at the first hole).
pub fn collect_frames(out_dir: &Path) -> Result<(Vec<PathBuf>, i64), CompileError> {
    let mut max_num = 0i64;
    let Ok(read) = std::fs::read_dir(out_dir) else {
        return Err(CompileError::Fatal(tf("no_frames_found", &[])));
    };
    let mut names: Vec<(i64, PathBuf)> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with('t') && name.ends_with(".png")) {
            continue;
        }
        let Ok(num) = name[1..name.len() - 4].parse::<i64>() else {
            continue;
        };
        if num > 0 {
            names.push((num, entry.path()));
            max_num = max_num.max(num);
        }
    }
    if names.is_empty() {
        return Err(CompileError::Fatal(tf("no_valid_frames", &[])));
    }
    names.sort_by_key(|(n, _)| *n);
    // One linear pass over the sorted numbers instead of `names.iter().any()`
    // inside the 1..=max loop (which was O(frames²): 50 000 frames meant 2.5
    // billion comparisons, i.e. seconds of pointless work before every encode).
    // Only the first few names are kept for the message; the count stays exact.
    let mut missing: Vec<String> = Vec::new();
    let mut missing_count = 0usize;
    let mut last_missing = 0i64;
    let mut expected = 1i64;
    for (n, _) in &names {
        while expected < *n {
            missing_count += 1;
            last_missing = expected;
            if missing.len() < 8 {
                missing.push(format!("t{expected}.png"));
            }
            expected += 1;
        }
        if *n >= expected {
            expected = *n + 1;
        }
    }
    if missing_count > 0 {
        let summary = if missing_count <= 8 {
            missing.join(", ")
        } else {
            format!(
                "{} ... t{last_missing}.png ({missing_count} frames)",
                missing[0]
            )
        };
        return Err(CompileError::Fatal(tf("missing_frames_error", &[&summary])));
    }
    let paths = names.into_iter().map(|(_, p)| p).collect();
    Ok((paths, max_num))
}

/// Runs ffmpeg over the frames in `out_dir`, streaming progress.
pub async fn compile(
    settings: &Settings,
    out_dir: &Path,
    events: tokio::sync::mpsc::UnboundedSender<EncodeEvent>,
    cancel: &CancellationToken,
) -> Result<CompileResult, CompileError> {
    events
        .send(EncodeEvent::Log(
            LogLevel::Processing,
            tf("compiling_timelapse", &[]),
        ))
        .ok();
    events
        .send(EncodeEvent::Status(tf("status_compiling_video", &[])))
        .ok();

    let ffmpeg = find_ffmpeg().ok_or_else(|| CompileError::Fatal(tf("ffmpeg_not_found", &[])))?;

    let (_, max_num) = collect_frames(out_dir)?;
    events
        .send(EncodeEvent::Progress {
            frame: 0,
            total: max_num,
        })
        .ok();

    let format = settings.output_format;
    let abs_out_dir = std::fs::canonicalize(out_dir).unwrap_or_else(|_| out_dir.to_path_buf());
    let out_path = abs_out_dir.join(format!("timelapse.{}", format.extension()));

    let mut plan = select_encoder(settings);
    let mut hardware_fallback_done = false;
    loop {
        let args = build_ffmpeg_args(&plan, settings, &out_path);

        let mut child = tokio::process::Command::new(&ffmpeg)
            .args(&args)
            .current_dir(out_dir)
            .env("APPIMAGE_EXTRACT_AND_RUN", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| CompileError::Fatal(tf("ffmpeg_start_error", &[])))?;

        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        // Progress lines come on stdout; errors accumulate on stderr.
        let stderr_task = tokio::spawn(async move {
            let mut collected = String::new();
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if collected.len() < 8192 {
                    collected.push_str(&line);
                    collected.push('\n');
                }
            }
            collected
        });

        let mut cancelled = false;

        let read_progress = {
            let events = events.clone();
            async move {
                let mut last_reported = 0i64;
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Some(frame) = line.strip_prefix("frame=") {
                        if let Ok(f) = frame.trim().parse::<i64>() {
                            if f > last_reported {
                                last_reported = f;
                                events
                                    .send(EncodeEvent::Progress {
                                        frame: f.min(max_num),
                                        total: max_num,
                                    })
                                    .ok();
                            }
                        }
                    }
                }
            }
        };
        tokio::pin!(read_progress);

        tokio::select! {
            _ = &mut read_progress => {}
            _ = cancel.cancelled() => {
                cancelled = true;
            }
        }

        if cancelled {
            // Politely terminate, then force.
            if let Some(pid) = child.id() {
                #[cfg(unix)]
                {
                    let _ = std::process::Command::new("kill")
                        .args(["-TERM", &pid.to_string()])
                        .status();
                }
                #[cfg(not(unix))]
                {
                    let _ = child.start_kill();
                }
            }

            // Give ffmpeg a moment to exit after TERM, then kill.
            match tokio::time::timeout(std::time::Duration::from_millis(3000), child.wait()).await {
                Ok(_) => {}
                Err(_) => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                }
            }
            let _ = stderr_task.await;
            events
                .send(EncodeEvent::Log(
                    LogLevel::Warning,
                    tf("process_cancelled", &[]),
                ))
                .ok();
            return Ok(CompileResult {
                output_path: out_path,
                cancelled: true,
            });
        }

        // Normal completion: wait for exit and gather stderr.
        let status = child.wait().await?;
        let stderr_text = stderr_task.await.unwrap_or_default();

        if status.success() {
            events
                .send(EncodeEvent::Progress {
                    frame: max_num,
                    total: max_num,
                })
                .ok();
            events
                .send(EncodeEvent::Log(
                    LogLevel::Success,
                    tf("timelapse_created", &[&out_path.to_string_lossy()]),
                ))
                .ok();
            events.send(EncodeEvent::Status(tf("ready", &[]))).ok();
            return Ok(CompileResult {
                output_path: out_path,
                cancelled: false,
            });
        }

        // Hardware encoders are chosen by *probe*, not by proof: the GPU can be
        // present while the chosen backend cannot open a session (driver
        // mismatch, no render node, a container without `/dev/dri`). Retry once
        // with software encoding instead of failing the whole render.
        if !hardware_fallback_done && plan.is_hardware() {
            hardware_fallback_done = true;
            events
                .send(EncodeEvent::Log(
                    LogLevel::Warning,
                    tf("hw_encoder_fallback", &[&plan.codec]),
                ))
                .ok();
            // Remove the partial file so the retry starts cleanly.
            let _ = std::fs::remove_file(&out_path);
            let mut fallback = settings.clone();
            fallback.hardware_acceleration = HardwareAcceleration::None;
            plan = select_encoder(&fallback);
            continue;
        }

        return Err(CompileError::FFmpeg(tf(
            "ffmpeg_error",
            &[tail_chars(stderr_text.trim(), 4000)],
        )));
    }
}

/// The last `max` bytes of `s`, moved forward to a UTF-8 boundary.
///
/// `&s[s.len() - max..]` panics when the cut lands inside a multi-byte
/// character — exactly what happens when ffmpeg's stderr ends with a localized
/// (e.g. Hungarian) error message.
fn tail_chars(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        // Deterministic: software encoding, so host GPU detection (NVENC/
        // VAAPI present or not) cannot change the expectations.
        Settings {
            hardware_acceleration: HardwareAcceleration::None,
            ..Default::default()
        }
    }

    #[test]
    fn encoder_selection_defaults_to_x264() {
        let plan = select_encoder(&settings());
        assert_eq!(plan.codec, "libx264");
        assert_eq!(plan.pix_fmt, "yuv420p");
        assert_eq!(plan.quality, vec!["-crf", "18", "-preset", "ultrafast"]);
    }

    #[test]
    fn encoder_selection_formats() {
        let mut s = settings();
        s.output_format = OutputFormat::Avi;
        assert_eq!(select_encoder(&s).codec, "mpeg4");
        s.output_format = OutputFormat::Webm;
        let plan = select_encoder(&s);
        assert_eq!(plan.codec, "libvpx-vp9");
        assert_eq!(plan.quality, vec!["-crf", "32", "-b:v", "0"]);

        s.output_format = OutputFormat::Mp4;
        s.video_codec = VideoCodec::H265;
        assert_eq!(select_encoder(&s).codec, "libx265");
        s.video_codec = VideoCodec::Av1;
        assert_eq!(select_encoder(&s).codec, "libsvtav1");
    }

    #[test]
    fn hardware_encoder_forced_selection() {
        let mut s = settings();
        s.hardware_acceleration = HardwareAcceleration::Nvenc;
        let plan = select_encoder(&s);
        assert_eq!(plan.codec, "h264_nvenc");
        assert_eq!(plan.quality, vec!["-cq", "18", "-preset", "p1"]);

        s.video_codec = VideoCodec::H265;
        assert_eq!(select_encoder(&s).codec, "hevc_nvenc");

        s.hardware_acceleration = HardwareAcceleration::None;
        assert_eq!(select_encoder(&s).codec, "libx265");
    }

    /// A Windows install has `ffmpeg.exe` on `PATH` while callers ask for
    /// `"ffmpeg"`; both spellings must be tried, and a name that already has
    /// the suffix must never be doubled.
    #[test]
    fn path_lookup_tries_the_windows_exe_suffix() {
        let plain = if cfg!(windows) {
            vec!["ffmpeg".to_string(), "ffmpeg.exe".to_string()]
        } else {
            vec!["ffmpeg".to_string()]
        };
        assert_eq!(path_program_names("ffmpeg"), plain);
        assert_eq!(
            path_program_names("ffmpeg.exe"),
            vec!["ffmpeg.exe".to_string()]
        );

        // The lookup itself works against a real directory on every platform.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tlm-probe"), b"x").unwrap();
        let saved = std::env::var_os("PATH");
        // Safe: this test binary is the only user of PATH in the test process
        // and the value is restored below.
        std::env::set_var("PATH", dir.path());
        let found = which("tlm-probe");
        match saved {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
        assert_eq!(
            found.as_deref(),
            Some(dir.path().join("tlm-probe").as_path())
        );
        assert_eq!(which("tlm-definitely-missing"), None);
    }

    /// The Windows dxdiag mapping used to be unreachable from any test (it was
    /// inside a `#[cfg(windows)]` block, which is how a non-existent method
    /// survived there); it is now plain portable code.
    #[test]
    fn dxdiag_reports_map_to_hardware_families() {
        assert_eq!(
            encoder_hint_from_dxdiag("Card name: AMD Radeon RX 6800"),
            Some("amf")
        );
        assert_eq!(
            encoder_hint_from_dxdiag("Processor: Intel(R) Core(TM) i7"),
            Some("qsv")
        );
        assert_eq!(encoder_hint_from_dxdiag("Card name: NVIDIA GeForce"), None);
        assert_eq!(encoder_hint_from_dxdiag(""), None);
    }

    /// On a machine without dxdiag the probe simply reports nothing.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn dxdiag_probe_is_silent_off_windows() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(dxdiag_encoder_hint(dir.path()), None);
        // The temporary report file is always cleaned up.
        assert!(!dir.path().join("dxdiag_tlm.txt").exists());
    }

    #[test]
    fn vaapi_without_device_falls_back() {
        let mut s = settings();
        s.hardware_acceleration = HardwareAcceleration::Vaapi;
        let plan = select_encoder(&s);
        // In CI/containers there is no render node; the V1 app fell back to
        // the software encoder in that case and so must we.
        if detect_vaapi_device().is_none() {
            assert_eq!(plan.codec, "libx264");
        } else {
            assert_eq!(plan.codec, "h264_vaapi");
            assert_eq!(plan.pix_fmt, "nv12");
            assert!(plan.vf.ends_with(",format=nv12,hwupload"));
        }
    }

    #[test]
    fn arg_vector_shape() {
        let s = settings();
        let plan = select_encoder(&s);
        let out = Path::new("/tmp/x/timelapse.mp4");
        let args = build_ffmpeg_args(&plan, &s, out);
        let idx = |name: &str| args.iter().position(|a| a == name).unwrap();
        assert_eq!(args[0], "-y");
        assert_eq!(args[idx("-framerate") + 1], "30");
        assert_eq!(args[idx("-i") + 1], "t%d.png");
        assert_eq!(args[idx("-c:v") + 1], "libx264");
        assert!(args.iter().any(|a| a == "+faststart"));
        assert_eq!(*args.last().unwrap(), "/tmp/x/timelapse.mp4");

        // mkv has no movflags.
        let mut s = settings();
        s.output_format = OutputFormat::Mkv;
        let args = build_ffmpeg_args(&plan, &s, Path::new("/tmp/x/timelapse.mkv"));
        assert!(!args.iter().any(|a| a == "+faststart"));
    }

    #[test]
    fn hardware_plan_detection_and_utf8_safe_tail() {
        let mut s = settings();
        s.hardware_acceleration = HardwareAcceleration::Nvenc;
        assert!(select_encoder(&s).is_hardware());
        assert!(!select_encoder(&settings()).is_hardware());

        // Cutting a multi-byte character must not panic.
        let text = "hiba: érvénytelen kodek";
        let tail = tail_chars(text, 8);
        assert!(text.ends_with(tail));
        assert!(tail.len() <= 8);
        assert_eq!(tail_chars("short", 100), "short");
    }

    #[test]
    fn frame_collection_validates_gaps() {
        let dir = tempfile::tempdir().unwrap();
        let p = |n: &str| dir.path().join(n);
        std::fs::write(p("t1.png"), b"x").unwrap();
        std::fs::write(p("t2.png"), b"x").unwrap();
        std::fs::write(p("t4.png"), b"x").unwrap(); // gap at 3

        assert!(collect_frames(dir.path()).is_err());

        std::fs::write(p("t3.png"), b"x").unwrap();
        let (paths, max) = collect_frames(dir.path()).unwrap();
        assert_eq!(max, 4);
        assert_eq!(paths.len(), 4);

        // Non-frame files are ignored.
        std::fs::write(p("metadata.json"), b"{}").unwrap();
        std::fs::write(p("timelapse.mp4"), b"x").unwrap();
        let (paths, _) = collect_frames(dir.path()).unwrap();
        assert_eq!(paths.len(), 4);

        let empty = tempfile::tempdir().unwrap();
        assert!(collect_frames(empty.path()).is_err());
    }
}
