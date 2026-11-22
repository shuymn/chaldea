//! The native macOS backend with FakeCli (`docs/design/pty-unix.md` §7). Every test holds
//! `LOCK` and a `Cleanup` that kills its fixtures by verified identity on any exit.

#![cfg(target_os = "macos")]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use chaldea_pty_runtime::fake::{identify, unix_writer_polls};
use chaldea_pty_runtime::{
    CloseBounds, Guard, OutputEnd, ProcessIdentity, PtySize, Session, SessionConfig, SpawnSpec,
    WriteOrigin, WriteOutcome, WriteUnit, completion_channel, native,
};
use chaldea_testkit::report::{Cleanup, Fixture, ReportListener};

const BIN: &str = env!("CARGO_BIN_EXE_chaldea-fake-cli");
const WAIT: Duration = Duration::from_secs(10);
const KILL: Duration = Duration::from_secs(5);

static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

fn soon() -> Instant {
    Instant::now() + WAIT
}

fn start_of(pid: u32) -> Option<String> {
    identify(pid)
        .ok()
        .flatten()
        .filter(|i| !i.group.is_empty())
        .map(|i| i.start_time)
}

/// A session running FakeCli with `args`, its report listener, its root fixture, and a
/// cleanup that tracks every fixture.
struct Rig {
    session: Option<Session>,
    listener: ReportListener,
    root: Fixture,
    cleanup: Cleanup,
}

fn temp_cwd() -> PathBuf {
    std::env::temp_dir().canonicalize().unwrap()
}

fn rig_sized(args: &[&str], size: PtySize) -> Rig {
    let listener = ReportListener::bind().unwrap();
    let report = listener.arg();
    let mut argv = vec![BIN, "--report", &report, "--max-life-secs", "60"];
    argv.extend_from_slice(args);
    let spec = SpawnSpec {
        argv: argv.iter().map(Into::into).collect(),
        cwd: temp_cwd(),
        env: Vec::new(),
        size,
    };
    let session = Session::spawn(&*native(), &spec, SessionConfig::new(|_| {})).unwrap();
    let mut cleanup = Cleanup::new(Some(start_of));
    let root = listener.accept(soon()).unwrap();
    cleanup.track(root.hello.pid);
    Rig {
        session: Some(session),
        listener,
        root,
        cleanup,
    }
}

fn rig(args: &[&str]) -> Rig {
    rig_sized(args, PtySize::new(80, 24).unwrap())
}

impl Rig {
    fn session(&self) -> &Session {
        self.session.as_ref().unwrap()
    }

    fn roles(&mut self, roles: &[&str]) -> BTreeMap<String, Fixture> {
        let got = self.listener.expect_roles(roles, soon()).unwrap();
        for f in got.values() {
            self.cleanup.track(f.hello.pid);
        }
        got
    }
}

