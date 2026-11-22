//! The fixture's view of its terminal and process ids. Unix-only facts are `null`
//! elsewhere; on Windows the console comes from `winapi-util`
//! (`docs/design/pty-windows.md` §2.8).

use serde_json::{Value, json};

#[cfg(unix)]
mod imp {
    use std::io::{stdin, stdout};

    use rustix::termios::{
        OptionalActions, isatty, tcgetattr, tcgetpgrp, tcgetwinsize, tcsetattr, ttyname,
    };
    use serde_json::{Value, json};

    pub(crate) fn stdin_is_tty() -> bool {
        isatty(stdin())
    }

    pub(crate) fn make_raw() -> bool {
        let Ok(mut t) = tcgetattr(stdin()) else {
            return false;
        };
        t.make_raw();
        tcsetattr(stdin(), OptionalActions::Now, &t).is_ok()
    }

    fn tty_of(fd: std::os::fd::BorrowedFd<'_>) -> Value {
        ttyname(fd, Vec::new()).map_or(Value::Null, |n| json!(n.to_string_lossy()))
    }

    pub(crate) fn identity() -> Value {
        use std::os::fd::AsFd;
        let pgrp = rustix::process::getpgrp().as_raw_pid();
        let sid = rustix::process::getsid(None)
            .map(rustix::process::Pid::as_raw_pid)
            .ok();
        let ppid = rustix::process::getppid().map_or(0, rustix::process::Pid::as_raw_pid);
        let ttys = [
            tty_of(stdin().as_fd()),
            tty_of(stdout().as_fd()),
            tty_of(std::io::stderr().as_fd()),
        ];
        let fg = tcgetpgrp(stdin())
            .map(|p| p.as_raw_pid() == pgrp)
            .unwrap_or(false);
        json!({
            "ppid": ppid,
            "pgid": pgrp,
            "sid": sid,
            "ttys": ttys,
            "ctty": std::fs::File::open("/dev/tty").is_ok(),
            "fg": fg,
        })
    }

    /// The open fds, without the listing's own directory handle.
    pub(crate) fn open_fds() -> Value {
        let Ok(dir) = std::fs::read_dir("/dev/fd") else {
            return Value::Null;
        };
        // The handle is the only directory among them; it is checked while still open.
        let mut fds: Vec<i32> = dir
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse().ok())
            .filter(|fd| !std::fs::metadata(format!("/dev/fd/{fd}")).is_ok_and(|m| m.is_dir()))
            .collect();
        fds.sort_unstable();
        json!(fds)
    }

    /// Bytes waiting on stdin when it is a terminal.
    pub(crate) fn inq() -> Option<u64> {
        if !stdin_is_tty() {
            return None;
        }
        rustix::io::ioctl_fionread(stdin()).ok()
    }

    /// `(cols, rows, xpixel, ypixel)` of the terminal on stdin.
    pub(crate) fn size() -> Option<(u16, u16, Option<u16>, Option<u16>)> {
        let w = tcgetwinsize(stdin()).ok()?;
        Some((w.ws_col, w.ws_row, Some(w.ws_xpixel), Some(w.ws_ypixel)))
    }
}

/// Windows: console mode and size through `winapi-util` (no `unsafe` in testkit). ConPTY
/// has no pixel size, and there is no safe input-event count.
#[cfg(windows)]
mod imp {
    use std::io::IsTerminal;

    use serde_json::{Value, json};
    use winapi_util::HandleRef;
    use winapi_util::console::{mode, screen_buffer_info, set_mode};

    const ENABLE_PROCESSED_INPUT: u32 = 0x1;
    const ENABLE_LINE_INPUT: u32 = 0x2;
    const ENABLE_ECHO_INPUT: u32 = 0x4;
    const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x200;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x4;

    pub(crate) fn stdin_is_tty() -> bool {
        std::io::stdin().is_terminal()
    }

    /// Raw input with VT input on stdin, VT processing on stdout, so reading modes get the
    /// bytes as on Unix.
    pub(crate) fn make_raw() -> bool {
        let (Ok(i), Ok(o)) = (mode(HandleRef::stdin()), mode(HandleRef::stdout())) else {
            return false;
        };
        let i = (i & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
            | ENABLE_VIRTUAL_TERMINAL_INPUT;
        set_mode(HandleRef::stdin(), i).is_ok()
            && set_mode(HandleRef::stdout(), o | ENABLE_VIRTUAL_TERMINAL_PROCESSING).is_ok()
    }

    pub(crate) fn identity() -> Value {
        json!({ "ppid": null, "pgid": null, "sid": null, "ttys": null, "ctty": null, "fg": null })
    }

    pub(crate) fn open_fds() -> Value {
        Value::Null
    }

    pub(crate) fn inq() -> Option<u64> {
        None
    }

    /// The console window's size; no pixels.
    pub(crate) fn size() -> Option<(u16, u16, Option<u16>, Option<u16>)> {
        let r = screen_buffer_info(HandleRef::stdout()).ok()?.window_rect();
        let cols = u16::try_from(i32::from(r.right) - i32::from(r.left) + 1).ok()?;
        let rows = u16::try_from(i32::from(r.bottom) - i32::from(r.top) + 1).ok()?;
        Some((cols, rows, None, None))
    }
}

pub(crate) use imp::{identity, inq, make_raw, open_fds, size, stdin_is_tty};

/// A `size` or `winch` event for a size already read; null fields when there is no
/// terminal size.
pub(crate) fn size_event_of(
    event: &str,
    size: Option<(u16, u16, Option<u16>, Option<u16>)>,
) -> Value {
    match size {
        Some((cols, rows, x, y)) => {
            json!({"event": event, "cols": cols, "rows": rows, "xpixel": x, "ypixel": y})
        }
        None => json!({"event": event, "cols": null, "rows": null, "xpixel": null, "ypixel": null}),
    }
}
