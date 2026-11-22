//! `sleeper`: one role per process, each isolating one way a process can relate to the
//! CLI's tree. A role sends its hello only once its mechanism is in place.
//!
//! | Role | How it is made; hello after |
//! |---|---|
//! | `plain` | child of spawn-tree |
//! | `ignore-term` | SIGHUP and SIGTERM handlers installed |
//! | `double-forked` | its parent `double-fork-parent` exited (`getppid() == 1`) |
//! | `own-group` | spawned in its own process group, same session |
//! | `setsid` | `--setsid`: its own session; its parent stays alive; starts `setsid-child` |
//! | `setsid-child` | child of a `setsid` sleeper |
//! | `setsid-orphan` | `--setsid`, parent `orphan-parent` exited; starts `orphan-child` |
//! | `orphan-child` | child of `setsid-orphan` |
//! | `unrelated` | started by the test itself |
//!
//! Windows supports `plain`, `double-forked`, and `unrelated`.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::{Opts, child_args};

/// How long a role waits to be reparented before it reports anyway.
const ORPHAN_WAIT: Duration = Duration::from_secs(10);

pub(crate) fn run(o: &Opts) -> i32 {
    let role = o.role.as_deref().unwrap_or("plain");
    #[cfg(unix)]
    if o.setsid && rustix::process::setsid().is_err() {
        return 1;
    }
    match role {
        "plain" | "unrelated" => {}
        "double-fork-parent" => {
            return exit_after_spawn(&mut sleeper(o, "double-forked"));
        }
        "double-forked" => wait_orphaned(),
        #[cfg(unix)]
        "ignore-term" => {
            let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            for sig in [signal_hook::consts::SIGHUP, signal_hook::consts::SIGTERM] {
                if signal_hook::flag::register(sig, flag.clone()).is_err() {
                    return 1;
                }
            }
        }
        #[cfg(unix)]
        "own-group" | "setsid" | "setsid-child" | "orphan-child" => {}
        #[cfg(unix)]
        "orphan-parent" => {
            let mut cmd = sleeper(o, "setsid-orphan");
            cmd.arg("--setsid");
            if o.raw {
                cmd.arg("--raw");
            }
            if !o.orphan_keeps_tty {
                cmd.stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
            }
            return exit_after_spawn(&mut cmd);
        }
        #[cfg(unix)]
        "setsid-orphan" => wait_orphaned(),
        _ => {
            eprintln!("chaldea-fake-cli: unknown role {role}");
            return 2;
        }
    }
    let raw = super::enter_raw(o, false);
    let _rep = super::hello(o, raw);
    let child = match role {
        "setsid" => Some("setsid-child"),
        "setsid-orphan" => Some("orphan-child"),
        _ => None,
    };
    if let Some(c) = child
        && sleeper(o, c).spawn().is_err()
    {
        return 1;
    }
    super::idle()
}

/// A FakeCli sleeper of `role`, inheriting the report address and life bound.
pub(crate) fn sleeper(o: &Opts, role: &str) -> Command {
    let exe = std::env::current_exe().unwrap_or_else(|_| "chaldea-fake-cli".into());
    let mut cmd = Command::new(exe);
    cmd.args(child_args(o, role));
    cmd
}

/// Spawns `cmd` and exits at once, so the child is reparented.
fn exit_after_spawn(cmd: &mut Command) -> i32 {
    match cmd.spawn() {
        Ok(_) => 0,
        Err(_) => 1,
    }
}

/// Waits until the parent has exited and the process was reparented to launchd/init.
fn wait_orphaned() {
    #[cfg(unix)]
    {
        let end = Instant::now() + ORPHAN_WAIT;
        while rustix::process::getppid().is_some_and(|p| p.as_raw_pid() != 1)
            && Instant::now() < end
        {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[cfg(not(unix))]
    let _ = (Instant::now(), ORPHAN_WAIT);
}
