//! The one place the CLI creates a child process (docs/design/daemon-lifecycle.md §2.11):
//! under the process-wide spawn lock shared with the PTY backends.

use std::io;
use std::process::{Child, Command};

/// Spawns `cmd` under the spawn lock.
///
/// # Errors
///
/// The spawn error.
#[allow(
    clippy::disallowed_methods,
    reason = "the CLI's one spawn site; every child is created under the spawn lock"
)]
pub(crate) fn spawn_locked(cmd: &mut Command) -> io::Result<Child> {
    chaldea_pty_runtime::with_spawn_lock(|| cmd.spawn())
}
