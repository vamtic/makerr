//! A tiny reusable pool of raw RGBA frame buffers.
//!
//! Streaming eliminates PNG encodes, but every composed frame still becomes a
//! byte buffer for ffmpeg. Reusing those buffers keeps the allocator (and the
//! OS page cache) out of the hot loop, especially at 4K resolutions where one
//! frame is ~33 MiB.

use std::sync::Mutex;

/// Fixed-size buffer pool. Buffers are returned to the pool after ffmpeg has
/// consumed them.
pub struct FrameBufferPool {
    size: usize,
    free: Mutex<Vec<Vec<u8>>>,
}

impl FrameBufferPool {
    /// Creates a pool for frames of `size` bytes.
    pub fn new(size: usize) -> Self {
        Self {
            size,
            free: Mutex::new(Vec::new()),
        }
    }

    pub fn frame_size(&self) -> usize {
        self.size
    }

    /// Takes a zeroed buffer of the right size (from the pool when possible).
    pub fn acquire(&self) -> Vec<u8> {
        let mut free = self.free.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(mut buf) = free.pop() {
            buf.clear();
            buf.resize(self.size, 0);
            return buf;
        }
        vec![0u8; self.size]
    }

    /// Returns a buffer to the pool. Buffers of the wrong size are discarded.
    pub fn release(&self, buf: Vec<u8>) {
        if buf.len() != self.size {
            return;
        }
        let mut free = self.free.lock().unwrap_or_else(|p| p.into_inner());
        // Keep the pool bounded so a burst of frames cannot pin gigabytes.
        if free.len() < 8 {
            free.push(buf);
        }
    }

    /// Number of buffers currently parked in the pool (for tests).
    pub fn available(&self) -> usize {
        self.free.lock().unwrap_or_else(|p| p.into_inner()).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers_are_reused_and_resized() {
        let pool = FrameBufferPool::new(16);
        let mut a = pool.acquire();
        assert_eq!(a.len(), 16);
        a[0] = 9;
        pool.release(a);
        assert_eq!(pool.available(), 1);
        let b = pool.acquire();
        assert_eq!(b.len(), 16);
        // Acquired buffers are zeroed, not stale.
        assert_eq!(b[0], 0);
        assert_eq!(pool.available(), 0);
    }

    #[test]
    fn wrong_sized_buffers_are_discarded_and_the_pool_stays_bounded() {
        let pool = FrameBufferPool::new(8);
        pool.release(vec![0; 3]);
        assert_eq!(pool.available(), 0);
        for _ in 0..32 {
            pool.release(vec![0; 8]);
        }
        assert!(pool.available() <= 8);
    }
}
