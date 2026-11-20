//! Persisted application settings.
//!
//! The JSON schema stays compatible with V1 (`settings.json`): same keys,
//! same clamping rules, same presets. Unknown keys and legacy values are
//! tolerated on load, and [`Settings::load_or_import`] imports a V1 file on
//! first launch.

use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

use crate::link::LinkMode;

pub const MIN_FPS: i32 = 1;
pub const MAX_FPS: i32 = 120;
pub const MIN_TIMEOUT: f64 = 5.0;
pub const MAX_TIMEOUT: f64 = 300.0;
pub const MIN_PREVIEW_LIMIT: i32 = 1;
pub const MAX_PREVIEW_LIMIT: i32 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    En,
    Hu,
}

impl Language {
    pub fn code(self) -> &'static str {
        match self {
            Language::En => "en",
            Language::Hu => "hu",
        }
    }

    pub fn from_code(code: &str) -> Self {
        if code.eq_ignore_ascii_case("hu") {
            Language::Hu
        } else {
            Language::En
        }
    }
}

/// Video container/encoder format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum OutputFormat {
    #[default]
    #[serde(rename = "mp4")]
    Mp4,
    #[serde(rename = "avi")]
    Avi,
    #[serde(rename = "mkv")]
    Mkv,
    #[serde(rename = "mov")]
    Mov,
    #[serde(rename = "webm")]
    Webm,
}

impl OutputFormat {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "avi" => OutputFormat::Avi,
            "mkv" => OutputFormat::Mkv,
            "mov" => OutputFormat::Mov,
            "webm" => OutputFormat::Webm,
            _ => OutputFormat::Mp4,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            OutputFormat::Mp4 => "mp4",
            OutputFormat::Avi => "avi",
            OutputFormat::Mkv => "mkv",
            OutputFormat::Mov => "mov",
            OutputFormat::Webm => "webm",
        }
    }

    pub fn all() -> &'static [OutputFormat] {
        &[
            OutputFormat::Mp4,
            OutputFormat::Avi,
            OutputFormat::Mkv,
            OutputFormat::Mov,
            OutputFormat::Webm,
        ]
    }
}

/// Hardware encoder family for ffmpeg.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum HardwareAcceleration {
    #[default]
    Auto,
    None,
    Nvenc,
    Amf,
    Qsv,
    Vaapi,
}

impl HardwareAcceleration {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "none" => HardwareAcceleration::None,
            "nvenc" => HardwareAcceleration::Nvenc,
            "amf" => HardwareAcceleration::Amf,
            "qsv" => HardwareAcceleration::Qsv,
            "vaapi" => HardwareAcceleration::Vaapi,
            _ => HardwareAcceleration::Auto,
        }
    }

    pub fn as_setting_str(self) -> &'static str {
        match self {
            HardwareAcceleration::Auto => "auto",
            HardwareAcceleration::None => "none",
            HardwareAcceleration::Nvenc => "nvenc",
            HardwareAcceleration::Amf => "amf",
            HardwareAcceleration::Qsv => "qsv",
            HardwareAcceleration::Vaapi => "vaapi",
        }
    }
}

/// Software video codec selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum VideoCodec {
    #[default]
    #[serde(rename = "h264")]
    H264,
    #[serde(rename = "h265")]
    H265,
    #[serde(rename = "av1")]
    Av1,
}

impl VideoCodec {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "h265" | "hevc" => VideoCodec::H265,
            "av1" => VideoCodec::Av1,
            _ => VideoCodec::H264,
        }
    }

    pub fn as_setting_str(self) -> &'static str {
        match self {
            VideoCodec::H264 => "h264",
            VideoCodec::H265 => "h265",
            VideoCodec::Av1 => "av1",
        }
    }
}

/// Automatic camera motion applied while rendering (Ken Burns effect).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CameraMotion {
    /// Static frame, exactly the selected region.
    #[default]
    None,
    /// Start zoomed in on the centre and pull back to the full region.
    ZoomOut,
    /// Start on the full region and push in towards the centre.
    ZoomIn,
    /// Glide from the left edge of the region to the right edge.
    PanLeftToRight,
    /// Glide from the right edge of the region to the left edge.
    PanRightToLeft,
}

