//! Set B: `chs fake <paste|queries|stall|tree-kill|auth|emit|teardown|all>`.

use std::ffi::OsString;
use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{
    Ctx, Launch, LaunchSpec, child_out, child_report, emitter_path, launch, write_snapshot,
};
use crate::cli::{self, Args};
use crate::paste::{self, Newline, PasteOpts};
use crate::receiver::{self, BindRule, Config, Receiver};
use crate::report::Report;
use crate::session::{Inject, StopReport};
use crate::verdict::Outcome;
use crate::{fake_cli, os, util};

const FAKE_UPS_WAIT: Duration = Duration::from_secs(5);

/// B scenarios check the harness against FakeCli; a failed comparison is the observation.
fn checked(b: bool) -> Outcome {
    Outcome::observed(b, "check failed; see evidence")
}

/// A session witness after the stop. It is platform evidence only when the
/// assignment succeeded, the witness was positively alive before the stop,
/// and both lookups succeeded; an unknown liveness is a harness failure.
fn killed_outcome(assigned: bool, before: Option<bool>, after: Option<bool>) -> Outcome {
    if !assigned {
        return Outcome::harness("job assignment failed");
    }
    match (before, after) {
        (None, _) | (_, None) => Outcome::harness("liveness lookup failed"),
        (Some(false), _) => Outcome::not_exercised("witness exited before the stop"),
        (Some(true), Some(false)) => Outcome::Pass,
        (Some(true), Some(true)) => Outcome::platform("witness survived the stop"),
    }
}

pub const SCENARIOS: &[&str] = &[
    "paste",
    "queries",
    "stall",
    "tree-kill",
    "auth",
    "emit",
    "teardown",
];

pub fn run(args: Args) -> i32 {
    let which = args.positional.get(1).cloned().unwrap_or_default();
    match which.as_str() {
        "all" => run_all(args),
        "teardown-case" => run_case(args),
        s if SCENARIOS.contains(&s) => {
            let ctx = match Ctx::new(args, &format!("fake {s}")) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("chs: {e}");
                    return 3;
                }
            };
            ctx.sink
                .with(|r| r.detail("emitter", json!(emitter_path(&ctx.args.root))));
            match s {
                "paste" => paste_scenario(&ctx),
                "queries" => queries(&ctx),
                "stall" => stall(&ctx),
                "tree-kill" => tree_kill(&ctx),
                "auth" => auth(&ctx),
                "emit" => {
                    emit(&ctx);
                    nested(&ctx);
                }
                "teardown" => teardown(&ctx),
                _ => unreachable!(),
            }
            ctx.finish()
        }
        other => {
            eprintln!("chs fake: unknown scenario {other:?}\n{}", cli::USAGE);
            2
        }
    }
}

fn fake_bin() -> PathBuf {
    util::sibling_bin("chs-fake-cli")
}

fn fake_spec(ctx: &Ctx, label: &str, mode: &str, extra: &[&str]) -> LaunchSpec {
    let mut argv: Vec<OsString> = vec![fake_bin().into(), "--mode".into(), mode.into()];
    argv.push("--emitter".into());
    argv.push(emitter_path(&ctx.args.root).into());
    argv.extend(extra.iter().map(OsString::from));
    LaunchSpec {
        label: label.into(),
        argv,
        cwd: ctx.run_dir.clone(),
        agent: "fake".into(),
        cli_version: util::FAKE_CLI_VERSION.into(),
        session_id: None,
        cli_names: vec!["chs-fake-cli".into()],
        extra_env: Vec::new(),
        responder: true,
        hold_assign: false,
        inject: None,
    }
}

fn stop_evidence(stop: &StopReport) -> Value {
    json!({
        "survivors": stop.survivors,
        "escaped_primary": stop.escaped_primary,
        "killed_by_fallback": stop.killed_by_fallback,
        "teardown_ms": stop.teardown_ms,
        "threads": stop.threads,
        "tree": stop.tree,
        "primary_steps": stop.primary_steps,
    })
}

fn bound_ups_count(l: &Launch) -> usize {
    l.receiver
        .events()
        .iter()
        .filter(|e| e.event == "UserPromptSubmit" && e.is_bound())
        .count()
}

