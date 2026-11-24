//! Native PTY runtime (`portable-pty` + bounded ring buffer + asciicast v2 `.cast` recording,
//! Phase 3).
//!
//! Design invariants:
//! - PTY raw output NEVER enters SQLite; scrollback lives in a strictly bounded per-session
//!   [`ByteRingBuffer`] (default 512 KiB) and an append-only capped `.cast` file.
//! - Output fan-out uses a bounded `tokio::sync::broadcast` channel so slow `/ws/pty`
//!   clients experience controlled lag without stalling the PTY reader or `/ws/events`.
//! - Child shells execute inside the session's configured [`Sandbox`] (with `keep_session = true`
//!   so `bwrap` preserves the controlling TTY while isolating namespaces, filesystem, and env),
//!   with a cleared minimal environment and dedicated process-group reaping.

pub mod cast;
pub mod ring;

use bytes::Bytes;
use cast::{CastWriter, DEFAULT_MAX_CAST_BYTES, safe_cast_path};
use om_core::ids::SessionId;
use om_core::sandbox::{CommandSpec, DirectSandbox, Sandbox, SandboxSpec};
use om_core::{CoreError, CoreResult};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use ring::{ByteRingBuffer, DEFAULT_RING_CAPACITY};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;
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
    killed: Arc<AtomicBool>,
    recording_bytes: Arc<AtomicU64>,
    ring: Arc<Mutex<ByteRingBuffer>>,
    output_tx: broadcast::Sender<PtyOutput>,
    control_tx: mpsc::Sender<PtyControl>,
    killer: Arc<Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>>,
    child_pid: Option<u32>,
}

