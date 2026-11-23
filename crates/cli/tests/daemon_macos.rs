//! PR8-CL-17 (macOS): a process that starts with `SIGCHLD` ignored and `SIGTERM`, `SIGINT`,
//! `SIGHUP`, `SIGCHLD` blocked gets them restored by `prepare_process`, so a child stays
//! waitable by exact pid.
//!
//! std's spawn clears the child's signal mask (and only `posix_spawn`/`exec` without std would
//! keep it), so the helper sets the inherited state on itself and then runs the same
//! `prepare_process` call `main` makes first; a control helper skips it and sees `ECHILD`.
#![cfg(target_os = "macos")]

mod support;

use std::ffi::OsStr;
use std::process::{Command, Stdio};
use std::time::Duration;

use chaldea_daemon::lifecycle::{prepare_process, spawn_locked};
use chaldea_pty_runtime::fake;
use chaldea_pty_runtime::host::{self, SigchldDisposition, UnixSignal};
use support::{report, run_helper};

const MODE: &str = "CHALDEA_CL17_MODE";
fn true_child() -> std::io::Result<std::process::ExitStatus> {
    let mut c = spawn_locked(
        Command::new("/usr/bin/true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    )?;
    c.wait()
}

fn in_helper(mode: &str) {
    fake::ignore_sigchld().unwrap();
    fake::block_signals(&UnixSignal::ALL).unwrap();
    let before = host::signal_state().unwrap();
    assert_eq!(before.sigchld, SigchldDisposition::Ignore);
    assert_eq!(before.blocked, UnixSignal::ALL.to_vec());
    if mode == "reset" {
        prepare_process();
        let after = host::signal_state().unwrap();
        assert_eq!(after.sigchld, SigchldDisposition::Default, "{after:?}");
        assert!(!after.no_cldwait, "{after:?}");
        assert!(after.blocked.is_empty(), "{after:?}");
        let status = true_child().unwrap();
        assert!(status.success(), "{status:?}");
        report("waited");
    } else {
        let e = true_child().unwrap_err();
        assert_eq!(
            e.raw_os_error(),
            Some(rustix::io::Errno::CHILD.raw_os_error()),
            "{e}"
        );
        report("echild");
    }
}

#[test]
fn inherited_signal_state_is_reset() {
    if let Ok(mode) = std::env::var(MODE) {
        in_helper(&mode);
        return;
    }
    for (mode, want) in [("reset", "waited"), ("control", "echild")] {
        let got = run_helper(
            "inherited_signal_state_is_reset",
            &[(MODE, OsStr::new(mode))],
            Duration::from_secs(30),
            |_| {},
        );
        assert_eq!(got, want, "{mode}");
    }
}
