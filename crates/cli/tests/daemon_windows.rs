//! Windows binary tests: PR8-CL-9 (Ctrl-C and Ctrl-Break stop a serving daemon) and the stop
//! listener half of PR8-CL-15 (the stop event is global, owner-only, and stops the daemon).
#![cfg(windows)]

mod support;

use std::ffi::OsStr;
use std::os::windows::process::CommandExt;

use chaldea_daemon::lifecycle::LockProbe;
use chaldea_pty_runtime::fake::{self, CtrlEvent};
use chaldea_pty_runtime::host;
use support::{Proc, free_port, lock_state, report, run_cmd, run_helper, serving_record, state};

/// `CREATE_NEW_CONSOLE` and `CREATE_NEW_PROCESS_GROUP` (`winbase.h`).
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

const EVENT: &str = "CHALDEA_CL9_EVENT";

/// In a helper on its own console: start a daemon there, ignore Ctrl-C only afterwards (so
/// the daemon does not inherit the ignore), send the event, and report the exit code and the
/// lock.
fn on_new_console(event: &str) {
    let dir = state();
    let mut cmd = run_cmd(&dir.path, Some(free_port()));
    if event == "break" {
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    let mut d = Proc::spawn(&mut cmd);
    fake::ignore_ctrl_c().unwrap();
    d.serving();
    let attached = fake::console_processes().unwrap();
    assert!(
        attached.contains(&d.pid()),
        "{attached:?} lacks {}",
        d.pid()
    );
    match event {
        "c" => fake::send_ctrl_event(CtrlEvent::CtrlC, 0).unwrap(),
        _ => fake::send_ctrl_event(CtrlEvent::CtrlBreak, d.pid()).unwrap(),
    }
    let code = d.code();
    eprintln!("daemon stderr:\n{}", d.stderr());
    report(&format!("code={code} lock={:?}", lock_state(&dir.path)));
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
