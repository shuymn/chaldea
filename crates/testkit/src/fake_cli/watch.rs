//! FakeCli's bounded children (`docs/design/hook-ingest.md` §2.14). Every child's deadlines
//! are measured from its own launch and live here: [`settle`] is one loop over a set of
//! children that kills each at its deadline, reaps it, and collects its counted outputs,
//! independently of the order in which their results are then reported.

use std::io::Read;
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
        let now = Instant::now();
        let mut busy = false;
        for (i, w) in ws.iter_mut().enumerate() {
            let cutoff = w.deadline + REAP;
            match state[i] {
                State::Running => match w.child.try_wait() {
                    Ok(Some(s)) => {
                        done[i].code = if now < w.deadline { s.code() } else { None };
                        done[i].elapsed = now - w.launched;
                        state[i] = State::Reaped;
                    }
                    Ok(None) if now < w.deadline => busy = true,
                    _ => {
                        let _ = w.child.kill();
                        done[i].elapsed = now - w.launched;
                        state[i] = State::Killed;
                        busy = true;
                    }
                },
                State::Killed => {
                    if matches!(w.child.try_wait(), Ok(None)) && now < cutoff {
                        busy = true;
                    } else {
                        state[i] = State::Reaped;
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
                    Err(TryRecvError::Empty) if now < cutoff => busy = true,
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
    #[cfg(unix)]
    fn launch(script: &str, wait: Duration) -> Watched {
        let launched = Instant::now();
        let mut child = sh(script).spawn().unwrap();
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

    /// Three runs that never exit, settled together: each is killed at its own 300 ms bound,
    /// so the three end together.
    #[cfg(unix)]
    #[test]
    fn each_run_is_bounded_from_its_launch() {
        let wait = Duration::from_millis(300);
        let t = Instant::now();
        let ws = (0..3).map(|_| launch("exec sleep 30", wait)).collect();
        for s in settle(ws) {
            assert_eq!(s.code, None);
            assert!(s.elapsed >= wait, "{:?}", s.elapsed);
            assert_eq!(s.outputs, [Some(0), Some(0)]);
        }
        // Restated: 300 ms each; serial bounds would end the last at 900 ms.
        assert!(
            t.elapsed() < Duration::from_millis(600),
            "{:?}",
            t.elapsed()
        );
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
