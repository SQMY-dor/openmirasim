//! Native PTY runtime (`portable-pty` + bounded ring buffer + asciicast v2 `.cast` recording,
//! Phase 3).
//!
//! Design invariants:
//! - PTY raw output NEVER enters SQLite; scrollback lives in a strictly bounded per-session
//!   [`ByteRingBuffer`] (default 512 KiB) and an append-only capped `.cast` file.
//! - Output fan-out uses a bounded `tokio::sync::broadcast` channel so slow `/ws/pty`
//!   clients experience controlled lag without stalling the PTY reader or `/ws/events`.
//! - Child processes run with a cleared minimal environment (never inheriting `OM_TOKEN` or
//!   upstream provider API keys) and are always reaped by a dedicated waiter thread to prevent
//!   zombie processes.

pub mod cast;
pub mod ring;

use bytes::Bytes;
use cast::{CastWriter, DEFAULT_MAX_CAST_BYTES, safe_cast_path};
use om_core::ids::SessionId;
use om_core::{CoreError, CoreResult};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use ring::{ByteRingBuffer, DEFAULT_RING_CAPACITY};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};

const READ_CHUNK_BYTES: usize = 4096;
const INPUT_QUEUE_CAPACITY: usize = 128;

/// Configuration for [`PtyManager`].
#[derive(Debug, Clone)]
pub struct PtyConfig {
    /// Directory storing `<session_id>.cast` asciicast v2 recordings (optional).
    pub recordings_dir: Option<PathBuf>,
    /// Maximum scrollback bytes kept in RAM per PTY session.
    pub ring_capacity: usize,
    /// Maximum `.cast` recording file size per session.
    pub max_cast_bytes: u64,
    /// Maximum simultaneous live PTY sessions on the host.
    pub max_sessions: usize,
    /// Broadcast channel capacity per PTY (chunks dropped cleanly for lagging clients).
    pub broadcast_capacity: usize,
    /// Default interactive shell spawned for a session's PTY.
    pub default_shell: String,
}

impl Default for PtyConfig {
    fn default() -> Self {
        let default_shell = if Path::new("/bin/bash").is_file() {
            "/bin/bash".into()
        } else {
            "/bin/sh".into()
        };
        Self {
            recordings_dir: None,
            ring_capacity: DEFAULT_RING_CAPACITY,
            max_cast_bytes: DEFAULT_MAX_CAST_BYTES,
            max_sessions: 32,
            broadcast_capacity: 256,
            default_shell,
        }
    }
}

/// Public status snapshot returned by `GET /api/v1/sessions/:id/pty`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtyStatus {
    pub active: bool,
    pub cols: u16,
    pub rows: u16,
    pub has_recording: bool,
    pub recording_bytes: u64,
    pub clients: usize,
}

/// Broadcast item delivered to `/ws/pty` subscribers.
#[derive(Debug, Clone)]
pub enum PtyOutput {
    Chunk(Bytes),
    Exited(Option<u32>),
}

enum PtyControl {
    Write(Bytes),
    Resize { cols: u16, rows: u16 },
    Kill,
}

struct LivePty {
    cols: Arc<AtomicU16>,
    rows: Arc<AtomicU16>,
    active: Arc<AtomicBool>,
    recording_bytes: Arc<AtomicU64>,
    ring: Arc<Mutex<ByteRingBuffer>>,
    output_tx: broadcast::Sender<PtyOutput>,
    control_tx: mpsc::Sender<PtyControl>,
    killer: Arc<Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>>,
}

/// Client write/resize control handle split from [`PtyAttachment`].
#[derive(Clone)]
pub struct PtyWriter {
    control_tx: mpsc::Sender<PtyControl>,
}

impl PtyWriter {
    /// Queues raw stdin bytes to the PTY (bounded queue; fails cleanly if closed).
    pub async fn write_input(&self, data: Bytes) -> CoreResult<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.control_tx
            .send(PtyControl::Write(data))
            .await
            .map_err(|_| CoreError::InvalidInput("PTY session is no longer active".into()))
    }

    /// Resizes the PTY window (`cols` x `rows`, clamped to sane terminal bounds).
    pub async fn resize(&self, cols: u16, rows: u16) -> CoreResult<()> {
        let cols = cols.clamp(10, 500);
        let rows = rows.clamp(4, 200);
        self.control_tx
            .send(PtyControl::Resize { cols, rows })
            .await
            .map_err(|_| CoreError::InvalidInput("PTY session is no longer active".into()))
    }
}

