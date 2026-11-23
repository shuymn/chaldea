//! PR9-CL-23's exits (hook-ingest §2.13 "Test stall", §7.4): this binary (`harness = false`)
//! re-executes itself as `hook-emit codex`, with the emitter's environment and the case's
//! role set only on that child, and the child calls the library's `dispatch` with the role's
//! `Stall`. The child first leaves an unterminated line in Rust's stdout buffer, which the
//! normal exit (`std::process::exit`) flushes and `host::exit_now` does not, so every case's
//! empty stdout also shows that the emitter ended through `exit_now`, on every OS. One case
//! per exit of the emitter:
//!
//! - `Stall::AfterStdin` blocks the main thread once stdin is read: the child must still exit
//!   0 between 2.9 s and 4.5 s, silently, with no request; only the watchdog that `dispatch`
//!   starts can end it.
//! - `Stall::PanicAfterStdin` panics once stdin is read: the child must exit 0 within 2.5 s
//!   (before the 3 s watchdog), silently, with no request; only the panic hook ends it so.
//! - `Stall::Off` with an accepting listener: the run's final exit, after one request.
//! - `Stall::Off` without `CHALDEA_HOOK_URL`: the exit before stdin.
//! - `Stall::Off` with stdin held open: the exit at the 1.5 s stdin cut.
//!
//! The parent kills a child at 10 s. The control is `hook_emit::accepting`: the production
//! binary, with the same stdin and environment, posts.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chaldea_cli::Stall;
use chaldea_daemon::lifecycle::spawn_locked;

/// The child's role, set only in the child's environment and read by this binary's `main`,
/// never by the emitter.
const ROLE: &str = "CHALDEA_TEST_HOOK_EMIT_STALL";
const PARENT_BOUND: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(5);
/// Left in the child's stdout buffer before `dispatch`.
const EXIT_PROBE: &[u8] = b"RUST-EXIT-PROBE";
const PAYLOAD: &[u8] = br#"{"hook_event_name":"Stop","session_id":"s"}"#;

struct Case {
    name: &'static str,
    role: &'static str,
    /// Whether the child gets `CHALDEA_HOOK_URL`.
    url: bool,
    /// Whether stdin stays open until the child exits.
    open_stdin: bool,
    /// The connections the listener must see.
    requests: usize,
    /// The exit's window from the spawn, restated from the design with slack for process
    /// start: the 3 s watchdog, the 1.5 s stdin cut, or a prompt exit.
    window: std::ops::Range<Duration>,
}

const fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

const CASES: [Case; 5] = [
    Case {
        name: "stall_is_ended_by_the_watchdog",
        role: "after-stdin",
        url: true,
        open_stdin: false,
        requests: 0,
        window: ms(2900)..ms(4500),
    },
    Case {
        name: "a_panic_exits_0_silently",
        role: "panic-after-stdin",
        url: true,
        open_stdin: false,
        requests: 0,
        window: ms(0)..ms(2500),
    },
    Case {
        name: "a_posted_run_exits_through_exit_now",
        role: "off",
        url: true,
        open_stdin: false,
        requests: 1,
        window: ms(0)..ms(4500),
    },
    Case {
        name: "a_missing_url_exits_through_exit_now",
        role: "off",
        url: false,
        open_stdin: true,
        requests: 0,
        window: ms(0)..ms(1400),
    },
    Case {
        name: "open_stdin_exits_through_exit_now",
        role: "off",
        url: true,
        open_stdin: true,
        requests: 0,
        window: ms(1500)..ms(4500),
    },
];

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "hook-emit") {
        let stall = match std::env::var(ROLE).as_deref() {
            Ok("after-stdin") => Stall::AfterStdin,
            Ok("panic-after-stdin") => Stall::PanicAfterStdin,
            _ => Stall::Off,
        };
        // Unterminated, so it stays in Rust's stdout buffer: the normal exit
        // (`std::process::exit`) flushes it, `exit_now` does not.
        std::io::stdout().write_all(EXIT_PROBE).unwrap();
        chaldea_cli::dispatch(&args, stall);
    }
    // libtest's protocol, as far as `cargo test` uses it.
    if args.iter().any(|a| a == "--list") {
        for c in &CASES {
            println!("{}: test", c.name);
        }
        return;
    }
    let filters: Vec<String> = args
        .iter()
        .skip(1)
        .filter_map(|a| a.to_str())
        .filter(|a| !a.starts_with('-'))
        .map(str::to_owned)
        .collect();
    let selected: Vec<&Case> = CASES
        .iter()
        .filter(|c| filters.is_empty() || filters.iter().any(|f| c.name.contains(f.as_str())))
        .collect();
    println!("running {} tests", selected.len());
    for c in selected {
        run_case(c);
        println!("test {} ... ok", c.name);
    }
}

