//! The supervisor end to end (`docs/design/supervisor.md` §6): a foreground `daemon run` of
//! this binary as `chaldea` with test seams, whose agent program is FakeCli, driven through
//! the agent commands of the real `chaldea` binary.

mod support;

use std::ffi::OsStr;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};

use serde_json::Value;
use support::faults::{
    AGENT_PROGRAM, FAKE_PTY_HOLD_WAITER, Gate, Labels, SUPERVISOR_GATE, SUPERVISOR_PAUSE, cmd,
};
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
        Case {
            name: "stop_kills_tree",
            run: stop_kills_tree,
        },
        Case {
            name: "retry_after_crash_is_unknown",
            run: retry_after_crash_is_unknown,
        },
        Case {
            name: "crash_after_child_created",
            run: crash_after_child_created,
        },
        Case {
            name: "left_running_exit",
            run: left_running_exit,
        },
        Case {
            name: "second_crash_before_probe",
            run: second_crash_before_probe,
        },
        #[cfg(target_os = "macos")]
        Case {
            name: "unrecorded_member_after_crash",
            run: unrecorded_member_after_crash,
        },
        #[cfg(windows)]
        Case {
            name: "a29_child_suspended",
            run: a29_child_suspended,
        },
        #[cfg(windows)]
        Case {
            name: "assign_failure_child_probed",
            run: assign_failure_child_probed,
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
    daemon_with(labels, dir, args, &[])
}

/// [`daemon`] with more environment for its seams.
fn daemon_with(labels: &Labels, dir: &Path, args: &[&str], env: &[(&str, &OsStr)]) -> (Proc, u16) {
    let mut argv = vec![fake_cli().to_string_lossy().into_owned()];
    argv.extend(args.iter().map(|a| (*a).to_owned()));
    argv.push("--".to_owned());
    let json = serde_json::to_string(&argv).unwrap();
    let mut all = vec![(AGENT_PROGRAM, OsStr::new(&json))];
    all.extend_from_slice(env);
    // `cmd` gives the first start the directory's reserved port; a restart takes the saved one.
    let p = Proc::spawn(&mut cmd(labels, dir, &["daemon", "run"], &all));
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

/// Every `hello` FakeCli processes send to `report`, as `(role, pid)`, on a channel.
fn hellos(report: TcpListener) -> std::sync::mpsc::Receiver<(String, u32)> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for conn in report.incoming() {
            let Ok(conn) = conn else { return };
            let tx = tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(conn).lines() {
                    let Ok(line) = line else { return };
                    let Ok(v) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if v["event"] == "hello"
                        && let (Some(role), Some(pid)) = (v["role"].as_str(), v["pid"].as_u64())
                    {
                        let _ = tx.send((role.to_owned(), u32::try_from(pid).unwrap()));
                    }
                }
            });
        }
    });
    rx
}

fn gone(pid: u32) -> bool {
    matches!(
        chaldea_pty_runtime::process::lookup(pid),
        chaldea_pty_runtime::process::Lookup::Gone
    )
}

