//! Zero-disk in-memory streaming to FFmpeg.

pub mod buffer_pool;
pub mod pipe;

pub use buffer_pool::FrameBufferPool;
pub use pipe::{build_stream_args, AsyncStreamEncoder, PipeEncoderConfig};
