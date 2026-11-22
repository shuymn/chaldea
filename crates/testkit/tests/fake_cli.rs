//! FakeCli without a PTY (all OSes): stdin is a pipe, so reading modes stay cooked and
//! `attachable` reports a null size.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use chaldea_testkit::report::{Cleanup, ReportListener};

const WAIT: Duration = Duration::from_secs(10);

fn soon() -> Instant {
    Instant::now() + WAIT
}

fn fake(args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_chaldea-fake-cli"));
    c.args(args).stdin(Stdio::piped()).stdout(Stdio::null());
    c
}

#[cfg(target_os = "macos")]
fn identify(pid: u32) -> Option<String> {
    chaldea_pty_runtime::fake::identify(pid)
        .ok()
        .flatten()
        .filter(|i| !i.group.is_empty())
        .map(|i| i.start_time)
}

fn cleanup() -> Cleanup {
    #[cfg(target_os = "macos")]
    return Cleanup::new(Some(identify));
    #[cfg(not(target_os = "macos"))]
    Cleanup::new(None)
}

/// Waits for `child` to exit by the bound; its code.
fn exit_code(child: &mut Child) -> Option<i32> {
    let end = soon();
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return s.code();
        }
        assert!(Instant::now() < end, "fake-cli did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn every_mode_reports_hello() {
    let l = ReportListener::bind().unwrap();
    let mut cl = cleanup();
    for mode in ["echo", "queries", "attachable", "spawn-tree", "sleeper"] {
        let report = l.arg();
        let mut args = vec!["--mode", mode, "--report", &report, "--max-life-secs", "60"];
        if mode == "spawn-tree" {
            args.extend(["--roles", "none"]);
        }
        let child = fake(&args).spawn().unwrap();
        let pid = child.id();
        cl.own(child);
        let f = l.accept(soon()).unwrap();
        assert_eq!(f.hello.mode, mode);
        assert_eq!(f.hello.pid, pid);
        assert!(!f.hello.raw, "{mode}: a pipe is never raw");
    }
}

#[test]
fn spawn_tree_roles_report_directly() {
    let l = ReportListener::bind().unwrap();
    let mut cl = cleanup();
    let child = fake(&[
        "--mode",
        "spawn-tree",
        "--report",
        &l.arg(),
        "--max-life-secs",
        "60",
    ])
    .spawn()
    .unwrap();
    let tree = child.id();
    cl.own(child);
    let root = l.accept(soon()).unwrap();
    assert_eq!(root.hello.role, "spawn-tree");
    #[cfg(unix)]
    let roles = [
        "plain",
        "ignore-term",
        "double-forked",
        "own-group",
        "setsid",
        "setsid-child",
    ];
    #[cfg(not(unix))]
    let roles = ["plain", "double-forked"];
    let got = l.expect_roles(&roles, soon()).unwrap();
    for f in got.values() {
        cl.track(f.hello.pid);
        assert_ne!(f.hello.pid, tree);
    }
    #[cfg(unix)]
    {
        let h = |r: &str| got[r].hello.clone();
        let sid = root.hello.sid.unwrap();
        assert_eq!(h("plain").ppid, Some(tree));
        assert_eq!(h("plain").pgid, root.hello.pgid);
        assert_eq!(h("ignore-term").ppid, Some(tree));
        assert_eq!(h("double-forked").ppid, Some(1));
        assert_eq!(h("double-forked").sid, Some(sid));
        assert_eq!(h("own-group").pgid, Some(h("own-group").pid));
        assert_eq!(h("own-group").sid, Some(sid));
        assert_eq!(h("setsid").sid, Some(h("setsid").pid));
        assert_eq!(h("setsid").ppid, Some(tree));
        assert_eq!(h("setsid-child").ppid, Some(h("setsid").pid));
        assert_eq!(h("setsid-child").sid, Some(h("setsid").pid));
    }
}

#[test]
fn sleeper_ends_after_max_life() {
    let l = ReportListener::bind().unwrap();
    let mut child = fake(&[
        "--mode",
        "sleeper",
        "--report",
        &l.arg(),
        "--max-life-secs",
        "1",
    ])
    .spawn()
    .unwrap();
    let mut f = l.accept(soon()).unwrap();
    assert!(f.ping(soon()).is_some());
    assert!(f.closed_by(soon()));
    assert_eq!(exit_code(&mut child), Some(0));
}

#[test]
fn attachable_on_a_pipe_reports_null_size() {
    let l = ReportListener::bind().unwrap();
    let mut cl = cleanup();
    let child = fake(&[
        "--mode",
        "attachable",
        "--report",
        &l.arg(),
        "--max-life-secs",
        "60",
    ])
    .spawn()
    .unwrap();
    cl.own(child);
    let mut f = l.accept(soon()).unwrap();
    let size = f.next_event(soon()).unwrap().unwrap();
    assert_eq!(size["event"], "size");
    for k in ["cols", "rows", "xpixel", "ypixel"] {
        assert!(size[k].is_null(), "{size}");
    }
}

#[test]
fn unknown_mode_exits_2() {
    let mut c = fake(&["--mode", "nonsense"]).spawn().unwrap();
    assert_eq!(exit_code(&mut c), Some(2));
    let mut c = fake(&[]).spawn().unwrap();
    assert_eq!(exit_code(&mut c), Some(2));
}

#[test]
fn unknown_flag_exits_2() {
    let mut c = fake(&["--mode", "sleeper", "--bogus"]).spawn().unwrap();
    assert_eq!(exit_code(&mut c), Some(2));
    let mut c = fake(&["--mode", "sleeper", "--role", "bogus"])
        .spawn()
        .unwrap();
    assert_eq!(exit_code(&mut c), Some(2));
}

#[test]
fn queries_in_pipe_mode_reports_each_unanswered_query() {
    let l = ReportListener::bind().unwrap();
    let mut c = fake(&["--mode", "queries", "--report", &l.arg()])
        .spawn()
        .unwrap();
    // stdin closed at once: every wait ends at EOF.
    drop(c.stdin.take());
    let mut f = l.accept(soon()).unwrap();
    let end = soon();
    let mut kinds = Vec::new();
    loop {
        let ev = f.next_event(end).unwrap().expect("stream closed early");
        if ev["event"] == "queries" {
            assert_eq!(ev["all_ok"], false);
            break;
        }
        assert_eq!(ev["event"], "query");
        assert_eq!(ev["ok"], false);
        kinds.push(ev["kind"].as_str().unwrap().to_owned());
    }
    assert_eq!(
        kinds,
        [
            "dsr6",
            "dsr5",
            "da1",
            "da2",
            "osc10",
            "osc11",
            "kitty_keyboard"
        ]
    );
    assert_eq!(exit_code(&mut c), Some(1));
}