/// PR10a1-CL-11 `stop_kills_tree`, P-2: a launch's leader with a sleeper child, and an
/// unrelated sleeper: within `RequestStart` + 5 s every pid of the tree is gone and the
/// unrelated one lives; the reply has `confirmed_within_deadline: true`; on macOS the
/// leader's exit shows a signal, on Windows an exit code and no signal.
fn stop_kills_tree() {
    let labels = Labels::new();
    let d = state();
    let cwd = tempfile::tempdir().unwrap();
    let report = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = format!("127.0.0.1:{}", report.local_addr().unwrap().port());
    let seen = hellos(report);
    let mut unrelated = Proc::spawn(
        std::process::Command::new(fake_cli())
            .args([
                "--mode",
                "sleeper",
                "--role",
                "unrelated",
                "--report",
                &addr,
                "--max-life-secs",
                "120",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped()),
    );
    let (mut daemon, _) = daemon(
        &labels,
        &d.path,
        &[
            "--mode",
            "spawn-tree",
            "--roles",
            "plain",
            "--report",
            &addr,
            "--max-life-secs",
            "120",
        ],
    );
    let t = spawn_codex(&labels, &d.path, cwd.path());
    let mut pids = std::collections::HashMap::new();
    while !["unrelated", "spawn-tree", "plain"]
        .iter()
        .all(|r| pids.contains_key(*r))
    {
        let (role, pid) = seen.recv_timeout(WAIT).expect("every hello");
        pids.insert(role, pid);
    }
    let started = std::time::Instant::now();
    let (code, v, err) = agent(&labels, &d.path, &["stop", &t]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(v["confirmed_within_deadline"], true, "{v}");
    for role in ["spawn-tree", "plain"] {
        assert!(
            support::wait_for(|| gone(pids[role])),
            "{role} {} is gone",
            pids[role]
        );
    }
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert!(!gone(pids["unrelated"]), "the unrelated sleeper lives");
    if cfg!(windows) {
        assert!(v["exit_code"].is_number() && v["signal"].is_null(), "{v}");
    } else {
        assert!(v["signal"].is_number(), "{v}");
    }
    assert_eq!(daemon.stop_and_wait(&d.path), 0, "{}", daemon.stderr());
    let _ = unrelated.child.kill();
}

/// The owner's `POST /api/commands/spawn` with `command_id`, on its own thread (it may
/// never be answered).
fn spawn_request(port: u16, token: String, body: String) -> std::thread::JoinHandle<(u16, String)> {
    std::thread::spawn(move || {
        request_complete(port, "POST", "/api/commands/spawn", &token, Some(&body))
    })
}

/// The one thread's events, as kinds and payloads.
fn thread_events(port: u16, token: &str) -> (String, Vec<(String, Value)>) {
    let (status, body) = request_complete(port, "GET", "/api/threads", token, None);
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    let threads = v["threads"].as_array().unwrap();
    assert_eq!(threads.len(), 1, "{v}");
    let t = threads[0]["thread_id"].as_str().unwrap().to_owned();
    let (status, body) = request_complete(
        port,
        "GET",
        &format!("/api/events?stream_id=thread:{t}&limit=1000"),
        token,
        None,
    );
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    let events = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["kind"].as_str().unwrap().to_owned(), e["payload"].clone()))
        .collect();
    (t, events)
}

