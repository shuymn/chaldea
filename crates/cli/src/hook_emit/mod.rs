//! `chaldea hook-emit <agent>` (hook-ingest §2.13): posts one hook payload from stdin to
//! `CHALDEA_HOOK_URL` with one `emit_id`, retries within its budget, and always exits 0
//! within 3 s with nothing on stdout or stderr.
//!
//! Budget, from `T0` (the start of [`main`]): the watchdog exits at `T0 + 3000 ms`; stdin
//! must reach EOF before `T0 + 1500 ms`; no attempt starts unless 300 ms remain before the
//! work end `E = T0 + 2900 ms`. Attempt `k` has the absolute deadline `min(E, s_k + 1800 ms)`
//! for `k = 1` and `min(E, s_k + 1000 ms)` for `k = 2, 3`, after a backoff of 0, 50, 100 ms.

mod client;
mod env;
mod watchdog;

use std::ffi::OsString;
use std::io::Read;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use self::client::{Tcp, Transport};
use self::env::Env;

/// The emitter's test seam (hook-ingest §2.13 "Test stall"). The shipped binary's `main`
/// always passes [`Stall::Off`]; no argument or environment variable changes it.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stall {
    Off,
    /// Blocks the main thread forever once stdin is read, so only the watchdog ends the run.
    AfterStdin,
}

/// The body cap (mvp §2.5): a longer stdin is sent as its first 4 MiB, marked truncated.
pub(crate) const MAX_BODY: usize = 4 * 1024 * 1024;

pub(crate) const WATCHDOG: Duration = Duration::from_millis(3000);
pub(crate) const WORK_END: Duration = Duration::from_millis(2900);
pub(crate) const STDIN_END: Duration = Duration::from_millis(1500);
const FIRST_ATTEMPT: Duration = Duration::from_millis(1800);
const LATER_ATTEMPT: Duration = Duration::from_millis(1000);
const MIN_ATTEMPT: Duration = Duration::from_millis(300);
const BACKOFF: [Duration; 3] = [
    Duration::ZERO,
    Duration::from_millis(50),
    Duration::from_millis(100),
];

/// Time as the retry loop reads it (a test clock can jump during a sleep).
pub(crate) trait Clock {
    fn now(&self) -> Instant;
    fn sleep(&self, d: Duration);
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// The run of §2.13; never returns, and exits 0 on every path.
pub(crate) fn main(args: &[OsString], stall: Stall) -> ! {
    let t0 = Instant::now();
    watchdog::start(t0 + WATCHDOG);
    let Some(env) = env::read() else {
        std::process::exit(0)
    };
    let Some(body) = read_stdin(t0 + STDIN_END) else {
        std::process::exit(0)
    };
    if stall == Stall::AfterStdin {
        let (_never_sent, rx) = mpsc::channel::<()>();
        let _ = rx.recv();
    }
    let agent = args
        .get(2)
        .and_then(|a| a.to_str())
        .filter(|a| env::header_safe(a));
    let request = Request {
        env: &env,
        agent,
        emit_id: uuid::Uuid::new_v4(),
        pid: std::process::id(),
        truncated: body.truncated,
    };
    let clock = SystemClock;
    let mut wire = Tcp { port: env.port };
    run(&clock, t0, |k, deadline| {
        next(client::attempt(
            &clock,
            &mut wire,
            &request.head(k, body.bytes.len()),
            &body.bytes,
            deadline,
        ))
    });
    std::process::exit(0)
}

/// Stdin as it is sent.
pub(crate) struct Body {
    pub(crate) bytes: Vec<u8>,
    pub(crate) truncated: bool,
}

/// What the stdin reader hands over: its result and when it completed.
pub(crate) struct StdinDone {
    pub(crate) result: std::io::Result<Vec<u8>>,
    pub(crate) at: Instant,
}

/// Reads stdin on a thread, up to `MAX_BODY + 1` bytes, until `until`: `None` when it is
/// still open then, or the read failed.
fn read_stdin(until: Instant) -> Option<Body> {
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::Builder::new()
        .name("hook-emit-stdin".to_owned())
        .spawn(move || {
            let result = read_capped(&mut std::io::stdin().lock());
            let _ = tx.send(StdinDone {
                result,
                at: Instant::now(),
            });
        });
    if reader.is_err() {
        return None;
    }
    receive_stdin(&SystemClock, &rx, until)
}

/// Reads `r` to EOF, keeping at most `MAX_BODY + 1` bytes: past that the rest is read and
/// discarded, so the result (a truncated prefix included) exists only once the input
/// ended. An input still open at the cut is therefore never sent, whatever its size.
pub(crate) fn read_capped(r: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    r.by_ref().take(MAX_BODY as u64 + 1).read_to_end(&mut buf)?;
    if buf.len() > MAX_BODY {
        std::io::copy(r, &mut std::io::sink())?;
    }
    Ok(buf)
}

/// Takes the reader's result, classified by when the reader completed, not by when this
/// thread woke: a result completed before `until` and already queued is sent even when the
/// receive happens after `until`.
pub(crate) fn receive_stdin(
    clock: &dyn Clock,
    rx: &mpsc::Receiver<StdinDone>,
    until: Instant,
) -> Option<Body> {
    let d = rx
        .recv_timeout(until.saturating_duration_since(clock.now()))
        .ok()?;
    stdin_outcome(Some(d.result), d.at, until)
}

/// A stdin result completed at `seen`: sent only when it completed before `until` (a result
/// completed at or after the cut is not sent), a longer one truncated to `MAX_BODY`.
pub(crate) fn stdin_outcome(
    r: Option<std::io::Result<Vec<u8>>>,
    seen: Instant,
    until: Instant,
) -> Option<Body> {
    if seen >= until {
        return None;
    }
    let mut bytes = r?.ok()?;
    let truncated = bytes.len() > MAX_BODY;
    bytes.truncate(MAX_BODY);
    Some(Body { bytes, truncated })
}

/// Whether an attempt's result calls for another attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Next {
    Retry,
    Stop,
}

