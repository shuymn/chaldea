//! Process-level OS primitives of the daemon and the CLI (`docs/design/daemon-lifecycle.md`
//! §2.2): process identity, the process-wide signal state, the state directory's privacy
//! checks and owner-only files, opens that never follow a final link, and the stop listener.
//!
//! Every function is safe; the `unsafe` calls are in `unix::sys` and `windows::sys`, the only
//! modules allowed them (`docs/design/pty-unix.md` §2.7).

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

pub use crate::backend::ProcessIdentity;

#[cfg(target_os = "macos")]
#[path = "unix.rs"]
mod macos;
#[cfg(not(any(target_os = "macos", windows)))]
mod other;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
use macos as imp;
#[cfg(not(any(target_os = "macos", windows)))]
use other as imp;
#[cfg(windows)]
use windows as imp;

// ---- Identity ----

/// The identity of a live process by pid; `None` when no process has that pid or it has
/// exited (a zombie on Unix).
///
/// # Errors
///
/// The process exists but its record cannot be read (another user's process).
pub fn process_identity(pid: u32) -> io::Result<Option<ProcessIdentity>> {
    imp::process_identity(pid)
}

/// The identity of this process.
///
/// # Errors
///
/// The OS refused the lookup.
pub fn current_identity() -> io::Result<ProcessIdentity> {
    process_identity(std::process::id())?
        .ok_or_else(|| io::Error::other("this process has no identity record"))
}

// ---- Process-wide signal state (Unix; Windows has none) ----

/// The signals whose state `prepare_process` resets (`docs/design/daemon-lifecycle.md` §2.11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnixSignal {
    Term,
    Int,
    Hup,
    Chld,
}

impl UnixSignal {
    pub const ALL: [UnixSignal; 4] = [
        UnixSignal::Term,
        UnixSignal::Int,
        UnixSignal::Hup,
        UnixSignal::Chld,
    ];
}

/// The `SIGCHLD` disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigchldDisposition {
    Default,
    Ignore,
    Handler,
}

/// The process-wide signal state that decides whether children stay waitable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalState {
    pub sigchld: SigchldDisposition,
    /// `SA_NOCLDWAIT` is set on `SIGCHLD`.
    pub no_cldwait: bool,
    /// `sa_flags` of `SIGCHLD` as read.
    pub sigchld_flags: i32,
    /// Which of [`UnixSignal::ALL`] the calling thread blocks.
    pub blocked: Vec<UnixSignal>,
}

/// `SIGCHLD` to `SIG_DFL` with no flags; `TERM`, `INT`, `HUP`, `CHLD` unblocked. Call before
/// any thread exists, so every later thread inherits the mask. `Ok` on Windows.
///
/// # Errors
///
/// The OS refused the change.
pub fn reset_process_signals() -> io::Result<()> {
    imp::reset_process_signals()
}

/// The current [`SignalState`] (Windows: the default state).
///
/// # Errors
///
/// The OS refused the query.
pub fn signal_state() -> io::Result<SignalState> {
    imp::signal_state()
}

// ---- Files and privacy (docs/design/daemon-lifecycle.md §2.4) ----

/// The directory the default state directory lives in: `$HOME` on macOS,
/// `FOLDERID_LocalAppData` on Windows.
///
/// # Errors
///
/// `$HOME` is unset or relative, or the known folder lookup failed.
pub fn default_state_base() -> io::Result<PathBuf> {
    imp::default_state_base()
}

/// What an entry must be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
}

/// The outcome of a privacy check: private to the account, or the first check that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privacy {
    Private,
    /// A symlink or a Windows reparse point.
    Link,
    /// Not owned by the account (checked first).
    NotOwned,
    /// Group or other mode bits are set (macOS).
    ModeTooOpen {
        mode: u32,
    },
    /// An ACL entry grants access outside the private trustees (an extended allow entry on
    /// macOS; an allow ACE for another trustee, or no DACL, on Windows).
    AclGrantsOthers,
    /// Not of the requested kind.
    NotOfKind,
}

impl Privacy {
    /// The check that failed, for a refusal message; empty for `Private`.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Privacy::Private => String::new(),
            Privacy::Link => "it is a link or reparse point".to_owned(),
            Privacy::NotOwned => "it is not owned by this account".to_owned(),
            Privacy::ModeTooOpen { mode } => {
                format!("its mode {mode:o} grants group or other access")
            }
            Privacy::AclGrantsOthers => {
                "its access control list grants access to others".to_owned()
            }
            Privacy::NotOfKind => "it is not of the expected kind".to_owned(),
        }
    }
}