/// Client handle returned by [`PtyManager::attach_or_spawn`].
pub struct PtyAttachment {
    pub cols: u16,
    pub rows: u16,
    pub snapshot: Bytes,
    pub has_recording: bool,
    pub rx: broadcast::Receiver<PtyOutput>,
    control_tx: mpsc::Sender<PtyControl>,
}

impl PtyAttachment {
    /// Splits this attachment into its write/resize handle and output broadcast receiver.
    pub fn split(self) -> (PtyWriter, broadcast::Receiver<PtyOutput>) {
        (
            PtyWriter {
                control_tx: self.control_tx,
            },
            self.rx,
        )
    }
}

/// Thread-safe supervisor for per-session native PTYs and `.cast` recordings.
#[derive(Clone)]
pub struct PtyManager {
    config: Arc<PtyConfig>,
    live: Arc<Mutex<HashMap<SessionId, Arc<LivePty>>>>,
}

impl PtyManager {
    pub fn new(config: PtyConfig) -> Self {
        if let Some(dir) = &config.recordings_dir {
            let _ = std::fs::create_dir_all(dir);
        }
        Self {
            config: Arc::new(config),
            live: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Attaches to an existing live PTY for `session_id` or spawns a new one inside `cwd`.
    ///
    /// When `allow_spawn` is `false` (e.g. `Viewer` role), attaches read-only only if a PTY
    /// is already live, returning `NotFound` otherwise.
    pub fn attach_or_spawn(
        &self,
        session_id: &SessionId,
        cwd: &str,
        cols: u16,
        rows: u16,
        allow_spawn: bool,
    ) -> CoreResult<PtyAttachment> {
        let cols = cols.clamp(20, 500);
        let rows = rows.clamp(5, 200);

        let mut map = self
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Drop stale exited entries before deciding whether to attach or spawn.
        if let Some(existing) = map.get(session_id)
            && !existing.active.load(Ordering::Acquire)
        {
            map.remove(session_id);
        }

        if let Some(existing) = map.get(session_id) {
            let snapshot = Bytes::from(
                existing
                    .ring
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .snapshot(),
            );
            return Ok(PtyAttachment {
                cols: existing.cols.load(Ordering::Relaxed),
                rows: existing.rows.load(Ordering::Relaxed),
                snapshot,
                has_recording: self.config.recordings_dir.is_some(),
                rx: existing.output_tx.subscribe(),
                control_tx: existing.control_tx.clone(),
            });
        }

        if !allow_spawn {
            return Err(CoreError::NotFound(format!(
                "no active PTY for session {session_id}"
            )));
        }

        if map.len() >= self.config.max_sessions {
            return Err(CoreError::LimitExceeded(format!(
                "maximum concurrent PTY sessions reached ({})",
                self.config.max_sessions
            )));
        }

        let workdir = PathBuf::from(cwd);
        let effective_cwd = if !cwd.is_empty() && workdir.is_dir() {
            workdir
        } else {
            std::env::temp_dir()
        };

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| CoreError::Io(std::io::Error::other(format!("openpty failed: {e}"))))?;

        let mut cmd = CommandBuilder::new(&self.config.default_shell);
        cmd.cwd(&effective_cwd);
        // Clear server environment so OM_TOKEN, DB paths, and upstream API keys NEVER leak into PTY.
        cmd.env_clear();
        cmd.env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| {
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/bin".into()
            }),
        );
        cmd.env("HOME", effective_cwd.to_string_lossy().as_ref());
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("LANG", "C.UTF-8");
        cmd.env("LC_ALL", "C.UTF-8");
        cmd.env("PS1", "openmirasim$ ");

        let mut child = pair.slave.spawn_command(cmd).map_err(|e| {
            CoreError::Io(std::io::Error::other(format!(
                "spawn PTY shell `{}` failed: {e}",
                self.config.default_shell
            )))
        })?;
        // CRITICAL: close slave FD in parent so master reader sees EOF when child exits.
        drop(pair.slave);