/// Waits for the `n`-th bound UserPromptSubmit and returns its prompt bytes + payload.
fn wait_ups(l: &Launch, n: usize, end: Instant) -> Option<(Vec<u8>, Value)> {
    loop {
        let ups: Vec<_> = l
            .receiver
            .events()
            .into_iter()
            .filter(|e| e.event == "UserPromptSubmit" && e.is_bound())
            .collect();
        if ups.len() >= n {
            let v = ups[n - 1].json()?;
            let prompt = v
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or("")
                .as_bytes()
                .to_vec();
            return Some((prompt, v));
        }
        if Instant::now() >= end {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ---------------- B-3 / B-4 paste ----------------

/// B-3 oracle, independent of the writer's mapper: normalize CRLF to LF,
/// split on LF, join with the mode's newline (a lone CR is kept).
fn expected_wire(text: &str, mode: Newline) -> Vec<u8> {
    let nl = match mode {
        Newline::Cr => "\r",
        Newline::Lf => "\n",
        Newline::Crlf => "\r\n",
    };
    text.replace("\r\n", "\n")
        .split('\n')
        .collect::<Vec<_>>()
        .join(nl)
        .into_bytes()
}

fn paste_scenario(ctx: &Ctx) {
    let corpus = paste::corpus();
    let _ = std::fs::write(
        ctx.out.join("corpus.json"),
        serde_json::to_vec_pretty(&corpus).unwrap_or_default(),
    );
    let modes: Vec<Newline> = ctx
        .args
        .paste_newline
        .map(|m| vec![m])
        .unwrap_or(Newline::ALL.to_vec());
    let chunks: Vec<usize> = if ctx.args.chunk > 0 {
        vec![ctx.args.chunk]
    } else {
        vec![0, 7]
    };
    let b3_case = |m: &Newline, id: &str, chunk: usize| format!("{}/{id}/chunk{chunk}", m.name());
    const B4: [&str; 4] = ["esc_injection", "etx", "nul", "bel"];
    for mode in &modes {
        for chunk in &chunks {
            for e in &corpus {
                ctx.require("B-3", &b3_case(mode, &e.id, *chunk));
            }
        }
        for name in B4 {
            ctx.require("B-4", &format!("{}/{name}", mode.name()));
        }
    }
    for mode in &modes {
        let l = match launch(
            ctx,
            fake_spec(ctx, &format!("paste-{}", mode.name()), "echo", &[]),
        ) {
            Ok(l) => l,
            Err(e) => {
                ctx.check(
                    "B-3",
                    &format!("{}/launch", mode.name()),
                    Outcome::harness("launch failed"),
                    json!({"launch_error": e}),
                );
                continue;
            }
        };
        let ss = ctx.until(Duration::from_secs(ctx.args.session_start_timeout));
        if !l.wait_gate(ss) {
            ctx.check(
                "B-3",
                &format!("{}/launch", mode.name()),
                Outcome::not_exercised("readiness gate never opened"),
                json!({"readiness_gate_failed": true, "bound": l.receiver.bound(),
                       "ambiguous": l.receiver.ambiguous()}),
            );
            ctx.detail(&format!("launch-{}", l.label), json!(l.finish().1));
            continue;
        }
        for chunk in &chunks {
            for e in &corpus {
                if ctx.past_cutoff() {
                    break;
                }
                let case = b3_case(mode, &e.id, *chunk);
                let n = bound_ups_count(&l) + 1;
                let o = paste::paste(
                    &l.session.io().tx,
                    e.text.as_bytes(),
                    &PasteOpts {
                        mode: *mode,
                        chunk: *chunk,
                        enter_delay: Duration::from_millis(ctx.args.enter_delay_ms),
                        write_deadline: ctx.until(Duration::from_secs(10)),
                    },
                    || l.ready(),
                );
                let expected = expected_wire(&e.text, *mode);
                let got = (o.state == "sent")
                    .then(|| wait_ups(&l, n, ctx.until(FAKE_UPS_WAIT)))
                    .flatten();
                let (res, ev) = match &got {
                    Some((prompt, _)) => {
                        let diff = util::first_diff(&expected, prompt);
                        (
                            checked(diff.is_none()),
                            json!({"byte_equal": diff.is_none(), "first_diff": diff, "len": expected.len(), "got_len": prompt.len()}),
                        )
                    }
                    None if o.state == "sent" => (
                        Outcome::harness("UserPromptSubmit missing after a sent paste"),
                        json!({"paste": o, "ups": "missing"}),
                    ),
                    None => (
                        Outcome::not_exercised(format!("paste {}", o.state)),
                        json!({"paste": o}),
                    ),
                };
                ctx.check("B-3", &case, res, ev);
            }
        }
        // B-4: rejected payloads write zero bytes.
        for (name, bad) in [
            ("esc_injection", b"ls\x1b[201~\rrm -rf x".to_vec()),
            ("etx", b"abc\x03".to_vec()),
            ("nul", b"\x00".to_vec()),
            ("bel", "日本\x07".as_bytes().to_vec()),
        ] {
            let o = paste::paste(
                &l.session.io().tx,
                &bad,
                &PasteOpts {
                    mode: *mode,
                    chunk: 0,
                    enter_delay: Duration::from_millis(0),
                    write_deadline: ctx.until(Duration::from_secs(2)),
                },
                || l.ready(),
            );
            let ok = o.state == "rejected" && o.bytes_after == o.bytes_before;
            ctx.check(
                "B-4",
                &format!("{}/{name}", mode.name()),
                checked(ok),
                json!(o),
            );
        }
        let (stop, summary) = l.finish();
        ctx.detail(
            &format!("paste-{}", mode.name()),
            json!({"stop": stop_evidence(&stop), "receiver_rejections": summary["rejections"]}),
        );
    }
}

// ---------------- B-5 queries ----------------

fn queries(ctx: &Ctx) {
    ctx.require("B-5", "responder");
    ctx.require("B-5", "negative_control_no_responder");
    let mut runs = Vec::new();
    for responder in [true, false] {
        let label = if responder {
            "queries-responder"
        } else {
            "queries-no-responder"
        };
        let mut spec = fake_spec(ctx, label, "queries", &[]);
        spec.responder = responder;
        let l = match launch(ctx, spec) {
            Ok(l) => l,
            Err(e) => {
                ctx.check(
                    "B-5",
                    label,
                    Outcome::harness("launch failed"),
                    json!({"launch_error": e}),
                );
                return;
            }
        };
        let code = l.session.wait_exit(ctx.until(Duration::from_secs(15)));
        let fake_report = l
            .receiver
            .wait_witness(ctx.until(Duration::from_secs(2)), |w| {
                w.role == "queries-result"
            })
            .map(|w| w.report);
        let log = l.query_log();
        let (stop, _) = l.finish();
        runs.push((responder, code, fake_report, log, stop));
    }
    let on = &runs[0];
    let off = runs.get(1);
    let on_ok = on.1 == Some(0) && on.2.as_ref().and_then(|r| r["all_ok"].as_bool()) == Some(true);
    ctx.check(
        "B-5",
        "responder",
        checked(on_ok),
        json!({"exit": on.1, "fake": on.2, "query_log": on.3}),
    );
    if let Some(off) = off {
        let failed = off.1 != Some(0);
        // Negative control: must fail on macOS; on Windows conhost may answer some queries itself.
        let required = os::os_name() != "windows";
        let res = Outcome::observed(
            failed || !required,
            "queries answered without the responder",
        );
        ctx.check(
            "B-5",
            "negative_control_no_responder",
            res,
            json!({"expected": "fake-cli fails", "exit": off.1, "fake": off.2, "query_log": off.3,
                   "note": "replies seen without harness writes come from conhost"}),
        );
    }
}

// ---------------- B-6 stall ----------------

/// Independently authored expected grid for FakeCli `stall` (not derived from `draw_stall`).
fn expected_stall_lines(rows: usize) -> Vec<String> {
    let mut v = vec![
        "Do you trust the files in this folder?".to_string(),
        "日本語の表示テスト｜ｶﾀｶﾅ".to_string(),
        "e\u{301} か\u{3099} 😀 end".to_string(),
        "abc".to_string(),
        "漢字".to_string(),
    ];
    v.resize(rows, String::new());
    v
}

fn stall(ctx: &Ctx) {
    let rows = ctx.args.rows as usize;
    for label in ["stall", "stall-silent", "banner"] {
        ctx.require("B-6", label);
    }
    for (label, mode, extra) in [
        ("stall", "stall", vec![]),
        ("stall-silent", "stall", vec!["--silent"]),
        ("banner", "banner", vec![]),
    ] {
        let l = match launch(ctx, fake_spec(ctx, label, mode, &extra)) {
            Ok(l) => l,
            Err(e) => {
                ctx.check(
                    "B-6",
                    label,
                    Outcome::harness("launch failed"),
                    json!({"launch_error": e}),
                );
                continue;
            }
        };
        let t = Instant::now();
        let bound = l
            .receiver
            .wait_bound(ctx.until(Duration::from_secs(ctx.args.session_start_timeout)));
        let latency = t.elapsed().as_millis() as u64;
        let mut snap = None;
        if bound.is_none() {
            std::thread::sleep(Duration::from_millis(100));
            let mut s = l.snapshot();
            s.decided_by = Some("hook_timeout".into());
            write_snapshot(ctx, label, &s);
            snap = Some(s);
        }
        let (stop, _) = l.finish();
        let (ok, ev) = match label {
            "stall" => {
                let s = snap.clone();
                let ok = s.as_ref().is_some_and(|s| {
                    s.lines == expected_stall_lines(rows)
                        && s.cursor == (6, 2)
                        && s.alt_screen
                        && s.bracketed_paste
                });
                let diff: Vec<Value> = s
                    .as_ref()
                    .map(|s| {
                        s.lines
                            .iter()
                            .zip(expected_stall_lines(rows))
                            .enumerate()
                            .filter(|(_, (a, b))| *a != b)
                            .map(|(i, (a, b))| json!({"row": i, "got": a, "want": b}))
                            .collect()
                    })
                    .unwrap_or_default();
                (
                    ok,
                    json!({"timed_out": bound.is_none(), "snapshot": s, "grid_diff": diff}),
                )
            }
            "stall-silent" => {
                let ok = snap
                    .as_ref()
                    .is_some_and(|s| s.lines.iter().all(String::is_empty) && s.cursor == (0, 0));
                (ok, json!({"timed_out": bound.is_none(), "snapshot": snap}))
            }
            _ => (
                bound.is_some() && snap.is_none(),
                json!({"bound": bound, "decided_by": null, "latency_ms": latency}),
            ),
        };
        ctx.check(
            "B-6",
            label,
            checked(ok && stop.survivors.is_empty()),
            json!({"check": ev, "stop": stop_evidence(&stop)}),
        );
    }
}

// ---------------- B-7 tree kill ----------------

/// Upper bound on every FakeCli sleeper's life, so a fixture that escapes
/// containment still ends by itself.
const SLEEPER_MAX_LIFE: &str = "300";

fn spawn_unrelated(env: &[(String, String)]) -> std::io::Result<Child> {
    let mut cmd = Command::new(fake_bin());
    cmd.args([
        "--mode",
        "sleeper",
        "--role",
        "unrelated",
        "--max-life-secs",
        SLEEPER_MAX_LIFE,
    ]);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

/// Role of the escape probe's orphaned `setsid` grandchild.
pub const ORPHAN_ROLE: &str = "setsid-orphan";

/// Runs B-7 once. With `escape_probe`, spawn-tree also starts a `setsid`
/// grandchild whose parent exits at once (a containment limit on macOS: no
/// subreaper, so neither pgid nor ppid reaches it); the case is reported with
/// `containment_limit: true` and the orphan, a fixture this harness spawned,
/// is cleaned up through its witness identity afterwards (`fixture_cleanup`).
pub fn tree_kill_once(ctx: &Ctx, hold_assign: bool, label: &str, escape_probe: bool) {
    let mut roles: Vec<&str> = os::spawn_tree_roles().to_vec();
    roles.push("spawn-tree");
    if escape_probe {
        roles.push(ORPHAN_ROLE);
    }
    for role in &roles {
        ctx.require("B-7", &format!("{label}/{role}"));
    }
    ctx.require("B-7", &format!("{label}/unrelated_negative_control"));
    ctx.require("B-7", &format!("{label}/no_session_survivors"));
    let mut extra = vec!["--max-life-secs", SLEEPER_MAX_LIFE];
    if escape_probe {
        extra.push("--escape-orphan");
    }
    let mut spec = fake_spec(ctx, label, "spawn-tree", &extra);
    spec.hold_assign = hold_assign;
    let l = match launch(ctx, spec) {
        Ok(l) => l,
        Err(e) => {
            ctx.check(
                "B-7",
                &format!("{label}/launch"),
                Outcome::harness("launch failed"),
                json!({"launch_error": e}),
            );
            return;
        }
    };
    let mut unrelated = spawn_unrelated(&l.env()).ok();
    let mut assign = json!("at_spawn");
    if hold_assign {
        let first = l
            .receiver
            .wait_witness(ctx.until(Duration::from_secs(5)), |w| {
                w.role == "spawn-tree-report"
            })
            .map(|w| w.report["first_child"].clone());
        assign = json!({"held_until_child": first, "tree": l.session.attach_tree()});
    }
    // The exercised case needs the job assignment (or process group) to
    // have succeeded; otherwise nothing below is platform evidence.
    let tree = l.session.tree_desc();
    let assign_error = tree.get("error").cloned();
    let mut want = roles.clone();
    want.push("unrelated");
    let all_seen = l
        .receiver
        .wait_witnesses(ctx.until(Duration::from_secs(5)), |ws| {
            want.iter().all(|r| ws.iter().any(|w| w.role == *r))
        });
    // Give the recorder one more pass after every witness exists.
    l.session.owned.record_scan();
    let witnesses = l.receiver.witnesses();
    // A witness counts only if it is positively alive before the stop.
    let pre_stop: Vec<(u32, Option<bool>)> = witnesses
        .iter()
        .map(|w| (w.pid, os::liveness(w.pid, w.start_time)))
        .collect();
    let (stop, _) = l.finish();
    // Verify within 5 s.
    let end = Instant::now() + Duration::from_secs(5);
    let session_lives = |ws: &[receiver::WitnessRecord]| -> Vec<(u32, Option<bool>)> {
        ws.iter()
            .filter(|w| w.session)
            .map(|w| (w.pid, os::liveness(w.pid, w.start_time)))
            .filter(|(_, l)| *l != Some(false))
            .collect()
    };
    let mut alive = session_lives(&witnesses);
    while !alive.is_empty() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
        alive = session_lives(&witnesses);
    }
    let pre = |pid: u32| {
        pre_stop
            .iter()
            .find(|(p, _)| *p == pid)
            .and_then(|(_, l)| *l)
    };
    for role in &roles {
        let w = witnesses.iter().find(|w| w.role == *role);
        let (res, mut ev) = match w {
            None => (
                Outcome::not_exercised("witness never seen"),
                json!({"witness": "never seen"}),
            ),
            Some(w) => {
                let after = os::liveness(w.pid, w.start_time);
                let by = if stop.killed_by_fallback.iter().any(|p| p.pid == w.pid) {
                    "fallback"
                } else {
                    "primary"
                };
                let dead = after == Some(false);
                (
                    killed_outcome(assign_error.is_none(), pre(w.pid), after),
                    json!({"pid": w.pid, "alive_before_stop": pre(w.pid), "alive_after_stop": after,
                           "dead": dead, "killed_by": dead.then_some(by)}),
                )
            }
        };
        if *role == ORPHAN_ROLE {
            ev["containment_limit"] = json!(true);
        }
        ctx.check("B-7", &format!("{label}/{role}"), res, ev);
    }
    let un = witnesses.iter().find(|w| w.role == "unrelated");
    let (res, ev) = match un {
        None => (
            Outcome::not_exercised("witness never seen"),
            json!({"witness": "never seen"}),
        ),
        Some(w) => {
            let a = os::liveness(w.pid, w.start_time);
            let o = match a {
                None => Outcome::harness("liveness lookup failed"),
                Some(a) => checked(a),
            };
            (o, json!({"pid": w.pid, "alive_after_stop": a}))
        }
    };
    ctx.check(
        "B-7",
        &format!("{label}/unrelated_negative_control"),
        res,
        ev,
    );
    let unknown = alive.iter().any(|(_, l)| l.is_none());
    ctx.check(
        "B-7",
        &format!("{label}/no_session_survivors"),
        if let Some(e) = &assign_error {
            Outcome::harness(format!("job assignment failed: {e}"))
        } else if !all_seen {
            Outcome::not_exercised("not every witness attested")
        } else if unknown {
            Outcome::harness("liveness lookup failed")
        } else {
            checked(alive.is_empty() && stop.survivors.is_empty())
        },
        json!({"alive_session_witnesses": alive, "assign": assign, "tree": tree,
               "stop": stop_evidence(&stop)}),
    );
    if escape_probe {
        ctx.detail("containment_limit", json!(true));
        ctx.detail("fixture_cleanup", fixture_cleanup(&witnesses));
    }
    if let Some(u) = unrelated.as_mut() {
        let _ = u.kill();
        let _ = u.wait();
    }
}

/// After the verdict: kills the escape probe's orphan, a fixture this harness
/// spawned, only if its (pid, start_time) still matches the witness and the
/// process is FakeCli. Not part of the kill set; witnesses never get kill
/// authority over sessions.
fn fixture_cleanup(witnesses: &[receiver::WitnessRecord]) -> Value {
    let fake = util::normalize_proc_name("chs-fake-cli");
    let out: Vec<Value> = witnesses
        .iter()
        .filter(|w| w.role == ORPHAN_ROLE)
        .map(|w| {
            let p = os::proc_info(w.pid);
            let identity = p.as_ref().is_some_and(|p| {
                p.start_time == w.start_time && util::normalize_proc_name(&p.name) == fake
            });
            let killed = identity && os::force_kill(w.pid, w.start_time);
            json!({"pid": w.pid, "role": w.role, "alive": p.is_some(), "identity_ok": identity, "killed": killed})
        })
        .collect();
    json!(out)
}

fn tree_kill(ctx: &Ctx) {
    let label = match (ctx.args.escape_probe, ctx.args.hold_assign) {
        (true, _) => "tree-escape-probe",
        (false, true) => "tree-hold-assign",
        (false, false) => "tree",
    };
    tree_kill_once(ctx, ctx.args.hold_assign, label, ctx.args.escape_probe);
}

// ---------------- B-8 auth ----------------

fn raw_request(port: u16, parts: &[(&[u8], Duration)], read_deadline: Duration) -> (u16, u128) {
    let t = Instant::now();
    let Ok(mut s) = TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(1),
    ) else {
        return (0, 0);
    };
    for (bytes, pause) in parts {
        let _ = s.write_all(bytes);
        std::thread::sleep(*pause);
    }
    let code = crate::http_client::read_response(&mut s, Instant::now() + read_deadline)
        .map(|r| r.0)
        .unwrap_or(0);
    (code, t.elapsed().as_millis())
}

fn count_files(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| {
                    if e.path().is_dir() {
                        count_files(&e.path())
                    } else {
                        1
                    }
                })
                .sum()
        })
        .unwrap_or(0)
}