impl CameraMotion {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().replace('-', "_").as_str() {
            "zoom_out" | "zoomout" => CameraMotion::ZoomOut,
            "zoom_in" | "zoomin" => CameraMotion::ZoomIn,
            "pan_left_to_right" | "pan_lr" => CameraMotion::PanLeftToRight,
            "pan_right_to_left" | "pan_rl" => CameraMotion::PanRightToLeft,
            _ => CameraMotion::None,
        }
    }

    pub fn as_setting_str(self) -> &'static str {
        match self {
            CameraMotion::None => "none",
            CameraMotion::ZoomOut => "zoom_out",
            CameraMotion::ZoomIn => "zoom_in",
            CameraMotion::PanLeftToRight => "pan_left_to_right",
            CameraMotion::PanRightToLeft => "pan_right_to_left",
        }
    }
}

/// Social video framing preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SocialPreset {
    /// Keep the exact selected region (no letterboxing).
    #[default]
    Source,
    /// TikTok / YouTube Shorts / Reels (9:16, blurred mirrored bars).
    Vertical916,
    /// Instagram square (1:1).
    Square11,
    /// YouTube / desktop widescreen (16:9).
    Wide169,
}

impl SocialPreset {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().replace('-', "_").as_str() {
            "vertical" | "9_16" | "vertical_9_16" | "vertical916" | "shorts" | "tiktok"
            | "reels" => SocialPreset::Vertical916,
            "square" | "1_1" | "square_1_1" | "square11" | "instagram" => SocialPreset::Square11,
            "wide" | "16_9" | "wide_16_9" | "wide169" | "youtube" => SocialPreset::Wide169,
            _ => SocialPreset::Source,
        }
    }

    pub fn as_setting_str(self) -> &'static str {
        match self {
            SocialPreset::Source => "source",
            SocialPreset::Vertical916 => "vertical_9_16",
            SocialPreset::Square11 => "square_1_1",
            SocialPreset::Wide169 => "wide_16_9",
        }
    }
}

/// Where the watermark text/logo is anchored inside the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WatermarkPosition {
    TopLeft,
    #[default]
    TopRight,
    BottomLeft,
    BottomRight,
}

impl WatermarkPosition {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().replace('-', "_").as_str() {
            "top_left" => WatermarkPosition::TopLeft,
            "bottom_left" => WatermarkPosition::BottomLeft,
            "bottom_right" => WatermarkPosition::BottomRight,
            _ => WatermarkPosition::TopRight,
        }
    }

    pub fn as_setting_str(self) -> &'static str {
        match self {
            WatermarkPosition::TopLeft => "top_left",
            WatermarkPosition::TopRight => "top_right",
            WatermarkPosition::BottomLeft => "bottom_left",
            WatermarkPosition::BottomRight => "bottom_right",
        }
    }
}

/// Site presets: display name -> (domain, storage domain).
pub const PRESETS: &[(&str, &str, &str)] = &[
    ("PPF", "pixelplanet.fun", "storage.gs-os.com"),
    ("PixelYa", "pixelya.fun", "storage.pixelya.fun/history"),
    ("PixMap", "pixmap.fun", "pixmap.fun/history/chunk"),
    ("PXU", "pxuniverse.fun", "pxuniverse.fun/backupdata"),
];

/// The storage base belonging to a preset domain, if `domain` names one.
///
/// The CLI uses this when the user passes `--domain` without `--storage`, so
/// choosing PixMap cannot silently keep another site's storage layout (which
/// produces 404s and a blank video).
pub fn preset_storage_for_domain(domain: &str) -> Option<&'static str> {
    let domain = domain.trim();
    PRESETS
        .iter()
        .find(|(_, preset_domain, _)| preset_domain.eq_ignore_ascii_case(domain))
        .map(|(_, _, storage)| *storage)
}

/// The default settings path used by both the CLI and the GUI.
pub fn default_settings_path() -> std::path::PathBuf {
    // `maker/settings.json` in the working directory, so both front-ends can
    // share one config.
    std::path::PathBuf::from("maker/settings.json")
}

