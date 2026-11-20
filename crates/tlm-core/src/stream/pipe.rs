//! In-memory FFmpeg streaming (zero-disk rendering).
//!
//! Instead of encoding thousands of intermediate PNGs and then asking ffmpeg to
//! decode them again, the compositor writes raw RGBA frames straight into
//! ffmpeg's stdin (`-f rawvideo -pix_fmt rgba -i pipe:0`). The only file that
//! hits the disk is the final video.

use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin};

use crate::settings::{OutputFormat, Settings};
use crate::video::{find_ffmpeg, plan_for_frame, CompileError, CompileResult, EncoderPlan};

/// Everything needed to start a streaming encoder.
#[derive(Debug, Clone)]
pub struct PipeEncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub output_path: PathBuf,
    pub settings: Settings,
    /// Optional soundtrack; it is looped to the video length and faded.
    pub audio_track: Option<PathBuf>,
    /// Number of frames that will be pushed (used for the audio fade-out and
    /// progress reporting).
    pub total_frames: i64,
}

impl PipeEncoderConfig {
    fn frame_bytes(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }

    fn duration_seconds(&self) -> f64 {
        let fps = self.fps.max(1) as f64;
        (self.total_frames.max(0) as f64 / fps).max(0.0)
    }
}

/// Retained frame buffer for [`AsyncStreamEncoder::duplicate_frame`].
///
/// Rendered frames are shared (`Arc`) with the pipeline and the GUI preview, so
/// keeping the handle avoids copying several MiB per frame into the encoder.
enum FrameBuf {
    Owned(Vec<u8>),
    Shared(std::sync::Arc<image::RgbaImage>),
}

impl FrameBuf {
    fn bytes(&self) -> &[u8] {
        match self {
            FrameBuf::Owned(data) => data,
            FrameBuf::Shared(image) => image.as_raw(),
        }
    }
}

/// A live ffmpeg child process that accepts raw RGBA frames on stdin.
pub struct AsyncStreamEncoder {
    child: Child,
    stdin: Option<ChildStdin>,
    frame_bytes: usize,
    /// The most recently pushed frame, kept so unchanged frames can be
    /// duplicated without allocating a new buffer in the pipeline.
    last_frame: Option<FrameBuf>,
    stdout_task: tokio::task::JoinHandle<()>,
    stderr_task: tokio::task::JoinHandle<String>,
    /// Cached ffmpeg stderr, collected when the encoder dies mid-stream so the
    /// real reason (not just "Broken pipe") reaches the caller.
    stderr_tail: Option<String>,
    output_path: PathBuf,
    frames_sent: i64,
}

impl AsyncStreamEncoder {
    /// Starts ffmpeg with a piped stdin. Fails fast when ffmpeg is missing.
    pub fn spawn(config: PipeEncoderConfig) -> Result<Self, CompileError> {
        let ffmpeg = find_ffmpeg()
            .ok_or_else(|| CompileError::Fatal(crate::i18n::tf("ffmpeg_not_found", &[])))?;
        let plan = plan_for_frame(&config.settings, config.width, config.height);
        let args = build_stream_args(&config, &plan);
        let frame_bytes = config.frame_bytes();
        let output_path = config.output_path.clone();

        let mut cmd = tokio::process::Command::new(&ffmpeg);
        cmd.args(&args)
            .env("APPIMAGE_EXTRACT_AND_RUN", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        crate::sys::hide_console_window_async(&mut cmd);
        let mut child = cmd
            .spawn()
            .map_err(|_| CompileError::Fatal(crate::i18n::tf("ffmpeg_start_error", &[])))?;

        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        // ffmpeg's `-progress` writer must always be drained or it blocks the
        // encoder once the pipe buffer fills up.
        let stdout_task = tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            while let Ok(Some(_)) = lines.next_line().await {}
        });