fn auth(ctx: &Ctx) {
    let fixtures = ctx.out.join("fixtures-auth");
    let token = util::token();
    let mut r = match Receiver::start(Config {
        token: token.clone(),
        thread_id: "thread".into(),
        agent: "fake".into(),
        cli_version: "auth".into(),
        launch_id: "auth".into(),
        rule: BindRule::SessionId("sid".into()),
        fixtures_dir: fixtures.clone(),
        run_id: ctx.run_id.clone(),
        cli_names: vec![],
    }) {
        Ok(r) => r,
        Err(e) => {
            ctx.error(format!("receiver: {e}"));
            return;
        }
    };
    let p = r.port();
    let host = format!("127.0.0.1:{p}");
    let auth = format!("Authorization: Bearer {token}\r\nX-Chaldea-Thread-Id: thread\r\n");
    let body = r#"{"hook_event_name":"SessionStart","session_id":"sid"}"#;
    let mk = |head: String| -> Vec<u8> { head.into_bytes() };
    let post = |hdrs: &str, b: &str| {
        mk(format!(
            "POST /hook HTTP/1.1\r\n{hdrs}Content-Length: {}\r\n\r\n{b}",
            b.len()
        ))
    };
    let h = format!("Host: {host}\r\n");
    let cases: Vec<(&str, Vec<u8>, u16)> = vec![
        (
            "bad_host",
            post(&format!("Host: evil.example:{p}\r\n{auth}"), body),
            421,
        ),
        (
            "host_wrong_port",
            post(&format!("Host: 127.0.0.1:1\r\n{auth}"), body),
            421,
        ),
        ("dup_host", post(&format!("{h}{h}{auth}"), body), 421),
        ("missing_host", post(&auth, body), 421),
        ("missing_token", post(&h, body), 401),
        (
            "wrong_token",
            post(
                &format!("{h}Authorization: Bearer {}\r\n", util::token()),
                body,
            ),
            401,
        ),
        ("dup_token", post(&format!("{h}{auth}{auth}"), body), 401),
        (
            "basic_scheme",
            post(&format!("{h}Authorization: Basic {token}\r\n"), body),
            401,
        ),
        (
            "wrong_method",
            mk(format!("GET /hook HTTP/1.1\r\n{h}{auth}\r\n")),
            405,
        ),
        (
            "wrong_path",
            mk(format!(
                "POST /hooks HTTP/1.1\r\n{h}{auth}Content-Length: 2\r\n\r\n{{}}"
            )),
            404,
        ),
        (
            "transfer_encoding",
            post(&format!("{h}{auth}Transfer-Encoding: chunked\r\n"), body),
            400,
        ),
        (
            "missing_content_length",
            mk(format!("POST /hook HTTP/1.1\r\n{h}{auth}\r\n")),
            411,
        ),
        (
            "dup_content_length",
            post(
                &format!("{h}{auth}Content-Length: {}\r\n", body.len()),
                body,
            ),
            411,
        ),
        (
            "non_numeric_content_length",
            mk(format!(
                "POST /hook HTTP/1.1\r\n{h}{auth}Content-Length: +5\r\n\r\nabcde"
            )),
            411,
        ),
        (
            "headers_over_8k",
            mk(format!(
                "POST /hook HTTP/1.1\r\n{h}{auth}X-Pad: {}\r\n\r\n",
                "a".repeat(9000)
            )),
            431,
        ),
        (
            "too_many_headers",
            mk(format!(
                "POST /hook HTTP/1.1\r\n{h}{auth}{}\r\n",
                "X-A: 1\r\n".repeat(40)
            )),
            431,
        ),
        (
            "body_over_4mib",
            mk(format!(
                "POST /hook HTTP/1.1\r\n{h}{auth}Content-Length: {}\r\n\r\n",
                receiver::MAX_BODY + 1
            )),
            413,
        ),
    ];
    for (name, _, _) in &cases {
        ctx.require("B-8", name);
    }
    for name in [
        "deadline_slow_head",
        "deadline_slow_body",
        "no_fixture_no_readiness",
        "positive_control",
    ] {
        ctx.require("B-8", name);
    }
    let before_files = count_files(&fixtures);
    for (name, bytes, want) in cases {
        let (got, ms) = raw_request(p, &[(&bytes, Duration::ZERO)], Duration::from_secs(7));
        let ok = got == want && count_files(&fixtures) == before_files && r.bound().is_none();
        ctx.check(
            "B-8",
            name,
            checked(ok),
            json!({"status": got, "want": want, "ms": ms}),
        );
    }
    // Deadline cases run concurrently: a stalled head and a stalled body.
    let head_part = format!("POST /hook HTTP/1.1\r\n{h}");
    let body_head = format!(
        "POST /hook HTTP/1.1\r\n{h}{auth}Content-Length: {}\r\n\r\n",
        body.len()
    );
    let slow = [
        ("deadline_slow_head", head_part.into_bytes(), Vec::new()),
        (
            "deadline_slow_body",
            body_head.into_bytes(),
            body.as_bytes()[..5].to_vec(),
        ),
    ];
    let handles: Vec<_> = slow
        .into_iter()
        .map(|(name, a, b)| {
            std::thread::spawn(move || {
                let (code, ms) = raw_request(
                    p,
                    &[(&a, Duration::ZERO), (&b, Duration::ZERO)],
                    Duration::from_secs(8),
                );
                (name, code, ms)
            })
        })
        .collect();
    for hnd in handles {
        if let Ok((name, code, ms)) = hnd.join() {
            let ok = (code == 408 || code == 0)
                && (4500..=6500).contains(&ms)
                && count_files(&fixtures) == before_files;
            ctx.check(
                "B-8",
                name,
                checked(ok),
                json!({"status": code, "elapsed_ms": ms, "deadline_ms": 5000}),
            );
        }
    }
    ctx.check(
        "B-8",
        "no_fixture_no_readiness",
        checked(count_files(&fixtures) == 0 && r.bound().is_none() && r.events().is_empty()),
        json!({"fixtures": count_files(&fixtures), "events": r.events().len(), "rejections": r.rejections()}),
    );
    // Positive control: the same receiver accepts a valid request (proves the checks can pass).
    let (code, _) = raw_request(
        p,
        &[(
            &post(&format!("Host: localhost:{p}\r\n{auth}"), body),
            Duration::ZERO,
        )],
        Duration::from_secs(5),
    );
    ctx.check(
        "B-8",
        "positive_control",
        checked(code == 200 && r.bound().is_some() && count_files(&fixtures) == 2),
        json!({"status": code, "fixtures": count_files(&fixtures)}),
    );
    r.stop();
}

