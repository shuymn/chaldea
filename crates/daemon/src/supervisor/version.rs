//! The CLI version (C1 D11): `<program> --version` once per Claude spawn or resume, before the
//! reservation, as the workspace probe runs git (a [`TrackedChild`] under the spawn lock, the
//! tracker, a deadline). The child runs in its own process tree (`host::own_tree`: a process
//! group, or a job it joins before running), and its stdout is an anonymous temporary file,
//! not a pipe, so no read can block and a descendant holding stdout never delays the probe.
//! Once the child's exit is seen, the whole tree's kill is requested and the child reaped
//! before the file is read, at an offset; a descendant outside the tree, or a write still in
//! flight, can change what is read (C1 §3). Every other end kills the tree too. One deadline
//! covers the child's exit, the read, and the result's adoption. A nonzero exit, output over
//! [`MAX_OUTPUT`], or the deadline gives `None` at once; the launch proceeds.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chaldea_core::ids::Agent;
use chaldea_pty_runtime::host::ProcessTree;

use super::tracker::TrackedChild;
use super::{Supervisor, lock};

/// The probe's one bound, from its start: the child's exit, its output, and the adoption.
pub(crate) const VERSION_PROBE: Duration = Duration::from_secs(5);
/// The most output a version answer may have; more ends the probe with `None`.
pub(crate) const MAX_OUTPUT: u64 = 4096;
/// The poll period of the child and its output.
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

/// Owns the child and its tree; its drop, on every path including a panic, kills the tree of
/// an unreaped child (its group id still its own), then the child, and moves the child with its
/// token into [`VersionLeft`] when it cannot be reaped at once. Dropping the tree closes a
/// Windows job, which kills what is left in it.
struct Held<'a> {
    left: &'a VersionLeft,
    child: Option<TrackedChild>,
    tree: Option<ProcessTree>,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        let Some(mut c) = self.child.take() else {
            return;
        };
        if c.reaped() {
            return;
        }
        if let Some(t) = &self.tree {
            t.kill();
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

/// `f`'s result when observed before `deadline` (supervisor clock, mapped to real time at each call).
fn before<T>(sup: &Supervisor, deadline: Instant, f: impl FnOnce() -> T) -> Option<T> {
    chaldea_pty_runtime::host::observed_before(sup.real(deadline), f)
}

/// `argv --version` bounded by `deadline` (supervisor clock): the parsed version when the
/// child exited 0 with at most [`MAX_OUTPUT`] bytes of output, all before the deadline.
pub(crate) fn probe(sup: &Supervisor, argv: &[OsString], deadline: Instant) -> Option<String> {
    let (program, lead) = argv.split_first()?;
    let out = tempfile::tempfile().ok()?;
    let mut cmd = Command::new(program);
    cmd.args(lead)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::from(out.try_clone().ok()?));
    chaldea_pty_runtime::host::own_tree(&mut cmd);
    let child =
        TrackedChild::spawn(&mut cmd, &sup.tracker, sup.real(deadline), sup.pauses())?.ok()?;
    // The command holds the child's handle to the file: drop it, so only the child keeps it.
    drop(cmd);
    let mut held = Held {
        left: &sup.version_left,
        child: Some(child),
        tree: None,
    };
    // On Windows the child waits suspended until it is in its job; a failure leaves it to
    // `Held`, which kills it.
    held.tree = Some(held.child.as_ref()?.adopt_tree()?.ok()?);
    let (Some(c), Some(tree)) = (held.child.as_mut(), held.tree.as_ref()) else {
        return None;
    };
    loop {
        if !sup.tracker.accepting() {
            return None;
        }
        if before(sup, deadline, || c.exited(tree))? {
            // The tree's kill is requested before the read, so its descendants stop writing.
            tree.kill();
            if !c.try_reap()?.success() {
                return None;
            }
            let text = read_head(&out)?;
            if text.len() > usize::try_from(MAX_OUTPUT).ok()? {
                return None;
            }
            return before(sup, deadline, || parse(&text))?;
        }
        if out.metadata().ok()?.len() > MAX_OUTPUT {
            return None;
        }
        before(sup, deadline, || std::thread::sleep(POLL))?;
    }
}

/// The file's first [`MAX_OUTPUT`] + 1 bytes (fewer at its end), each read at its offset.
fn read_head(f: &File) -> Option<Vec<u8>> {
    let mut buf = vec![0; usize::try_from(MAX_OUTPUT).ok()? + 1];
    let mut n = 0;
    while n < buf.len() {
        let k = read_at(f, &mut buf[n..], u64::try_from(n).ok()?).ok()?;
        if k == 0 {
            break;
        }
        n += k;
    }
    buf.truncate(n);
    Some(buf)
}

#[cfg(unix)]
fn read_at(f: &File, buf: &mut [u8], at: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(f, buf, at)
}

#[cfg(windows)]
fn read_at(f: &File, buf: &mut [u8], at: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(f, buf, at)
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
        crate::deadline::settle_before(|| self.now(), deadline, self.timer(deadline), task)
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