        let stderr_task = tokio::spawn(async move {
            let mut collected = String::new();
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if collected.len() < 8192 {
                    collected.push_str(&line);
                    collected.push('\n');
                }
            }
            collected
        });

        Ok(Self {
            child,
            stdin: Some(stdin),
            frame_bytes,
            last_frame: None,
            stdout_task,
            stderr_task,
            stderr_tail: None,
            output_path,
            frames_sent: 0,
        })
    }

    pub fn frame_bytes(&self) -> usize {
        self.frame_bytes
    }

    pub fn frames_sent(&self) -> i64 {
        self.frames_sent
    }

    pub fn output_path(&self) -> &std::path::Path {
        &self.output_path
    }

    /// Pushes one raw RGBA frame into ffmpeg's stdin.
    ///
    /// The buffer is not retained; use [`Self::push_frame_owned`] when the
    /// frame may be needed again for [`Self::duplicate_frame`].
    pub async fn push_frame(&mut self, rgba_data: &[u8]) -> Result<(), CompileError> {
        self.write_frame(rgba_data, false).await
    }

    /// Pushes a frame and keeps its buffer for later duplication (unchanged
    /// frames), avoiding a copy on the hot path.
    pub async fn push_frame_owned(&mut self, rgba_data: Vec<u8>) -> Result<(), CompileError> {
        self.write_frame(&rgba_data, false).await?;
        self.last_frame = Some(FrameBuf::Owned(rgba_data));
        Ok(())
    }

    /// Pushes a shared frame without copying it into the encoder.
    ///
    /// The pipeline renders each frame into an `Arc` that it also hands to the
    /// PNG writer and the preview channel, so the encoder only needs to keep
    /// the handle around for [`Self::duplicate_frame`].
    pub async fn push_frame_image(
        &mut self,
        frame: std::sync::Arc<image::RgbaImage>,
    ) -> Result<(), CompileError> {
        self.write_frame(frame.as_raw(), false).await?;
        self.last_frame = Some(FrameBuf::Shared(frame));
        Ok(())
    }

    /// Sends the previous frame again (an unchanged snapshot).
    pub async fn duplicate_frame(&mut self) -> Result<(), CompileError> {
        let Some(frame) = self.last_frame.take() else {
            return Err(CompileError::Fatal(
                "duplicate_frame called before any frame was pushed".into(),
            ));
        };
        // Move the buffer out and back: `write_frame` needs `&mut self`, but
        // the frame data stays in the same allocation (no 8–33 MiB copy per
        // unchanged frame).
        let result = self.write_frame(frame.bytes(), true).await;
        self.last_frame = Some(frame);
        result
    }

    async fn write_frame(&mut self, data: &[u8], repeat: bool) -> Result<(), CompileError> {
        if data.len() != self.frame_bytes {
            return Err(CompileError::Fatal(format!(
                "frame size mismatch: expected {} bytes, got {}",
                self.frame_bytes,
                data.len()
            )));
        }
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| CompileError::Fatal("ffmpeg stdin is closed".into()))?;
        if let Err(err) = stdin.write_all(data).await {
            return Err(self.encoder_failure(err).await);
        }
        if !repeat {
            self.frames_sent += 1;
        }
        Ok(())
    }

    /// Kills the dying encoder and returns its stderr, so a failed write can
    /// report the actual ffmpeg complaint instead of a bare `Broken pipe`.
    async fn encoder_failure(&mut self, err: std::io::Error) -> CompileError {
        let tail = self.collect_stderr().await;
        let tail = tail.trim();
        if tail.is_empty() {
            CompileError::FFmpeg(format!("ffmpeg stopped while receiving a frame: {err}"))
        } else {
            CompileError::FFmpeg(format!(
                "ffmpeg stopped while receiving a frame: {err}\n{}",
                tail_chars(tail, 4000)
            ))
        }
    }

    async fn collect_stderr(&mut self) -> String {
        if let Some(tail) = &self.stderr_tail {
            return tail.clone();
        }
        let _ = self.stdin.take();
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        self.stdout_task.abort();
        let handle =
            std::mem::replace(&mut self.stderr_task, tokio::spawn(async { String::new() }));
        let tail = handle.await.unwrap_or_default();
        self.stderr_tail = Some(tail.clone());
        tail
    }

    /// Closes stdin, waits for ffmpeg and returns the finished file.
    pub async fn finish(mut self) -> Result<CompileResult, CompileError> {
        self.stdin.take(); // drops the pipe -> EOF
        let status = self.child.wait().await?;
        let _ = self.stdout_task.await;
        let stderr = match self.stderr_tail.take() {
            Some(tail) => tail,
            None => self.stderr_task.await.unwrap_or_default(),
        };
        if status.success() {
            Ok(CompileResult {
                output_path: self.output_path,
                cancelled: false,
            })
        } else {
            let _ = std::fs::remove_file(&self.output_path);
            Err(CompileError::FFmpeg(format!(
                "ffmpeg failed: {}",
                tail_chars(stderr.trim(), 4000)
            )))
        }
    }

    /// Terminates ffmpeg and discards the partial output.
    pub async fn cancel(mut self) -> CompileResult {
        let _ = self.stdin.take();
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        self.stdout_task.abort();
        self.stderr_task.abort();
        let _ = std::fs::remove_file(&self.output_path);
        CompileResult {
            output_path: self.output_path,
            cancelled: true,
        }
    }
}

