//! asciicast v2 (`.cast`) append-only PTY recorder and safe path resolver.

use om_core::{CoreError, CoreResult};
use serde_json::json;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Default per-recording file size cap (64 MiB) to protect host disk from runaway output.
pub const DEFAULT_MAX_CAST_BYTES: u64 = 64 * 1024 * 1024;

/// Validates a session id before turning it into a `.cast` filename.
pub fn safe_cast_path(dir: &Path, session_id: &str) -> CoreResult<PathBuf> {
    let trimmed = session_id.trim();
    if trimmed.is_empty()
        || trimmed.len() > 128
        || !trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(CoreError::InvalidInput(format!(
            "invalid session id for PTY recording: {session_id}"
        )));
    }
    Ok(dir.join(format!("{trimmed}.cast")))
}

/// Append-only asciicast v2 file writer with hard byte-size capping and UTF-8 boundary carry.
pub struct CastWriter {
    writer: BufWriter<File>,
    started: Instant,
    bytes_written: u64,
    max_bytes: u64,
    truncated: bool,
    /// Up to 3 trailing bytes of an incomplete UTF-8 sequence carried to the next chunk.
    utf8_carry: Vec<u8>,
}

impl CastWriter {
    /// Creates (or replaces) an asciicast v2 file without following symlinks and writes the
    /// mandatory JSON header line.
    pub fn create(
        path: &Path,
        cols: u16,
        rows: u16,
        shell: &str,
        max_bytes: u64,
    ) -> CoreResult<Self> {
        if let Some(parent) = path.parent() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                if !parent.exists() {
                    std::fs::DirBuilder::new()
                        .recursive(true)
                        .mode(0o700)
                        .create(parent)?;
                }
            }
            #[cfg(not(unix))]
            {
                std::fs::create_dir_all(parent)?;
            }
        }
        let mut opts = OpenOptions::new();
        opts.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = opts.open(path)?;
        let mut writer = BufWriter::new(file);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let header = json!({
            "version": 2,
            "width": cols,
            "height": rows,
            "timestamp": timestamp,
            "env": {
                "TERM": "xterm-256color",
                "SHELL": shell,
            }
        });
        let mut line = serde_json::to_string(&header)
            .map_err(|e| CoreError::Storage(format!("serialize cast header: {e}")))?;
        line.push('\n');
        writer.write_all(line.as_bytes())?;
        writer.flush()?;

        Ok(Self {
            writer,
            started: Instant::now(),
            bytes_written: line.len() as u64,
            max_bytes: max_bytes.max(4096),
            truncated: false,
            utf8_carry: Vec::with_capacity(4),
        })
    }

    fn decode_utf8_streaming(&mut self, bytes: &[u8]) -> String {
        let owned;
        let mut slice = if self.utf8_carry.is_empty() {
            bytes
        } else {
            let mut combined = Vec::with_capacity(self.utf8_carry.len() + bytes.len());
            combined.extend_from_slice(&self.utf8_carry);
            combined.extend_from_slice(bytes);
            self.utf8_carry.clear();
            owned = combined;
            owned.as_slice()
        };

        let mut out = String::with_capacity(slice.len());
        loop {
            match std::str::from_utf8(slice) {
                Ok(valid) => {
                    out.push_str(valid);
                    break;
                }
                Err(e) => {
                    let valid_up_to = e.valid_up_to();
                    if valid_up_to > 0 {
                        // SAFETY: `valid_up_to` is guaranteed by `std::str::from_utf8` to be valid UTF-8.
                        if let Ok(valid) = std::str::from_utf8(&slice[..valid_up_to]) {
                            out.push_str(valid);
                        }
                    }
                    match e.error_len() {
                        Some(invalid_len) => {
                            out.push('\u{FFFD}');
                            slice = &slice[valid_up_to + invalid_len..];
                        }
                        None => {
                            // Incomplete multi-byte sequence at chunk end: carry up to 3 bytes.
                            self.utf8_carry.extend_from_slice(&slice[valid_up_to..]);
                            break;
                        }
                    }
                }
            }
        }
        out
    }

    fn write_event_text(&mut self, text: &str) {
        if text.is_empty() || self.truncated {
            return;
        }
        let elapsed = self.started.elapsed().as_secs_f64();
        let Ok(mut line) = serde_json::to_string(&json!([elapsed, "o", text])) else {
            return;
        };
        line.push('\n');

        if self.bytes_written.saturating_add(line.len() as u64) > self.max_bytes {
            self.truncated = true;
            let _ = self.writer.flush();
            return;
        }

        if self.writer.write_all(line.as_bytes()).is_ok() {
            self.bytes_written = self.bytes_written.saturating_add(line.len() as u64);
            let _ = self.writer.flush();
        }
    }

    /// Appends one asciicast v2 output event `[elapsed_secs, "o", data]`.
    pub fn record_output(&mut self, bytes: &[u8]) {
        if bytes.is_empty() || self.truncated {
            return;
        }
        let text = self.decode_utf8_streaming(bytes);
        self.write_event_text(&text);
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    pub fn is_truncated(&self) -> bool {
        self.truncated
    }
}

impl Drop for CastWriter {
    fn drop(&mut self) {
        if !self.utf8_carry.is_empty() && !self.truncated {
            let leftover = std::mem::take(&mut self.utf8_carry);
            let text = String::from_utf8_lossy(&leftover).into_owned();
            self.write_event_text(&text);
        }
        let _ = self.writer.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cast_writer_emits_valid_asciicast_v2_and_respects_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = safe_cast_path(dir.path(), "sess-123").unwrap();
        let mut writer = CastWriter::create(&path, 100, 30, "/bin/sh", 4096).unwrap();
        writer.record_output(b"hello world\r\n");
        writer.record_output(b"\x1b[32mok\x1b[0m\r\n");
        drop(writer);

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);

        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["version"], 2);
        assert_eq!(header["width"], 100);
        assert_eq!(header["height"], 30);

        let ev1: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(ev1[1], "o");
        assert_eq!(ev1[2], "hello world\r\n");
    }

    #[test]
    fn cast_writer_preserves_multibyte_utf8_split_across_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = safe_cast_path(dir.path(), "sess-utf8").unwrap();
        let mut writer = CastWriter::create(&path, 80, 24, "/bin/sh", 4096).unwrap();
        let bytes = "你好世界".as_bytes();
        // Split right in the middle of the second 3-byte Chinese character.
        writer.record_output(&bytes[..4]);
        writer.record_output(&bytes[4..]);
        drop(writer);

        let content = std::fs::read_to_string(&path).unwrap();
        let combined: String = content
            .lines()
            .skip(1)
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter_map(|v| v.get(2).and_then(|s| s.as_str()).map(ToOwned::to_owned))
            .collect();
        assert_eq!(combined, "你好世界");
        assert!(!combined.contains('\u{FFFD}'));
    }

    #[test]
    fn safe_cast_path_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        assert!(safe_cast_path(dir.path(), "../etc/passwd").is_err());
        assert!(safe_cast_path(dir.path(), "a/b").is_err());
        assert!(safe_cast_path(dir.path(), "").is_err());
    }
}
