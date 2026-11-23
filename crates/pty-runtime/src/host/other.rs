//! `host` on an OS without a native backend: everything is unsupported, so the workspace
//! still builds there (the host platforms are macOS and Windows).

use std::ffi::OsStr;
use std::fs::{File, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{
    Access, Breakaway, EntryKind, JobState, LaunchctlOutput, Privacy, ProcessIdentity, ReadyArg,
    ReadyRead, SignalState, StopTarget,
};

fn unsupported<T>() -> io::Result<T> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "not supported on this OS",
    ))
}

pub(super) fn exit_now(code: i32) -> ! {
    std::process::exit(code)
}

pub(super) fn process_identity(_pid: u32) -> io::Result<Option<ProcessIdentity>> {
    unsupported()
}

pub(super) fn reset_process_signals() -> io::Result<()> {
    unsupported()
}

pub(super) fn signal_state() -> io::Result<SignalState> {
    unsupported()
}

pub(super) fn default_state_base() -> io::Result<PathBuf> {
    unsupported()
}

pub(super) fn create_private_dir(_path: &Path) -> io::Result<()> {
    unsupported()
}

pub(super) fn create_new_private(_path: &Path) -> io::Result<File> {
    unsupported()
}

pub(super) fn privacy(_path: &Path, _kind: EntryKind) -> io::Result<Privacy> {
    unsupported()
}

pub(super) fn handle_privacy(_file: &File) -> io::Result<Privacy> {
    unsupported()
}

pub(super) fn open_no_follow(_path: &Path, _access: Access) -> io::Result<File> {
    unsupported()
}

pub(super) fn hard_links(_path: &Path) -> io::Result<u64> {
    unsupported()
}

pub(super) fn is_reparse(_m: &Metadata) -> bool {
    false
}

pub(super) struct StopInner;

pub(super) fn listen_for_stop(
    _instance: &str,
    _on_stop: Box<dyn FnOnce() + Send>,
) -> io::Result<(String, StopInner)> {
    unsupported()
}

pub(super) fn detach_session() -> io::Result<()> {
    unsupported()
}

pub(super) fn leave_launchd_job() -> io::Result<()> {
    Ok(())
}

pub(super) fn euid() -> u32 {
    0
}

pub(super) fn current_umask() -> u32 {
    0
}

pub(super) fn launchctl(_args: &[&OsStr], _deadline: Instant) -> io::Result<LaunchctlOutput> {
    unsupported()
}

pub(super) struct DetachedInner;

pub(super) fn spawn_detached(
    _exe: &Path,
    _args: &[std::ffi::OsString],
    _cwd: &Path,
    _breakaway: Breakaway,
    _ready: &mut ReadyInner,
    _deadline: Instant,
) -> io::Result<DetachedInner> {
    unsupported()
}

pub(super) fn detached_id(_inner: &DetachedInner) -> u32 {
    0
}

pub(super) fn detached_exit_before(
    _inner: &mut DetachedInner,
    _deadline: Instant,
) -> io::Result<Option<String>> {
    unsupported()
}

pub(super) struct ReadyInner;

pub(super) fn ready_dir(_inner: &ReadyInner) -> Option<&Path> {
    None
}

pub(super) fn ready_write_plist(
    _inner: &mut ReadyInner,
    _contents: &[u8],
) -> io::Result<std::path::PathBuf> {
    unsupported()
}

pub(super) fn ready_channel_in(_temp: &Path) -> io::Result<(ReadyInner, ReadyArg)> {
    unsupported()
}

pub(super) fn ready_read(
    _inner: &mut ReadyInner,
    _deadline: Instant,
    _child: Option<&mut DetachedInner>,
) -> io::Result<ReadyRead> {
    unsupported()
}

pub(super) struct ReadyWriterInner;

pub(super) fn ready_writer(_arg: &ReadyArg) -> io::Result<ReadyWriterInner> {
    unsupported()
}

pub(super) fn ready_write(_inner: &mut ReadyWriterInner, _line: &str) -> io::Result<()> {
    unsupported()
}

pub(super) fn ready_close(_inner: ReadyWriterInner) {}

pub(super) fn job_state() -> io::Result<JobState> {
    Ok(JobState::NONE)
}

pub(super) fn has_console() -> Option<bool> {
    None
}

pub(super) const CHECK_BEFORE_DELIVERY: bool = true;
pub(super) const DELIVERY_GONE: i32 = -1;
pub(super) const DELIVERY_DENIED: i32 = -1;

pub(super) struct StopDelivery;

pub(super) fn open_stop(_target: &StopTarget) -> io::Result<StopDelivery> {
    unsupported()
}

pub(super) fn fire_stop(_d: &StopDelivery) -> io::Result<()> {
    unsupported()
}

pub(super) fn open_url(_url: &str) -> io::Result<()> {
    unsupported()
}
