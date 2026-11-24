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

/// Append-only asciicast v2 file writer with hard byte-size capping.
pub struct CastWriter {
    writer: BufWriter<File>,
    started: Instant,
    bytes_written: u64,
    max_bytes: u64,
    truncated: bool,
}

impl CastWriter {
    /// Creates (or replaces) an asciicast v2 file and writes the mandatory JSON header line.
    pub fn create(
        path: &Path,
        cols: u16,
        rows: u16,
        shell: &str,
        max_bytes: u64,
    ) -> CoreResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;
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
        })
    }

    /// Appends one asciicast v2 output event `[elapsed_secs, "o", data]`.
    pub fn record_output(&mut self, bytes: &[u8]) {
        if bytes.is_empty() || self.truncated {
            return;
        }
        let elapsed = self.started.elapsed().as_secs_f64();
        let text = String::from_utf8_lossy(bytes);
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

    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    pub fn is_truncated(&self) -> bool {
        self.truncated
    }
}

impl Drop for CastWriter {
    fn drop(&mut self) {
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
    fn safe_cast_path_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        assert!(safe_cast_path(dir.path(), "../etc/passwd").is_err());
        assert!(safe_cast_path(dir.path(), "a/b").is_err());
        assert!(safe_cast_path(dir.path(), "").is_err());
    }
}