// ---------------- B-9 emitter ----------------

struct EmitRun {
    exit: Option<i32>,
    stdout: Vec<u8>,
    ms: u128,
}

fn run_emitter(
    emitter: &Path,
    env: &[(&str, String)],
    stdin: Option<Vec<u8>>,
    keep_open: bool,
) -> EmitRun {
    let t = Instant::now();
    let mut cmd = Command::new(emitter);
    cmd.arg("fake");
    for k in [
        "CHALDEA_HOOK_URL",
        "CHALDEA_HOOK_TOKEN",
        "CHALDEA_THREAD_ID",
        "CHALDEA_HOOK_LOG",
    ] {
        cmd.env_remove(k);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let Ok(mut child) = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return EmitRun {
            exit: None,
            stdout: Vec::new(),
            ms: 0,
        };
    };
    let mut si = child.stdin.take();
    let writer = stdin.map(|data| {
        let mut s = si.take();
        std::thread::spawn(move || {
            if let Some(s) = s.as_mut() {
                let _ = s.write_all(&data);
                if keep_open {
                    std::thread::sleep(Duration::from_secs(6));
                }
            }
        })
    });
    let mut so = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(o) = so.as_mut() {
            let _ = std::io::Read::read_to_end(o, &mut v);
        }
        v
    });
    let end = Instant::now() + Duration::from_secs(8);
    let exit = loop {
        if let Ok(Some(st)) = child.try_wait() {
            break st.code();
        }
        if Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let ms = t.elapsed().as_millis();
    drop(si);
    let stdout = reader.join().unwrap_or_default();
    drop(writer);
    EmitRun { exit, stdout, ms }
}