/// Candidate locations of a V1 `settings.json`. V1 kept the file next to the
/// script/executable, so check the working directory and the executable's
/// directory when the V2 settings file is missing.
fn legacy_settings_paths(path: &Path) -> Vec<std::path::PathBuf> {
    let mut candidates = Vec::new();

    // `<cwd>/maker/settings.json` -> `<cwd>/settings.json`
    if let Some(maker_dir) = path.parent() {
        if maker_dir
            .file_name()
            .is_some_and(|name| name == std::ffi::OsStr::new("maker"))
        {
            if let Some(root) = maker_dir.parent() {
                candidates.push(root.join("settings.json"));
            }
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("settings.json");
            if !candidates.contains(&sibling) {
                candidates.push(sibling);
            }
        }
    }

    candidates
}

/// Default User-Agent sent by V2. Kept short so servers see a stable,
/// versioned client identity instead of a browser string.
pub const DEFAULT_USER_AGENT: &str = "TimelapseMaker/2.0";

/// User-Agent values written by older releases (V1 and the pre-release V2
/// builds) that should be upgraded to [`DEFAULT_USER_AGENT`]. A custom value
/// set by the user is never touched.
const LEGACY_DEFAULT_USER_AGENTS: [&str; 3] = [
    "V1.2.2 by vamtic:",
    "TimelapseMaker/4.0-rs",
    "TimelapseMaker/4.0",
];

/// Schema version used to migrate settings written by older releases.
const SETTINGS_VERSION: u32 = 3;

fn old_settings_version() -> u32 {
    0
}

