//! `host` on macOS: rustix and std for the safe calls, `unix::sys` for `sigaction`, the
//! signal mask, and the extended ACL reads.

use std::ffi::CString;
use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use super::{
    Access, EntryKind, Privacy, ProcessIdentity, SigchldDisposition, SignalState, UnixSignal,
};
use crate::unix::sys;

pub(super) fn process_identity(pid: u32) -> io::Result<Option<ProcessIdentity>> {
    crate::unix::identify_live(pid)
}

fn raw_signal(s: UnixSignal) -> i32 {
    match s {
        UnixSignal::Term => libc::SIGTERM,
        UnixSignal::Int => libc::SIGINT,
        UnixSignal::Hup => libc::SIGHUP,
        UnixSignal::Chld => libc::SIGCHLD,
    }
}

pub(super) fn reset_process_signals() -> io::Result<()> {
    sys::reset_process_signals()
}

pub(super) fn signal_state() -> io::Result<SignalState> {
    let (handler, flags, mask) = sys::signal_state()?;
    let sigchld = if handler == libc::SIG_DFL {
        SigchldDisposition::Default
    } else if handler == libc::SIG_IGN {
        SigchldDisposition::Ignore
    } else {
        SigchldDisposition::Handler
    };
    Ok(SignalState {
        sigchld,
        no_cldwait: flags & libc::SA_NOCLDWAIT != 0,
        sigchld_flags: flags,
        blocked: UnixSignal::ALL
            .into_iter()
            .filter(|s| sys::in_set(mask, raw_signal(*s)))
            .collect(),
    })
}

pub(super) fn default_state_base() -> io::Result<PathBuf> {
    match std::env::var_os("HOME") {
        Some(h) if Path::new(&h).is_absolute() => Ok(PathBuf::from(h)),
        _ => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "HOME is not set to an absolute path",
        )),
    }
}

pub(super) fn create_private_dir(path: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new().mode(0o700).create(path)
}

fn judge(
    m: &Metadata,
    kind: EntryKind,
    acl_allows: impl FnOnce() -> io::Result<bool>,
) -> io::Result<Privacy> {
    let ft = m.file_type();
    if ft.is_symlink() {
        return Ok(Privacy::Link);
    }
    let of_kind = match kind {
        EntryKind::File => ft.is_file(),
        EntryKind::Directory => ft.is_dir(),
    };
    if !of_kind {
        return Ok(Privacy::NotOfKind);
    }
    if m.uid() != rustix::process::geteuid().as_raw() {
        return Ok(Privacy::NotOwned);
    }
    let mode = m.mode() & 0o7777;
    if mode & 0o077 != 0 {
        return Ok(Privacy::ModeTooOpen { mode });
    }
    if acl_allows()? {
        return Ok(Privacy::AclGrantsOthers);
    }
    Ok(Privacy::Private)
}

pub(super) fn privacy(path: &Path, kind: EntryKind) -> io::Result<Privacy> {
    let m = std::fs::symlink_metadata(path)?;
    judge(&m, kind, || {
        let c = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in the path"))?;
        sys::acl_allows_link(&c)
    })
}

pub(super) fn handle_privacy(file: &File) -> io::Result<Privacy> {
    let m = file.metadata()?;
    judge(&m, EntryKind::File, || sys::acl_allows_fd(file.as_fd()))
}

fn not_regular() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "not a regular file")
}

pub(super) fn open_no_follow(path: &Path, access: Access) -> io::Result<File> {
    let mut o = OpenOptions::new();
    match access {
        Access::Read => {
            o.read(true);
        }
        Access::ReadWriteCreate => {
            o.read(true).write(true).create(true).mode(0o600);
        }
        Access::AppendCreate => {
            o.append(true).create(true).mode(0o600);
        }
    }
    // Non-blocking, so a FIFO or device fails the type check below instead of blocking the
    // open; for a regular file the flag changes nothing.
    o.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let f = o.open(path).map_err(|e| {
        if e.raw_os_error() == Some(libc::ELOOP) {
            io::Error::new(io::ErrorKind::InvalidInput, "a link")
        } else {
            e
        }
    })?;
    if !f.metadata()?.file_type().is_file() {
        return Err(not_regular());
    }
    Ok(f)
}

pub(super) fn create_new_private(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

pub(super) fn hard_links(path: &Path) -> io::Result<u64> {
    Ok(std::fs::symlink_metadata(path)?.nlink())
}

pub(super) fn is_reparse(_m: &Metadata) -> bool {
    false
}

/// Nothing: a stop on Unix is `SIGTERM`.
pub(super) struct StopInner;

pub(super) fn listen_for_stop(
    _instance: &str,
    _on_stop: Box<dyn FnOnce() + Send>,
) -> io::Result<(String, StopInner)> {
    Ok((String::new(), StopInner))
}
