//! Fixed-capacity in-memory byte ring buffer for PTY scrollback catch-up.

use std::collections::VecDeque;

/// Default per-session PTY scrollback memory cap (512 KiB).
pub const DEFAULT_RING_CAPACITY: usize = 512 * 1024;

/// Strictly bounded FIFO byte ring buffer (`O(1)` memory per PTY session).
#[derive(Debug, Clone)]
pub struct ByteRingBuffer {
    buf: VecDeque<u8>,
    capacity: usize,
    total_written: u64,
}

impl ByteRingBuffer {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1024);
        Self {
            buf: VecDeque::with_capacity(cap.min(64 * 1024)),
            capacity: cap,
            total_written: 0,
        }
    }

    /// Appends `data`, evicting oldest bytes if needed so `len() <= capacity`.
    pub fn push(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.total_written = self.total_written.saturating_add(data.len() as u64);

        if data.len() >= self.capacity {
            self.buf.clear();
            let tail = &data[data.len() - self.capacity..];
            self.buf.extend(tail.iter().copied());
            return;
        }

        let needed = self.buf.len() + data.len();
        if needed > self.capacity {
            let to_drop = needed - self.capacity;
            self.buf.drain(..to_drop);
        }
        self.buf.extend(data.iter().copied());
    }

    /// Returns a contiguous snapshot of the current scrollback buffer.
    pub fn snapshot(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.buf.len());
        let (a, b) = self.buf.as_slices();
        out.extend_from_slice(a);
        out.extend_from_slice(b);
        out
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn total_written(&self) -> u64 {
        self.total_written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_bounds_memory_and_preserves_tail() {
        let mut ring = ByteRingBuffer::new(1024);
        ring.push(&vec![b'a'; 800]);
        ring.push(&vec![b'b'; 500]);
        assert_eq!(ring.len(), 1024);
        assert_eq!(ring.total_written(), 1300);
        let snap = ring.snapshot();
        assert_eq!(&snap[..524], &vec![b'a'; 524][..]);
        assert_eq!(&snap[524..], &vec![b'b'; 500][..]);

        // Chunk larger than capacity keeps exact trailing slice
        let mut big = vec![b'x'; 1200];
        big.extend_from_slice(b"TAIL");
        ring.push(&big);
        assert_eq!(ring.len(), 1024);
        let snap2 = ring.snapshot();
        assert!(snap2.ends_with(b"TAIL"));
    }
}
