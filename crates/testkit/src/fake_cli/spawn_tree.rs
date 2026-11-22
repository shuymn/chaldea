//! `spawn-tree`: starts one sleeper per role, each sending its own hello, then idles and
//! reaps the children that exit.

use std::process::Child;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

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

pub(crate) fn run(o: &Opts) -> i32 {
    let raw = super::enter_raw(o, false);
    let _rep = super::hello(o, raw);
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
    loop {
        children.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
        std::thread::sleep(Duration::from_millis(100));
    }
}