/// A daemon held at `pause` while a spawn runs, then, after `before_kill`, killed by its
/// PID.
fn crash_at(
    labels: &Labels,
    dir: &Path,
    args: &[&str],
    pause: &str,
    body: &str,
    before_kill: impl FnOnce(),
) {
    let gate = Gate::new();
    let (mut d, port) = daemon_with(
        labels,
        dir,
        args,
        &[
            (SUPERVISOR_PAUSE, OsStr::new(pause)),
            (SUPERVISOR_GATE, gate.path().as_os_str()),
        ],
    );
    let token = chaldea_daemon::lifecycle::read_owner_token(dir).unwrap();
    // The daemon dies before it answers: the reply is not read.
    let request = format!(
        "POST /api/commands/spawn HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut conn = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    std::io::Write::write_all(&mut conn, request.as_bytes()).unwrap();
    assert!(gate.reached(WAIT), "the supervisor reached {pause}");
    before_kill();
    d.child.kill().unwrap();
    let _ = d.wait();
    gate.waiter_killed();
    drop(conn);
}

/// PR10a1-CL-8 `retry_after_crash_is_unknown`: a crash after the reservation committed;
/// after the restart the same `command_id` is 409 `unknown_after_crash`, with one
/// `launch_reserved` and the launch `unknown_after_crash`.
fn retry_after_crash_is_unknown() {
    let labels = Labels::new();
    let d = state();
    let cwd = tempfile::tempdir().unwrap();
    let body = serde_json::json!({
        "command_id": uuid_v4(),
        "args": { "agent": "codex", "cwd": cwd.path().to_string_lossy() }
    })
    .to_string();
    let args = ["--mode", "sleeper", "--max-life-secs", "120"];
    crash_at(&labels, &d.path, &args, "after_reserve", &body, || {});
    let (mut daemon, port) = daemon(&labels, &d.path, &args);
    let token = chaldea_daemon::lifecycle::read_owner_token(&d.path).unwrap();
    let (status, reply) = spawn_request(port, token.clone(), body).join().unwrap();
    assert_eq!(status, 409, "{reply}");
    assert!(reply.contains("unknown_after_crash"), "{reply}");
    let (_, events) = thread_events(port, &token);
    let reserved = events
        .iter()
        .filter(|(k, _)| k == "launch_reserved")
        .count();
    assert_eq!(reserved, 1, "{events:?}");
    let exited = &events.iter().find(|(k, _)| k == "launch_exited").unwrap().1;
    assert_eq!(exited["reason"], "unknown_after_crash");
    assert_eq!(daemon.stop_and_wait(&d.path), 0, "{}", daemon.stderr());
}

/// PR10a1-CL-8 `crash_after_child_created`: a crash after `Session::spawn` returned, with
/// the real child created (its `hello` seen): after the restart the launch is
/// `unknown_after_crash`, death unconfirmed.
fn crash_after_child_created() {
    let labels = Labels::new();
    let d = state();
    let cwd = tempfile::tempdir().unwrap();
    let report = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = format!("127.0.0.1:{}", report.local_addr().unwrap().port());
    let seen = hellos(report);
    let body = serde_json::json!({
        "command_id": uuid_v4(),
        "args": { "agent": "codex", "cwd": cwd.path().to_string_lossy() }
    })
    .to_string();
    let args = [
        "--mode",
        "sleeper",
        "--report",
        &addr,
        "--max-life-secs",
        "120",
    ];
    let mut child = 0;
    crash_at(&labels, &d.path, &args, "after_spawn", &body, || {
        child = seen.recv_timeout(WAIT).expect("the child was created").1;
    });
    let (mut daemon, port) = daemon(&labels, &d.path, &args);
    let token = chaldea_daemon::lifecycle::read_owner_token(&d.path).unwrap();
    let (_, events) = thread_events(port, &token);
    assert!(
        !events.iter().any(|(k, _)| k == "launch_started"),
        "{events:?}"
    );
    let exited = &events.iter().find(|(k, _)| k == "launch_exited").unwrap().1;
    assert_eq!(exited["reason"], "unknown_after_crash");
    assert_eq!(exited["death_confirmed"], false);
    assert_eq!(daemon.stop_and_wait(&d.path), 0, "{}", daemon.stderr());
    // The crashed daemon's child: ended by the test that caused it.
    #[cfg(unix)]
    if !gone(child) {
        support::term(child, rustix::process::Signal::KILL);
    }
    assert!(support::wait_for(|| gone(child)));
}

/// PR10a1-CL-36 `left_running_exit`: a daemon whose launch's close abandons `pty-wait`
/// keeps that token counted: `chaldea daemon stop` prints `stopped` and exits 0, while the
/// daemon exits 1 with `lock_held_at_exit`.
fn left_running_exit() {
    let labels = Labels::new();
    let d = state();
    let cwd = tempfile::tempdir().unwrap();
    let (mut daemon, _) = daemon_with(
        &labels,
        &d.path,
        &["--mode", "sleeper"],
        &[(FAKE_PTY_HOLD_WAITER, OsStr::new("1"))],
    );
    let _t = spawn_codex(&labels, &d.path, cwd.path());
    let (code, out, err) = run_to_exit(&mut cmd(&labels, &d.path, &["daemon", "stop"], &[]));
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("stopped"), "{out}");
    assert_eq!(daemon.code(), 1, "{}", daemon.stderr());
    assert!(
        daemon.stderr().contains("lock_held_at_exit"),
        "{}",
        daemon.stderr()
    );
}

fn uuid_v4() -> String {
    chaldea_core::ids::CommandId(uuid::Uuid::new_v4()).to_string()
}

/// The thread stream `t`'s envelopes.
fn envelopes(port: u16, token: &str, t: &str) -> Vec<Value> {
    let (status, body) = request_complete(
        port,
        "GET",
        &format!("/api/events?stream_id=thread:{t}&limit=1000"),
        token,
        None,
    );
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    v["events"].as_array().unwrap().clone()
}

fn of_kind<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["kind"] == kind).collect()
}

/// A daemon held at `prober_start` on `gate`.
fn held_at_probe(labels: &Labels, dir: &Path, args: &[&str], gate: &Gate) -> (Proc, u16) {
    let (d, port) = daemon_with(
        labels,
        dir,
        args,
        &[
            (SUPERVISOR_PAUSE, OsStr::new("prober_start")),
            (SUPERVISOR_GATE, gate.path().as_os_str()),
        ],
    );
    assert!(gate.reached(WAIT), "the prober reached prober_start");
    (d, port)
}

