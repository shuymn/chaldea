//! `spawn-tree`: starts one sleeper per role, each sending its own hello, then idles and
//! reaps the children that exit. On Windows the `breakaway` role is started with
//! `CREATE_BREAKAWAY_FROM_JOB`, again without it when that is denied, and the result is
//! reported on spawn-tree's own stream. `--emit-output` then writes `OUTPUT-<n>` lines every
//! 50 ms, acknowledging each with an `output` event once its write and flush returned.

use std::io::Write;
use std::process::Child;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde_json::json;

use super::Opts;
use super::sleeper::sleeper;

#[cfg(unix)]
const DEFAULT_ROLES: &[&str] = &[
    "plain",
    "ignore-term",
    "double-forked",
    "own-group",
    "setsid",
];
#[cfg(not(unix))]
const DEFAULT_ROLES: &[&str] = &["plain", "double-forked"];

const REAP_EVERY: Duration = Duration::from_millis(100);
const EMIT_EVERY: Duration = Duration::from_millis(50);

/// `CREATE_BREAKAWAY_FROM_JOB`.
#[cfg(windows)]
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
#[cfg(windows)]
const ERROR_ACCESS_DENIED: i32 = 5;

pub(crate) fn run(o: &Opts) -> i32 {
    let raw = super::enter_raw(o, false);
    let rep = super::hello(o, raw);
    let roles: Vec<String> = o
        .roles
        .clone()
        .unwrap_or_else(|| DEFAULT_ROLES.iter().map(|r| (*r).to_owned()).collect());
    let mut children: Vec<Child> = Vec::new();
    for role in &roles {
        let mut cmd = match role.as_str() {
            "double-forked" => sleeper(o, "double-fork-parent"),
            #[cfg(unix)]
            "own-group" => {
                let mut c = sleeper(o, "own-group");
                c.process_group(0);
                c
            }
            #[cfg(unix)]
            "setsid" => {
                let mut c = sleeper(o, "setsid");
                c.arg("--setsid");
                c
            }
            #[cfg(windows)]
            "breakaway" => {
                use std::os::windows::process::CommandExt;
                let mut c = sleeper(o, "breakaway");
                c.creation_flags(CREATE_BREAKAWAY_FROM_JOB);
                let (child, result) = match c.spawn() {
                    Ok(child) => (child, "accepted"),
                    Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
                        match sleeper(o, "breakaway").spawn() {
                            Ok(child) => (child, "denied"),
                            Err(_) => return 1,
                        }
                    }
                    Err(_) => return 1,
                };
                rep.send(&json!({"event": "breakaway", "result": result}));
                children.push(child);
                continue;
            }
            r => sleeper(o, r),
        };
        match cmd.spawn() {
            Ok(c) => children.push(c),
            Err(_) => return 1,
        }
    }
    #[cfg(unix)]
    if o.escape_orphan {
        let mut cmd = sleeper(o, "orphan-parent");
        if o.raw {
            cmd.arg("--raw");
        }
        if o.orphan_keeps_tty {
            cmd.arg("--orphan-keeps-tty");
        }
        match cmd.spawn() {
            Ok(c) => children.push(c),
            Err(_) => return 1,
        }
    }
    let mut out = std::io::stdout();
    let mut n = 0u64;
    let mut reap = Instant::now();
    loop {
        if o.emit_output {
            let _ = write!(out, "OUTPUT-{n}\r\n");
            let _ = out.flush();
            rep.send(&json!({"event": "output", "n": n}));
            n += 1;
        }
        if Instant::now() >= reap {
            children.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
            reap = Instant::now() + REAP_EVERY;
        }
        std::thread::sleep(if o.emit_output {
            EMIT_EVERY
        } else {
            REAP_EVERY
        });
    }
}