/// Application settings. Field defaults match the V1 constructor values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Missing in files written before the versioned migrations.
    #[serde(default = "old_settings_version")]
    pub settings_version: u32,
    pub domain: String,
    pub storage_domain: String,
    #[serde(default, deserialize_with = "tolerant_enum")]
    pub language: Language,
    pub user_agent: String,

    pub frame_skip: i32,
    pub max_connections: i32,
    pub max_concurrent_tiles: i32,
    /// Let the downloaders grow/shrink their concurrency from the server's
    /// responses (AIMD). The connection limits stay as starting points and
    /// upper hints, so a user who wants the old fixed behaviour can turn this
    /// off.
    pub adaptive_concurrency: bool,
    pub timeout: f64,
    pub quality: i32,
    pub max_retries: i32,
    pub max_concurrent_saves: i32,
    /// How unchanged frames are attached to their predecessor (see
    /// [`crate::link`]): `auto` hardlinks, falls back to symlinks, then copies.
    #[serde(default, deserialize_with = "tolerant_enum")]
    pub link_mode: LinkMode,

    // GUI-only settings (kept for config compatibility with the V1 app).
    pub font_family: String,
    pub font_size: i32,
    pub accent_color: String,
    pub hover_color: String,

    #[serde(default, deserialize_with = "tolerant_enum")]
    pub output_format: OutputFormat,
    pub fps: i32,
    pub preview_limit: i32,
    #[serde(default, deserialize_with = "tolerant_enum")]
    pub hardware_acceleration: HardwareAcceleration,
    #[serde(default, deserialize_with = "tolerant_enum")]
    pub video_codec: VideoCodec,
    pub cache_ttl_days: i32,
    pub notify_on_completion: bool,
    pub onboarding_done: bool,

    // ---- GUI window/panel state (not used by the CLI) -----------------------
    /// Whether the Studio FX panel is expanded. Remembered across sessions.
    pub studio_expanded: bool,
    /// Whether the console drawer is expanded. Remembered across sessions.
    pub console_expanded: bool,
    /// Last window size in physical pixels (0 = keep the preferred size).
    pub window_width: i32,
    pub window_height: i32,

    // ---- Studio settings ----------------------------------------------------
    /// Stream raw RGBA frames straight into ffmpeg instead of writing a PNG
    /// sequence first (zero-disk rendering).
    pub stream_video: bool,
    /// Keep the intermediate `t*.png` frames when streaming. Default on so a
    /// finished run always leaves inspectable frames on disk unless the user
    /// explicitly opts out.
    pub save_png_sequence: bool,
    /// Burn an animated date/time clock into every frame.
    pub show_timestamp: bool,
    /// `chrono` format string used by the timestamp overlay.
    pub timestamp_format: String,
    /// Draw a progress line along the bottom edge of every frame.
    pub show_progress_bar: bool,
    /// Optional custom branding text burned into every frame.
    pub watermark_text: String,
    #[serde(default, deserialize_with = "tolerant_enum")]
    pub watermark_position: WatermarkPosition,
    /// Integer scale of the built-in pixel font (1..=8).
    pub overlay_font_scale: i32,
    #[serde(default, deserialize_with = "tolerant_enum")]
    pub camera_motion: CameraMotion,
    /// Camera motion strength in percent (1..=90).
    pub camera_strength: i32,
    /// Optional audio track mixed into the video (empty = silent).
    pub audio_path: String,
    /// Audio fade-in/out duration in milliseconds.
    pub audio_fade_ms: i32,
    #[serde(default, deserialize_with = "tolerant_enum")]
    pub social_preset: SocialPreset,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            // A loaded file is migrated in `load`; `save` stamps the current
            // version. In-memory defaults stay at 0 until then.
            settings_version: 0,
            domain: "pixelplanet.fun".into(),
            storage_domain: "storage.gs-os.com".into(),
            language: Language::En,
            user_agent: DEFAULT_USER_AGENT.into(),

            frame_skip: 0,
            max_connections: 50,
            max_concurrent_tiles: 64,
            adaptive_concurrency: true,
            timeout: 30.0,
            quality: 0,
            max_retries: 3,
            max_concurrent_saves: 2,
            link_mode: LinkMode::Auto,

            font_family: "Segoe UI".into(),
            font_size: 14,
            accent_color: "#00e6a0".into(),
            hover_color: "#00b37e".into(),

            output_format: OutputFormat::Mp4,
            fps: 30,
            preview_limit: 512,
            hardware_acceleration: HardwareAcceleration::Auto,
            video_codec: VideoCodec::H264,
            cache_ttl_days: 7,
            notify_on_completion: true,
            onboarding_done: false,

            studio_expanded: false,
            console_expanded: false,
            window_width: 0,
            window_height: 0,

            stream_video: true,
            save_png_sequence: true,
            show_timestamp: false,
            timestamp_format: "%Y-%m-%d %H:%M".into(),
            show_progress_bar: false,
            watermark_text: String::new(),
            watermark_position: WatermarkPosition::TopRight,
            overlay_font_scale: 2,
            camera_motion: CameraMotion::None,
            camera_strength: 20,
            audio_path: String::new(),
            audio_fade_ms: 1500,
            social_preset: SocialPreset::Source,
        }
    }
}

fn clamp_i32(v: i32, lo: i32, hi: i32) -> i32 {
    v.clamp(lo, hi)
}

/// Deserializes an enum field through `serde_json::Value`, falling back to the
/// enum's default when the stored spelling is unknown.
///
/// The V1 app's `settings.json` is shared between the two front-ends, and a
/// single value the Rust enums do not know (e.g. `"output_format": "gif"`) used
/// to make the *whole* `Settings` deserialization fail, silently resetting
/// every other setting. Tolerating it per field keeps the rest of the file
/// intact; `Settings::clamp` then normalizes the default.
fn tolerant_enum<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned + Default,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

impl Settings {
    /// True when at least one Studio FX option is non-default. The GUI uses
    /// this to reveal the panel even when the remembered state is collapsed,
    /// so active effects are never hidden behind a summary.
    pub fn has_active_studio_fx(&self) -> bool {
        let defaults = Settings::default();
        !self.stream_video
            || self.save_png_sequence != defaults.save_png_sequence
            || self.show_timestamp != defaults.show_timestamp
            || self.show_progress_bar != defaults.show_progress_bar
            || !self.watermark_text.trim().is_empty()
            || self.camera_motion != defaults.camera_motion
            || self.social_preset != defaults.social_preset
            || !self.audio_path.trim().is_empty()
    }