/// Accepts connections on 127.0.0.1 and holds them without answering for
/// `hold`; the thread ends by itself. Returns the port.
fn slow_listener(hold: Duration) -> std::io::Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    l.set_nonblocking(true)?;
    let port = l.local_addr()?.port();
    std::thread::spawn(move || {
        let end = Instant::now() + hold;
        let mut held = Vec::new();
        while Instant::now() < end {
            if let Ok((s, _)) = l.accept() {
                held.push(s);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    Ok(port)
}

/// (name, env, stdin, keep stdin open, max exit time in ms)
type EmitCase<'a> = (String, Vec<(&'a str, String)>, Option<Vec<u8>>, bool, u128);

fn emit(ctx: &Ctx) {
    let fixtures = ctx.out.join("fixtures-emit");
    let token = util::token();
    let mut r = match Receiver::start(Config {
        token: token.clone(),
        thread_id: "thread-emit".into(),
        agent: "fake".into(),
        cli_version: "emit".into(),
        launch_id: "emit".into(),
        rule: BindRule::SessionId("sid-emit".into()),
        fixtures_dir: fixtures,
        run_id: ctx.run_id.clone(),
        cli_names: vec![],
    }) {
        Ok(r) => r,
        Err(e) => {
            ctx.error(format!("receiver: {e}"));
            return;
        }
    };
    let em = emitter_path(&ctx.args.root);
    let p = r.port();
    let url = r.url();
    let payload = br#"{"hook_event_name":"SessionStart","session_id":"sid-emit"}"#.to_vec();
    let seen = |r: &Receiver| {
        r.events().len() + r.rejections().values().map(|v| *v as usize).sum::<usize>()
    };
    let mut cases: Vec<EmitCase> = vec![
        (
            "url_missing_stdin_open".into(),
            vec![("CHALDEA_HOOK_TOKEN", token.clone())],
            Some(payload.clone()),
            true,
            1000,
        ),
        (
            "token_missing_stdin_open".into(),
            vec![("CHALDEA_HOOK_URL", url.clone())],
            Some(payload.clone()),
            true,
            1000,
        ),
    ];
    for bad in [
        format!("http://127.0.0.1:{p}/hook/"),
        format!("https://127.0.0.1:{p}/hook"),
        format!("http://127.0.0.2:{p}/hook"),
        format!("http://localhost:{p}/hooks"),
        format!("http://user@127.0.0.1:{p}/hook"),
        format!("http://[::1]:{p}/hook"),
        format!("http://127.0.0.1:0{p}/hook"),
        format!("http://127.0.0.1:{p}/hook?x=1"),
        format!("http://example.com:{p}/hook"),
    ] {
        cases.push((
            format!("url_not_exact:{bad}"),
            vec![
                ("CHALDEA_HOOK_URL", bad),
                ("CHALDEA_HOOK_TOKEN", token.clone()),
            ],
            Some(payload.clone()),
            true,
            1000,
        ));
    }
    let valid = vec![
        ("CHALDEA_HOOK_URL", url.clone()),
        ("CHALDEA_HOOK_TOKEN", token.clone()),
        ("CHALDEA_THREAD_ID", "thread-emit".to_string()),
    ];
    cases.push((
        "stdin_over_4mib".into(),
        valid.clone(),
        Some(vec![b'a'; crate::emitter::MAX_STDIN + 1]),
        false,
        3500,
    ));
    cases.push((
        "stdin_open_past_3s".into(),
        valid.clone(),
        Some(b"{\"partial\":".to_vec()),
        true,
        3500,
    ));
    // A listener that accepts and never answers: the 3 s budget still holds.
    let slow = match slow_listener(Duration::from_secs(8)) {
        Ok(p) => p,
        Err(e) => {
            ctx.check(
                "B-9",
                "slow_post",
                Outcome::harness("slow listener setup failed"),
                json!({"setup_error": e.to_string()}),
            );
            0
        }
    };
    if slow != 0 {
        cases.push((
            "slow_post".into(),
            vec![
                ("CHALDEA_HOOK_URL", format!("http://127.0.0.1:{slow}/hook")),
                ("CHALDEA_HOOK_TOKEN", token.clone()),
                ("CHALDEA_THREAD_ID", "thread-emit".to_string()),
            ],
            Some(payload.clone()),
            false,
            3500,
        ));
    }
    for c in &cases {
        ctx.require("B-9", &c.0);
    }
    ctx.require("B-9", "positive_control");
    for (name, env, stdin, keep_open, max_ms) in cases {
        let before = seen(&r);
        let run = run_emitter(&em, &env, stdin, keep_open);
        std::thread::sleep(Duration::from_millis(50));
        let quiet = seen(&r) == before;
        let ok = run.exit == Some(0) && run.stdout.is_empty() && quiet && run.ms <= max_ms;
        ctx.check(
            "B-9",
            &name,
            checked(ok),
            json!({"exit": run.exit, "stdout_len": run.stdout.len(), "receiver_saw": !quiet, "ms": run.ms, "max_ms": max_ms}),
        );
    }
    // Positive control: a valid run is delivered, still quiet on stdout.
    let before = r.events().len();
    let run = run_emitter(&em, &valid, Some(payload), false);
    std::thread::sleep(Duration::from_millis(50));
    let delivered = r.events().len() == before + 1;
    ctx.check(
        "B-9",
        "positive_control",
        checked(run.exit == Some(0) && run.stdout.is_empty() && delivered),
        json!({"exit": run.exit, "stdout_len": run.stdout.len(), "delivered": delivered, "ms": run.ms}),
    );
    r.stop();
}

// ---------------- B-11 binding ----------------

fn nested(ctx: &Ctx) {
    let sid = uuid::Uuid::new_v4().to_string();
    let l = match launch(
        ctx,
        fake_spec(
            ctx,
            "echo-nested",
            "echo",
            &["--nested", "--session-id", &sid],
        ),
    ) {
        Ok(l) => l,
        Err(e) => {
            ctx.check(
                "B-11",
                "nested",
                Outcome::harness("launch failed"),
                json!({"launch_error": e}),
            );
            return;
        }
    };
    let bound = l
        .receiver
        .wait_bound(ctx.until(Duration::from_secs(ctx.args.session_start_timeout)));
    // Wait for the nested child's second SessionStart (3 SessionStarts total).
    let end = ctx.until(Duration::from_secs(8));
    while l
        .receiver
        .events()
        .iter()
        .filter(|e| e.event == "SessionStart")
        .count()
        < 3
        && Instant::now() < end
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    let events = l.receiver.events();
    let out_dir = ctx.out.clone();
    let (stop, _) = l.finish();
    let starts: Vec<_> = events
        .iter()
        .filter(|e| e.event == "SessionStart")
        .collect();
    let real: Vec<_> = starts
        .iter()
        .filter(|e| e.session_id.as_deref() == Some(sid.as_str()))
        .collect();
    let foreign: Vec<_> = starts
        .iter()
        .filter(|e| e.session_id.as_deref() != Some(sid.as_str()))
        .collect();
    let real_seq = real.first().map(|e| e.seq);
    let order_ok = real_seq
        .is_some_and(|rs| foreign.iter().any(|f| f.seq < rs) && foreign.iter().any(|f| f.seq > rs));
    ctx.check(
        "B-11",
        "nested_session_start_binds_real_one",
        checked(bound.as_deref() == Some(sid.as_str()) && real.iter().all(|e| e.is_bound()) && order_ok),
        json!({"bound": bound, "expected": sid, "foreign_before_and_after": order_ok,
               "session_starts": starts.iter().map(|e| json!({"seq": e.seq, "class": e.class, "reason": e.reason, "nearest_cli": e.nearest_cli})).collect::<Vec<_>>()}),
    );
    ctx.check(
        "B-11",
        "nested_kept_as_foreign_session",
        checked(
            foreign.len() >= 2
                && foreign.iter().all(|e| {
                    e.class == "raw_unknown" && e.reason.as_deref() == Some("foreign_session")
                }),
        ),
        json!({"foreign": foreign.len()}),
    );
    let transcript_null = real
        .first()
        .and_then(|e| e.json())
        .map(|v| v.get("transcript_path") == Some(&Value::Null));
    ctx.check(
        "B-11",
        "codex_shaped_transcript_path_null_stored",
        checked(transcript_null == Some(true)),
        json!({"transcript_path_null": transcript_null}),
    );
    let unknown = events.iter().find(|e| e.event == "unknown");
    let verbatim = unknown
        .and_then(|e| std::fs::read(out_dir.join(&e.fixture)).ok())
        .map(|b| b == fake_cli::UNKNOWN_PAYLOAD.as_bytes());
    ctx.check(
        "B-11",
        "unknown_payload_verbatim",
        checked(verbatim == Some(true) && unknown.is_some_and(|e| e.class == "raw_unknown")),
        json!({"stored_verbatim": verbatim, "class": unknown.map(|e| e.class.clone())}),
    );
    ctx.detail("nested_stop", stop_evidence(&stop));
}

// ---------------- B-10 teardown ----------------

pub const TEARDOWN_CASES: &[(&str, &str, u64)] = &[
    ("pass", "pass", 40),
    ("fail", "fail", 40),
    ("panic", "error", 40),
    ("ctrl-c", "error", 40),
    ("ctrl-c-during-stop", "error", 40),
    ("timeout", "timeout", 14),
    ("hard-deadline", "timeout", 14),
    ("setup-fail", "error", 40),
    ("non-reading-child", "pass", 40),
    ("query-mid-paste", "pass", 40),
];

fn teardown(ctx: &Ctx) {
    for (case, _, _) in TEARDOWN_CASES {
        ctx.require("B-10", case);
    }
    for (case, want, deadline) in TEARDOWN_CASES {
        if ctx.past_cutoff() {
            ctx.check(
                "B-10",
                case,
                Outcome::not_exercised("scenario cutoff"),
                json!({"skipped": "scenario cutoff"}),
            );
            continue;
        }
        let (out, run_id) = match child_out(&ctx.out, &format!("teardown-{case}")) {
            Ok(v) => v,
            Err(e) => {
                ctx.check(
                    "B-10",
                    case,
                    Outcome::harness(format!("not_run: {e}")),
                    json!({}),
                );
                continue;
            }
        };
        let mut cmd = Command::new(std::env::current_exe().unwrap_or_default());
        cmd.args([
            "fake",
            "teardown-case",
            "--case",
            case,
            "--deadline-secs",
            &deadline.to_string(),
            "--run-id",
            &run_id,
            "--out",
        ])
        .arg(&out)
        .args(cli::forward_common(&ctx.args));
        os::interruptible(&mut cmd);
        let t = Instant::now();
        let Ok(mut child) = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            ctx.check(
                "B-10",
                case,
                Outcome::harness("teardown case spawn failed"),
                json!({"spawn": "failed"}),
            );
            continue;
        };
        let hard = t + Duration::from_secs(deadline + 5);
        let mut interrupted = false;
        let code = loop {
            let marker = if *case == "ctrl-c-during-stop" {
                "stopping"
            } else {
                "ready"
            };
            if case.starts_with("ctrl-c") && !interrupted && out.join(marker).exists() {
                interrupted = os::send_interrupt(child.id()).is_ok();
            }
            if let Ok(Some(st)) = child.try_wait() {
                break st.code();
            }
            if Instant::now() >= hard {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let elapsed = t.elapsed();
        let rep = match child_report(&out, &run_id) {
            Ok(r) => r,
            Err(e) => {
                ctx.check(
                    "B-10",
                    case,
                    Outcome::harness(e),
                    json!({"exit": code, "elapsed_ms": elapsed.as_millis() as u64}),
                );
                continue;
            }
        };
        let rep = Some(rep);
        let (survivors, owned) = rep.as_ref().map(case_survivors).unwrap_or_default();
        let status = rep.as_ref().map(|r| r.status.clone());
        let within = elapsed <= Duration::from_secs(*deadline) + Duration::from_millis(1500);
        let ok = status.as_deref() == Some(*want) && survivors.is_empty() && within && owned > 0;
        ctx.check(
            "B-10",
            case,
            checked(ok),
            json!({"want_status": want, "status": status, "exit": code, "elapsed_ms": elapsed.as_millis() as u64,
                   "deadline_s": deadline, "owned_checked": owned, "survivors": survivors, "interrupt_sent": interrupted,
                   "errors": rep.as_ref().map(|r| r.errors.clone()), "case_details": rep.as_ref().map(|r| r.details.get("case").cloned())}),
        );
    }
}

/// Every owned pid and session witness the case recorded, still alive now.
fn case_survivors(r: &Report) -> (Vec<Value>, usize) {
    let mut pids: Vec<(u32, u64)> = Vec::new();
    let d = &r.details;
    for key in ["/owned", "/witnesses"] {
        if let Some(arr) = d.pointer(key).and_then(Value::as_array) {
            for p in arr {
                if p.get("session").and_then(Value::as_bool) == Some(false) {
                    continue;
                }
                if let (Some(pid), Some(st)) = (p["pid"].as_u64(), p["start_time"].as_u64()) {
                    pids.push((pid as u32, st));
                }
            }
        }
    }
    if let Some(k) = d.pointer("/watchdog/killed").and_then(Value::as_array) {
        for o in k {
            for p in o["recorded"].as_array().into_iter().flatten() {
                if let (Some(pid), Some(st)) = (p["pid"].as_u64(), p["start_time"].as_u64()) {
                    pids.push((pid as u32, st));
                }
            }
        }
    }
    pids.sort();
    pids.dedup();
    let n = pids.len();
    let survivors = pids
        .into_iter()
        .filter(|(p, s)| os::is_alive(*p, *s))
        .map(|(p, s)| json!({"pid": p, "start_time": s}))
        .collect();
    (survivors, n)
}

/// Child side of B-10: one case per process.
fn run_case(args: Args) -> i32 {
    let case = args.case.clone().unwrap_or_default();
    let ctx = match Ctx::new(args, &format!("fake teardown-case {case}")) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("chs: {e}");
            return 3;
        }
    };
    let ready = |ctx: &Ctx| {
        let _ = std::fs::write(ctx.out.join("ready"), b"1");
    };
    let record = |ctx: &Ctx, l: &Launch| {
        l.session.owned.record_scan();
        let owned: Vec<Value> = l
            .session
            .owned
            .recorded()
            .iter()
            .map(|p| json!(p))
            .collect();
        ctx.detail("owned", json!(owned));
        ctx.detail("witnesses", json!(l.receiver.witnesses()));
        let _ = ctx.sink.write();
    };
    match case.as_str() {
        "non-reading-child" | "query-mid-paste" => {
            let extra: &[&str] = if case == "non-reading-child" {
                &["--ignore-input"]
            } else {
                &["--query-mid-paste"]
            };
            let l = match launch(&ctx, fake_spec(&ctx, &case, "echo", extra)) {
                Ok(l) => l,
                Err(e) => {
                    ctx.error(e);
                    return ctx.finish();
                }
            };
            let gate = l.wait_gate(ctx.until(Duration::from_secs(ctx.args.session_start_timeout)));
            let _ = l
                .receiver
                .wait_witness(ctx.until(Duration::from_secs(3)), |w| w.role == "echo");
            record(&ctx, &l);
            let payload = paste::corpus()
                .into_iter()
                .find(|e| e.id == "size_64k")
                .map(|e| e.text)
                .unwrap_or_default();
            let t = Instant::now();
            let o = paste::paste(
                &l.session.io().tx,
                payload.as_bytes(),
                &PasteOpts {
                    mode: Newline::Cr,
                    chunk: 0,
                    enter_delay: Duration::from_millis(ctx.args.enter_delay_ms),
                    write_deadline: ctx.until(Duration::from_secs(3)),
                },
                || l.ready(),
            );
            let paste_ms = t.elapsed().as_millis() as u64;
            let ups = if case == "query-mid-paste" {
                wait_ups(&l, 1, ctx.until(FAKE_UPS_WAIT))
            } else {
                None
            };
            record(&ctx, &l);
            let (stop, _) = l.finish();
            let ok = if case == "non-reading-child" {
                gate && o.state == "write_timeout"
            } else {
                let expected = paste::map_newlines(payload.as_bytes(), Newline::Cr);
                ups.as_ref().is_some_and(|(p, v)| {
                    *p == expected && v["chs_query_reply_after_paste"] == json!(true)
                })
            };
            ctx.check(
                "B-10",
                &case,
                checked(ok && stop.survivors.is_empty()),
                json!({"paste": o, "paste_ms": paste_ms, "bytes_written": o.bytes_after - o.bytes_before,
                       "ups_reply_after_paste": ups.as_ref().map(|(_, v)| v["chs_query_reply_after_paste"].clone()),
                       "stop": stop_evidence(&stop)}),
            );
            ctx.detail(
                "case",
                json!({"teardown_ms": stop.teardown_ms, "threads": stop.threads}),
            );
        }
        _ => {
            let mut spec = fake_spec(
                &ctx,
                &case,
                "spawn-tree",
                &["--max-life-secs", SLEEPER_MAX_LIFE],
            );
            if case == "setup-fail" {
                spec.inject = Some(Inject::SetupFail);
            }
            if case == "setup-fail" {
                // The guard must kill the child spawned before the injected failure.
                let res = launch(&ctx, spec);
                let spawned: Vec<Value> = crate::session::spawned_children()
                    .iter()
                    .map(|(p, s)| json!({"pid": p, "start_time": s}))
                    .collect();
                ctx.detail("owned", json!(spawned));
                ctx.error(match res {
                    Err(e) => e,
                    Ok(_) => "injection did not fail".into(),
                });
                return ctx.finish();
            }
            let l = match launch(&ctx, spec) {
                Ok(l) => l,
                Err(e) => {
                    ctx.error(e);
                    return ctx.finish();
                }
            };
            let mut want: Vec<&str> = os::spawn_tree_roles().to_vec();
            want.push("spawn-tree");
            let _ = l
                .receiver
                .wait_witnesses(ctx.until(Duration::from_secs(5)), |ws| {
                    want.iter().all(|r| ws.iter().any(|w| w.role == *r))
                });
            record(&ctx, &l);
            ready(&ctx);
            match case.as_str() {
                "panic" => panic!("injected panic (B-10)"),
                "ctrl-c" => loop {
                    // The watchdog handles the interrupt; this wait honors the cutoff only.
                    if ctx.past_cutoff() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                },
                "timeout" => {
                    let b = l.receiver.wait_bound(ctx.until(Duration::from_secs(3600)));
                    if b.is_none() {
                        ctx.sink.with(|r| r.status = "timeout".into());
                    }
                }
                "hard-deadline" => loop {
                    // Ignores the cutoff on purpose; only the watchdog can end this.
                    std::thread::sleep(Duration::from_millis(100));
                },
                "ctrl-c-during-stop" => {
                    // Teardown has started but not killed yet when Ctrl-C lands:
                    // the watchdog must still kill the tree.
                    // The marker is written from inside teardown, so the
                    // interrupt always lands after teardown has begun.
                    let marker = ctx.out.join("stopping");
                    *l.session
                        .owned
                        .before_kill
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = Some(Box::new(move || {
                        let _ = std::fs::write(&marker, b"1");
                        std::thread::sleep(Duration::from_secs(5));
                    }));
                }
                _ => {}
            }
            let (stop, _) = l.finish();
            ctx.detail(
                "case",
                json!({"teardown_ms": stop.teardown_ms, "threads": stop.threads}),
            );
            match case.as_str() {
                "fail" => ctx.check(
                    "B-10",
                    "injected_failure",
                    Outcome::platform("injected failure (B-10)"),
                    json!({}),
                ),
                "timeout" => {}
                _ => ctx.check(
                    "B-10",
                    &case,
                    checked(stop.survivors.is_empty()),
                    stop_evidence(&stop),
                ),
            }
        }
    }
    ctx.finish()
}