/// An accepting listener that answers 200 and counts connections; it stops when dropped.
struct Listener {
    port: u16,
    accepted: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Listener {
    fn start() -> Listener {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (count, halt) = (accepted.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !halt.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut s, _)) => {
                        count.fetch_add(1, Ordering::SeqCst);
                        s.set_nonblocking(false).unwrap();
                        s.set_read_timeout(Some(PARENT_BOUND)).unwrap();
                        s.set_write_timeout(Some(PARENT_BOUND)).unwrap();
                        let mut buf = [0u8; 4096];
                        let _ = s.read(&mut buf);
                        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(POLL);
                    }
                    Err(e) => panic!("accept: {e}"),
                }
            }
        });
        Listener {
            port,
            accepted,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Collects `r` on a thread; the result arrives on the returned channel.
fn collect(r: Option<impl Read + Send + 'static>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut r) = r {
            let _ = r.read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });
    rx
}

/// Kills `child` and reaps it, waiting at most [`PARENT_BOUND`].
fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let end = Instant::now() + PARENT_BOUND;
    while matches!(child.try_wait(), Ok(None)) && Instant::now() < end {
        std::thread::sleep(POLL);
    }
}

fn run_case(c: &Case) {
    let listener = Listener::start();
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["hook-emit", "codex"])
        .env_remove("CHALDEA_HOOK_URL")
        .env("CHALDEA_HOOK_TOKEN", format!("chd_{}", "a".repeat(43)))
        .env("CHALDEA_THREAD_ID", "0198c0de-0000-7000-8000-000000000001")
        .env(ROLE, c.role)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if c.url {
        cmd.env(
            "CHALDEA_HOOK_URL",
            format!("http://127.0.0.1:{}/hook", listener.port),
        );
    }
    // The lower bound is measured from before the spawn (never shorter than the child's
    // life), the upper bound from the spawn's return (the spawn lock's wait not counted).
    let before = Instant::now();
    let mut child = spawn_locked(&mut cmd).unwrap();
    let started = Instant::now();
    let out = collect(child.stdout.take());
    let err = collect(child.stderr.take());
    let mut stdin = child.stdin.take().unwrap();
    let held = if c.open_stdin {
        Some(stdin)
    } else {
        stdin.write_all(PAYLOAD).unwrap();
        drop(stdin);
        None
    };
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if started.elapsed() >= PARENT_BOUND {
            kill_and_reap(&mut child);
            panic!("{}: the child did not exit within {PARENT_BOUND:?}", c.name);
        }
        std::thread::sleep(POLL);
    };
    let (elapsed, spanned) = (started.elapsed(), before.elapsed());
    drop(held);
    let out = out.recv_timeout(PARENT_BOUND).expect("stdout not closed");
    let err = err.recv_timeout(PARENT_BOUND).expect("stderr not closed");
    let name = c.name;
    assert_eq!(status.code(), Some(0), "{name}: {status:?}");
    assert!(out.is_empty(), "{name}: stdout: {out:?}");
    assert!(
        err.is_empty(),
        "{name}: stderr: {:?}",
        String::from_utf8_lossy(&err)
    );
    assert!(
        spanned >= c.window.start && elapsed < c.window.end,
        "{name}: exited after {elapsed:?} ({spanned:?} from before the spawn), not within {:?}",
        c.window
    );
    assert_eq!(
        listener.accepted.load(Ordering::SeqCst),
        c.requests,
        "{name}: connections"
    );
}