/// A FakeCli `unrelated` sleeper the test starts itself, outside the CLI's session, on its
/// own listener.
fn unrelated(cleanup: &mut Cleanup) -> Fixture {
    let l = ReportListener::bind().unwrap();
    let child = Command::new(BIN)
        .args([
            "--mode",
            "sleeper",
            "--role",
            "unrelated",
            "--report",
            &l.arg(),
        ])
        .args(["--max-life-secs", "60"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    cleanup.own(child);
    let f = l.accept(soon()).unwrap();
    assert_eq!(f.hello.role, "unrelated");
    f
}

fn next_kind(f: &mut Fixture, kind: &str) -> serde_json::Value {
    loop {
        let v = f.next_event(soon()).unwrap().expect("stream closed");
        if v["event"] == kind {
            return v;
        }
    }
}

// ---- CL-4 ----

#[test]
fn size_reaches_the_cli() {
    let _l = lock();
    let mut r = rig_sized(&["--mode", "attachable"], PtySize::new(100, 30).unwrap());
    let size = next_kind(&mut r.root, "size");
    assert_eq!(
        (
            &size["cols"],
            &size["rows"],
            &size["xpixel"],
            &size["ypixel"]
        ),
        (&100.into(), &30.into(), &800.into(), &480.into()),
        "{size}"
    );
    let _resized = r.session().resize(PtySize::new(120, 40).unwrap()).unwrap();
    let winch = next_kind(&mut r.root, "winch");
    assert_eq!(
        (
            &winch["cols"],
            &winch["rows"],
            &winch["xpixel"],
            &winch["ypixel"]
        ),
        (&120.into(), &40.into(), &960.into(), &640.into()),
        "{winch}"
    );
}

// ---- CL-6 ----

#[test]
fn child_leads_its_session_on_the_pty() {
    let _l = lock();
    let r = rig(&["--mode", "sleeper"]);
    let h = &r.root.hello;
    assert_eq!(h.pgid, Some(h.pid));
    assert_eq!(h.sid, Some(h.pid));
    let ttys = h.ttys.clone().unwrap();
    assert_eq!(ttys.len(), 3);
    let tty = ttys[0].clone().expect("stdin is a terminal");
    assert!(tty.starts_with("/dev/ttys"), "{tty}");
    assert!(
        ttys.iter().all(|t| t.as_deref() == Some(tty.as_str())),
        "{ttys:?}"
    );
    assert_eq!(h.ctty, Some(true));
    assert_eq!(h.fg, Some(true));
    assert_eq!(h.fds, Some(vec![0, 1, 2]));
}

// ---- CL-8 ----

/// Close bounds whose kill sees an expired deadline: the leader survives the close and
/// keeps holding the slave.
fn no_kill() -> CloseBounds {
    CloseBounds {
        kill: Duration::ZERO,
        ..CloseBounds::default()
    }
}

#[test]
fn close_with_a_surviving_tty_holder_is_bounded() {
    let _l = lock();
    let mut r = rig(&["--mode", "sleeper"]);
    let t = Instant::now();
    let out = r.session.take().unwrap().close(&[], no_kill());
    assert!(!out.kill.tree_dead, "{out:?}");
    assert!(matches!(out.output, Some(OutputEnd::Error(_))), "{out:?}");
    assert!(out.abandoned.is_empty(), "{out:?}");
    assert!(t.elapsed() < Duration::from_secs(10));
    // The leader held the slave when the wake ended the drain; the hangup of the master's
    // last close then ends it, and its cleanup covers the rest.
}

/// macOS revokes the controlling terminal for every holder when the session leader exits:
/// a slave kept by an escaped orphan does not hold the drain open.
#[test]
fn leader_exit_revokes_the_tty_for_every_holder() {
    let _l = lock();
    let mut r = rig(&[
        "--mode",
        "spawn-tree",
        "--roles",
        "none",
        "--escape-orphan",
        "--orphan-keeps-tty",
    ]);
    let mut got = r.roles(&["setsid-orphan", "orphan-child"]);
    let orphan = got.get_mut("setsid-orphan").unwrap();
    assert_eq!(
        orphan.ping(soon()).unwrap().inq,
        Some(0),
        "stdin is the terminal"
    );
    let out = r.session.take().unwrap().close(&[], CloseBounds::default());
    assert!(out.kill.tree_dead, "{out:?}");
    assert_eq!(out.output, Some(OutputEnd::Eof), "{out:?}");
    // Alive, but its stdin is no longer a terminal.
    assert_eq!(orphan.ping(soon()).unwrap().inq, None);
}

// ---- CL-10 ----

#[test]
fn large_write_reaches_the_cli_whole() {
    let _l = lock();
    let mut r = rig(&["--mode", "echo"]);
    assert!(r.root.hello.raw);
    let payload = "漢😀a".repeat(32 * 1024).into_bytes();
    assert_eq!(payload.len(), 262_144);
    let (done, rx) = completion_channel();
    r.session()
        .submit(
            WriteUnit::bytes(WriteOrigin::Key, payload.clone()),
            Guard::until(soon()),
            done,
        )
        .unwrap();
    let mut got = Vec::new();
    while got.len() < payload.len() {
        let v = next_kind(&mut r.root, "input");
        let hex = v["hex"].as_str().unwrap();
        got.extend(
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()),
        );
    }
    assert!(got == payload, "the CLI received other bytes");
    let rec = rx.recv_deadline(soon()).unwrap();
    assert_eq!(rec.outcome, WriteOutcome::Written);
    assert_eq!(rec.bytes_written, 262_144);
    assert_eq!(rec.split_writes, 0);
}

#[test]
fn close_releases_a_blocked_writer() {
    let _l = lock();
    // The leader keeps the raw slave open and never reads.
    let mut r = rig(&["--mode", "sleeper", "--raw"]);
    assert!(r.root.hello.raw);
    let before = unix_writer_polls();
    let (done, rx) = completion_channel();
    r.session()
        .submit(
            WriteUnit::bytes(WriteOrigin::Key, vec![b'x'; 64 * 1024]),
            Guard::until(soon()),
            done,
        )
        .unwrap();
    let end = soon();
    while unix_writer_polls() <= before {
        assert!(Instant::now() < end, "the writer never blocked");
        std::thread::yield_now();
    }
    let out = r.session.take().unwrap().close(&[], no_kill());
    assert!(out.released.input, "{out:?}");
    assert!(out.abandoned.is_empty(), "{out:?}");
    let rec = rx.recv_deadline(soon()).unwrap();
    // "closed": the wake ended the write, not a hangup (`EIO`).
    assert_eq!(
        rec.outcome,
        WriteOutcome::Incomplete {
            error: "closed".to_owned()
        },
        "{rec:?}"
    );
    assert!(
        rec.bytes_written > 0 && rec.bytes_written < 64 * 1024,
        "{rec:?}"
    );
}

// ---- CL-13 ----

#[test]
fn spawn_tree_dies_and_unrelated_survives() {
    let _l = lock();
    let mut r = rig(&["--mode", "spawn-tree"]);
    let mut other = unrelated(&mut r.cleanup);
    let mut got = r.roles(&[
        "plain",
        "ignore-term",
        "double-forked",
        "own-group",
        "setsid",
        "setsid-child",
    ]);
    let tree = r.root.hello.clone();
    let h = |f: &BTreeMap<String, Fixture>, role: &str| f[role].hello.clone();
    assert_eq!(h(&got, "plain").pgid, Some(tree.pid));
    assert_eq!(h(&got, "double-forked").ppid, Some(1));
    assert_eq!(h(&got, "own-group").sid, Some(tree.pid));
    assert_ne!(h(&got, "own-group").pgid, Some(tree.pid));
    assert_eq!(h(&got, "setsid").sid, Some(h(&got, "setsid").pid));
    assert_eq!(h(&got, "setsid-child").ppid, Some(h(&got, "setsid").pid));
    let k = r.session().kill(&[], Instant::now() + KILL);
    assert!(k.tree_dead, "{k:?}");
    assert_eq!(k.method, "unix_killpg_session_scan");
    assert!(r.root.closed_by(soon()));
    for (role, f) in &mut got {
        assert!(f.closed_by(soon()), "{role} survived");
    }
    assert!(
        other.ping(soon()).is_some(),
        "the unrelated process was killed"
    );
}

// ---- CL-15 ----

#[test]
fn extra_reaches_an_orphan_and_spares_a_stale_identity() {
    let _l = lock();
    let mut r = rig(&["--mode", "spawn-tree", "--roles", "none", "--escape-orphan"]);
    let mut other = unrelated(&mut r.cleanup);
    let mut got = r.roles(&["setsid-orphan", "orphan-child"]);
    let k1 = r.session().kill(&[], Instant::now() + KILL);
    assert!(k1.tree_dead, "{k1:?}");
    assert!(r.root.closed_by(soon()));
    // Outside the tree without `extra`: the containment limit.
    let orphan = got.get_mut("setsid-orphan").unwrap();
    assert!(orphan.ping(soon()).is_some());
    let orphan_id = identify(orphan.hello.pid).unwrap().unwrap();
    let mut stale: ProcessIdentity = identify(other.hello.pid).unwrap().unwrap();
    stale.start_time = (stale.start_time.parse::<u64>().unwrap() + 1).to_string();
    let k2 = r.session().kill(&[orphan_id, stale], Instant::now() + KILL);
    assert!(k2.tree_dead, "{k2:?}");
    for (role, f) in &mut got {
        assert!(f.closed_by(soon()), "{role} survived");
    }
    assert!(
        other.ping(soon()).is_some(),
        "the stale identity was signalled"
    );
}

// ---- CL-22 ----

#[test]
fn queries_are_answered_through_the_pty() {
    let _l = lock();
    let mut r = rig(&["--mode", "queries", "--reply-timeout-ms", "5000"]);
    let mut n = 0;
    loop {
        let v = r.root.next_event(soon()).unwrap().expect("stream closed");
        if v["event"] == "queries" {
            assert_eq!(v["all_ok"], true);
            break;
        }
        assert_eq!(v["ok"], true, "{v}");
        n += 1;
    }
    assert_eq!(n, 7);
}
