//! FakeCli's bounded children (`docs/design/hook-ingest.md` §2.14). Every child's deadlines
//! are measured from its own launch and live here: [`settle`] is one loop over a set of
//! children that kills each at its deadline, reaps it, and collects its counted outputs,
//! independently of the order in which their results are then reported.

use std::io::{self, Read};
use std::process::Child;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// How long past a child's deadline its counted outputs and its reap are awaited.
pub(crate) const REAP: Duration = Duration::from_secs(1);
const POLL: Duration = Duration::from_millis(10);

/// A launched child under its deadline.
pub(crate) struct Watched {
    child: Child,
    launched: Instant,
    deadline: Instant,
    /// Each counted output's length and the instant of its EOF, sent once by its thread.
    outputs: Vec<Receiver<(usize, Instant)>>,
}

/// How a child ended.
pub(crate) struct Settled {
    /// The exit code of an exit seen before the deadline; `None` when it was killed, was
    /// still running at the deadline, or ended by a signal.
    pub(crate) code: Option<i32>,
    /// From the launch to the exit, or to the kill.
    pub(crate) elapsed: Duration,
    /// Per counted output, in the order added: its length when its EOF came by the
    /// deadline plus [`REAP`]; `None` past that (a descendant holding the pipe).
    pub(crate) outputs: Vec<Option<usize>>,
}

impl Watched {
    /// `child`, launched at `launched`, to be killed `wait` after it.
    pub(crate) fn new(child: Child, launched: Instant, wait: Duration) -> Watched {
        Watched {
            child,
            launched,
            deadline: launched + wait,
            outputs: Vec::new(),
        }
    }

    /// Counts the bytes of `r` until its EOF, on a thread.
    pub(crate) fn count(&mut self, r: Option<impl Read + Send + 'static>) {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut r) = r {
                let _ = r.read_to_end(&mut buf);
            }
            let _ = tx.send((buf.len(), Instant::now()));
        });
        self.outputs.push(rx);
    }

    /// Moves the deadline to now: the next [`settle`] kills the child unless it has exited.
    pub(crate) fn end_now(&mut self) {
        self.deadline = Instant::now();
    }
}

/// An exit with `code` seen at `seen` keeps its code only when seen before `deadline`.
pub(crate) fn exit_code(code: Option<i32>, seen: Instant, deadline: Instant) -> Option<i32> {
    if seen < deadline { code } else { None }
}

/// What one poll of a running child decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Next {
    /// Still running before its deadline.
    Wait,
    /// Exited: its code by [`exit_code`], and the instant read after the poll.
    Exited(Option<i32>, Instant),
    /// Still running at its deadline, or the poll failed: kill it (a failure, `None`).
    Kill(Instant),
}

/// Polls a running child once with `try_wait` (its exit code when it has exited), retrying
/// an interrupted call, and classifies the result by the clock read after the poll returned.
pub(crate) fn next(
    mut try_wait: impl FnMut() -> io::Result<Option<Option<i32>>>,
    deadline: Instant,
) -> Next {
    loop {
        let r = try_wait();
        let at = Instant::now();
        return match r {
            Ok(Some(code)) => Next::Exited(exit_code(code, at, deadline), at),
            Ok(None) if at < deadline => Next::Wait,
            Ok(None) => Next::Kill(at),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => Next::Kill(at),
        };
    }
}

/// An output counted at `at` is kept only when it closed by `cutoff`.
pub(crate) fn by_cutoff((len, at): (usize, Instant), cutoff: Instant) -> Option<usize> {
    (at <= cutoff).then_some(len)
}

/// Where one child is in [`settle`].
enum State {
    Running,
    /// Killed; awaiting its reap until the deadline plus [`REAP`].
    Killed,
    Reaped,
}

/// Runs every child of `ws` to its end, in one loop: an exit seen before its deadline keeps
/// its code; a child still running at its deadline is killed (`None`) and reaped until
/// [`REAP`] after it; each counted output is awaited until the same bound.
pub(crate) fn settle(ws: Vec<Watched>) -> Vec<Settled> {
    settle_reporting(ws, |_| {})
}

