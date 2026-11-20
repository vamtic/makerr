//! TimelapseMaker core engine — the Rust rewrite of the V1 application.
//!
//! Modules follow the V1 feature boundaries:
//! - [`coord`]      ← `x_y` corner parsing
//! - [`site`]       ← per-site quirks and canvas lists
//! - [`tileurl`]    ← tile URL construction
//! - [`settings`]   ← settings load/save/clamp + V1 import
//! - [`i18n`]       ← EN/HU dictionaries
//! - [`cache`]      ← tile cache API and packed redb store
//! - [`net`]        ← HTTP client, headers, retries
//! - [`history`]    ← canvas list + per-day snapshot history
//! - [`compose`]    ← tile decode + frame composition
//! - [`pipeline`]   ← the async download/compose/encode pipeline
//! - [`video`]      ← ffmpeg discovery and encoding
//! - [`template`]   ← headless template JSON import/export

pub mod adaptive;
pub mod cache;
pub mod compose;
pub mod coord;
pub mod history;
pub mod i18n;
pub mod link;
pub mod net;
pub mod pipeline;
pub mod settings;
pub mod simd;
pub mod site;
pub mod stream;
pub mod studio;
pub mod sys;
pub mod template;
pub mod tileurl;
pub mod video;

pub use coord::Region;
pub use settings::Settings;
pub use template::Template;
