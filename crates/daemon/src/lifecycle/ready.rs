//! The readiness verdict of a detached child (design §2.10): exactly one line, then the
//! channel closed. 8a defines the seam; the channel itself (a Unix socket on macOS, the
//! stdout pipe on Windows) arrives with `daemon start`.

use std::io;

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