/// Creates the last component of `path` private to the account: mode `0700` on macOS, an
/// explicit protected DACL (owner and `SYSTEM`) on Windows. The parent must exist.
///
/// # Errors
///
/// The OS error (`AlreadyExists` when something is there).
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    imp::create_private_dir(path)
}

/// Whether `path` is private to the account and of `kind`, never following a final link.
///
/// # Errors
///
/// The entry cannot be inspected (`NotFound` when absent).
pub fn privacy(path: &Path, kind: EntryKind) -> io::Result<Privacy> {
    imp::privacy(path, kind)
}

/// The checks of [`privacy`] on an open regular file.
///
/// # Errors
///
/// The handle cannot be inspected.
pub fn handle_privacy(file: &File) -> io::Result<Privacy> {
    imp::handle_privacy(file)
}

/// How [`open_no_follow`] opens a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Read only; never creates.
    Read,
    /// Read and write, created (mode `0600`) if missing, never truncated.
    ReadWriteCreate,
    /// Append, created (mode `0600`) if missing.
    AppendCreate,
}

/// Opens `path` without following a final link (macOS `O_NOFOLLOW`; Windows
/// `FILE_FLAG_OPEN_REPARSE_POINT` and a reparse point refused), and requires the opened handle
/// to be a regular file.
///
/// # Errors
///
/// The OS error; `InvalidInput` for a link, a reparse point, or a non-regular file.
pub fn open_no_follow(path: &Path, access: Access) -> io::Result<File> {
    imp::open_no_follow(path, access)
}

/// Creates `path` (it must not exist) for reading and writing with an explicit owner-only
/// descriptor (macOS mode `0600`; Windows a protected DACL for the owner and `SYSTEM`),
/// never through a link, and verifies the handle with [`handle_privacy`] before returning
/// it; a file that fails the check is removed (this call created it).
///
/// # Errors
///
/// The OS error, or `PermissionDenied` when the created file is not private.
pub fn create_private_file(path: &Path) -> io::Result<File> {
    let file = imp::create_new_private(path)?;
    match handle_privacy(&file) {
        Ok(Privacy::Private) => Ok(file),
        Ok(p) => {
            drop(file);
            let _ = std::fs::remove_file(path);
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("the new file is not private: {}", p.describe()),
            ))
        }
        Err(e) => {
            drop(file);
            let _ = std::fs::remove_file(path);
            Err(e)
        }
    }
}

/// Whether `path` is absent or a regular file that is not a link or a reparse point.
///
/// # Errors
///
/// The entry cannot be inspected for another reason than its absence.
pub fn is_plain_file_or_absent(path: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e),
        Ok(m) => Ok(m.file_type().is_file() && !imp::is_reparse(&m)),
    }
}

/// How many hard links name the entry at `path` (never following a final link): above 1,
/// another path shares the file.
///
/// # Errors
///
/// The entry cannot be inspected (`NotFound` when absent).
pub fn hard_links(path: &Path) -> io::Result<u64> {
    imp::hard_links(path)
}

// ---- Stop channel (docs/design/daemon-lifecycle.md §2.10) ----

/// The daemon's stop listener: on Windows a `Global\chaldea-stop-<instance>` event with an
/// account-only DACL, waited on by the `daemon-stop` thread; on Unix nothing (a stop is
/// `SIGTERM`). Dropping it ends the thread without calling the callback.
pub struct StopListener {
    name: String,
    _inner: imp::StopInner,
}

impl StopListener {
    /// The event's name (Windows); empty on Unix.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl std::fmt::Debug for StopListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StopListener")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Starts the stop listener of `instance` (the instance's UUID text): `on_stop` runs once
/// when a stop request arrives.
///
/// # Errors
///
/// The event cannot be created; `AlreadyExists` when its name is taken (Windows).
pub fn listen_for_stop(
    instance: &str,
    on_stop: Box<dyn FnOnce() + Send>,
) -> io::Result<StopListener> {
    if instance.is_empty() || !instance.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the instance is not a UUID",
        ));
    }
    let (name, inner) = imp::listen_for_stop(instance, on_stop)?;
    Ok(StopListener {
        name,
        _inner: inner,
    })
}

/// The stop event's name of `instance` (Windows).
#[must_use]
pub fn stop_event_name(instance: &str) -> String {
    format!("Global\\chaldea-stop-{instance}")
}
