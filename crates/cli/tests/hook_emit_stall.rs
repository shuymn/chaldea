//! PR9-CL-23's watchdog and panic hook (hook-ingest §2.13 "Test stall", §7.4): this binary
//! (`harness = false`) re-executes itself as `hook-emit codex`, with the emitter's
//! environment and the stall it should use set only on that child, and the child calls the
//! library's `dispatch` with that `Stall`. Against an accepting listener:
//!
//! - `Stall::AfterStdin` blocks the main thread once stdin is read: the child must still exit
//!   0 between 2.9 s and 4.5 s, silently, with no request; only the watchdog that `dispatch`
//!   starts can end it.
//! - `Stall::PanicAfterStdin` panics once stdin is read: the child must exit 0 within 1 s,
//!   silently, with no request; only the panic hook ends it so.
//!
//! The parent kills a child at 10 s. The control is `hook_emit::accepting`: the production
//! binary, with the same stdin and environment, posts.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use chaldea_cli::Stall;
use chaldea_daemon::lifecycle::spawn_locked;

/// The child's stall, set only in the child's environment and read by this binary's `main`,
/// never by the emitter.
const ROLE: &str = "CHALDEA_TEST_HOOK_EMIT_STALL";
const PARENT_BOUND: Duration = Duration::from_secs(10);
const PAYLOAD: &[u8] = br#"{"hook_event_name":"Stop","session_id":"s"}"#;

struct Case {
    name: &'static str,
    role: &'static str,
    /// The exit's window from the spawn: the 3 s watchdog (restated from the design) with
    /// slack for process start, or a prompt exit.
    window: std::ops::Range<Duration>,
}

const CASES: [Case; 2] = [
    Case {
        name: "stall_is_ended_by_the_watchdog",
        role: "after-stdin",
        window: Duration::from_millis(2900)..Duration::from_millis(4500),
    },
    Case {
        name: "a_panic_exits_0_silently",
        role: "panic-after-stdin",
        window: Duration::ZERO..Duration::from_millis(1000),
    },
];

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "hook-emit") {
        let stall = match std::env::var(ROLE).as_deref() {
            Ok("panic-after-stdin") => Stall::PanicAfterStdin,
            _ => Stall::AfterStdin,
        };
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

/// An accepting listener; returns its port and the count of accepted connections.
fn listener() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = accepted.clone();
    std::thread::spawn(move || {
        loop {
            match listener.accept() {
                Ok((mut s, _)) => {
                    count.fetch_add(1, Ordering::SeqCst);
                    let _ = s.set_nonblocking(false);
                    let _ = s.set_read_timeout(Some(PARENT_BOUND));
                    let mut buf = [0u8; 4096];
                    let _ = s.read(&mut buf);
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    });
    (port, accepted)
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

fn run_case(c: &Case) {
    let (port, accepted) = listener();
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["hook-emit", "codex"])
        .env("CHALDEA_HOOK_URL", format!("http://127.0.0.1:{port}/hook"))
        .env("CHALDEA_HOOK_TOKEN", format!("chd_{}", "a".repeat(43)))
        .env("CHALDEA_THREAD_ID", "0198c0de-0000-7000-8000-000000000001")
        .env(ROLE, c.role)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = spawn_locked(&mut cmd).unwrap();
    let started = Instant::now();
    let out = collect(child.stdout.take());
    let err = collect(child.stderr.take());
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(PAYLOAD).unwrap();
    drop(stdin);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if started.elapsed() >= PARENT_BOUND {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{}: the child did not exit within {PARENT_BOUND:?}", c.name);
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let elapsed = started.elapsed();
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
        c.window.contains(&elapsed),
        "{name}: exited after {elapsed:?}, not within {:?}",
        c.window
    );
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        0,
        "{name}: the child posted"
    );
}