    /// Loads settings from `path`, falling back to defaults for missing
    /// fields and files.
    pub fn load(path: impl AsRef<Path>) -> Settings {
        match std::fs::read(path) {
            Ok(bytes) => Self::from_bytes(&bytes),
            Err(_) => Settings::default(),
        }
    }

    /// Loads V2 settings, or imports a V1 `settings.json` when the V2 file does
    /// not exist yet. This is the entry point used by the GUI and the CLI when
    /// no explicit `--settings` path was given.
    ///
    /// The V1 file is never modified: the imported result is written to the V2
    /// location so a user can still go back to V1 without losing anything.
    pub fn load_or_import(path: impl AsRef<Path>) -> Settings {
        let path = path.as_ref();
        if path.exists() {
            return Self::load(path);
        }

        if let Some(legacy) = legacy_settings_paths(path)
            .into_iter()
            .find(|candidate| candidate.exists())
        {
            let mut settings = std::fs::read(&legacy)
                .map(|bytes| Self::from_bytes(&bytes))
                .unwrap_or_default();
            // A V1 user has already used the app; do not show them the
            // first-run guide again.
            settings.onboarding_done = true;
            let _ = settings.save(path);
            return settings;
        }

        Settings::default()
    }

    /// Parses a settings file, translating V1-only fields and running the
    /// versioned migrations.
    fn from_bytes(bytes: &[u8]) -> Settings {
        let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap_or_default();

        // V1 stored the editor font as `[family, size]`; V2 has two fields.
        let legacy_font = value
            .get("font")
            .and_then(|font| font.as_array())
            .map(|font| {
                (
                    font.first()
                        .and_then(|family| family.as_str())
                        .map(str::to_string),
                    font.get(1).and_then(|size| size.as_i64()),
                )
            });
        if let Some((family, size)) = legacy_font {
            if let Some(family) = family {
                value["font_family"] = serde_json::Value::String(family);
            }
            if let Some(size) = size {
                value["font_size"] = serde_json::Value::from(size);
            }
        }

        let mut settings: Settings = serde_json::from_value(value).unwrap_or_default();
        settings.migrate();
        settings.clamp();
        settings
    }

    /// Applies one-time migrations for settings written by older releases.
    fn migrate(&mut self) {
        let version = self.settings_version;

        if version < 2 {
            // Older releases streamed without keeping PNGs. The new default is
            // to retain them, so upgrade existing configs once. A later save
            // records the current version and preserves an explicit opt-out.
            self.save_png_sequence = true;
        }

        if version < 3 && LEGACY_DEFAULT_USER_AGENTS.contains(&self.user_agent.trim()) {
            self.user_agent = DEFAULT_USER_AGENT.into();
        }

        self.settings_version = SETTINGS_VERSION;
    }

    /// Atomically writes the settings (temp file + rename, like QSaveFile).
    pub fn save(&mut self, path: impl AsRef<Path>) -> std::io::Result<()> {
        self.settings_version = SETTINGS_VERSION;
        self.clamp();
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).expect("settings serialize");
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }

    /// Enforces the same value ranges as the V1 implementation.
    pub fn clamp(&mut self) {
        self.max_connections = clamp_i32(self.max_connections, 1, 500);
        self.max_concurrent_tiles = clamp_i32(self.max_concurrent_tiles, 1, 256);
        if !self.timeout.is_finite() {
            self.timeout = 30.0;
        } else {
            self.timeout = self.timeout.clamp(MIN_TIMEOUT, MAX_TIMEOUT);
        }
        self.quality = clamp_i32(self.quality, -1, 9);
        self.max_retries = clamp_i32(self.max_retries, 0, 10);
        self.fps = clamp_i32(self.fps, MIN_FPS, MAX_FPS);
        self.frame_skip = self.frame_skip.max(0);
        self.preview_limit = clamp_i32(self.preview_limit, MIN_PREVIEW_LIMIT, MAX_PREVIEW_LIMIT);
        self.cache_ttl_days = clamp_i32(self.cache_ttl_days, 0, 90);
        self.max_concurrent_saves = clamp_i32(self.max_concurrent_saves, 1, 8);
        self.font_size = clamp_i32(self.font_size, 6, 72);
        self.overlay_font_scale = clamp_i32(self.overlay_font_scale, 1, 8);
        self.camera_strength = clamp_i32(self.camera_strength, 1, 90);
        self.audio_fade_ms = clamp_i32(self.audio_fade_ms, 0, 10_000);
        if self.timestamp_format.trim().is_empty() {
            self.timestamp_format = "%Y-%m-%d %H:%M".into();
        }
        self.output_format = OutputFormat::parse(self.output_format.extension());
        if !is_valid_hex_color(&self.accent_color) {
            self.accent_color = "#00e6a0".into();
        }
        if !is_valid_hex_color(&self.hover_color) {
            self.hover_color = "#00b37e".into();
        }
        if self.window_width != 0 {
            self.window_width = clamp_i32(self.window_width, 640, 16384);
        }
        if self.window_height != 0 {
            self.window_height = clamp_i32(self.window_height, 480, 16384);
        }
    }
}

