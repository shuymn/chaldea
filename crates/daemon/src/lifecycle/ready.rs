//! The readiness verdict of a detached child (design §2.10): exactly one line, then the
//! channel closed. The channel is `host`'s: a Unix socket on macOS, the stdout pipe on
//! Windows.

use std::io;

use chaldea_pty_runtime::host::{self, ReadyArg};

use super::log::diag_line;

/// The largest `fail` message, cut at a character boundary.
pub(crate) const MAX_MESSAGE_BYTES: usize = 512;

/// The child's end of the readiness channel.
pub trait ReadyWriter: Send {
    /// Writes one line (with its `\n`).
    ///
    /// # Errors
    ///
    /// The starter stopped reading; the daemon ignores it.
    fn write_line(&mut self, line: &str) -> io::Result<()>;
    /// Closes the channel explicitly.
    fn close(self: Box<Self>);
}

/// Opens the readiness writer at step 1.
pub type ReadyOpener = Box<dyn FnOnce() -> io::Result<Box<dyn ReadyWriter>> + Send>;

pub(crate) fn ready_line(port: u16, pid: u32) -> String {
    format!("ready {port} {pid}\n")
}

/// `fail <reason> [port_saved=<n>] <message>\n`: the message redacted, on one line, and cut
/// to 512 bytes.
pub(crate) fn fail_line(reason: &str, port_saved: Option<u16>, message: &str) -> String {
    let mut m = diag_line(message);
    if m.len() > MAX_MESSAGE_BYTES {
        let mut end = MAX_MESSAGE_BYTES;
        while !m.is_char_boundary(end) {
            end -= 1;
        }
        m.truncate(end);
    }
    match port_saved {
        Some(p) => format!("fail {reason} port_saved={p} {m}\n"),
        None => format!("fail {reason} {m}\n"),
    }
}

/// The child's end of `host`'s readiness channel.
struct HostReady(host::ReadyWriter);

impl ReadyWriter for HostReady {
    fn write_line(&mut self, line: &str) -> io::Result<()> {
        self.0.write_line(line)
    }

    fn close(self: Box<Self>) {
        self.0.close();
    }
}

/// The channel of a starter that already left (its socket gone): every write fails, which
/// the daemon ignores, so it serves without anyone reading its verdict (design §2.10, D10).
struct StarterGone;

impl ReadyWriter for StarterGone {
    fn write_line(&mut self, _line: &str) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "the starter left before the verdict",
        ))
    }

    fn close(self: Box<Self>) {}
}

/// The opener of a detached child's readiness writer on `host`'s channel `arg` (`--ready`). A
/// socket the starter already removed or stopped listening on (`ENOENT`, `ECONNREFUSED`: it
/// timed out and left) is not an error: the child continues and serves.
#[must_use]
pub fn host_ready_opener(arg: ReadyArg) -> ReadyOpener {
    Box::new(move || match host::ready_writer(&arg) {
        Ok(w) => Ok(Box::new(HostReady(w)) as Box<dyn ReadyWriter>),
        Err(e)
            if matches!(arg, ReadyArg::Socket(_))
                && matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
        {
            Ok(Box::new(StarterGone) as Box<dyn ReadyWriter>)
        }
        Err(e) => Err(e),
    })
}

/// The `fail <reason> <message>` line a process that never reached [`super::run`] sends (the
/// launchd job process whose `fork` failed, design §2.10).
#[must_use]
pub fn early_fail_line(reason: super::FailReason, message: &str) -> String {
    fail_line(reason.as_str(), None, message)
}
