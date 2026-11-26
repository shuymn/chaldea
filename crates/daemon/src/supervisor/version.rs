//! The CLI version (C1 D11): `<program> --version` once per Claude spawn or resume, before the
//! reservation, as the workspace probe runs git (a [`TrackedChild`] under the spawn lock, the
//! tracker, a deadline). One bound covers the child's exit and the read of its stdout, which
//! is read while the child runs, at most [`MAX_OUTPUT`] bytes. Any failure or timeout gives
//! `None`; the launch proceeds.

use std::ffi::OsString;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chaldea_core::ids::Agent;

use super::tracker::TrackedChild;
use super::{Supervisor, lock};

/// The probe's one bound, from its start: the child's exit and its stdout.
pub(crate) const VERSION_PROBE: Duration = Duration::from_secs(5);
/// The most stdout read; the rest is never read.
pub(crate) const MAX_OUTPUT: usize = 4096;
/// The poll period of the child and its stdout.
const POLL: Duration = Duration::from_millis(10);

/// Version children killed at their deadline and not yet reaped, with their tokens: reaped by
/// each tick and by the shutdown participant.
#[derive(Debug, Default)]
pub(crate) struct VersionLeft(Mutex<Vec<TrackedChild>>);

impl VersionLeft {
    /// Kills and reaps every child left, keeping those not yet reaped.
    pub(crate) fn reap(&self) {
        lock(&self.0).retain_mut(|c| {
            c.kill();
            c.try_reap().is_none()
        });
    }

    pub(crate) fn is_empty(&self) -> bool {
        lock(&self.0).is_empty()
    }
}

/// Owns the child; its drop, on every path including a panic, kills an unreaped child and
/// moves it with its token into [`VersionLeft`].
struct Held {
    left: Arc<VersionLeft>,
    child: Option<TrackedChild>,
}

impl Drop for Held {
    fn drop(&mut self) {
        let Some(mut c) = self.child.take() else {
            return;
        };
        if c.reaped() {
            return;
        }
        c.kill();
        if c.try_reap().is_none() {
            lock(&self.left.0).push(c);
        }
    }
}

/// The version `--version` printed: its first line's first word, when it starts with an ASCII
/// digit and holds only ASCII letters, digits, `.`, `+`, and `-` (`2.1.331 (Claude Code)` →
/// `2.1.331`).
pub(crate) fn parse(out: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(out).ok()?;
    let word = text.lines().next()?.split_whitespace().next()?;
    let ok = word.len() <= 64
        && word.starts_with(|c: char| c.is_ascii_digit())
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'));
    ok.then(|| word.to_owned())
}

/// `argv --version` bounded by `deadline` (supervisor clock): the parsed version when the
/// child exited 0 and its stdout reached its end (or [`MAX_OUTPUT`]) before the deadline.
pub(crate) fn probe(sup: &Supervisor, argv: &[OsString], deadline: Instant) -> Option<String> {
    let (program, lead) = argv.split_first()?;
    let mut cmd = Command::new(program);
    cmd.args(lead)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped());
    chaldea_pty_runtime::host::no_console_window(&mut cmd);
    let real_deadline = sup.real(deadline);
    let child = TrackedChild::spawn(&mut cmd, &sup.tracker, real_deadline, sup.pauses())?.ok()?;
    let mut held = Held {
        left: sup.version_left.clone(),
        child: Some(child),
    };
    let c = held.child.as_mut()?;
    // Read while the child runs: a child that fills the pipe never blocks on it, and the read
    // ends at the pipe's end or the cap, whichever comes first.
    let (tx, rx) = mpsc::channel();
    let stdout = c.take_stdout()?;
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.take(MAX_OUTPUT as u64).read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let mut exited = None;
    let mut out = None;
    loop {
        if !sup.tracker.accepting() {
            return None;
        }
        if exited.is_none() {
            exited = chaldea_pty_runtime::host::observed_before(real_deadline, || c.try_reap())?;
        }
        if out.is_none() {
            match rx.try_recv() {
                Ok(b) => out = Some(b),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => return None,
            }
        }
        if let (Some(status), Some(bytes)) = (&exited, &out) {
            if !status.success() {
                return None;
            }
            return parse(bytes);
        }
        chaldea_pty_runtime::host::observed_before(real_deadline, || ())?;
        std::thread::sleep(POLL);
    }
}

impl Supervisor {
    /// The CLI version of a launch of `agent`: Claude's from [`probe`] on a blocking task,
    /// `None` for Codex (X1 adds it) and on any failure.
    pub(crate) async fn cli_version(self: &Arc<Self>, agent: Agent) -> Option<String> {
        if agent != Agent::Claude {
            return None;
        }
        let argv = self.program(agent)?;
        let deadline = self.now() + VERSION_PROBE;
        let s = self.clone();
        let task = self.rt.spawn_blocking(move || probe(&s, &argv, deadline));
        let wait = deadline + Duration::from_secs(1);
        crate::deadline::settle_before(|| self.now(), wait, self.timer(wait), task)
            .await
            .in_time()
            .and_then(Result::ok)
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn parse_versions() {
        assert_eq!(
            parse(b"2.1.331 (Claude Code)\n").as_deref(),
            Some("2.1.331")
        );
        assert_eq!(
            parse(b"1.0.0-beta.1+x\r\n").as_deref(),
            Some("1.0.0-beta.1+x")
        );
        assert_eq!(parse(b"  0.9\nmore").as_deref(), Some("0.9"));
        for bad in [
            &b""[..],
            b"\n2.1.0",
            b"claude 2.1.0",
            b"2.1.0;rm",
            b"\xff2.1",
            b"2.1.\xc3\xa9",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
        assert_eq!(parse("9".repeat(65).as_bytes()), None);
    }
}