/// Builds ffmpeg's argv for a raw RGBA stdin stream. Split out so the exact
/// shape is unit-testable without launching a process.
pub fn build_stream_args(config: &PipeEncoderConfig, plan: &EncoderPlan) -> Vec<String> {
    let settings = &config.settings;
    let mut args: Vec<String> = vec![
        "-y".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostats".into(),
        "-progress".into(),
        "pipe:1".into(),
    ];
    args.extend(plan.hw_init.iter().cloned());
    args.extend([
        "-framerate".into(),
        config.fps.max(1).to_string(),
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        "rgba".into(),
        "-s".into(),
        format!("{}x{}", config.width, config.height),
        "-i".into(),
        "pipe:0".into(),
    ]);

    let has_audio = config.audio_track.is_some();
    if let Some(audio) = &config.audio_track {
        args.extend([
            "-stream_loop".into(),
            "-1".into(),
            "-i".into(),
            audio.to_string_lossy().into_owned(),
        ]);
    }

    if !plan.vf.is_empty() {
        args.extend(["-vf".into(), plan.vf.clone()]);
    }
    args.extend([
        "-c:v".into(),
        plan.codec.clone(),
        "-pix_fmt".into(),
        plan.pix_fmt.clone(),
    ]);
    args.extend(plan.quality.iter().cloned());
    args.extend(["-threads".into(), "0".into()]);

    if has_audio {
        if let Some(filter) =
            crate::studio::audio::fade_filter(config.duration_seconds(), settings.audio_fade_ms)
        {
            args.extend(["-af".into(), filter]);
        }
        // WebM containers do not accept AAC.
        let audio_codec = if matches!(settings.output_format, OutputFormat::Webm) {
            "libopus"
        } else {
            "aac"
        };
        args.extend([
            "-map".into(),
            "0:v:0".into(),
            "-map".into(),
            "1:a:0".into(),
            "-c:a".into(),
            audio_codec.into(),
            "-b:a".into(),
            "192k".into(),
            "-shortest".into(),
        ]);
    }

    if matches!(
        settings.output_format,
        OutputFormat::Mp4 | OutputFormat::Mov
    ) {
        args.extend(["-movflags".into(), "+faststart".into()]);
    }
    args.push(config.output_path.to_string_lossy().into_owned());
    args
}

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
    use crate::settings::{HardwareAcceleration, VideoCodec};
    use crate::video::select_encoder;

    fn config() -> PipeEncoderConfig {
        PipeEncoderConfig {
            width: 640,
            height: 360,
            fps: 30,
            output_path: PathBuf::from("/tmp/out/timelapse.mp4"),
            settings: Settings {
                hardware_acceleration: HardwareAcceleration::None,
                ..Settings::default()
            },
            audio_track: None,
            total_frames: 300,
        }
    }

    fn idx(args: &[String], name: &str) -> usize {
        args.iter().position(|a| a == name).expect(name)
    }

    #[test]
    fn rawvideo_stdin_args_are_complete() {
        let cfg = config();
        let plan = select_encoder(&cfg.settings);
        let args = build_stream_args(&cfg, &plan);
        assert_eq!(args[idx(&args, "-f") + 1], "rawvideo");
        assert_eq!(args[idx(&args, "-pix_fmt") + 1], "rgba");
        assert_eq!(args[idx(&args, "-s") + 1], "640x360");
        assert_eq!(args[idx(&args, "-framerate") + 1], "30");
        assert_eq!(args[idx(&args, "-i") + 1], "pipe:0");
        assert_eq!(args[idx(&args, "-c:v") + 1], "libx264");
        assert_eq!(*args.last().unwrap(), "/tmp/out/timelapse.mp4");
        assert!(!args.iter().any(|a| a == "t%d.png"));
        assert!(!args.iter().any(|a| a == "-start_number"));
    }

    #[test]
    fn audio_input_is_looped_and_faded_to_the_video_length() {
        let mut cfg = config();
        cfg.total_frames = 300; // 10 s
        cfg.audio_track = Some(PathBuf::from("/music/loop.mp3"));
        cfg.settings.audio_fade_ms = 1500;
        let plan = select_encoder(&cfg.settings);
        let args = build_stream_args(&cfg, &plan);
        assert_eq!(args[idx(&args, "-stream_loop") + 1], "-1");
        assert_eq!(args[idx(&args, "-map") + 1], "0:v:0");
        assert_eq!(args[idx(&args, "-c:a") + 1], "aac");
        assert!(args.iter().any(|a| a == "-shortest"));
        // Fade-out starts at 10 s - 1.5 s = 8.5 s; fade is capped at half the
        // duration for very short clips.
        let af = &args[idx(&args, "-af") + 1];
        assert!(af.contains("afade=t=in:st=0:d=1.500"), "{af}");
        assert!(af.contains("st=8.500"), "{af}");
    }

    #[test]
    fn webm_audio_uses_opus() {
        let mut cfg = config();
        cfg.audio_track = Some(PathBuf::from("/music/loop.mp3"));
        cfg.settings.output_format = OutputFormat::Webm;
        let plan = select_encoder(&cfg.settings);
        let args = build_stream_args(&cfg, &plan);
        assert_eq!(args[idx(&args, "-c:a") + 1], "libopus");
    }

    #[test]
    fn short_clip_fade_is_capped_at_half_the_duration() {
        let mut cfg = config();
        cfg.total_frames = 3; // 0.1 s
        cfg.audio_track = Some(PathBuf::from("/music/loop.mp3"));
        cfg.settings.audio_fade_ms = 1500;
        let plan = select_encoder(&cfg.settings);
        let args = build_stream_args(&cfg, &plan);
        let af = &args[idx(&args, "-af") + 1];
        assert!(af.contains("d=0.050"), "{af}");
        assert!(af.contains("st=0.050"), "{af}");
    }

    #[test]
    fn frame_size_is_rgba_and_validated() {
        let cfg = config();
        assert_eq!(cfg.frame_bytes(), 640 * 360 * 4);
        let plan = select_encoder(&cfg.settings);
        // The arg builder never depends on the frame count or audio fade when
        // there is no audio track.
        assert!(!build_stream_args(&cfg, &plan).iter().any(|a| a == "-af"));
    }

    #[test]
    fn odd_frame_dimensions_are_padded_to_even() {
        let mut cfg = config();
        cfg.width = 9026;
        cfg.height = 7833;
        let plan = select_encoder(&cfg.settings);
        let args = build_stream_args(&cfg, &plan);
        let vf = &args[idx(&args, "-vf") + 1];
        assert!(vf.contains("pad=ceil(iw/2)*2:ceil(ih/2)*2"), "{vf}");
    }

    #[test]
    fn h265_codec_is_honoured() {
        let mut cfg = config();
        cfg.settings.video_codec = VideoCodec::H265;
        let plan = select_encoder(&cfg.settings);
        let args = build_stream_args(&cfg, &plan);
        assert_eq!(args[idx(&args, "-c:v") + 1], "libx265");
    }
}