fn is_valid_hex_color(s: &str) -> bool {
    let body = s.strip_prefix('#').unwrap_or("");
    body.len() == 6 && body.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_v1() {
        let s = Settings::default();
        assert_eq!(s.user_agent, DEFAULT_USER_AGENT);
        assert_eq!(s.domain, "pixelplanet.fun");
        assert_eq!(s.storage_domain, "storage.gs-os.com");
        assert_eq!(s.max_connections, 50);
        assert_eq!(s.max_concurrent_tiles, 64);
        assert!(s.adaptive_concurrency, "adaptive limits are the default");
        assert_eq!(s.timeout, 30.0);
        assert_eq!(s.fps, 30);
        assert_eq!(s.preview_limit, 512);
        assert_eq!(s.cache_ttl_days, 7);
        assert_eq!(s.output_format, OutputFormat::Mp4);
        assert!(s.notify_on_completion);
        assert!(!s.onboarding_done);
    }

    #[test]
    fn clamp_bounds_everything() {
        let mut s = Settings {
            max_connections: 1000,
            max_concurrent_tiles: 0,
            timeout: 1.0,
            quality: 42,
            max_retries: 99,
            fps: 1000,
            frame_skip: -5,
            preview_limit: 0,
            cache_ttl_days: -1,
            max_concurrent_saves: 100,
            font_size: 2,
            accent_color: "not-a-color".into(),
            hover_color: "#xyz".into(),
            ..Settings::default()
        };
        s.clamp();
        assert_eq!(s.max_connections, 500);
        assert_eq!(s.max_concurrent_tiles, 1);
        assert_eq!(s.timeout, MIN_TIMEOUT);

        // NaN and infinity timeout clamp safely
        s.timeout = f64::NAN;
        s.clamp();
        assert_eq!(s.timeout, 30.0);
        s.timeout = f64::INFINITY;
        s.clamp();
        assert_eq!(s.timeout, 30.0);
        assert_eq!(s.quality, 9);
        assert_eq!(s.max_retries, 10);
        assert_eq!(s.fps, MAX_FPS);
        assert_eq!(s.frame_skip, 0);
        assert_eq!(s.preview_limit, MIN_PREVIEW_LIMIT);
        assert_eq!(s.cache_ttl_days, 0);
        assert_eq!(s.max_concurrent_saves, 8);
        assert_eq!(s.font_size, 6);
        assert_eq!(s.accent_color, "#00e6a0");
        assert_eq!(s.hover_color, "#00b37e");
    }

    #[test]
    fn old_settings_migrate_png_retention_once() {
        let mut s: Settings = serde_json::from_str(r#"{"save_png_sequence": false}"#).unwrap();
        assert_eq!(s.settings_version, 0);
        assert!(!s.save_png_sequence);

        s.migrate();
        assert!(s.save_png_sequence);
        assert_eq!(s.settings_version, SETTINGS_VERSION);

        // Once saved at the new version, an explicit opt-out survives a reload.
        s.save_png_sequence = false;
        s.migrate();
        assert!(!s.save_png_sequence);
    }

    #[test]
    fn imports_v1_settings_and_translates_the_font_tuple() {
        let dir = tempfile::tempdir().unwrap();
        let v1_path = dir.path().join("settings.json");
        std::fs::write(
            &v1_path,
            r#"{
                "domain": "pixmap.fun",
                "font": ["Fira Code", 15],
                "user_agent": "V1.2.2 by vamtic:",
                "max_connections": 1000
            }"#,
        )
        .unwrap();

        let v2_path = dir.path().join("maker/settings.json");
        assert!(!v2_path.exists());

        let s = Settings::load_or_import(&v2_path);
        assert_eq!(s.domain, "pixmap.fun");
        assert_eq!(s.font_family, "Fira Code");
        assert_eq!(s.font_size, 15);
        assert_eq!(s.user_agent, DEFAULT_USER_AGENT);
        assert!(s.onboarding_done);
        assert!(v2_path.exists(), "the imported settings are saved for V2");
        assert!(v1_path.exists(), "the V1 file is left untouched");
    }

    #[test]
    fn keeps_a_custom_user_agent_during_migration() {
        let mut s: Settings = serde_json::from_str(r#"{"user_agent": "my-custom-agent"}"#).unwrap();
        s.migrate();
        assert_eq!(s.user_agent, "my-custom-agent");
        assert_eq!(s.settings_version, SETTINGS_VERSION);
    }

    #[test]
    fn round_trips_json_schema() {
        // The JSON layout must stay compatible with the V1 app's
        // maker/settings.json (same key names and value spellings).
        let s = Settings::default();
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["domain"], "pixelplanet.fun");
        assert_eq!(json["storage_domain"], "storage.gs-os.com");
        assert_eq!(json["language"], "en");
        assert_eq!(json["user_agent"], s.user_agent);
        assert_eq!(json["frame_skip"], 0);
        assert_eq!(json["max_connections"], 50);
        assert_eq!(json["max_concurrent_tiles"], 64);
        assert_eq!(json["timeout"], 30.0);
        assert_eq!(json["output_format"], "mp4");
        assert_eq!(json["hardware_acceleration"], "auto");
        assert_eq!(json["video_codec"], "h264");
        assert_eq!(json["cache_ttl_days"], 7);
        assert_eq!(json["notify_on_completion"], true);
        assert_eq!(json["onboarding_done"], false);

        // Loading a V1-style file (string enums, missing keys) succeeds.
        let v1_file = r#"{
            "domain": "pixmap.fun",
            "storage_domain": "pixmap.fun/history/chunk",
            "language": "hu",
            "fps": 60,
            "output_format": "webm",
            "hardware_acceleration": "nvenc",
            "video_codec": "h265"
        }"#;
        let mut s: Settings = serde_json::from_str(v1_file).unwrap();
        s.clamp();
        assert_eq!(s.domain, "pixmap.fun");
        assert_eq!(s.language, Language::Hu);
        assert_eq!(s.fps, 60);
        assert_eq!(s.output_format, OutputFormat::Webm);
        assert_eq!(s.hardware_acceleration, HardwareAcceleration::Nvenc);
        assert_eq!(s.video_codec, VideoCodec::H265);
        // Fields absent from the file keep their defaults.
        assert_eq!(s.max_connections, 50);
    }

    #[test]
    fn unknown_enum_values_do_not_reset_other_fields() {
        // One unknown spelling must not discard the whole settings file. The
        // offending field falls back to its default; everything else survives.
        let json = r#"{
            "domain": "pixmap.fun",
            "fps": 60,
            "output_format": "gif",
            "video_codec": "vp8",
            "language": "xx",
            "hardware_acceleration": "bogus",
            "link_mode": "magic"
        }"#;
        let s: Settings = serde_json::from_str(json).expect("tolerant parse");
        assert_eq!(s.domain, "pixmap.fun");
        assert_eq!(s.fps, 60);
        assert_eq!(s.output_format, OutputFormat::Mp4);
        assert_eq!(s.video_codec, VideoCodec::H264);
        assert_eq!(s.language, Language::En);
        assert_eq!(s.hardware_acceleration, HardwareAcceleration::Auto);
        assert_eq!(s.link_mode, LinkMode::Auto);
    }

    #[test]
    fn studio_defaults_stream_and_overlays_are_parsed() {
        let s = Settings::default();
        assert!(s.stream_video, "zero-disk streaming is the default");
        assert!(
            s.save_png_sequence,
            "the intermediate PNG frames are retained by default"
        );
        assert!(!s.show_timestamp);
        assert!(!s.show_progress_bar);
        assert_eq!(s.watermark_position, WatermarkPosition::TopRight);
        assert_eq!(s.camera_motion, CameraMotion::None);
        assert_eq!(s.overlay_font_scale, 2);
        assert_eq!(s.audio_fade_ms, 1500);
        assert_eq!(s.social_preset, SocialPreset::Source);

        // An old settings file without the new keys keeps its values.
        let old = r#"{"domain":"pixmap.fun","fps":24}"#;
        let parsed: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(parsed.domain, "pixmap.fun");
        assert!(parsed.stream_video);

        // New enum spellings are parsed, unknown ones fall back.
        let json = r#"{
            "watermark_position": "bottom_left",
            "camera_motion": "zoom_out",
            "audio_fade_ms": 900
        }"#;
        let parsed: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.watermark_position, WatermarkPosition::BottomLeft);
        assert_eq!(parsed.camera_motion, CameraMotion::ZoomOut);
        assert_eq!(parsed.audio_fade_ms, 900);

        assert_eq!(SocialPreset::parse("shorts"), SocialPreset::Vertical916);
        assert_eq!(SocialPreset::parse("16-9"), SocialPreset::Wide169);
        assert_eq!(SocialPreset::parse("bogus"), SocialPreset::Source);

        let json = r#"{"watermark_position":"middle","camera_motion":"orbit"}"#;
        let parsed: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.watermark_position, WatermarkPosition::TopRight);
        assert_eq!(parsed.camera_motion, CameraMotion::None);
    }

    #[test]
    fn studio_clamps_and_camera_parse_aliases() {
        let mut s = Settings {
            overlay_font_scale: 99,
            camera_strength: 0,
            audio_fade_ms: -5,
            timestamp_format: "   ".into(),
            ..Settings::default()
        };
        s.clamp();
        assert_eq!(s.overlay_font_scale, 8);
        assert_eq!(s.camera_strength, 1);
        assert_eq!(s.audio_fade_ms, 0);
        assert_eq!(s.timestamp_format, "%Y-%m-%d %H:%M");

        assert_eq!(CameraMotion::parse("zoom-in"), CameraMotion::ZoomIn);
        assert_eq!(CameraMotion::parse("pan_rl"), CameraMotion::PanRightToLeft);
        assert_eq!(CameraMotion::parse("orbit"), CameraMotion::None);
        assert_eq!(
            WatermarkPosition::parse("bottom-right"),
            WatermarkPosition::BottomRight
        );
        assert_eq!(CameraMotion::ZoomOut.as_setting_str(), "zoom_out");
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/settings.json");
        let mut s = Settings {
            fps: 24,
            ..Default::default()
        };
        s.save(&path).unwrap();
        let loaded = Settings::load(&path);
        assert_eq!(loaded, s);
    }

    #[test]
    fn presets_match_v1() {
        assert_eq!(PRESETS[0], ("PPF", "pixelplanet.fun", "storage.gs-os.com"));
        assert_eq!(
            PRESETS[1],
            ("PixelYa", "pixelya.fun", "storage.pixelya.fun/history")
        );
        assert_eq!(
            PRESETS[2],
            ("PixMap", "pixmap.fun", "pixmap.fun/history/chunk")
        );
        assert_eq!(
            PRESETS[3],
            ("PXU", "pxuniverse.fun", "pxuniverse.fun/backupdata")
        );
    }

    #[test]
    fn preset_storage_lookup_is_case_insensitive() {
        assert_eq!(
            preset_storage_for_domain("pixmap.fun"),
            Some("pixmap.fun/history/chunk")
        );
        assert_eq!(
            preset_storage_for_domain("  PIXELPLANET.FUN "),
            Some("storage.gs-os.com")
        );
        assert_eq!(preset_storage_for_domain("custom.example.com"), None);
    }
}
