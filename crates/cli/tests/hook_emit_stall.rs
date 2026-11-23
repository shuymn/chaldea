//! PR9-CL-23's watchdog (hook-ingest §2.13 "Test stall", §7.4): this binary (`harness =
//! false`) re-executes itself as `hook-emit codex`, with the emitter's environment set only on
//! that child, and the child calls the library's `dispatch` with `Stall::AfterStdin`, which
//! blocks the main thread once stdin is read. Against an accepting listener the child must
//! still exit 0 between 2.9 s and 3.5 s, silently, with no request: only the watchdog that
//! `dispatch` starts can end it. The parent kills it at 10 s.
//!
//! The control is `hook_emit::accepting`: the production binary, with the same stdin and
//! environment, posts.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use chaldea_cli::Stall;
use chaldea_daemon::lifecycle::spawn_locked;

const NAME: &str = "stall_is_ended_by_the_watchdog";
const LOWER: Duration = Duration::from_millis(2900);
const UPPER: Duration = Duration::from_millis(3500);
const PARENT_BOUND: Duration = Duration::from_secs(10);
const PAYLOAD: &[u8] = br#"{"hook_event_name":"Stop","session_id":"s"}"#;

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "hook-emit") {
        chaldea_cli::dispatch(&args, Stall::AfterStdin);
    }
    // libtest's protocol, as far as `cargo test` uses it.
    if args.iter().any(|a| a == "--list") {
        println!("{NAME}: test");
        return;
    }
    let filters: Vec<String> = args
        .iter()
        .skip(1)
        .filter_map(|a| a.to_str())
        .filter(|a| !a.starts_with('-'))
        .map(str::to_owned)
        .collect();
    if !filters.is_empty() && !filters.iter().any(|f| NAME.contains(f.as_str())) {
        println!("running 0 tests");
        return;
    }
    println!("running 1 test");
    stall_is_ended_by_the_watchdog();
    println!("test {NAME} ... ok");
}

fn stall_is_ended_by_the_watchdog() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    {
        let accepted = accepted.clone();
        std::thread::spawn(move || {
            loop {
                match listener.accept() {
                    Ok((mut s, _)) => {
                        accepted.fetch_add(1, Ordering::SeqCst);
                        let _ = s.set_nonblocking(false);
                        let mut buf = [0u8; 4096];
                        let _ = s.read(&mut buf);
                        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
    }
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["hook-emit", "codex"])
        .env("CHALDEA_HOOK_URL", format!("http://127.0.0.1:{port}/hook"))
        .env("CHALDEA_HOOK_TOKEN", format!("chd_{}", "a".repeat(43)))
        .env("CHALDEA_THREAD_ID", "t")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = spawn_locked(&mut cmd).unwrap();
    let started = Instant::now();
    let collect = |r: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut r) = r {
                let _ = r.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = collect(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let err = collect(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
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
            panic!("the stalled emitter did not exit within {PARENT_BOUND:?}: no watchdog");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let elapsed = started.elapsed();
    let (out, err) = (out.join().unwrap(), err.join().unwrap());
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert!(out.is_empty(), "stdout: {out:?}");
    assert!(err.is_empty(), "stderr: {err:?}");
    assert!(
        (LOWER..UPPER).contains(&elapsed),
        "exited after {elapsed:?}, not between {LOWER:?} and {UPPER:?}"
    );
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        0,
        "a stalled emitter posted"
    );
}