fn terminate_pty_child(
    killer: &Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>,
    child_pid: Option<u32>,
) {
    #[cfg(unix)]
    if let Some(pid) = child_pid
        && pid > 0
        && let Ok(pgid) = i32::try_from(pid)
    {
        unsafe {
            libc::kill(-pgid, libc::SIGHUP);
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
    if let Ok(mut k) = killer.lock() {
        let _ = k.kill();
    }
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

/// Client handle returned by [`PtyManager::attach_or_spawn_in_sandbox`].
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

#[derive(Default)]
struct LiveTable {
    entries: HashMap<SessionId, Arc<LivePty>>,
    spawning: HashSet<SessionId>,
}

struct SpawnReservation {
    table: Arc<Mutex<LiveTable>>,
    session_id: SessionId,
    committed: bool,
}

impl Drop for SpawnReservation {
    fn drop(&mut self) {
        if !self.committed {
            let mut guard = self.table.lock().unwrap_or_else(PoisonError::into_inner);
            guard.spawning.remove(&self.session_id);
        }
    }
}

/// Thread-safe supervisor for per-session native PTYs and `.cast` recordings.
#[derive(Clone)]
pub struct PtyManager {
    config: Arc<PtyConfig>,
    live: Arc<Mutex<LiveTable>>,
}

impl PtyManager {
    pub fn new(config: PtyConfig) -> Self {
        if let Some(dir) = &config.recordings_dir {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                let _ = std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(dir);
            }
            #[cfg(not(unix))]
            {
                let _ = std::fs::create_dir_all(dir);
            }
        }
        Self {
            config: Arc::new(config),
            live: Arc::new(Mutex::new(LiveTable::default())),
        }
    }

    /// Attaches to an existing live PTY for `session_id` or spawns a new one inside `cwd`
    /// using `DirectSandbox` (prefer [`Self::attach_or_spawn_in_sandbox`] in API handlers).
    pub fn attach_or_spawn(
        &self,
        session_id: &SessionId,
        cwd: &str,
        cols: u16,
        rows: u16,
        allow_spawn: bool,
    ) -> CoreResult<PtyAttachment> {
        self.attach_or_spawn_in_sandbox(
            session_id,
            cwd,
            Arc::new(DirectSandbox),
            cols,
            rows,
            allow_spawn,
        )
    }

    /// Attaches to an existing live PTY for `session_id` or spawns a new sandboxed shell in `cwd`.
    ///
    /// When `allow_spawn` is `false` (e.g. `Viewer` role), attaches read-only only if a PTY
    /// is already live, returning `NotFound` otherwise.
    pub fn attach_or_spawn_in_sandbox(
        &self,
        session_id: &SessionId,
        cwd: &str,
        sandbox: Arc<dyn Sandbox>,
        cols: u16,
        rows: u16,
        allow_spawn: bool,
    ) -> CoreResult<PtyAttachment> {
        let cols = cols.clamp(20, 500);
        let rows = rows.clamp(5, 200);

        // Phase 1: check existing live PTY or reserve a spawn slot without holding the mutex
        // across `openpty` / `fork` / `exec`.
        {
            let mut table = self.live.lock().unwrap_or_else(PoisonError::into_inner);

            if let Some(existing) = table.entries.get(session_id)
                && !existing.active.load(Ordering::Acquire)
            {
                table.entries.remove(session_id);
            }

            if let Some(existing) = table.entries.get(session_id) {
                // Hold `ring` lock across snapshot + subscribe so the client observes an exact
                // gap-free, duplicate-free handoff into live broadcast chunks.
                let ring_guard = existing.ring.lock().unwrap_or_else(PoisonError::into_inner);
                let snapshot = Bytes::from(ring_guard.snapshot());
                let rx = existing.output_tx.subscribe();
                drop(ring_guard);
                return Ok(PtyAttachment {
                    cols: existing.cols.load(Ordering::Relaxed),
                    rows: existing.rows.load(Ordering::Relaxed),
                    snapshot,
                    has_recording: self.config.recordings_dir.is_some(),
                    rx,
                    control_tx: existing.control_tx.clone(),
                });
            }

            if !allow_spawn {
                return Err(CoreError::NotFound(format!(
                    "no active PTY for session {session_id}"
                )));
            }

            if table.spawning.contains(session_id) {
                return Err(CoreError::InvalidInput(format!(
                    "PTY for session {session_id} is already starting"
                )));
            }

            let total = table.entries.len() + table.spawning.len();
            if total >= self.config.max_sessions {
                return Err(CoreError::LimitExceeded(format!(
                    "maximum concurrent PTY sessions reached ({})",
                    self.config.max_sessions
                )));
            }

            table.spawning.insert(session_id.clone());
        }

        let mut reservation = SpawnReservation {
            table: self.live.clone(),
            session_id: session_id.clone(),
            committed: false,
        };

        let workdir = PathBuf::from(cwd);
        if cwd.trim().is_empty() || !workdir.is_absolute() || !workdir.is_dir() {
            return Err(CoreError::InvalidInput(format!(
                "session working directory is missing or not absolute: {cwd}"
            )));
        }

        let mut base_cmd = CommandSpec::new(&self.config.default_shell).cwd(&workdir);
        base_cmd.clear_env = true;
        base_cmd = base_cmd
            .env(
                "PATH",
                std::env::var("PATH").unwrap_or_else(|_| {
                    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/bin".into()
                }),
            )
            .env("HOME", workdir.to_string_lossy().as_ref())
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .env("PS1", "openmirasim$ ");

        let sb_spec = SandboxSpec {
            writable_paths: vec![workdir.clone()],
            network: false,
            keep_session: true,
            ..SandboxSpec::default()
        };
        let wrapped = sandbox.wrap(&sb_spec, base_cmd)?;

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| CoreError::Io(std::io::Error::other(format!("openpty failed: {e}"))))?;

        let mut cmd = CommandBuilder::new(&wrapped.program);
        for arg in &wrapped.args {
            cmd.arg(arg);
        }
        if wrapped.clear_env {
            cmd.env_clear();
        }
        for (k, v) in &wrapped.env {
            cmd.env(k, v);
        }
        if let Some(dir) = &wrapped.cwd {
            cmd.cwd(dir);
        } else {
            cmd.cwd(&workdir);
        }

        let mut child = pair.slave.spawn_command(cmd).map_err(|e| {
            CoreError::Io(std::io::Error::other(format!(
                "spawn PTY `{}` failed: {e}",
                wrapped.program
            )))
        })?;
        // CRITICAL: close slave FD in parent so master reader sees EOF when child exits.
        drop(pair.slave);

        let child_pid = child.process_id();
        let killer: Arc<Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>> =
            Arc::new(Mutex::new(child.clone_killer()));
        let mut reader = pair.master.try_clone_reader().map_err(|e| {
            terminate_pty_child(&killer, child_pid);
            let _ = child.wait();
            CoreError::Io(std::io::Error::other(format!("clone PTY reader: {e}")))
        })?;
        let mut writer = pair.master.take_writer().map_err(|e| {
            terminate_pty_child(&killer, child_pid);
            let _ = child.wait();
            CoreError::Io(std::io::Error::other(format!("take PTY writer: {e}")))
        })?;
        let master = pair.master;

        let mut cast_writer = if let Some(dir) = &self.config.recordings_dir {
            let cast_path = safe_cast_path(dir, session_id.as_str()).inspect_err(|_| {
                terminate_pty_child(&killer, child_pid);
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
                    terminate_pty_child(&killer, child_pid);
                    let _ = child.wait();
                })?,
            )
        } else {
            None
        };

        let cols_atom = Arc::new(AtomicU16::new(cols));
        let rows_atom = Arc::new(AtomicU16::new(rows));
        let active_atom = Arc::new(AtomicBool::new(true));
        let killed_atom = Arc::new(AtomicBool::new(false));
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
            killed: killed_atom.clone(),
            recording_bytes: rec_bytes.clone(),
            ring: ring.clone(),
            output_tx: output_tx.clone(),
            control_tx: control_tx.clone(),
            killer: killer.clone(),
            child_pid,
        });

        // 1. Dedicated child reaper thread (prevents zombie processes unconditionally).
        let exit_sync: Arc<(Mutex<Option<Option<u32>>>, Condvar)> =
            Arc::new((Mutex::new(None), Condvar::new()));
        let exit_sync_writer = exit_sync.clone();
        if let Err(e) = std::thread::Builder::new()
            .name(format!("pty-reap-{}", session_id.as_str()))
            .spawn(move || {
                let code = child.wait().ok().map(|s| s.exit_code());
                let (lock, cvar) = &*exit_sync_writer;
                *lock.lock().unwrap_or_else(PoisonError::into_inner) = Some(code);
                cvar.notify_all();
            })
        {
            terminate_pty_child(&killer, child_pid);
            return Err(CoreError::Io(std::io::Error::other(format!(
                "failed to spawn pty-reap thread: {e}"
            ))));
        }

        // 2. Dedicated writer/resize/kill control thread.
        let ctrl_cols = cols_atom.clone();
        let ctrl_rows = rows_atom.clone();
        let ctrl_killer = killer.clone();
        let ctrl_killed = killed_atom.clone();
        if let Err(e) = std::thread::Builder::new()
            .name(format!("pty-ctrl-{}", session_id.as_str()))
            .spawn(move || {
                while let Some(cmd) = control_rx.blocking_recv() {
                    if ctrl_killed.load(Ordering::Acquire) {
                        terminate_pty_child(&ctrl_killer, child_pid);
                        break;
                    }
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
                            terminate_pty_child(&ctrl_killer, child_pid);
                            break;
                        }
                    }
                }
                drop(writer);
                drop(master);
            })
        {
            killed_atom.store(true, Ordering::Release);
            terminate_pty_child(&killer, child_pid);
            return Err(CoreError::Io(std::io::Error::other(format!(
                "failed to spawn pty-ctrl thread: {e}"
            ))));
        }

        // Commit entry before starting the reader thread so it can safely remove itself on EOF.
        {
            let mut table = self.live.lock().unwrap_or_else(PoisonError::into_inner);
            table.spawning.remove(session_id);
            table.entries.insert(session_id.clone(), live_entry);
            reservation.committed = true;
        }

        // 3. Dedicated master reader thread: ring buffer + .cast + broadcast fan-out.
        let live_table = self.live.clone();
        let sid_owned = session_id.clone();
        let ctrl_on_eof = control_tx.clone();
        let read_killed = killed_atom.clone();
        let read_killer = killer.clone();
        if let Err(e) = std::thread::Builder::new()
            .name(format!("pty-read-{}", session_id.as_str()))
            .spawn(move || {
                let mut buf = [0u8; READ_CHUNK_BYTES];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let slice = &buf[..n];
                            if let Some(cw) = cast_writer.as_mut() {
                                cw.record_output(slice);
                                rec_bytes.store(cw.bytes_written(), Ordering::Relaxed);
                            }
                            // Hold `ring` lock across push + broadcast send so late subscribers
                            // never miss or duplicate chunks between snapshot and stream.
                            let mut guard = ring.lock().unwrap_or_else(PoisonError::into_inner);
                            guard.push(slice);
                            let _ = output_tx.send(PtyOutput::Chunk(Bytes::copy_from_slice(slice)));
                        }
                    }
                }
                active_atom.store(false, Ordering::Release);
                read_killed.store(true, Ordering::Release);
                let _ = ctrl_on_eof.try_send(PtyControl::Kill);
                // Wait briefly for `pty-reap` to populate the child's exit code.
                let code = {
                    let (lock, cvar) = &*exit_sync;
                    let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                    if let Some(c) = *guard {
                        c
                    } else {
                        let (g, _) = cvar
                            .wait_timeout_while(guard, Duration::from_millis(200), |s| s.is_none())
                            .unwrap_or_else(PoisonError::into_inner);
                        g.flatten()
                    }
                };
                let _ = output_tx.send(PtyOutput::Exited(code));
                let mut table = live_table.lock().unwrap_or_else(PoisonError::into_inner);
                if table
                    .entries
                    .get(&sid_owned)
                    .is_some_and(|e| !e.active.load(Ordering::Acquire))
                {
                    table.entries.remove(&sid_owned);
                }
            })
        {
            killed_atom.store(true, Ordering::Release);
            active_atom.store(false, Ordering::Release);
            terminate_pty_child(&read_killer, child_pid);
            let _ = control_tx.try_send(PtyControl::Kill);
            self.live
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entries
                .remove(session_id);
            return Err(CoreError::Io(std::io::Error::other(format!(
                "failed to spawn pty-read thread: {e}"
            ))));
        }

        Ok(PtyAttachment {
            cols,
            rows,
            snapshot: Bytes::new(),
            has_recording: self.config.recordings_dir.is_some(),
            rx: output_rx,
            control_tx,
        })
    }

    /// Returns whether a live PTY is currently active or starting for `session_id`.
    pub fn is_active(&self, session_id: &SessionId) -> bool {
        let table = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        table.spawning.contains(session_id)
            || table
                .entries
                .get(session_id)
                .is_some_and(|e| e.active.load(Ordering::Acquire))
    }

    /// Terminates a live PTY for `session_id` (keeps any `.cast` recording on disk).
    pub fn kill(&self, session_id: &SessionId) -> bool {
        let entry = self
            .live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .remove(session_id);
        if let Some(pty) = entry {
            pty.active.store(false, Ordering::Release);
            pty.killed.store(true, Ordering::Release);
            terminate_pty_child(&pty.killer, pty.child_pid);
            let _ = pty.control_tx.try_send(PtyControl::Kill);
            true
        } else {
            false
        }
    }

    /// Terminates every active PTY (called on graceful server shutdown).
    pub fn shutdown_all(&self) {
        let drained: Vec<Arc<LivePty>> = {
            let mut table = self.live.lock().unwrap_or_else(PoisonError::into_inner);
            table.entries.drain().map(|(_, v)| v).collect()
        };
        for pty in drained {
            pty.active.store(false, Ordering::Release);
            pty.killed.store(true, Ordering::Release);
            terminate_pty_child(&pty.killer, pty.child_pid);
            let _ = pty.control_tx.try_send(PtyControl::Kill);
        }
    }

    /// Returns status and recording metadata for `session_id`.
    pub fn status(&self, session_id: &SessionId) -> PtyStatus {
        let (active, cols, rows, clients, live_bytes) = {
            let table = self.live.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(pty) = table.entries.get(session_id)
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
            .and_then(|p| std::fs::symlink_metadata(p).ok())
            .filter(|m| m.is_file())
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

    /// Resolves and validates the `.cast` recording path for `session_id`.
    pub fn recording_path(&self, session_id: &SessionId) -> CoreResult<PathBuf> {
        let Some(dir) = &self.config.recordings_dir else {
            return Err(CoreError::NotFound(format!(
                "PTY recording for session {session_id}"
            )));
        };
        let path = safe_cast_path(dir, session_id.as_str())?;
        let meta = std::fs::symlink_metadata(&path).map_err(|_| {
            CoreError::NotFound(format!("PTY recording for session {session_id}"))
        })?;
        if !meta.is_file() {
            return Err(CoreError::NotFound(format!(
                "PTY recording for session {session_id}"
            )));
        }
        Ok(path)
    }

    /// Reads the asciicast v2 (`.cast`) recording bytes for `session_id`.
    pub fn read_recording(&self, session_id: &SessionId) -> CoreResult<Vec<u8>> {
        let path = self.recording_path(session_id)?;
        Ok(std::fs::read(path)?)
    }
}
