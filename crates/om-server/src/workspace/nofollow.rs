//! Reading a file under a root without following symlinks (review P1-5).
//!
//! Each path component is opened relative to its parent's descriptor with
//! `O_NOFOLLOW`, so a symlink planted (or swapped in) anywhere on the path makes the
//! open fail instead of escaping the root. The final file is opened `O_NONBLOCK` (a
//! FIFO cannot hang the server), checked with `fstat` to be a regular file, and read
//! with a size cap.

use om_core::{CoreError, CoreResult};
use std::ffi::CString;
use std::fs::File;
use std::io::Read;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

fn open_at(
    dir: Option<&OwnedFd>,
    name: &std::ffi::OsStr,
    flags: libc::c_int,
) -> std::io::Result<OwnedFd> {
    let c = CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let dirfd = dir.map_or(libc::AT_FDCWD, std::os::fd::AsRawFd::as_raw_fd);
    // SAFETY: `c` is a valid NUL-terminated string and `dirfd` a live descriptor (or
    // AT_FDCWD); the returned descriptor is owned by the `OwnedFd`.
    let fd = unsafe { libc::openat(dirfd, c.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was just returned by `openat` and is not owned elsewhere.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn not_found_or_invalid(e: std::io::Error, rel: &str) -> CoreError {
    match e.raw_os_error() {
        Some(libc::ENOENT) => CoreError::NotFound(format!("file {rel}")),
        Some(libc::ELOOP | libc::ENOTDIR) => {
            CoreError::InvalidInput(format!("path escapes the worktree: {rel}"))
        }
        _ => CoreError::Workspace(format!("reading {rel}: {e}")),
    }
}

/// Reads `root/rel` (rel: normal components only) up to `max` bytes.
pub(super) fn read(root: &Path, rel: &Path, max: u64, display: &str) -> CoreResult<Vec<u8>> {
    let mut dir = open_at(
        None,
        root.as_os_str(),
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
    )
    .map_err(|e| CoreError::Workspace(format!("opening worktree: {e}")))?;
    let comps: Vec<_> = rel.components().collect();
    let Some((last, parents)) = comps.split_last() else {
        return Err(CoreError::InvalidInput("empty path".into()));
    };
    for c in parents {
        let Component::Normal(name) = c else {
            return Err(CoreError::InvalidInput(format!("invalid path {display:?}")));
        };
        dir = open_at(
            Some(&dir),
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
        .map_err(|e| not_found_or_invalid(e, display))?;
    }
    let Component::Normal(name) = last else {
        return Err(CoreError::InvalidInput(format!("invalid path {display:?}")));
    };
    let fd = open_at(
        Some(&dir),
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY,
    )
    .map_err(|e| not_found_or_invalid(e, display))?;
    let file = File::from(fd);
    let meta = file
        .metadata()
        .map_err(|e| CoreError::Workspace(format!("reading {display}: {e}")))?;
    if !meta.is_file() {
        return Err(CoreError::InvalidInput(format!(
            "not a regular file: {display}"
        )));
    }
    if meta.len() > max {
        return Err(CoreError::InvalidInput(format!(
            "file is larger than {max} bytes"
        )));
    }
    let mut buf = Vec::with_capacity(meta.len() as usize);
    file.take(max.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|e| CoreError::Workspace(format!("reading {display}: {e}")))?;
    if buf.len() as u64 > max {
        return Err(CoreError::InvalidInput(format!(
            "file is larger than {max} bytes"
        )));
    }
    Ok(buf)
}

/// Reads up to `max` bytes of a regular file under `root/rel` without following any
/// symlinks along the path and without blocking on FIFOs. Returns `None` if the entry
/// is missing, escapes via a symlink, or is not a regular file; otherwise returns
/// `(bytes, truncated)`.
pub(super) fn read_truncated(root: &Path, rel: &Path, max: u64) -> Option<(Vec<u8>, bool)> {
    let mut dir = open_at(
        None,
        root.as_os_str(),
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
    )
    .ok()?;
    let comps: Vec<_> = rel.components().collect();
    let (last, parents) = comps.split_last()?;
    for c in parents {
        let Component::Normal(name) = c else {
            return None;
        };
        dir = open_at(
            Some(&dir),
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
        .ok()?;
    }
    let Component::Normal(name) = last else {
        return None;
    };
    let fd = open_at(
        Some(&dir),
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY,
    )
    .ok()?;
    let file = File::from(fd);
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let cap = meta.len().min(max) as usize;
    let mut buf = Vec::with_capacity(cap);
    file.take(max.saturating_add(1))
        .read_to_end(&mut buf)
        .ok()?;
    if buf.len() as u64 > max {
        buf.truncate(max as usize);
        Some((buf, true))
    } else {
        Some((buf, false))
    }
}

/// Returns the byte length of `root/rel` if every intermediate directory is a real
/// directory (not a symlink) and the leaf itself is not a directory.
pub(super) fn stat_non_dir(root: &Path, rel: &Path) -> Option<u64> {
    let mut dir = open_at(
        None,
        root.as_os_str(),
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
    )
    .ok()?;
    let comps: Vec<_> = rel.components().collect();
    let (last, parents) = comps.split_last()?;
    for c in parents {
        let Component::Normal(name) = c else {
            return None;
        };
        dir = open_at(
            Some(&dir),
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
        .ok()?;
    }
    let Component::Normal(name) = last else {
        return None;
    };
    let c = CString::new(name.as_bytes()).ok()?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `dir` is an open directory fd, `c` is NUL-terminated, and `st` is valid.
    let rc = unsafe {
        libc::fstatat(
            std::os::fd::AsRawFd::as_raw_fd(&dir),
            c.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return None;
    }
    let kind = st.st_mode & libc::S_IFMT;
    if kind == libc::S_IFDIR {
        return None;
    }
    u64::try_from(st.st_size).ok()
}

/// Verifies that `root/rel` exists and neither `root`, any intermediate directory,
/// nor the leaf entry is a symlink.
pub(super) fn verify_no_symlinks_under(root: &Path, rel: &Path) -> CoreResult<()> {
    let mut dir = open_at(
        None,
        root.as_os_str(),
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
    )
    .map_err(|e| CoreError::Workspace(format!("opening {}: {e}", root.display())))?;
    let comps: Vec<_> = rel.components().collect();
    let Some((last, parents)) = comps.split_last() else {
        return Ok(());
    };
    for c in parents {
        let Component::Normal(name) = c else {
            return Err(CoreError::Workspace(format!(
                "invalid path component in {}",
                rel.display()
            )));
        };
        dir = open_at(
            Some(&dir),
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
        .map_err(|e| {
            CoreError::Workspace(format!(
                "unsafe or missing directory under {}: {e}",
                root.display()
            ))
        })?;
    }
    let Component::Normal(name) = last else {
        return Err(CoreError::Workspace(format!(
            "invalid leaf component in {}",
            rel.display()
        )));
    };
    let c = CString::new(name.as_bytes())
        .map_err(|_| CoreError::Workspace("invalid NUL in path".into()))?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `dir` is an open directory fd, `c` is NUL-terminated, and `st` is valid.
    let rc = unsafe {
        libc::fstatat(
            std::os::fd::AsRawFd::as_raw_fd(&dir),
            c.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return Err(CoreError::Workspace(format!(
            "session git dir is missing {}",
            root.join(rel).display()
        )));
    }
    if (st.st_mode & libc::S_IFMT) == libc::S_IFLNK {
        return Err(CoreError::Workspace(format!(
            "session git dir entry must not be a symlink: {}",
            root.join(rel).display()
        )));
    }
    Ok(())
}