/// [`settle`] that calls `reaped(i)` when it has reaped child `i` (after its exit, or after
/// its kill within [`REAP`]).
fn settle_reporting(ws: Vec<Watched>, mut reaped: impl FnMut(usize)) -> Vec<Settled> {
    let mut state: Vec<State> = ws.iter().map(|_| State::Running).collect();
    let mut done: Vec<Settled> = ws
        .iter()
        .map(|w| Settled {
            code: None,
            elapsed: Duration::ZERO,
            outputs: vec![None; w.outputs.len()],
        })
        .collect();
    let mut pending: Vec<Vec<bool>> = ws.iter().map(|w| vec![true; w.outputs.len()]).collect();
    let mut ws = ws;
    loop {
        let mut busy = false;
        for (i, w) in ws.iter_mut().enumerate() {
            let cutoff = w.deadline + REAP;
            match state[i] {
                State::Running => {
                    let child = &mut w.child;
                    match next(|| child.try_wait().map(|s| s.map(|s| s.code())), w.deadline) {
                        Next::Wait => busy = true,
                        Next::Exited(code, at) => {
                            done[i].code = code;
                            done[i].elapsed = at - w.launched;
                            state[i] = State::Reaped;
                            reaped(i);
                        }
                        Next::Kill(at) => {
                            let _ = w.child.kill();
                            done[i].elapsed = at - w.launched;
                            state[i] = State::Killed;
                            busy = true;
                        }
                    }
                }
                State::Killed => {
                    let gone = !matches!(w.child.try_wait(), Ok(None));
                    if !gone && Instant::now() < cutoff {
                        busy = true;
                    } else {
                        state[i] = State::Reaped;
                        if gone {
                            reaped(i);
                        }
                    }
                }
                State::Reaped => {}
            }
            for (j, rx) in w.outputs.iter().enumerate() {
                if !pending[i][j] {
                    continue;
                }
                match rx.try_recv() {
                    Ok(c) => {
                        done[i].outputs[j] = by_cutoff(c, cutoff);
                        pending[i][j] = false;
                    }
                    Err(TryRecvError::Empty) if Instant::now() < cutoff => busy = true,
                    Err(_) => pending[i][j] = false,
                }
            }
        }
        if !busy {
            return done;
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exit_counts_only_when_seen_before_the_deadline() {
        let deadline = Instant::now();
        assert_eq!(exit_code(Some(0), deadline - POLL, deadline), Some(0));
        assert_eq!(exit_code(Some(0), deadline, deadline), None);
        assert_eq!(exit_code(Some(0), deadline + POLL, deadline), None);
        assert_eq!(exit_code(None, deadline - POLL, deadline), None);
    }

    /// The decision uses the clock read after the poll: an exit status returned by a poll
    /// that ends after the deadline is not a success.
    #[test]
    fn a_poll_that_returns_after_the_deadline_is_not_a_success() {
        let deadline = Instant::now() + Duration::from_millis(20);
        let late = || {
            std::thread::sleep(Duration::from_millis(40));
            Ok(Some(Some(0)))
        };
        let Next::Exited(code, at) = next(late, deadline) else {
            panic!("not an exit")
        };
        assert!(at >= deadline);
        assert_eq!(code, None);
        // Before the deadline: the code is kept.
        let far = Instant::now() + Duration::from_secs(30);
        assert!(matches!(
            next(|| Ok(Some(Some(0))), far),
            Next::Exited(Some(0), _)
        ));
    }

    /// An interrupted poll is retried; any other poll error kills the child.
    #[test]
    fn a_poll_error_kills_unless_interrupted() {
        let far = Instant::now() + Duration::from_secs(30);
        let mut calls = 0;
        let interrupted_once = || {
            calls += 1;
            if calls == 1 {
                Err(io::Error::from(io::ErrorKind::Interrupted))
            } else {
                Ok(None)
            }
        };
        assert_eq!(next(interrupted_once, far), Next::Wait);
        assert_eq!(calls, 2);
        assert!(matches!(
            next(|| Err(io::Error::other("ECHILD")), far),
            Next::Kill(_)
        ));
    }

    #[test]
    fn an_output_counts_only_when_it_closed_by_the_cutoff() {
        let cutoff = Instant::now();
        assert_eq!(by_cutoff((7, cutoff), cutoff), Some(7));
        assert_eq!(by_cutoff((7, cutoff - POLL), cutoff), Some(7));
        assert_eq!(by_cutoff((7, cutoff + POLL), cutoff), None);
    }

    #[cfg(unix)]
    fn sh(script: &str) -> std::process::Command {
        let mut c = std::process::Command::new("/bin/sh");
        c.args(["-c", script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        c
    }

    /// Launches `script` with both outputs counted, to be killed after `wait`.
    ///
    /// Every spawn of these tests holds the process-wide spawn lock: std sets close-on-exec
    /// on a new pipe in a second step on macOS (no `pipe2`), and a child another test spawns
    /// in between would inherit the pipe and hold a counted output open.
    #[cfg(unix)]
    fn launch(script: &str, wait: Duration) -> Watched {
        let launched = Instant::now();
        let mut c = sh(script);
        let mut child = chaldea_pty_runtime::with_spawn_lock(|| c.spawn()).unwrap();
        let (out, err) = (child.stdout.take(), child.stderr.take());
        let mut w = Watched::new(child, launched, wait);
        w.count(out);
        w.count(err);
        w
    }

    /// A `sleep 30` started in the background by a script, its pid written to a file, so the
    /// test can kill it by pid.
    #[cfg(unix)]
    struct Descendant(std::path::PathBuf);

    #[cfg(unix)]
    impl Descendant {
        fn new() -> Descendant {
            let dir = std::env::temp_dir().join(format!(
                "chaldea-watch-{}",
                super::super::hooks::random_uuid()
            ));
            std::fs::create_dir(&dir).unwrap();
            Descendant(dir)
        }
        /// A script that starts the descendant, holding the script's stdout, and exits 0.
        fn script(&self) -> String {
            format!(
                "sleep 30 2>/dev/null & echo $! > '{}'; exit 0",
                self.0.join("pid").display()
            )
        }
    }

    #[cfg(unix)]
    impl Drop for Descendant {
        fn drop(&mut self) {
            if let Some(pid) = std::fs::read_to_string(self.0.join("pid"))
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .and_then(rustix::process::Pid::from_raw)
            {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Each run's deadline is its own launch plus its wait. Two runs with the same 40 s wait
    /// and launch instants set in the past once both are spawned, settled together: X,
    /// launched 40 s ago, is at its deadline as `settle` starts and never exits; Y, launched
    /// 20 s ago, has 20 s left and exits only when the test lets it, which it does once
    /// `settle` reports X reaped. A deadline counted from `settle`'s entry (X not killed),
    /// one deadline shared by both (X not killed, or Y killed), or runs settled one after
    /// another (X not watched while Y runs) fails: no report within 5 s, or Y killed.
    #[cfg(unix)]
    #[test]
    fn each_run_is_bounded_from_its_launch() {
        let wait = Duration::from_secs(40);
        let mut y = sh("read -r _; exit 0");
        y.stdin(std::process::Stdio::piped());
        let spawn = |mut c: std::process::Command| {
            chaldea_pty_runtime::with_spawn_lock(|| c.spawn()).unwrap()
        };
        let mut children = [spawn(y), spawn(sh("exec sleep 30"))];
        let stdin = children[0].stdin.take().unwrap();
        // Set after the spawns, so X's reap bound (`REAP` past its deadline) starts now.
        let now = Instant::now();
        let ago = |s: u64| {
            now.checked_sub(Duration::from_secs(s))
                .expect("the host has been up for 40 s")
        };
        let ws: Vec<Watched> = children
            .into_iter()
            .zip([ago(20), ago(40)])
            .map(|(mut child, launched)| {
                let (out, err) = (child.stdout.take(), child.stderr.take());
                let mut w = Watched::new(child, launched, wait);
                w.count(out);
                w.count(err);
                w
            })
            .collect();
        let (tx, rx) = mpsc::channel();
        let gate = std::thread::spawn(move || {
            let x_reaped = rx.recv_timeout(Duration::from_secs(5)) == Ok(1);
            drop(stdin);
            x_reaped
        });
        let r = settle_reporting(ws, move |i| {
            let _ = tx.send(i);
        });
        assert!(gate.join().unwrap(), "X was not reaped first, within 5 s");
        assert_eq!(
            (r[0].code, &r[0].outputs),
            (Some(0), &vec![Some(0), Some(0)]),
            "Y"
        );
        assert_eq!(r[1].code, None, "X");
        assert!(r[1].elapsed >= wait, "{:?}", r[1].elapsed);
        assert_eq!(r[1].outputs, [Some(0), Some(0)]);
    }

    /// A run that exits while a descendant holds its stdout: the count is given up at the
    /// deadline plus the 1 s reap.
    #[cfg(unix)]
    #[test]
    fn an_output_held_open_is_bounded() {
        let d = Descendant::new();
        let wait = Duration::from_millis(300);
        let t = Instant::now();
        let s = settle(vec![launch(&d.script(), wait)]).remove(0);
        let took = t.elapsed();
        drop(d);
        assert_eq!(s.code, Some(0));
        assert_eq!(
            s.outputs,
            [None, Some(0)],
            "stdout is held by the descendant"
        );
        // Restated: the 300 ms bound plus the 1 s reap.
        assert!(took < Duration::from_millis(2000), "{took:?}");
        assert!(took >= Duration::from_millis(1300), "{took:?}");
    }

    /// Two runs launched together: A exits at once while a descendant holds its stdout, B
    /// outlives its deadline and then exits 0. B is killed at its own deadline, whatever A's
    /// output does, and is not a success.
    #[cfg(unix)]
    #[test]
    fn a_sibling_is_killed_at_its_own_deadline() {
        let d = Descendant::new();
        let wait = Duration::from_millis(300);
        let a = launch(&d.script(), wait);
        let b = launch("sleep 0.8; exit 0", wait);
        let r = settle(vec![a, b]);
        drop(d);
        assert_eq!((r[0].code, r[0].outputs[0]), (Some(0), None));
        assert_eq!(r[1].code, None, "B outlived its deadline");
        // Restated: B's 300 ms bound, with slack well under A's 1 s reap.
        assert!(
            r[1].elapsed < Duration::from_millis(700),
            "{:?}",
            r[1].elapsed
        );
    }
}
