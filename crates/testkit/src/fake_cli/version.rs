//! `--version` after `--` (the daemon's version probe): sleeps `--version-sleep-ms`, then,
//! with `--version-descendant`, starts a FakeCli `sleeper` that inherits stdout and lives
//! 30 s, prints `--version-text` (`0.0.0 (FakeCli)` by default) and a newline, then
//! `--version-flood` bytes of `x`, sleeps `--version-linger-ms`, and exits with
//! `--version-exit`.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Debug, Clone)]
pub(crate) struct VersionOpts {
    pub(crate) text: String,
    pub(crate) exit: i32,
    pub(crate) sleep_ms: u64,
    pub(crate) flood: usize,
    pub(crate) linger_ms: u64,
    pub(crate) descendant: bool,
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
        }
    }
}

pub(crate) fn run(v: &VersionOpts) -> i32 {
    std::thread::sleep(Duration::from_millis(v.sleep_ms));
    if v.descendant {
        let _ = Command::new(std::env::current_exe().unwrap_or_default())
            .args(["--mode", "sleeper", "--max-life-secs", "30"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
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
    std::thread::sleep(Duration::from_millis(v.linger_ms));
    v.exit
}
