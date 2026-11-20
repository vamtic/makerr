//! Content-creator studio helpers: overlays, camera motion and audio.

pub mod audio;
pub mod camera;
pub mod overlays;
pub mod social;

pub use overlays::{apply_frame_overlays, OverlayConfig};
