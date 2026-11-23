//! Windows binary tests: PR8-CL-9 (Ctrl-C and Ctrl-Break stop a serving daemon, also one that
//! inherited Ctrl-C ignored) and the stop listener half of PR8-CL-15 (the stop event is
//! global, owner-only, and stops the daemon).
#![cfg(windows)]

mod support;

use std::ffi::OsStr;
use std::os::windows::process::CommandExt;
use std::time::Duration;

use chaldea_daemon::lifecycle::LockProbe;
use chaldea_pty_runtime::fake::{self, CtrlEvent};
use chaldea_pty_runtime::host;
use support::{Proc, free_port, lock_state, report, run_cmd, run_helper, serving_record, state};

/// `CREATE_NEW_CONSOLE` and `CREATE_NEW_PROCESS_GROUP` (`winbase.h`).
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

const EVENT: &str = "CHALDEA_CL9_EVENT";

/// In a helper on its own console: ignore Ctrl-C (an attribute the daemon inherits, as from
/// `start /b`), start a daemon there, send the event, and report the exit code and the lock.
/// Each step is printed to stderr, so a failure names the step it stopped at.
fn on_new_console(event: &str) {
    let dir = state();
    fake::ignore_ctrl_c().unwrap();
    eprintln!("cl9 {event}: the helper ignores Ctrl-C; the daemon inherits that");
    let mut cmd = run_cmd(&dir.path, Some(free_port()));
    if event == "break" {
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    let mut d = Proc::spawn(&mut cmd);
    eprintln!("cl9 {event}: daemon pid {}", d.pid());
    let port = d.serving();
    eprintln!("cl9 {event}: serving on {port}");
    let attached = fake::console_processes().unwrap();
    eprintln!("cl9 {event}: console processes {attached:?}");
    assert!(
        attached.contains(&d.pid()),
        "{attached:?} lacks {}",
        d.pid()
    );
    match event {
        "c" => fake::send_ctrl_event(CtrlEvent::CtrlC, 0).unwrap(),
        _ => fake::send_ctrl_event(CtrlEvent::CtrlBreak, d.pid()).unwrap(),
    }
    eprintln!("cl9 {event}: event sent");
    let status = d.wait_within(Duration::from_secs(20));
    eprintln!(
        "cl9 {event}: daemon exited {status:?}; its stderr:\n{}",
        d.stderr()
    );
    report(&format!(
        "code={} lock={:?}",
        status.code().unwrap_or(-1),
        lock_state(&dir.path)
    ));
}

#[test]
fn ctrl_c_and_ctrl_break_stop() {
    if let Ok(event) = std::env::var(EVENT) {
        on_new_console(&event);
        return;
    }
    for event in ["c", "break"] {
        let got = run_helper(
            "ctrl_c_and_ctrl_break_stop",
            &[(EVENT, OsStr::new(event))],
            Duration::from_secs(90),
            |c| {
                c.creation_flags(CREATE_NEW_CONSOLE);
            },
        );
        assert_eq!(got, format!("code=0 lock={:?}", LockProbe::Free), "{event}");
    }
}

#[test]
fn stop_event_is_global() {
    let dir = state();
    let mut d = Proc::spawn(&mut run_cmd(&dir.path, Some(free_port())));
    d.serving();
    let r = serving_record(&dir.path);
    let name = host::stop_event_name(&r.instance);
    assert!(name.starts_with("Global\\"), "{name}");
    let (_owner, allows) = fake::event_sids(&name).unwrap();
    assert_eq!(allows, vec![fake::user_sid().unwrap()]);
    fake::set_stop_event(&name).unwrap();
    assert_eq!(d.code(), 0, "{}", d.stderr());
    assert_eq!(lock_state(&dir.path), LockProbe::Free);
}
