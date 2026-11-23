//! The files of the state directory (design §2.4–§2.6): the single-instance lock and its
//! probe, the discovery record and the liveness rule, `write_replace`, and the readers of the
//! port and owner token files.

use std::fs::{File, TryLockError};
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use chaldea_pty_runtime::host::{self, Access, EntryKind, Privacy};
use serde::{Deserialize, Serialize};

use super::names;

/// The record's format version.
pub const RECORD_VERSION: u32 = 1;
/// The largest record, port, or token file a reader accepts.
const MAX_RECORD_BYTES: u64 = 64 * 1024;
const MAX_TOKEN_BYTES: u64 = 128;
const MAX_PORT_BYTES: u64 = 16;
/// `write_replace`'s rename retries (design §2.4 step 3).
const RENAME_RETRY_EVERY: Duration = Duration::from_millis(50);
const RENAME_RETRY_FOR: Duration = Duration::from_millis(500);
/// The lock retry's poll interval (design §2.5).
const LOCK_POLL: Duration = Duration::from_millis(50);

// ---- The single-instance lock (design §2.5) ----

/// Why [`InstanceLock::acquire`] failed.
#[derive(Debug)]
pub enum LockError {
    /// Another handle holds the lock (after the retry).
    Held,
    /// Anything else: not contention, not retried.
    Io(io::Error),
}

/// The exclusive lock on `daemon.lock`. Dropping it never releases the lock: the handle is
/// leaked, so only [`InstanceLock::release`] or the process's exit does.
#[derive(Debug)]
pub struct InstanceLock {
    file: Option<File>,
}

impl InstanceLock {
    /// Opens `dir/daemon.lock` without following a link (created `0600` if missing, never
    /// truncated or written) and takes the exclusive lock, retried every 50 ms within `retry`
    /// on contention only.
    ///
    /// # Errors
    ///
    /// [`LockError`].
    pub fn acquire(dir: &Path, retry: Duration) -> Result<InstanceLock, LockError> {
        let file = host::open_no_follow(&dir.join(names::LOCK), Access::ReadWriteCreate)
            .map_err(LockError::Io)?;
        let end = Instant::now() + retry;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(InstanceLock { file: Some(file) }),
                Err(TryLockError::WouldBlock) => {
                    let now = Instant::now();
                    if now >= end {
                        return Err(LockError::Held);
                    }
                    std::thread::sleep(LOCK_POLL.min(end - now));
                }
                Err(TryLockError::Error(e)) => return Err(LockError::Io(e)),
            }
        }
    }

    /// Unlocks and closes. Only the coordinator calls it (design §2.8 step 6).
    pub(crate) fn release(mut self) {
        if let Some(f) = self.file.take() {
            let _ = f.unlock();
            drop(f);
        }
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Leaked on purpose: an unwind, an early return, or a forgotten value never releases
        // the lock in-process.
        if let Some(f) = self.file.take() {
            std::mem::forget(f);
        }
    }
}

/// What a client's probe of the lock saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockProbe {
    Held,
    Free,
    /// No `daemon.lock` (never created).
    Missing,
}

/// Opens `daemon.lock` read-only (never creating it) and tries a shared lock: `WouldBlock` is
/// `Held`, success `Free` (unlocked at once).
///
/// # Errors
///
/// The file cannot be opened or locked for another reason.
pub fn probe(dir: &Path) -> io::Result<LockProbe> {
    let file = match host::open_no_follow(&dir.join(names::LOCK), Access::Read) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(LockProbe::Missing),
        Err(e) => return Err(e),
    };
    match file.try_lock_shared() {
        Ok(()) => {
            let _ = file.unlock();
            Ok(LockProbe::Free)
        }
        Err(TryLockError::WouldBlock) => Ok(LockProbe::Held),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

// ---- Discovery (design §2.6) ----

/// The daemon's published state (design §2.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordState {
    Starting,
    Serving,
    /// The shutdown began; written before HTTP shuts down, so it never serves again.
    Stopping,
}

/// What the doctor reads about how the daemon was started: step 2's job state (`in_job` and
/// the immediate job's limits; recorded on Windows, `null` elsewhere) and whether it has a
/// console window (`null` off Windows).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub job: Option<serde_json::Value>,
    pub console: Option<bool>,
}

/// `daemon.json`: internal, read by the same binary and the doctor; never a secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryRecord {
    pub version: u32,
    pub instance: String,
    pub pid: u32,
    pub start_time: String,
    pub state: RecordState,
    pub port: Option<u16>,
    pub url: Option<String>,
    pub started_at: String,
    pub detached: Option<String>,
    pub evidence: Evidence,
}

impl DiscoveryRecord {
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut v = serde_json::to_vec(self).unwrap_or_default();
        v.push(b'\n');
        v
    }
}

fn read_capped(f: File, cap: u64) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    f.take(cap + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > cap {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "file too large"));
    }
    Ok(buf)
}