        let killer: Arc<Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>> =
            Arc::new(Mutex::new(child.clone_killer()));
        let mut reader = pair.master.try_clone_reader().map_err(|e| {
            if let Ok(mut k) = killer.lock() {
                let _ = k.kill();
            }
            let _ = child.wait();
            CoreError::Io(std::io::Error::other(format!("clone PTY reader: {e}")))
        })?;
        let mut writer = pair.master.take_writer().map_err(|e| {
            if let Ok(mut k) = killer.lock() {
                let _ = k.kill();
            }
            let _ = child.wait();
            CoreError::Io(std::io::Error::other(format!("take PTY writer: {e}")))
        })?;
        let master = pair.master;

        let mut cast_writer = if let Some(dir) = &self.config.recordings_dir {
            let cast_path = safe_cast_path(dir, session_id.as_str()).inspect_err(|_| {
                if let Ok(mut k) = killer.lock() {
                    let _ = k.kill();
                }
                let _ = child.wait();
            })?;
            Some(
                CastWriter::create(
                    &cast_path,
                    cols,
                    rows,
                    &self.config.default_shell,
                    self.config.max_cast_bytes,
                )
                .inspect_err(|_| {
                    if let Ok(mut k) = killer.lock() {
                        let _ = k.kill();
                    }
                    let _ = child.wait();
                })?,
            )
        } else {
            None
        };

        let cols_atom = Arc::new(AtomicU16::new(cols));
        let rows_atom = Arc::new(AtomicU16::new(rows));
        let active_atom = Arc::new(AtomicBool::new(true));
        let rec_bytes = Arc::new(AtomicU64::new(
            cast_writer
                .as_ref()
                .map(CastWriter::bytes_written)
                .unwrap_or(0),
        ));
        let ring = Arc::new(Mutex::new(ByteRingBuffer::new(self.config.ring_capacity)));
        let (output_tx, output_rx) = broadcast::channel(self.config.broadcast_capacity);
        let (control_tx, mut control_rx) = mpsc::channel::<PtyControl>(INPUT_QUEUE_CAPACITY);

        let live_entry = Arc::new(LivePty {
            cols: cols_atom.clone(),
            rows: rows_atom.clone(),
            active: active_atom.clone(),
            recording_bytes: rec_bytes.clone(),
            ring: ring.clone(),
            output_tx: output_tx.clone(),
            control_tx: control_tx.clone(),
            killer: killer.clone(),
        });
        map.insert(session_id.clone(), live_entry);

        // 1. Dedicated child reaper thread (prevents zombie processes unconditionally).
        let exit_code_slot: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
        let exit_code_writer = exit_code_slot.clone();
        std::thread::Builder::new()
            .name(format!("pty-reap-{}", session_id.as_str()))
            .spawn(move || {
                let code = child.wait().ok().map(|s| s.exit_code());
                if let Ok(mut slot) = exit_code_writer.lock() {
                    *slot = code;
                }
            })
            .ok();

        // 2. Dedicated writer/resize/kill control thread.
        let ctrl_cols = cols_atom.clone();
        let ctrl_rows = rows_atom.clone();
        let ctrl_killer = killer.clone();
        std::thread::Builder::new()
            .name(format!("pty-ctrl-{}", session_id.as_str()))
            .spawn(move || {
                while let Some(cmd) = control_rx.blocking_recv() {
                    match cmd {
                        PtyControl::Write(bytes) => {
                            if writer.write_all(&bytes).is_err() {
                                break;
                            }
                            let _ = writer.flush();
                        }
                        PtyControl::Resize { cols, rows } => {
                            if master
                                .resize(PtySize {
                                    rows,
                                    cols,
                                    pixel_width: 0,
                                    pixel_height: 0,
                                })
                                .is_ok()
                            {
                                ctrl_cols.store(cols, Ordering::Relaxed);
                                ctrl_rows.store(rows, Ordering::Relaxed);
                            }
                        }
                        PtyControl::Kill => {
                            if let Ok(mut k) = ctrl_killer.lock() {
                                let _ = k.kill();
                            }
                            break;
                        }
                    }
                }
                drop(writer);
                drop(master);
            })
            .ok();