// ---------------- all ----------------

fn run_all(mut args: Args) -> i32 {
    if !args.explicit_deadline {
        args.deadline_secs = 1500;
    }
    let ctx = match Ctx::new(args, "fake all") {
        Ok(c) => c,
        Err(e) => {
            eprintln!("chs: {e}");
            return 3;
        }
    };
    ctx.require("B-1", "binaries_built");
    ctx.require("B-2", "all_fake_scenarios_pass");
    let bins: Vec<(String, bool)> = ["chs", "chs-hook-emit", "chs-fake-cli"]
        .iter()
        .map(|b| (b.to_string(), util::sibling_bin(b).is_file()))
        .collect();
    ctx.check(
        "B-1",
        "binaries_built",
        checked(bins.iter().all(|b| b.1)),
        json!({"bins": bins, "exe_dir": util::exe_dir(), "os": os::os_name(), "arch": std::env::consts::ARCH,
               "emitter_used": emitter_path(&ctx.args.root)}),
    );
    let mut child_status = Vec::new();
    for s in SCENARIOS {
        let (out, run_id) = match child_out(&ctx.out, s) {
            Ok(v) => v,
            Err(e) => {
                ctx.error(format!("{s}: not_run: {e}"));
                child_status.push(json!({"scenario": s, "status": "not_run"}));
                continue;
            }
        };
        let mut cmd = Command::new(std::env::current_exe().unwrap_or_default());
        cmd.args(["fake", s, "--out"])
            .arg(&out)
            .args(["--run-id", &run_id])
            .args(cli::forward_common(&ctx.args));
        if *s == "stall" {
            cmd.args(["--session-start-timeout", "5"]);
        }
        if ctx.args.hold_assign && *s == "tree-kill" {
            cmd.arg("--hold-assign-until-child");
        }
        let t = Instant::now();
        let r = super::run_bounded(
            &mut cmd,
            Duration::from_secs(ctx.args.deadline_secs.min(600)),
        );
        let rep = child_report(&out, &run_id);
        let status = rep
            .as_ref()
            .map(|r| r.status.clone())
            .unwrap_or_else(|_| "not_run".into());
        println!("  fake {s}: {status} ({} ms)", t.elapsed().as_millis());
        ctx.sink.with(|agg| {
            if let Ok(rep) = &rep {
                for c in &rep.checks {
                    agg.checks.push(crate::report::Check {
                        case: format!("{s}/{}", c.case),
                        ..c.clone()
                    });
                }
                for e in &rep.errors {
                    agg.errors.push(format!("{s}: {e}"));
                }
            } else if let Err(e) = &rep {
                agg.errors.push(format!("{s}: {e} ({r:?})"));
            }
        });
        child_status
            .push(json!({"scenario": s, "status": status, "report": out.join("report.json")}));
    }
    let all_pass = child_status.iter().all(|c| c["status"] == "pass");
    ctx.check(
        "B-2",
        "all_fake_scenarios_pass",
        checked(all_pass),
        json!({"children": child_status}),
    );
    ctx.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// r3 C-3/B-7: unknown liveness or a failed assignment is never platform
    /// evidence, and a witness that exited before the stop does not count.
    #[test]
    fn b7_liveness_is_positive_evidence_only() {
        assert_eq!(killed_outcome(true, Some(true), Some(false)), Outcome::Pass);
        assert!(matches!(
            killed_outcome(true, Some(true), Some(true)),
            Outcome::PlatformFail { .. }
        ));
        for (a, b, c) in [
            (true, Some(true), None),
            (true, None, Some(false)),
            (false, Some(true), Some(true)),
        ] {
            assert!(
                matches!(killed_outcome(a, b, c), Outcome::HarnessFail { .. }),
                "{a} {b:?} {c:?}"
            );
        }
        assert!(matches!(
            killed_outcome(true, Some(false), Some(false)),
            Outcome::NotExercised { .. }
        ));
    }

    /// The B-3 oracle against hand-written wire bytes (not the writer's mapper).
    #[test]
    fn b3_oracle_hand_written() {
        let t = "日\nb\r\nc\rd";
        assert_eq!(expected_wire(t, Newline::Cr), b"\xe6\x97\xa5\rb\rc\rd");
        assert_eq!(expected_wire(t, Newline::Lf), b"\xe6\x97\xa5\nb\nc\rd");
        assert_eq!(
            expected_wire(t, Newline::Crlf),
            b"\xe6\x97\xa5\r\nb\r\nc\rd"
        );
        for e in paste::corpus() {
            for m in Newline::ALL {
                assert_eq!(
                    expected_wire(&e.text, m),
                    paste::map_newlines(e.text.as_bytes(), m),
                    "{} {}",
                    e.id,
                    m.name()
                );
            }
        }
    }
}