/// Reads `daemon.json` without following a link; `None` when absent.
///
/// # Errors
///
/// It cannot be read, is a link, or does not parse as a version-1 record.
pub fn read_record(dir: &Path) -> io::Result<Option<DiscoveryRecord>> {
    let f = match host::open_no_follow(&dir.join(names::RECORD), Access::Read) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let bytes = read_capped(f, MAX_RECORD_BYTES)?;
    let r: DiscoveryRecord = serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if r.version != RECORD_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unknown record version",
        ));
    }
    Ok(Some(r))
}

/// What a client may believe about a state directory (design §2.6 "Liveness rule").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discovery {
    /// The lock is free or missing; a leftover record is ignored.
    NotRunning,
    /// The lock is held but the record is missing, unreadable, or names another process.
    Unknown,
    Starting(DiscoveryRecord),
    Serving(DiscoveryRecord),
    Stopping(DiscoveryRecord),
}

/// Applies the liveness rule: a record is used only when the lock is held, the record parses
/// (read without following links), its `(pid, start_time)` is a live process, the lock is
/// still held after the record was read, so a record its daemon removed before releasing the
/// lock is never returned once the lock is free, and the record read again after that check is
/// unchanged, so a state the daemon published during the check (`stopping`) is never answered
/// with the one before it. Never removes anything.
#[must_use]
pub fn discover(dir: &Path) -> Discovery {
    discover_paused(dir, || {})
}

/// [`discover`] with `read` run after the record and its identity were read, before the lock
/// is checked again (a test's pause).
pub(crate) fn discover_paused(dir: &Path, read: impl FnOnce()) -> Discovery {
    let held = || match probe(dir) {
        Ok(LockProbe::Held) => Some(true),
        Ok(LockProbe::Free | LockProbe::Missing) => Some(false),
        Err(_) => None,
    };
    match held() {
        Some(true) => {}
        Some(false) => return Discovery::NotRunning,
        None => return Discovery::Unknown,
    }
    let Ok(Some(r)) = read_record(dir) else {
        return Discovery::Unknown;
    };
    let alive = matches!(
        host::process_identity(r.pid),
        Ok(Some(id)) if id.start_time == r.start_time
    );
    read();
    match held() {
        Some(true) => {}
        Some(false) => return Discovery::NotRunning,
        None => return Discovery::Unknown,
    }
    if !alive || !matches!(read_record(dir), Ok(Some(again)) if again == r) {
        return Discovery::Unknown;
    }
    match r.state {
        RecordState::Starting => Discovery::Starting(r),
        RecordState::Serving => Discovery::Serving(r),
        RecordState::Stopping => Discovery::Stopping(r),
    }
}

// ---- write_replace (design §2.4) ----

/// Where a `write_replace` call is; a test hook may fail any stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplaceStage {
    /// Writing the temporary (before the rename).
    Write,
    Rename,
    /// Syncing the directory after the rename.
    DirSync,
    /// After the rename and the directory sync.
    Renamed,
}

/// Called at each stage; an `Err` is injected as that stage's failure.
pub(crate) type ReplaceHook<'a> = &'a (dyn Fn(&str, ReplaceStage) -> io::Result<()> + Sync);

#[cfg_attr(test, allow(dead_code, reason = "tests pass their own hook"))]
pub(crate) fn no_hook(_: &str, _: ReplaceStage) -> io::Result<()> {
    Ok(())
}

/// Why `write_replace` did not commit; `name` is unchanged and no temporary is left.
#[derive(Debug)]
pub(crate) enum ReplaceError {
    /// `name` is a link, reparse point, directory, or anything but a regular file.
    Refused,
    Io(io::Error),
}

impl std::fmt::Display for ReplaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplaceError::Refused => f.write_str("it is not a regular file"),
            ReplaceError::Io(e) => e.fmt(f),
        }
    }
}

/// A committed replacement; `durability_warning` when the directory sync failed after the
/// rename (the new content is visible, but may not survive a power loss).
#[derive(Debug)]
pub(crate) struct Replaced {
    pub(crate) durability_warning: Option<String>,
}

