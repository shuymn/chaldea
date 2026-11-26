//! `--version` after `--` (the daemon's version probe): waits for the file `--version-gate`
//! names, if any; sleeps `--version-sleep-ms`; prints `--version-text` (`0.0.0 (FakeCli)` by
//! default) and a newline, then `--version-flood` bytes of `x`; with `--version-descendant`,
//! starts a FakeCli that inherits stdout and lives 30 s, a `sleeper`, or with
//! `--version-descendant-writes` one that writes an `x` every 10 ms (`--version-trickle`),
//! writing its pid to `--version-pidfile`; sleeps `--version-linger-ms`; and exits with
//! `--version-exit`. A descendant that cannot be started exits 98; a gate that does not open in
//! 60 s exits 99.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub(crate) struct VersionOpts {
    pub(crate) text: String,
    pub(crate) exit: i32,
    pub(crate) sleep_ms: u64,
    pub(crate) flood: usize,
    pub(crate) linger_ms: u64,
    pub(crate) descendant: bool,
    pub(crate) descendant_writes: bool,
    pub(crate) pidfile: Option<PathBuf>,
    pub(crate) gate: Option<PathBuf>,
    pub(crate) trickle: bool,
}

impl Default for VersionOpts {
    fn default() -> Self {
        VersionOpts {
            text: "0.0.0 (FakeCli)".to_owned(),
            exit: 0,
            sleep_ms: 0,
            flood: 0,
            linger_ms: 0,
            descendant: false,
            descendant_writes: false,
            pidfile: None,
            gate: None,
            trickle: false,
        }
    }
}

/// How long a descendant lives, and how long a gate is waited for.
const LIFE: Duration = Duration::from_secs(30);
const GATE: Duration = Duration::from_secs(60);

pub(crate) fn run(v: &VersionOpts) -> i32 {
    if v.trickle {
        return trickle();
    }
    if let Some(gate) = &v.gate {
        let end = Instant::now() + GATE;
        while !gate.exists() {
            if Instant::now() >= end {
                return 99;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    std::thread::sleep(Duration::from_millis(v.sleep_ms));
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", v.text);
    let chunk = [b'x'; 8192];
    let mut left = v.flood;
    while left > 0 {
        let n = left.min(chunk.len());
        if out.write_all(&chunk[..n]).is_err() {
            break;
        }
        left -= n;
    }
    let _ = out.flush();
    drop(out);
    if v.descendant || v.descendant_writes {
        let exe = std::env::current_exe().unwrap_or_default();
        let mut cmd = Command::new(exe);
        if v.descendant_writes {
            cmd.args(["--mode", "sleeper", "--version-trickle", "--", "--version"]);
        } else {
            cmd.args(["--mode", "sleeper", "--max-life-secs", "30"]);
        }
        match cmd.stdin(Stdio::null()).stderr(Stdio::null()).spawn() {
            Ok(child) => {
                if let Some(p) = &v.pidfile {
                    let _ = std::fs::write(p, child.id().to_string());
                }
            }
            Err(e) => {
                eprintln!("fake-cli: the version descendant did not start: {e}");
                return 98;
            }
        }
    }
    std::thread::sleep(Duration::from_millis(v.linger_ms));
    v.exit
}

/// An `x` on stdout every 10 ms for [`LIFE`], ending early when stdout fails.
fn trickle() -> i32 {
    let end = Instant::now() + LIFE;
    let mut out = std::io::stdout();
    while Instant::now() < end {
        if out.write_all(b"x").and_then(|()| out.flush()).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    0
}
