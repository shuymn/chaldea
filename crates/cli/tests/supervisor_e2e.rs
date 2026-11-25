//! The supervisor end to end (`docs/design/supervisor.md` §6): a foreground `daemon run` of
//! this binary as `chaldea` with test seams, whose agent program is FakeCli, driven through
//! the agent commands of the real `chaldea` binary.

mod support;

use std::ffi::OsStr;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};

use serde_json::Value;
use support::faults::{AGENT_PROGRAM, Labels, cmd};
use support::runner::{Case, main as run_cases};
use support::{Proc, WAIT, request_complete, run_to_exit, state};

fn main() {
    run_cases(&[
        Case {
            name: "wait_until_exited",
            run: wait_until_exited,
        },
        Case {
            name: "banner_changes_nothing",
            run: banner_changes_nothing,
        },
    ]);
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

/// A foreground daemon whose agent program is FakeCli with `args` (then `--`, so the
/// launch's own arguments are ignored).
fn daemon(labels: &Labels, dir: &Path, args: &[&str]) -> (Proc, u16) {
    let mut argv = vec![fake_cli().to_string_lossy().into_owned()];
    argv.extend(args.iter().map(|a| (*a).to_owned()));
    argv.push("--".to_owned());
    let json = serde_json::to_string(&argv).unwrap();
    let p = Proc::spawn(&mut cmd(
        labels,
        dir,
        &["daemon", "run"],
        &[(AGENT_PROGRAM, OsStr::new(&json))],
    ));
    let port = p.serving();
    (p, port)
}

/// `chaldea --state-dir <dir> <args>`: its exit code and stdout as JSON (or `Null`).
fn agent(labels: &Labels, dir: &Path, args: &[&str]) -> (i32, Value, String) {
    let (code, out, err) = run_to_exit(&mut cmd(labels, dir, args, &[]));
    (
        code,
        serde_json::from_str(out.trim()).unwrap_or(Value::Null),
        err,
    )
}

fn spawn_codex(labels: &Labels, dir: &Path, cwd: &Path) -> String {
    let (code, v, err) = agent(
        labels,
        dir,
        &["spawn", "--agent", "codex", "--cwd", &cwd.to_string_lossy()],
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(v["outcome"], "started", "{v}");
    v["thread_id"].as_str().unwrap().to_owned()
}

/// PR10a1-CL-26: `wait` returns the summary once the launch exited, and at once when it
/// already has.
fn wait_until_exited() {
    let labels = Labels::new();
    let d = state();
    let cwd = tempfile::tempdir().unwrap();
    let (mut daemon, _) = daemon(
        &labels,
        &d.path,
        &["--mode", "banner", "--max-life-secs", "120"],
    );
    let t = spawn_codex(&labels, &d.path, cwd.path());
    let mut waiting = Proc::spawn(&mut cmd(
        &labels,
        &d.path,
        &["wait", &t, "--until", "exited", "--timeout", "60s"],
        &[],
    ));
    let (code, v, err) = agent(&labels, &d.path, &["stop", &t]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(v["death_confirmed"], true, "{v}");
    assert_eq!(waiting.code(), 0, "{}", waiting.stderr());
    let summary: Value = serde_json::from_str(waiting.stdout().trim()).unwrap();
    assert_eq!(summary["process_state"], "exited");
    // Already exited: at once.
    let (code, v, err) = agent(&labels, &d.path, &["wait", &t, "--timeout", "5s"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(v["process_state"], "exited");
    // `idle` on an exited launch: exit 1.
    let (code, _, err) = agent(
        &labels,
        &d.path,
        &["wait", &t, "--until", "idle", "--timeout", "5s"],
    );
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("the launch exited"), "{err}");
    assert_eq!(daemon.stop_and_wait(&d.path), 0, "{}", daemon.stderr());
}

/// The thread's events with what differs between runs removed: ids, stamps, positions, and
/// process identities.
fn shape(port: u16, token: &str, thread: &str) -> Vec<Value> {
    let (status, body) = request_complete(
        port,
        "GET",
        &format!("/api/events?stream_id=thread:{thread}&limit=1000"),
        token,
        None,
    );
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    v["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let mut p = e["payload"].clone();
            if let Some(o) = p.as_object_mut() {
                for k in [
                    "launch_id",
                    "input_id",
                    "argv_digest",
                    "pid",
                    "start_time",
                    "pgid_or_job",
                    "sent_after_seq",
                    "expected_previous",
                    "thread_id",
                    "worktree_id",
                ] {
                    o.remove(k);
                }
            }
            serde_json::json!({ "kind": e["kind"], "payload": p })
        })
        .collect()
}

/// PR10a1-CL-18: what the CLI prints changes no state: a launch that printed state-shaped
/// text and a silent one, each stopped after its output was written (the stop's close
/// finishes the drain before the exit is recorded), record the same events.
fn banner_changes_nothing() {
    let labels = Labels::new();
    let mut runs = Vec::new();
    for role in ["banner", "silent"] {
        let d = state();
        let cwd = tempfile::tempdir().unwrap();
        let report = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("127.0.0.1:{}", report.local_addr().unwrap().port());
        let (mut daemon, port) = daemon(
            &labels,
            &d.path,
            &[
                "--mode",
                "banner",
                "--role",
                role,
                "--report",
                &addr,
                "--max-life-secs",
                "120",
            ],
        );
        let t = spawn_codex(&labels, &d.path, cwd.path());
        report.set_nonblocking(false).unwrap();
        let (conn, _) = report.accept().unwrap();
        conn.set_read_timeout(Some(WAIT)).unwrap();
        let mut lines = BufReader::new(conn).lines();
        let written = lines.any(|l| l.is_ok_and(|l| l.contains("banner_written")));
        assert!(written, "{role}: FakeCli wrote its output");
        let (code, _, err) = agent(&labels, &d.path, &["stop", &t]);
        assert_eq!(code, 0, "{err}");
        let token = chaldea_daemon::lifecycle::read_owner_token(&d.path).unwrap();
        runs.push(shape(port, &token, &t));
        assert_eq!(daemon.stop_and_wait(&d.path), 0, "{}", daemon.stderr());
    }
    assert_eq!(runs[0], runs[1]);
}
