//! `host` on an OS without a native backend: everything is unsupported, so the workspace
//! still builds there (the host platforms are macOS and Windows).

use std::fs::{File, Metadata};
use std::io;
use std::path::{Path, PathBuf};

use super::{Access, EntryKind, Privacy, ProcessIdentity, SignalState};

fn unsupported<T>() -> io::Result<T> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "not supported on this OS",
    ))
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
