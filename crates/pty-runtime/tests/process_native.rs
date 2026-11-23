//! `process::lookup` and `process::ancestry` against real processes
//! (`docs/design/hook-ingest.md` §7.3, PR9-CL-26): a grandchild's walk passes through its
//! parent to this test process, with identities equal to the lookups and to the identity
//! functions of the backend.
//!
//! The child and the grandchild are this test binary again, running the `helper_role` test
//! with a role in the environment; every wait is bounded, and each helper exits on its own
//! after [`HELPER_LIFE`].

#![cfg(any(target_os = "macos", windows))]

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use chaldea_pty_runtime::host;
use chaldea_pty_runtime::process::{self, Lookup, ProcessEntry};
use chaldea_pty_runtime::with_spawn_lock;

const ROLE: &str = "CHALDEA_PROCESS_NATIVE_ROLE";
const MARK: &str = "@@ grandchild ";
const WAIT: Duration = Duration::from_secs(30);
const HELPER_LIFE: Duration = Duration::from_secs(60);

fn helper(role: &str) -> Command {
    let mut c = Command::new(std::env::current_exe().unwrap());
    c.args(["helper_role", "--exact", "--nocapture", "--test-threads=1"])
        .env(ROLE, role)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    c
}

fn spawn(cmd: &mut Command) -> Child {
    with_spawn_lock(|| cmd.spawn()).unwrap()
}

/// Exits the helper after [`HELPER_LIFE`] whatever it waits on.
fn life_bound() {
    std::thread::spawn(|| {
        std::thread::sleep(HELPER_LIFE);
        std::process::exit(3);
    });
}

/// Waits for stdin's EOF (the test, or the child, closing it).
fn until_stdin_closes() {
    let mut sink = Vec::new();
    let _ = std::io::stdin().lock().read_to_end(&mut sink);
}

/// The helper roles: `child` starts a grandchild, reports its pid, and kills it when stdin
/// closes; `grandchild` waits for stdin to close. Without a role, nothing.
#[test]
fn helper_role() {
    match std::env::var(ROLE).as_deref() {
        Ok("child") => {
            life_bound();
            let mut g = spawn(&mut helper("grandchild"));
            let mut out = std::io::stdout().lock();
            writeln!(out, "{MARK}{}", g.id()).unwrap();
            out.flush().unwrap();
            drop(out);
            until_stdin_closes();
            let _ = g.kill();
            let _ = g.wait();
        }
        Ok("grandchild") => {
            life_bound();
            until_stdin_closes();
        }
        _ => {}
    }
}

/// Kills and reaps a child on drop.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        drop(self.0.stdin.take());
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn live(pid: u32) -> ProcessEntry {
    match process::lookup(pid) {
        Lookup::Live(e) => e,
        other => panic!("{pid}: {other:?}"),
    }
}

/// The `(pid, start_time)` part of an identity, the part comparisons use.
fn key(e: &ProcessEntry) -> (u32, String) {
    (e.identity.pid, e.identity.start_time.clone())
}

#[test]
fn ancestry_of_a_grandchild_reaches_this_process() {
    let mut child = Reaped(spawn(&mut helper("child")));
    let stdout = child.0.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(i) = line.find(MARK) {
                let _ = tx.send(line[i + MARK.len()..].trim().parse::<u32>().unwrap());
                return;
            }
        }
    });
    let grandchild = rx.recv_timeout(WAIT).expect("no grandchild pid in time");
    let me = std::process::id();

    let a = process::ancestry(grandchild, 16);
    let pids: Vec<u32> = a.links.iter().map(|l| l.identity.pid).collect();
    assert!(
        pids.len() >= 3 && pids[..3] == [grandchild, child.0.id(), me],
        "{pids:?} ended {:?}",
        a.end
    );
    for link in &a.links[..3] {
        let pid = link.identity.pid;
        assert_eq!(key(link), key(&live(pid)), "{pid}");
        let id = host::process_identity(pid).unwrap().unwrap();
        assert_eq!(
            (id.pid, id.start_time),
            key(link),
            "{pid}: the backend's identity"
        );
    }
    assert_eq!(a.links[1].ppid, me);
    // All three run this binary; the name is its lowercased stem (macOS may truncate it).
    let stem = std::env::current_exe()
        .unwrap()
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .to_ascii_lowercase();
    for link in &a.links[..3] {
        assert!(
            !link.name.is_empty() && stem.starts_with(&link.name),
            "{} vs {stem}",
            link.name
        );
    }
    drop(child);
}

/// An exited process is gone before it is reaped (a zombie on macOS; a signalled handle, or
/// no snapshot entry, on Windows), while its pid cannot be reused yet.
#[test]
fn an_exited_process_is_gone_before_its_reap() {
    let mut c = Reaped(spawn(&mut helper("grandchild")));
    let pid = c.0.id();
    let alive = live(pid);
    assert_eq!(alive.ppid, std::process::id());
    drop(c.0.stdin.take());
    let end = Instant::now() + WAIT;
    while process::lookup(pid) != Lookup::Gone {
        assert!(Instant::now() < end, "{pid} still not gone");
        std::thread::sleep(Duration::from_millis(10));
    }
    let a = process::ancestry(pid, 16);
    assert!(a.links.is_empty(), "{a:?}");
    assert_eq!(a.end, process::AncestryEnd::Gone(pid));
    drop(c);
}

/// FakeCli, built by `cargo test --workspace` (testkit's binary) next to the test binaries'
/// `deps/` directory.
fn fake_cli() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap();
    let p = dir.join(format!("chaldea-fake-cli{}", std::env::consts::EXE_SUFFIX));
    assert!(
        p.is_file(),
        "{} is missing: build with cargo test --workspace",
        p.display()
    );
    p
}

#[test]
fn name_is_normalized() {
    let mut cmd = Command::new(fake_cli());
    cmd.args(["--mode", "sleeper", "--max-life-secs", "60"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let c = Reaped(spawn(&mut cmd));
    let e = live(c.0.id());
    assert_eq!(e.name, "chaldea-fake-cli");
    assert_eq!(e.ppid, std::process::id());
    drop(c);
}