/// PR10a2-CL-4 `second_crash_before_probe`: daemon 1 is killed with a live leader and a
/// positional input `sent`; daemon 2 is killed at `prober_start` after its reconcilers
/// committed; daemon 3's probe, once released, records the leader `gone` with the host's
/// unchecked scope. No command is sent, death stays unconfirmed, daemon 1's events stay a
/// prefix of the stream, and the one input change is the restart's `sent -> unconfirmed`.
fn second_crash_before_probe() {
    let labels = Labels::new();
    let d = state();
    let cwd = tempfile::tempdir().unwrap();
    let report = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = format!("127.0.0.1:{}", report.local_addr().unwrap().port());
    let seen = hellos(report);
    // macOS: a leader that survives the hangup of its terminal, so only the probe ends it.
    #[cfg(unix)]
    let (role, args) = (
        "ignore-term",
        [
            "--mode",
            "sleeper",
            "--role",
            "ignore-term",
            "--report",
            &addr,
            "--max-life-secs",
            "120",
        ],
    );
    #[cfg(windows)]
    let (role, args) = (
        "plain",
        [
            "--mode",
            "sleeper",
            "--role",
            "plain",
            "--report",
            &addr,
            "--max-life-secs",
            "120",
        ],
    );
    let (mut d1, port) = daemon(&labels, &d.path, &args);
    let token = chaldea_daemon::lifecycle::read_owner_token(&d.path).unwrap();
    let (code, v, err) = agent(
        &labels,
        &d.path,
        &[
            "spawn",
            "--agent",
            "codex",
            "--cwd",
            &cwd.path().to_string_lossy(),
            "--prompt",
            "hello",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let t = v["thread_id"].as_str().unwrap().to_owned();
    let before = envelopes(port, &token, &t);
    let started = &of_kind(&before, "launch_started")[0]["payload"];
    let leader = u32::try_from(started["pid"].as_u64().unwrap()).unwrap();
    // Its hello: the leader's handlers are in place.
    assert_eq!(
        seen.recv_timeout(WAIT).expect("the leader's hello"),
        (role.to_owned(), leader)
    );
    let start = started["start_time"].as_str().unwrap().to_owned();
    assert!(
        of_kind(&before, "input_state_changed")
            .iter()
            .any(|e| e["payload"]["to"] == "sent"),
        "{before:?}"
    );
    d1.child.kill().unwrap();
    let _ = d1.wait();
    // Daemon 2: its reconcilers committed, killed before its probe.
    let gate = Gate::new();
    let (mut d2, _) = held_at_probe(&labels, &d.path, &args, &gate);
    d2.child.kill().unwrap();
    let _ = d2.wait();
    gate.waiter_killed();
    // Daemon 3: the leader is still live with its recorded start (macOS), then the probe.
    let gate = Gate::new();
    let (mut d3, port) = held_at_probe(&labels, &d.path, &args, &gate);
    let instance = support::serving_record(&d.path).instance;
    #[cfg(unix)]
    match chaldea_pty_runtime::process::lookup(leader) {
        chaldea_pty_runtime::process::Lookup::Live(e) => {
            assert_eq!(e.identity.start_time, start);
        }
        other => panic!("the leader before the probe: {other:?}"),
    }
    gate.release();
    let mut leader_at_probe = None;
    assert!(
        support::wait_for(|| {
            if of_kind(&envelopes(port, &token, &t), "launch_evidence").is_empty() {
                return false;
            }
            leader_at_probe = Some(chaldea_pty_runtime::process::lookup(leader));
            true
        }),
        "the probe's evidence"
    );
    let after = envelopes(port, &token, &t);
    assert_eq!(
        leader_at_probe,
        Some(chaldea_pty_runtime::process::Lookup::Gone)
    );
    assert_eq!(after[..before.len()], before[..], "an unchanged prefix");
    let evidence = of_kind(&after, "launch_evidence");
    assert_eq!(evidence.len(), 1, "{after:?}");
    let scope = if cfg!(windows) {
        "unrecorded_job_and_outside_job"
    } else {
        "unrecorded_session_members"
    };
    assert_eq!(
        evidence[0]["payload"],
        serde_json::json!({
            "launch_id": started["launch_id"],
            "source": "probe",
            "identities": [{ "pid": leader, "start_time": start, "observed": "gone" }],
            "unchecked": [scope],
            "probe_run": instance,
        })
    );
    let tail = &after[before.len()..];
    let inputs = of_kind(tail, "input_state_changed");
    assert_eq!(inputs.len(), 1, "{tail:?}");
    assert_eq!(inputs[0]["payload"]["from"], "sent");
    assert_eq!(inputs[0]["payload"]["to"], "unconfirmed");
    assert!(of_kind(&after, "launch_death_confirmed").is_empty());
    assert!(tail.iter().all(|e| e["command_id"].is_null()), "{tail:?}");
    let (status, body) = request_complete(port, "GET", &format!("/api/threads/{t}"), &token, None);
    assert_eq!(status, 200, "{body}");
    let summary: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(summary["launch"]["exit_reason"], "daemon_restart");
    assert_eq!(summary["launch"]["death_confirmed"], false);
    assert_eq!(d3.stop_and_wait(&d.path), 0, "{}", d3.stderr());
    #[cfg(unix)]
    if !gone(leader) {
        support::term(leader, rustix::process::Signal::KILL);
    }
}

/// The documentation test of RD9's exception (`docs/design/supervisor-recovery.md` §6): a
/// spawn-tree's `ignore-term` sleeper stays in the crashed launch's session with no recorded
/// identity; the restart's probe names `unrecorded_session_members`. The sleeper's fate is
/// printed, not asserted, and the test ends it.
#[cfg(target_os = "macos")]
fn unrecorded_member_after_crash() {
    let labels = Labels::new();
    let d = state();
    let cwd = tempfile::tempdir().unwrap();
    let report = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = format!("127.0.0.1:{}", report.local_addr().unwrap().port());
    let seen = hellos(report);
    let args = [
        "--mode",
        "spawn-tree",
        "--roles",
        "ignore-term",
        "--report",
        &addr,
        "--max-life-secs",
        "120",
    ];
    let (mut d1, _) = daemon(&labels, &d.path, &args);
    let t = spawn_codex(&labels, &d.path, cwd.path());
    let sleeper = loop {
        let (role, pid) = seen.recv_timeout(WAIT).expect("the sleeper's hello");
        if role == "ignore-term" {
            break pid;
        }
    };
    d1.child.kill().unwrap();
    let _ = d1.wait();
    let (mut d2, port) = daemon(&labels, &d.path, &args);
    let token = chaldea_daemon::lifecycle::read_owner_token(&d.path).unwrap();
    let mut evidence = Value::Null;
    assert!(
        support::wait_for(|| {
            let events = envelopes(port, &token, &t);
            match of_kind(&events, "launch_evidence").first() {
                Some(e) => {
                    evidence = e["payload"].clone();
                    true
                }
                None => false,
            }
        }),
        "the probe's evidence"
    );
    assert_eq!(
        evidence["unchecked"],
        serde_json::json!(["unrecorded_session_members"])
    );
    eprintln!(
        "the unrecorded sleeper {sleeper} after the probe: {:?}",
        chaldea_pty_runtime::process::lookup(sleeper)
    );
    assert_eq!(d2.stop_and_wait(&d.path), 0, "{}", d2.stderr());
    if !gone(sleeper) {
        support::term(sleeper, rustix::process::Signal::KILL);
    }
    assert!(support::wait_for(|| gone(sleeper)));
}

/// PR10a2-CL-10 `a29_child_suspended` (Windows): the helper daemon runs inside a test job
/// from its first instruction; its first spawn is held after the suspended create, before
/// the assignment, and the helper is killed there. After a restart the launch is
/// `unknown_after_crash`, and the child is still suspended in the test's job, which ends it.
#[cfg(windows)]
fn a29_child_suspended() {
    use std::os::windows::process::CommandExt;

    use chaldea_pty_runtime::fake::{self, TestJob};
    use chaldea_pty_runtime::host::JobLimits;
    use support::faults::A29_GATE;

    /// `CREATE_SUSPENDED` (`winbase.h`).
    const CREATE_SUSPENDED: u32 = 0x0000_0004;

    let labels = Labels::new();
    let d = state();
    let cwd = tempfile::tempdir().unwrap();
    // FakeCli under the agent's image name, so the child's image is the one PR 21 looks for.
    let bin = tempfile::tempdir().unwrap();
    let codex = bin.path().join("codex.exe");
    std::fs::copy(fake_cli(), &codex).unwrap();
    let args = ["--mode", "sleeper", "--max-life-secs", "120"];
    let mut argv = vec![codex.to_string_lossy().into_owned()];
    argv.extend(args.iter().map(|a| (*a).to_owned()));
    argv.push("--".to_owned());
    let json = serde_json::to_string(&argv).unwrap();
    let gate = Gate::new();
    let job = TestJob::new(JobLimits {
        kill_on_close: true,
        breakaway_ok: false,
        silent_breakaway_ok: false,
    })
    .unwrap();
    let run = run_args(&d.path);
    let run: Vec<&str> = run.iter().map(String::as_str).collect();
    let mut c = cmd(
        &labels,
        &d.path,
        &run,
        &[
            (AGENT_PROGRAM, OsStr::new(&json)),
            (A29_GATE, gate.path().as_os_str()),
        ],
    );
    c.creation_flags(CREATE_SUSPENDED);
    let mut helper = Proc::spawn(&mut c);
    job.assign(helper.pid()).unwrap();
    assert!(job.contains(helper.pid()).unwrap());
    assert!(fake::resume_process(helper.pid()).unwrap() >= 1);
    let port = helper.serving();
    let token = chaldea_daemon::lifecycle::read_owner_token(&d.path).unwrap();
    let body = serde_json::json!({
        "command_id": uuid_v4(),
        "args": { "agent": "codex", "cwd": cwd.path().to_string_lossy() }
    })
    .to_string();
    let request = format!(
        "POST /api/commands/spawn HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut conn = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    std::io::Write::write_all(&mut conn, request.as_bytes()).unwrap();
    assert!(gate.reached(WAIT), "the suspended create");
    let child: u32 = std::fs::read_to_string(gate.path().join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    helper.child.kill().unwrap();
    let _ = helper.wait();
    gate.waiter_killed();
    drop(conn);
    let (mut d2, port) = daemon(&labels, &d.path, &args);
    let (t, events) = thread_events(port, &token);
    let exited = &events.iter().find(|(k, _)| k == "launch_exited").unwrap().1;
    assert_eq!(exited["reason"], "unknown_after_crash");
    assert_eq!(exited["death_confirmed"], false);
    let suspended = fake::suspend_count(child).unwrap() >= 1;
    assert!(suspended, "the child runs");
    assert!(
        job.contains(child).unwrap(),
        "the child is in the test's job"
    );
    // PR 21's predicate on the real child: its image, its creation time, and the
    // reservation's `recorded_at`; the running daemon holds no job of it.
    let all = envelopes(port, &token, &t);
    let reserved = of_kind(&all, "launch_reserved")[0]["recorded_at"]
        .as_str()
        .unwrap()
        .to_owned();
    let reserved_ms = chaldea_core::launch::stamp_ms(&reserved).expect("a recorded_at stamp");
    let image = fake::image_path(child).unwrap();
    let created: u64 = fake::identify(child)
        .unwrap()
        .expect("the child's identity")
        .start_time
        .parse()
        .unwrap();
    let process = chaldea_core::launch::A29Process {
        suspended,
        in_daemon_job: false,
        image_path: &image.to_string_lossy(),
        created_filetime: created,
    };
    assert!(
        chaldea_core::launch::a29_candidate(chaldea_core::ids::Agent::Codex, reserved_ms, &process),
        "{image:?} created {created}, reserved {reserved}"
    );
    assert_eq!(d2.stop_and_wait(&d.path), 0, "{}", d2.stderr());
    // Closing the kill-on-close job ends the child the test caused.
    job.close();
    assert!(support::wait_for(|| matches!(
        chaldea_pty_runtime::process::lookup(child),
        chaldea_pty_runtime::process::Lookup::Gone
    )));
    drop(bin);
}

/// PR10a2-CL-3 on Windows: a spawn whose job assignment and then termination fail ends
/// `spawn_failed` with its suspended child alive and recorded in `failed_start`; after a
/// crash, the child is still alive with that identity when the next start reaches its probe,
/// and that probe signals it by its recorded identity and records it `gone`, with this run
/// as `probe_run`. Daemon 1 runs in a kill-on-close job the test owns from before it starts,
/// so its child is in that job too, and is held at `prober_start`, so only daemon 2 probes.
#[cfg(windows)]
fn assign_failure_child_probed() {
    use std::os::windows::process::CommandExt;

    use chaldea_pty_runtime::fake::{self, TestJob};
    use chaldea_pty_runtime::host::JobLimits;
    use chaldea_pty_runtime::process::{Lookup, lookup};
    use support::faults::WINDOWS_FAULT;

    /// `CREATE_SUSPENDED` (`winbase.h`).
    const CREATE_SUSPENDED: u32 = 0x0000_0004;

    /// Ends the failed start's child by its recorded identity when the test ends, a failed
    /// assertion included.
    struct EndChild(u32, String);

    impl Drop for EndChild {
        fn drop(&mut self) {
            if lookup(self.0) != Lookup::Gone && !fake::kill_verified(self.0, &self.1) {
                eprintln!("the failed start's child {} was not ended", self.0);
            }
        }
    }

    let labels = Labels::new();
    let d = state();
    let cwd = tempfile::tempdir().unwrap();
    let args = ["--mode", "sleeper", "--max-life-secs", "120"];
    let mut argv = vec![fake_cli().to_string_lossy().into_owned()];
    argv.extend(args.iter().map(|a| (*a).to_owned()));
    argv.push("--".to_owned());
    let json = serde_json::to_string(&argv).unwrap();
    let job = TestJob::new(JobLimits {
        kill_on_close: true,
        breakaway_ok: false,
        silent_breakaway_ok: false,
    })
    .unwrap();
    let gate = Gate::new();
    let run = run_args(&d.path);
    let run: Vec<&str> = run.iter().map(String::as_str).collect();
    let mut c = cmd(
        &labels,
        &d.path,
        &run,
        &[
            (AGENT_PROGRAM, OsStr::new(&json)),
            (WINDOWS_FAULT, OsStr::new("assign_terminate_denied")),
            (SUPERVISOR_PAUSE, OsStr::new("prober_start")),
            (SUPERVISOR_GATE, gate.path().as_os_str()),
        ],
    );
    c.creation_flags(CREATE_SUSPENDED);
    let mut d1 = Proc::spawn(&mut c);
    job.assign(d1.pid()).unwrap();
    assert!(fake::resume_process(d1.pid()).unwrap() >= 1);
    let port = d1.serving();
    assert!(gate.reached(WAIT), "daemon 1 held before its probe");
    let token = chaldea_daemon::lifecycle::read_owner_token(&d.path).unwrap();
    let (_, v, err) = agent(
        &labels,
        &d.path,
        &[
            "spawn",
            "--agent",
            "codex",
            "--cwd",
            &cwd.path().to_string_lossy(),
        ],
    );
    let (t, events) = thread_events(port, &token);
    let exited = &events
        .iter()
        .find(|(k, _)| k == "launch_exited")
        .unwrap_or_else(|| panic!("{v} {err} {events:?}"))
        .1;
    assert_eq!(exited["reason"], "spawn_failed", "{exited}");
    assert_eq!(exited["death_confirmed"], false);
    let child = u32::try_from(exited["failed_start"]["pid"].as_u64().unwrap()).unwrap();
    let start = exited["failed_start"]["start_time"]
        .as_str()
        .unwrap()
        .to_owned();
    let _end = EndChild(child, start.clone());
    assert!(
        job.contains(child).unwrap(),
        "the child is in the test's job"
    );
    d1.child.kill().unwrap();
    let _ = d1.wait();
    gate.waiter_killed();
    // Daemon 2, held before its probe: the child outlived daemon 1 with its recorded start.
    let gate = Gate::new();
    let (mut d2, port) = held_at_probe(&labels, &d.path, &args, &gate);
    let instance = support::serving_record(&d.path).instance;
    match lookup(child) {
        Lookup::Live(e) => assert_eq!(e.identity.start_time, start),
        other => panic!("the failed start's child before the probe: {other:?}"),
    }
    gate.release();
    let probes = |port| -> Vec<Value> {
        envelopes(port, &token, &t)
            .into_iter()
            .filter(|e| e["kind"] == "launch_evidence" && e["payload"]["source"] == "probe")
            .map(|e| e["payload"].clone())
            .collect()
    };
    assert!(
        support::wait_for(|| !probes(port).is_empty()),
        "the probe's evidence"
    );
    assert_eq!(
        probes(port),
        vec![serde_json::json!({
            "launch_id": exited["launch_id"],
            "source": "probe",
            "identities": [{ "pid": child, "start_time": start, "observed": "gone" }],
            "unchecked": ["unrecorded_job_and_outside_job"],
            "probe_run": instance,
        })]
    );
    assert_eq!(lookup(child), Lookup::Gone);
    assert_eq!(d2.stop_and_wait(&d.path), 0, "{}", d2.stderr());
}
