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

#[cfg(windows)]
fn identify(pid: u32) -> Option<String> {
    chaldea_pty_runtime::fake::identify(pid)
        .ok()
        .flatten()
        .map(|i| i.start_time)
}

fn cleanup() -> Cleanup {
    #[cfg(target_os = "macos")]
    return Cleanup::new(Some(identify));
    #[cfg(windows)]
    return Cleanup::new(Some(identify)).kill_with(chaldea_pty_runtime::fake::kill_verified);
    #[cfg(not(any(target_os = "macos", windows)))]
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
    // The watchdog starts with the process, so the stream closes no sooner than the bound
    // after the spawn began, and within it plus `SLACK` for process start and teardown.
    const LIFE: Duration = Duration::from_secs(1);
    const SLACK: Duration = Duration::from_millis(1500);
    let l = ReportListener::bind().unwrap();
    let started = Instant::now();
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
    let lived = started.elapsed();
    assert!(lived >= LIFE && lived < LIFE + SLACK, "{lived:?}");
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

// ---- PR4-CL-24 ----

/// A fresh file path in the OS temp dir (the file itself is removed on drop).
struct TempPath(std::path::PathBuf);

impl TempPath {
    fn new(tag: &str) -> TempPath {
        TempPath(
            std::env::temp_dir().join(format!("chaldea-fakecli-{tag}-{}.json", std::process::id())),
        )
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn report_process_writes_the_file() {
    let out = TempPath::new("report");
    let path = out.0.to_str().unwrap();
    let mut c = fake(&["--mode", "report-process", "--out", path, "--", "a b", ""])
        .env_clear()
        .env("K", "v")
        .spawn()
        .unwrap();
    assert_eq!(exit_code(&mut c), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&out.0).unwrap()).unwrap();
    let argv: Vec<&str> = v["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(
        &argv[1..],
        ["--mode", "report-process", "--out", path, "--", "a b", ""]
    );
    assert_eq!(v["env"], serde_json::json!([["K", "v"]]));
    assert!(v["cwd"].is_string());
    assert_eq!(v["stdin_console"], false, "stdin is a pipe");
    assert!(!std::path::Path::new(&format!("{path}.tmp")).exists());
}

#[test]
fn emit_output_reports_lines() {
    let l = ReportListener::bind().unwrap();
    let mut cl = cleanup();
    let report = l.arg();
    let mut c = fake(&[
        "--mode",
        "spawn-tree",
        "--roles",
        "none",
        "--emit-output",
        "--report",
        &report,
        "--max-life-secs",
        "60",
    ]);
    c.stdout(Stdio::piped());
    let mut child = c.spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    cl.own(child);
    let mut f = l.accept(soon()).unwrap();
    let mut lines = std::io::BufRead::lines(std::io::BufReader::new(stdout));
    for n in 0..3u64 {
        let ev = f.next_event(soon()).unwrap().unwrap();
        assert_eq!(
            (&ev["event"], &ev["n"]),
            (&"output".into(), &n.into()),
            "{ev}"
        );
        // The event acknowledges a completed write: the line is already in the pipe.
        assert_eq!(lines.next().unwrap().unwrap(), format!("OUTPUT-{n}"));
    }
}

#[test]
fn validate_input_checks_the_pattern() {
    for (payload, ok) in [
        (
            (0..1000u32)
                .map(|i| b'a' + u8::try_from(i % 26).unwrap())
                .collect::<Vec<u8>>(),
            true,
        ),
        (vec![b'a'; 1000], false),
    ] {
        let l = ReportListener::bind().unwrap();
        let mut c = fake(&[
            "--mode",
            "echo",
            "--validate-input",
            "1000",
            "--report",
            &l.arg(),
            "--max-life-secs",
            "60",
        ])
        .spawn()
        .unwrap();
        let mut f = l.accept(soon()).unwrap();
        std::io::Write::write_all(c.stdin.as_mut().unwrap(), &payload).unwrap();
        let ev = f.next_event(soon()).unwrap().unwrap();
        assert_eq!(ev["event"], "validated", "{ev}");
        assert_eq!(
            (&ev["bytes"], &ev["ok"]),
            (&1000.into(), &ok.into()),
            "{ev}"
        );
        assert_eq!(exit_code(&mut c), Some(0));
    }
    // Too few bytes: EOF first.
    let l = ReportListener::bind().unwrap();
    let mut c = fake(&[
        "--mode",
        "echo",
        "--validate-input",
        "10",
        "--report",
        &l.arg(),
    ])
    .spawn()
    .unwrap();
    let mut f = l.accept(soon()).unwrap();
    let mut stdin = c.stdin.take().unwrap();
    std::io::Write::write_all(&mut stdin, b"abc").unwrap();
    drop(stdin);
    let ev = f.next_event(soon()).unwrap().unwrap();
    assert_eq!(
        (&ev["bytes"], &ev["ok"]),
        (&3.into(), &false.into()),
        "{ev}"
    );
    assert_eq!(exit_code(&mut c), Some(0));
}

/// The test starts `double-fork-parent` and holds its `Child`: the parent exits, then the
/// grandchild reports and answers.
#[test]
fn double_forked_outlives_its_parent() {
    let l = ReportListener::bind().unwrap();
    let mut cl = cleanup();
    let mut parent = fake(&[
        "--mode",
        "sleeper",
        "--role",
        "double-fork-parent",
        "--report",
        &l.arg(),
        "--max-life-secs",
        "60",
    ])
    .spawn()
    .unwrap();
    assert_eq!(exit_code(&mut parent), Some(0), "the parent exited");
    let mut g = l.accept(soon()).unwrap();
    cl.track(g.hello.pid);
    assert_eq!(g.hello.role, "double-forked");
    assert_ne!(g.hello.pid, parent.id());
    assert!(g.ping(soon()).is_some(), "the grandchild is alive");
}

#[cfg(windows)]
#[test]
fn breakaway_role_reports_its_result() {
    let l = ReportListener::bind().unwrap();
    let mut cl = cleanup();
    let child = fake(&[
        "--mode",
        "spawn-tree",
        "--roles",
        "breakaway",
        "--report",
        &l.arg(),
        "--max-life-secs",
        "60",
    ])
    .spawn()
    .unwrap();
    cl.own(child);
    let mut root = l.accept(soon()).unwrap();
    let ev = root.next_event(soon()).unwrap().unwrap();
    assert_eq!(ev["event"], "breakaway", "{ev}");
    let result = ev["result"].as_str().unwrap();
    assert!(result == "accepted" || result == "denied", "{ev}");
    let role = l.accept(soon()).unwrap();
    cl.track(role.hello.pid);
    assert_eq!(role.hello.role, "breakaway");
}