/// Retry on a transport error, 408, or 503; stop on any other status.
pub(crate) fn next(r: Result<u16, Transport>) -> Next {
    match r {
        Err(_) | Ok(408 | 503) => Next::Retry,
        Ok(_) => Next::Stop,
    }
}

/// The retry loop of §2.13 step 4: at most three attempts, each started only when its
/// minimum time fits before the work end, checked before the backoff and again with the
/// clock read after it. Returns the start of each attempt made.
pub(crate) fn run(
    clock: &dyn Clock,
    t0: Instant,
    mut attempt: impl FnMut(u8, Instant) -> Next,
) -> Vec<Instant> {
    let end = t0 + WORK_END;
    let mut starts = Vec::new();
    for (k, backoff) in (1u8..).zip(BACKOFF) {
        if clock.now() + backoff + MIN_ATTEMPT > end {
            break;
        }
        if !backoff.is_zero() {
            clock.sleep(backoff);
        }
        let start = clock.now();
        if start + MIN_ATTEMPT > end {
            break;
        }
        let window = if k == 1 { FIRST_ATTEMPT } else { LATER_ATTEMPT };
        starts.push(start);
        if attempt(k, end.min(start + window)) == Next::Stop {
            break;
        }
    }
    starts
}

/// The request of one hook (hook-ingest §2.2), with the attempt number per head.
pub(crate) struct Request<'a> {
    pub(crate) env: &'a Env,
    pub(crate) agent: Option<&'a str>,
    pub(crate) emit_id: uuid::Uuid,
    pub(crate) pid: u32,
    pub(crate) truncated: bool,
}