        // 3. Dedicated master reader thread: ring buffer + .cast + broadcast fan-out.
        let live_map = self.live.clone();
        let sid_owned = session_id.clone();
        let ctrl_on_eof = control_tx.clone();
        std::thread::Builder::new()
            .name(format!("pty-read-{}", session_id.as_str()))
            .spawn(move || {
                let mut buf = [0u8; READ_CHUNK_BYTES];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let slice = &buf[..n];
                            if let Ok(mut guard) = ring.lock() {
                                guard.push(slice);
                            }
                            if let Some(cw) = cast_writer.as_mut() {
                                cw.record_output(slice);
                                rec_bytes.store(cw.bytes_written(), Ordering::Relaxed);
                            }
                            let _ = output_tx.send(PtyOutput::Chunk(Bytes::copy_from_slice(slice)));
                        }
                    }
                }
                active_atom.store(false, Ordering::Release);
                let _ = ctrl_on_eof.try_send(PtyControl::Kill);
                let code = exit_code_slot.lock().ok().and_then(|g| *g);
                let _ = output_tx.send(PtyOutput::Exited(code));
                if let Ok(mut guard) = live_map.lock()
                    && guard
                        .get(&sid_owned)
                        .is_some_and(|e| !e.active.load(Ordering::Acquire))
                {
                    guard.remove(&sid_owned);
                }
            })
            .ok();

        Ok(PtyAttachment {
            cols,
            rows,
            snapshot: Bytes::new(),
            has_recording: self.config.recordings_dir.is_some(),
            rx: output_rx,
            control_tx,
        })
    }

    /// Terminates a live PTY for `session_id` (keeps any `.cast` recording on disk).
    pub fn kill(&self, session_id: &SessionId) -> bool {
        let entry = self
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id);
        if let Some(pty) = entry {
            pty.active.store(false, Ordering::Release);
            if let Ok(mut k) = pty.killer.lock() {
                let _ = k.kill();
            }
            let _ = pty.control_tx.try_send(PtyControl::Kill);
            true
        } else {
            false
        }
    }

    /// Terminates every active PTY (called on graceful server shutdown).
    pub fn shutdown_all(&self) {
        let drained: Vec<Arc<LivePty>> = {
            let mut map = self
                .live
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.drain().map(|(_, v)| v).collect()
        };
        for pty in drained {
            pty.active.store(false, Ordering::Release);
            if let Ok(mut k) = pty.killer.lock() {
                let _ = k.kill();
            }
            let _ = pty.control_tx.try_send(PtyControl::Kill);
        }
    }

    /// Returns status and recording metadata for `session_id`.
    pub fn status(&self, session_id: &SessionId) -> PtyStatus {
        let (active, cols, rows, clients, live_bytes) = {
            let map = self
                .live
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(pty) = map.get(session_id)
                && pty.active.load(Ordering::Acquire)
            {
                (
                    true,
                    pty.cols.load(Ordering::Relaxed),
                    pty.rows.load(Ordering::Relaxed),
                    pty.output_tx.receiver_count(),
                    pty.recording_bytes.load(Ordering::Relaxed),
                )
            } else {
                (false, 0, 0, 0, 0)
            }
        };

        let disk_bytes = self
            .config
            .recordings_dir
            .as_deref()
            .and_then(|dir| safe_cast_path(dir, session_id.as_str()).ok())
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .unwrap_or(0);

        let recording_bytes = live_bytes.max(disk_bytes);
        PtyStatus {
            active,
            cols,
            rows,
            has_recording: recording_bytes > 0,
            recording_bytes,
            clients,
        }
    }

    /// Reads the asciicast v2 (`.cast`) recording bytes for `session_id`.
    pub fn read_recording(&self, session_id: &SessionId) -> CoreResult<Vec<u8>> {
        let Some(dir) = &self.config.recordings_dir else {
            return Err(CoreError::NotFound(format!(
                "PTY recording for session {session_id}"
            )));
        };
        let path = safe_cast_path(dir, session_id.as_str())?;
        if !path.is_file() {
            return Err(CoreError::NotFound(format!(
                "PTY recording for session {session_id}"
            )));
        }
        Ok(std::fs::read(path)?)
    }
}