/// Replaces `dir/name` with `bytes`: only an absent entry or a regular file, through a
/// temporary `.<name>.tmp-<instance>` created owner-only and a rename, which is the commit.
/// A failed rename is retried every 50 ms until `retry_until`, or for 500 ms from the first
/// attempt when it is `None`.
pub(crate) fn write_replace(
    dir: &Path,
    name: &str,
    bytes: &[u8],
    instance: &str,
    retry_until: Option<Instant>,
    hook: ReplaceHook<'_>,
) -> Result<Replaced, ReplaceError> {
    let target = dir.join(name);
    match std::fs::symlink_metadata(&target) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(ReplaceError::Io(e)),
        Ok(_) => {
            if !host::is_plain_file_or_absent(&target).map_err(ReplaceError::Io)? {
                return Err(ReplaceError::Refused);
            }
        }
    }
    let tmp = dir.join(format!(".{name}.tmp-{instance}"));
    // Only a temporary file this call created is ours to remove.
    let mut created = false;
    let written = (|| {
        let mut f = host::create_private_file(&tmp)?;
        created = true;
        hook(name, ReplaceStage::Write)?;
        f.write_all(bytes)?;
        f.sync_all()
    })();
    if let Err(e) = written {
        if created {
            let _ = std::fs::remove_file(&tmp);
        }
        return Err(ReplaceError::Io(e));
    }
    let end = retry_until.unwrap_or_else(|| Instant::now() + RENAME_RETRY_FOR);
    loop {
        let renamed =
            hook(name, ReplaceStage::Rename).and_then(|()| std::fs::rename(&tmp, &target));
        match renamed {
            Ok(()) => break,
            Err(e) => {
                let now = Instant::now();
                if now >= end {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(ReplaceError::Io(e));
                }
                std::thread::sleep(RENAME_RETRY_EVERY.min(end - now));
            }
        }
    }
    let durability_warning = hook(name, ReplaceStage::DirSync)
        .and_then(|()| sync_dir(dir))
        .err()
        .map(|e| {
            format!("{name} was replaced but may not survive a power loss (directory sync: {e})")
        });
    let _ = hook(name, ReplaceStage::Renamed);
    Ok(Replaced { durability_warning })
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

// ---- The port and owner token files (design §2.6, §2.13) ----

/// Parses the `port` file's content: one decimal number in 1–65535 and a newline.
pub(crate) fn parse_port(bytes: &[u8]) -> Option<u16> {
    let digits = bytes.strip_suffix(b"\n")?;
    if digits.is_empty() || digits.len() > 5 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let n: u32 = std::str::from_utf8(digits).ok()?.parse().ok()?;
    u16::try_from(n).ok().filter(|p| *p != 0)
}

/// Reads the saved port: `Ok(None)` when absent, `Err` with the reason when malformed or
/// unreadable.
pub(crate) fn read_port(dir: &Path) -> Result<Option<u16>, String> {
    let f = match host::open_no_follow(&dir.join(names::PORT), Access::Read) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let bytes = read_capped(f, MAX_PORT_BYTES).map_err(|e| e.to_string())?;
    parse_port(&bytes)
        .map(Some)
        .ok_or_else(|| "not a port number".to_owned())
}

/// Whether `s` is the owner token file's form: `chd_` and 43 base64url characters, then `\n`.
pub(crate) fn token_file_form(bytes: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(bytes.strip_suffix(b"\n")?).ok()?;
    crate::http::has_secret_form(s, crate::http::TOKEN_PREFIX).then(|| s.to_owned())
}

/// The owner token file as step 6 found it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TokenFile {
    Absent,
    /// Private and well-formed; the secret without its newline.
    Present(String),
}

/// Why step 6 refused the token file.
#[derive(Debug)]
pub(crate) enum TokenFileError {
    NotPrivate(Privacy),
    BadForm,
    Io(io::Error),
}

/// Step 6's check: privacy first (never reading a file that is not private), then the form.
pub(crate) fn check_token_file(dir: &Path) -> Result<TokenFile, TokenFileError> {
    let path = dir.join(names::OWNER_TOKEN);
    match host::privacy(&path, EntryKind::File) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(TokenFile::Absent),
        Err(e) => return Err(TokenFileError::Io(e)),
        Ok(Privacy::Private) => {}
        Ok(p) => return Err(TokenFileError::NotPrivate(p)),
    }
    let f = host::open_no_follow(&path, Access::Read).map_err(TokenFileError::Io)?;
    let bytes = read_capped(f, MAX_TOKEN_BYTES).map_err(|_| TokenFileError::BadForm)?;
    token_file_form(&bytes)
        .map(TokenFile::Present)
        .ok_or(TokenFileError::BadForm)
}

/// The owner token for a client (design §2.13 `chaldea open`, §2.14): the same checks as the
/// daemon's step 6 (private, regular, not a link, of the token form), the secret without its
/// newline.
///
/// # Errors
///
/// What is wrong with the file, as a message without the token.
pub fn read_owner_token(dir: &Path) -> Result<String, String> {
    match check_token_file(dir) {
        Ok(TokenFile::Present(t)) => Ok(t),
        Ok(TokenFile::Absent) => Err(format!(
            "{} is missing; start the daemon to issue it",
            dir.join(names::OWNER_TOKEN).display()
        )),
        Err(TokenFileError::NotPrivate(p)) => Err(format!(
            "{} is not private ({}); its token may be exposed",
            dir.join(names::OWNER_TOKEN).display(),
            p.describe()
        )),
        Err(TokenFileError::BadForm) => Err(format!(
            "{} is not a Chaldea token file",
            dir.join(names::OWNER_TOKEN).display()
        )),
        Err(TokenFileError::Io(e)) => Err(format!(
            "{} cannot be read: {e}",
            dir.join(names::OWNER_TOKEN).display()
        )),
    }
}