impl Request<'_> {
    pub(crate) fn head(&self, attempt: u8, body_len: usize) -> Vec<u8> {
        use std::fmt::Write as _;
        let e = self.env;
        let mut h = String::with_capacity(512);
        let _ = write!(
            h,
            "POST /hook HTTP/1.1\r\n\
             Host: {}:{}\r\n\
             Connection: close\r\n\
             Authorization: Bearer {}\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {body_len}\r\n\
             X-Chaldea-Emit-Id: {}\r\n\
             X-Chaldea-Attempt: {attempt}\r\n\
             X-Chaldea-Emitter-Pid: {}\r\n",
            e.host,
            e.port,
            e.token,
            self.emit_id.hyphenated(),
            self.pid,
        );
        if let Some(t) = &e.thread {
            let _ = write!(h, "X-Chaldea-Thread-Id: {t}\r\n");
        }
        if let Some(a) = self.agent {
            let _ = write!(h, "X-Chaldea-Agent: {a}\r\n");
        }
        let _ = write!(
            h,
            "X-Chaldea-Emitter-Version: {}\r\n",
            env!("CARGO_PKG_VERSION")
        );
        if self.truncated {
            h.push_str("X-Chaldea-Body-Truncated: 1\r\n");
        }
        h.push_str("\r\n");
        h.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;

    /// A clock that advances only through `sleep` (plus `jump` once, inside the first sleep)
    /// and through attempts that take time.
    struct TestClock {
        now: Cell<Instant>,
        jump: Cell<Duration>,
        slept: RefCell<Vec<Duration>>,
    }

    impl TestClock {
        fn at(t: Instant) -> TestClock {
            TestClock {
                now: Cell::new(t),
                jump: Cell::new(Duration::ZERO),
                slept: RefCell::new(Vec::new()),
            }
        }
        fn advance(&self, d: Duration) {
            self.now.set(self.now.get() + d);
        }
    }

    impl Clock for TestClock {
        fn now(&self) -> Instant {
            self.now.get()
        }
        fn sleep(&self, d: Duration) {
            self.slept.borrow_mut().push(d);
            self.advance(d + self.jump.replace(Duration::ZERO));
        }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Runs the loop with attempts that each take `takes[k-1]` and answer `answers[k-1]`;
    /// returns `(attempt number, start offset, deadline offset)` per attempt.
    fn script(
        clock: &TestClock,
        t0: Instant,
        takes: &[u64],
        answers: &[Next],
    ) -> Vec<(u8, Duration, Duration)> {
        let seen = RefCell::new(Vec::new());
        let starts = run(clock, t0, |k, deadline| {
            let i = usize::from(k - 1);
            seen.borrow_mut().push((k, clock.now() - t0, deadline - t0));
            clock.advance(ms(takes[i]));
            answers[i]
        });
        let seen = seen.into_inner();
        assert_eq!(starts.len(), seen.len());
        seen
    }

    #[test]
    fn three_attempts_with_backoff_and_absolute_deadlines() {
        let t0 = Instant::now();
        let clock = TestClock::at(t0);
        let seen = script(&clock, t0, &[10, 10, 10], &[Next::Retry; 3]);
        assert_eq!(
            seen,
            [
                (1, ms(0), ms(1800)),
                (2, ms(60), ms(1060)),
                (3, ms(170), ms(1170)),
            ]
        );
    }

    #[test]
    fn stops_on_a_final_status() {
        let t0 = Instant::now();
        let clock = TestClock::at(t0);
        let seen = script(
            &clock,
            t0,
            &[10, 10, 10],
            &[Next::Retry, Next::Stop, Next::Retry],
        );
        assert_eq!(seen.len(), 2);
    }

    #[test]
    fn a_silent_first_attempt_leaves_one_more() {
        // Attempt 1 runs to its deadline (1800 ms); attempt 2 starts at 1850 ms and is cut
        // by the work end (2900 ms); attempt 3 would start at 2850 + 100 > 2900 - 300.
        let t0 = Instant::now();
        let clock = TestClock::at(t0);
        let seen = script(&clock, t0, &[1800, 1000, 0], &[Next::Retry; 3]);
        assert_eq!(seen, [(1, ms(0), ms(1800)), (2, ms(1850), ms(2850))]);
    }

    #[test]
    fn deadlines_never_pass_the_work_end() {
        let t0 = Instant::now();
        let clock = TestClock::at(t0 + ms(2000));
        let seen = script(&clock, t0, &[100, 100, 100], &[Next::Retry; 3]);
        assert_eq!(
            seen,
            [
                (1, ms(2000), ms(2900)),
                (2, ms(2150), ms(2900)),
                (3, ms(2350), ms(2900)),
            ]
        );
        // The last start that fits: 300 ms before the work end.
        let clock = TestClock::at(t0 + ms(2600));
        assert_eq!(script(&clock, t0, &[0, 0, 0], &[Next::Retry; 3]).len(), 1);
        let clock = TestClock::at(t0 + ms(2601));
        assert!(script(&clock, t0, &[0, 0, 0], &[Next::Retry; 3]).is_empty());
    }

    #[test]
    fn no_backoff_when_the_next_attempt_cannot_fit() {
        // After attempt 1, 2560 + 50 + 300 > 2900: no sleep and no attempt 2.
        let t0 = Instant::now();
        let clock = TestClock::at(t0 + ms(2500));
        let seen = script(&clock, t0, &[60, 0, 0], &[Next::Retry; 3]);
        assert_eq!(seen, [(1, ms(2500), ms(2900))]);
        assert!(clock.slept.borrow().is_empty());
        // At 2550 the backoff fits (2550 + 50 + 300 = 2900).
        let clock = TestClock::at(t0 + ms(2500));
        let seen = script(&clock, t0, &[50, 0, 0], &[Next::Retry; 3]);
        assert_eq!(seen.len(), 2);
        assert_eq!(*clock.slept.borrow(), [ms(50)]);
    }

    #[test]
    fn budget_rechecked_after_backoff() {
        // Before the backoff of attempt 2 the budget fits (2000 + 50 + 300 <= 2900), but the
        // clock jumps 600 ms during the sleep: attempt 2 is not made.
        let t0 = Instant::now();
        let clock = TestClock::at(t0 + ms(2000));
        clock.jump.set(ms(600));
        let seen = script(&clock, t0, &[0, 0, 0], &[Next::Retry; 3]);
        assert_eq!(seen, [(1, ms(2000), ms(2900))]);
    }

    #[test]
    fn retry_policy() {
        assert_eq!(next(Err(Transport::Timeout)), Next::Retry);
        assert_eq!(next(Err(Transport::Eof)), Next::Retry);
        assert_eq!(next(Err(Transport::Malformed)), Next::Retry);
        assert_eq!(
            next(Err(Transport::Io(std::io::ErrorKind::ConnectionReset))),
            Next::Retry
        );
        assert_eq!(next(Ok(408)), Next::Retry);
        assert_eq!(next(Ok(503)), Next::Retry);
        for final_status in [200, 201, 400, 401, 403, 404, 411, 413, 421, 500, 502, 504] {
            assert_eq!(next(Ok(final_status)), Next::Stop, "{final_status}");
        }
    }

    #[test]
    fn a_result_completed_before_the_cut_is_sent_when_received_after_it() {
        let t0 = Instant::now();
        let until = t0 + ms(1500);
        let (tx, rx) = mpsc::channel();
        tx.send(StdinDone {
            result: Ok(b"{}".to_vec()),
            at: until - ms(10),
        })
        .unwrap();
        // The receiver wakes 10 ms after the cut.
        let clock = TestClock::at(until + ms(10));
        let got = receive_stdin(&clock, &rx, until).map(|b| b.bytes);
        assert_eq!(got.as_deref(), Some(&b"{}"[..]));
        // Completed at the cut: not sent, whenever it is received.
        tx.send(StdinDone {
            result: Ok(b"{}".to_vec()),
            at: until,
        })
        .unwrap();
        let clock = TestClock::at(until - ms(1));
        assert!(receive_stdin(&clock, &rx, until).is_none());
        // Nothing by the cut: not sent.
        let clock = TestClock::at(until);
        assert!(receive_stdin(&clock, &rx, until).is_none());
    }

    #[test]
    fn an_oversized_input_is_read_to_its_end() {
        let mut input = std::io::Cursor::new(vec![b'x'; MAX_BODY + 4096]);
        let got = read_capped(&mut input).unwrap();
        assert_eq!(got.len(), MAX_BODY + 1);
        assert_eq!(input.position(), (MAX_BODY + 4096) as u64, "drained to EOF");
        let mut short = std::io::Cursor::new(b"{}".to_vec());
        assert_eq!(read_capped(&mut short).unwrap(), b"{}");
    }

    #[test]
    fn stdin_cut() {
        let until = Instant::now();
        let before = until - ms(1);
        let got = |r: Option<std::io::Result<Vec<u8>>>, seen| {
            stdin_outcome(r, seen, until).map(|b| (b.bytes.len(), b.truncated))
        };
        assert_eq!(got(Some(Ok(b"{}".to_vec())), before), Some((2, false)));
        assert_eq!(got(Some(Ok(Vec::new())), before), Some((0, false)));
        // EOF and the cut at one poll: the cut wins.
        assert_eq!(got(Some(Ok(b"{}".to_vec())), until), None);
        assert_eq!(got(Some(Ok(b"{}".to_vec())), until + ms(1)), None);
        assert_eq!(got(None, before), None);
        assert_eq!(
            got(Some(Err(std::io::ErrorKind::BrokenPipe.into())), before),
            None
        );
        assert_eq!(
            got(Some(Ok(vec![b'x'; MAX_BODY])), before),
            Some((MAX_BODY, false))
        );
        assert_eq!(
            got(Some(Ok(vec![b'x'; MAX_BODY + 1])), before),
            Some((MAX_BODY, true))
        );
    }

    #[test]
    fn head_shape() {
        let env = Env {
            host: "localhost",
            port: 4321,
            token: format!("chd_{}", "a".repeat(43)),
            thread: Some("t-1".to_owned()),
        };
        let id = uuid::Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
        let mut r = Request {
            env: &env,
            agent: Some("codex"),
            emit_id: id,
            pid: 77,
            truncated: false,
        };
        let head = String::from_utf8(r.head(2, 9)).unwrap();
        assert_eq!(
            head,
            format!(
                "POST /hook HTTP/1.1\r\nHost: localhost:4321\r\nConnection: close\r\n\
                 Authorization: Bearer chd_{a}\r\nContent-Type: application/json\r\n\
                 Content-Length: 9\r\nX-Chaldea-Emit-Id: 01234567-89ab-cdef-0123-456789abcdef\r\n\
                 X-Chaldea-Attempt: 2\r\nX-Chaldea-Emitter-Pid: 77\r\nX-Chaldea-Thread-Id: t-1\r\n\
                 X-Chaldea-Agent: codex\r\nX-Chaldea-Emitter-Version: {v}\r\n\r\n",
                a = "a".repeat(43),
                v = env!("CARGO_PKG_VERSION"),
            )
        );
        r.truncated = true;
        r.agent = None;
        let head = String::from_utf8(r.head(1, 0)).unwrap();
        assert!(
            head.ends_with("X-Chaldea-Body-Truncated: 1\r\n\r\n"),
            "{head}"
        );
        assert!(!head.contains("X-Chaldea-Agent"), "{head}");
    }
}
