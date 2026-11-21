//! Set B: `chs fake <paste|queries|stall|tree-kill|auth|emit|teardown|all>`, plus
//! `chs fake raw-inject` (F-2c, Windows raw backend only).

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
        "raw-inject" => {
            let ctx = match Ctx::new(args, "fake raw-inject") {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("chs: {e}");
                    return 3;
                }
            };
            raw_inject(&ctx);
            ctx.finish()
        }
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
        responder: true,
        hold_assign: false,
        inject: None,
        before_spawn: None,
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
        "launch": stop.launch,
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
    let v = l
        .wait_nth_bound("UserPromptSubmit", n, end, Duration::from_millis(10))?
        .json()?;
    let prompt = v
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or("")
        .as_bytes()
        .to_vec();
    Some((prompt, v))
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
                let o = l.submit(ctx, &e.text, *mode, *chunk);
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
        let _ = l.finish();
        let output = std::fs::read(ctx.out.join(format!("pty-output-{label}.bin"))).ok();
        runs.push((
            responder,
            B5Run {
                exit: code,
                fake: fake_report,
                log: log.as_array().cloned().unwrap_or_default(),
                output,
            },
        ));
    }
    for (responder, run) in &runs {
        let case = if *responder {
            "responder"
        } else {
            "negative_control_no_responder"
        };
        let (o, ev) = b5_expect(os::os_name(), *responder, run);
        ctx.check("B-5", case, o, ev);
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

/// One B-8 request. With `withhold`, only the head is sent and the body
/// follows after the response was read: a status read first shows the
/// decision came before any body byte. With `body_up_front`, the body is
/// written from another thread while the response is read (drain case).
fn b8_request(
    port: u16,
    bytes: &[u8],
    withhold: bool,
    body_up_front: Option<Vec<u8>>,
) -> (u16, u128) {
    let t = Instant::now();
    let Ok(mut s) = TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(1),
    ) else {
        return (0, 0);
    };
    let split = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .filter(|p| withhold && *p < bytes.len());
    let (head, rest) = bytes.split_at(split.unwrap_or(bytes.len()));
    let _ = s.write_all(head);
    let writer = body_up_front.and_then(|b| {
        let mut w = s.try_clone().ok()?;
        Some(std::thread::spawn(move || {
            let _ = w.write_all(&b);
        }))
    });
    let code = crate::http_client::read_response(&mut s, Instant::now() + Duration::from_secs(8))
        .map(|r| r.0)
        .unwrap_or(0);
    let ms = t.elapsed().as_millis();
    if !rest.is_empty() {
        let _ = s.write_all(rest);
    }
    drop(s);
    if let Some(w) = writer {
        let _ = w.join();
    }
    (code, ms)
}

/// The receiver's records of connections after the first `before`, waiting
/// (bounded) until `n` new ones exist.
fn new_conns(r: &Receiver, before: usize, n: usize) -> Vec<receiver::ConnRecord> {
    let end = Instant::now() + Duration::from_secs(9);
    while r.connections().len() < before + n && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
    }
    r.connections().into_iter().skip(before).collect()
}

/// F-3c: decided, written, and client-observed status all equal `want`.
fn statuses_agree(got: u16, want: u16, rec: Option<&receiver::ConnRecord>) -> bool {
    got == want && rec.is_some_and(|c| c.decided == Some(want) && c.write_ok == Some(true))
}

fn conn_evidence(rec: Option<&receiver::ConnRecord>) -> Value {
    rec.map_or(json!(null), |c| {
        json!({"decided": c.decided, "bytes_written": c.bytes_written, "write_ok": c.write_ok,
               "write_error": c.write_error, "drained": c.drained, "drain_stop": c.drain_stop,
               "total_ms": c.total_ms})
    })
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
        "reject_with_body_sent",
        "deadline_slow_head",
        "deadline_slow_body",
        "no_fixture_no_readiness",
        "positive_control",
    ] {
        ctx.require("B-8", name);
    }
    let before_files = count_files(&fixtures);
    for (name, bytes, want) in cases {
        let n0 = r.connections().len();
        let (got, ms) = b8_request(p, &bytes, true, None);
        let rec = new_conns(&r, n0, 1).pop();
        let ok = statuses_agree(got, want, rec.as_ref())
            && count_files(&fixtures) == before_files
            && r.bound().is_none();
        ctx.check(
            "B-8",
            name,
            checked(ok),
            json!({"status": got, "want": want, "ms": ms, "receiver": conn_evidence(rec.as_ref()),
                   "body_withheld_until_response": true}),
        );
    }
    // The body sent up front (1 MiB): the receiver drains it after the 401.
    let n0 = r.connections().len();
    let (got, ms) = b8_request(
        p,
        format!(
            "POST /hook HTTP/1.1\r\n{h}Authorization: Bearer {}\r\nContent-Length: {}\r\n\r\n",
            util::token(),
            1 << 20
        )
        .as_bytes(),
        false,
        Some(vec![b'x'; 1 << 20]),
    );
    let rec = new_conns(&r, n0, 1).pop();
    ctx.check(
        "B-8",
        "reject_with_body_sent",
        checked(statuses_agree(got, 401, rec.as_ref()) && count_files(&fixtures) == before_files),
        json!({"status": got, "want": 401, "ms": ms, "receiver": conn_evidence(rec.as_ref())}),
    );
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
    let n0 = r.connections().len();
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
    let joined: Vec<_> = handles.into_iter().filter_map(|h| h.join().ok()).collect();
    let recs = new_conns(&r, n0, 2);
    for (name, code, ms) in joined {
        // The stalled head is never routed; the stalled body reached `/hook`.
        let route = if name == "deadline_slow_head" {
            "unrouted"
        } else {
            "hook"
        };
        let rec = recs.iter().find(|c| c.route == route);
        let ok = statuses_agree(code, 408, rec)
            && (4500..=6500).contains(&ms)
            && count_files(&fixtures) == before_files;
        ctx.check(
            "B-8",
            name,
            checked(ok),
            json!({"status": code, "elapsed_ms": ms, "deadline_ms": 5000,
                   "receiver": conn_evidence(rec)}),
        );
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
        let (survivors, owned) = case_survivors(&rep);
        let within = elapsed <= Duration::from_secs(*deadline) + Duration::from_millis(1500);
        let o = teardown_outcome(case, want, &rep, within, survivors.is_empty(), owned);
        let status = Some(rep.status.clone());
        let rep = Some(rep);
        ctx.check(
            "B-10",
            case,
            o,
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
            // Windows: conhost may accept the whole paste; keep writing
            // ungated filler until the writer blocks or 16 MiB went in.
            let filler =
                (case == "non-reading-child" && os::os_name() == "windows" && o.state == "sent")
                    .then(|| write_filler(&l.session.io().tx, &ctx));
            let ups = if case == "query-mid-paste" {
                wait_ups(&l, 1, ctx.until(FAKE_UPS_WAIT))
            } else {
                None
            };
            let log = l.query_log();
            record(&ctx, &l);
            let (stop, _) = l.finish();
            let harness_dsr_written = log
                .as_array()
                .into_iter()
                .flatten()
                .any(|e| e["kind"] == json!("dsr6") && e["harness_wrote"] == json!(true));
            let run = B10Run {
                gate,
                paste_state: o.state.clone(),
                filler,
                survivors_empty: stop.survivors.is_empty(),
                prompt: ups.as_ref().map(|(p, _)| p.clone()),
                expected: paste::map_newlines(payload.as_bytes(), Newline::Cr),
                reply_after_paste: ups
                    .as_ref()
                    .and_then(|(_, v)| v["chs_query_reply_after_paste"].as_bool()),
                harness_dsr_written,
            };
            let (outcome, mut ev) = b10_expect(os::os_name(), &case, &run);
            let output =
                std::fs::read(ctx.out.join(format!("pty-output-{case}.bin"))).unwrap_or_default();
            ev["query_log"] = log;
            ev["dsr_excerpt"] =
                json!(find_bytes(&output, b"\x1b[6n").map(|at| excerpt(&output, at, 4)));
            ev["paste"] = json!(o);
            ev["paste_ms"] = json!(paste_ms);
            ev["bytes_written"] = json!(o.bytes_after - o.bytes_before);
            ev["stop"] = stop_evidence(&stop);
            ctx.check("B-10", &case, outcome, ev);
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

// ---------------- conhost expectations (F-3e) ----------------

/// The queries FakeCli `queries` sends, as they appear in the PTY output.
const B5_QUERIES: &[(&str, &[u8])] = &[
    ("dsr6", b"\x1b[6n"),
    ("dsr5", b"\x1b[5n"),
    ("da1", b"\x1b[c"),
    ("da2", b"\x1b[>c"),
    ("osc10", b"\x1b]10;?"),
    ("osc11", b"\x1b]11;?"),
    ("kitty_keyboard", b"\x1b[?u"),
];

/// Bytes of PTY output kept around each query in the report.
const EXCERPT_MAX: usize = 256;

fn find_bytes(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

/// At most `EXCERPT_MAX` bytes of `out` around `[at, at + len)`, escaped.
pub fn excerpt(out: &[u8], at: usize, len: usize) -> String {
    let side = (EXCERPT_MAX.saturating_sub(len)) / 2;
    let start = at.saturating_sub(side);
    let end = (at + len + side).min(out.len()).min(start + EXCERPT_MAX);
    String::from_utf8_lossy(&out[start..end])
        .escape_debug()
        .to_string()
}

/// Per query: its first offset in the PTY output and an excerpt (null when
/// the query never reached the PTY output).
fn query_excerpts(out: &[u8]) -> Value {
    let m: serde_json::Map<String, Value> = B5_QUERIES
        .iter()
        .map(|(k, q)| {
            let v = find_bytes(out, q)
                .map(|at| json!({"offset": at, "excerpt": excerpt(out, at, q.len())}));
            (k.to_string(), v.unwrap_or(Value::Null))
        })
        .collect();
    Value::Object(m)
}

/// One FakeCli `queries` run as B-5 sees it.
pub struct B5Run {
    pub exit: Option<u32>,
    /// The `queries-result` witness report.
    pub fake: Option<Value>,
    pub log: Vec<Value>,
    /// The PTY output file, when it could be read.
    pub output: Option<Vec<u8>>,
}

/// B-5 expectation per OS. macOS: today's checks. Windows (§3 conhost
/// facts): DSR 5/6 and DA1/DA2 pass when answered validly within 500 ms
/// with no harness write (`answered_by_conhost`); OSC 10/11 pass when no
/// reply arrived, the query log has no OSC entry, and the PTY output has no
/// OSC 10/11 query (`not_forwarded`); kitty must be harness-answered, and
/// the negative control must fail on kitty. Missing evidence is a harness
/// failure.
pub fn b5_expect(os_name: &str, responder: bool, r: &B5Run) -> (Outcome, Value) {
    let mut ev = json!({"exit": r.exit, "fake": r.fake, "query_log": r.log,
                        "excerpts": r.output.as_deref().map(query_excerpts)});
    let all_ok = r.fake.as_ref().and_then(|f| f["all_ok"].as_bool()) == Some(true);
    if os_name != "windows" {
        let o = if responder {
            checked(r.exit == Some(0) && all_ok)
        } else {
            Outcome::observed(r.exit != Some(0), "queries answered without the responder")
        };
        return (o, ev);
    }
    let Some(fake) = &r.fake else {
        return (Outcome::harness("no queries-result report"), ev);
    };
    let Some(out) = &r.output else {
        return (Outcome::harness("no PTY output recorded"), ev);
    };
    let result = |k: &str| {
        fake["results"]
            .as_array()
            .and_then(|a| a.iter().find(|x| x["kind"] == json!(k)))
            .map(|x| x["ok"] == json!(true))
    };
    let wrote = |k: &str| {
        r.log
            .iter()
            .any(|e| e["kind"] == json!(k) && e["harness_wrote"] == json!(true))
    };
    let osc_logged = r.log.iter().any(|e| {
        e["kind"].as_str().is_some_and(|k| k.starts_with("osc"))
            || e["reply"].as_str().is_some_and(|s| s.starts_with("\x1b]1"))
    });
    if !responder {
        return match result("kitty_keyboard") {
            None => (Outcome::harness("no kitty_keyboard result"), ev),
            Some(true) => (
                Outcome::platform("kitty query answered without the responder"),
                ev,
            ),
            Some(false) => (Outcome::Pass, ev),
        };
    }
    let (mut conhost, mut not_fwd, mut fails) = (Vec::new(), Vec::new(), Vec::new());
    for (k, q) in B5_QUERIES {
        let Some(ok) = result(k) else {
            return (Outcome::harness(format!("no {k} result")), ev);
        };
        match *k {
            "kitty_keyboard" => {
                if !(ok && wrote(k)) {
                    fails.push(*k);
                }
            }
            "osc10" | "osc11" => {
                if !ok {
                    if !osc_logged && find_bytes(out, q).is_none() {
                        not_fwd.push(*k);
                    } else {
                        fails.push(*k);
                    }
                }
            }
            _ => {
                if !ok {
                    fails.push(*k);
                } else if !wrote(k) {
                    conhost.push(*k);
                }
            }
        }
    }
    ev["platform_fact"] = json!({"answered_by_conhost": conhost, "not_forwarded": not_fwd});
    if fails.is_empty() {
        (Outcome::Pass, ev)
    } else {
        (
            Outcome::platform(format!("unexpected query handling: {fails:?}")),
            ev,
        )
    }
}

/// Ungated filler after a 64 KiB paste to a non-reading child (Windows).
pub const FILLER_MAX: u64 = 16 << 20;
const FILLER_WRITE: usize = 64 * 1024;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Filler {
    pub accepted: u64,
    pub blocked: bool,
    pub error: Option<String>,
}

fn write_filler(tx: &crate::pty::WriterTx, ctx: &Ctx) -> Filler {
    let chunk = vec![b'x'; FILLER_WRITE];
    let mut f = Filler {
        accepted: 0,
        blocked: false,
        error: None,
    };
    while f.accepted < FILLER_MAX {
        match tx.write_wait(vec![chunk.clone()], ctx.until(Duration::from_secs(3))) {
            Ok(()) => f.accepted += FILLER_WRITE as u64,
            Err(e) if e == "write_timeout" => {
                f.blocked = true;
                break;
            }
            Err(e) => {
                f.error = Some(e);
                break;
            }
        }
    }
    f
}

/// Exactly one CPR reply (`ESC[<r>;<c>R`) in `prompt`, whose removal
/// leaves `expected`: its byte offset and text.
pub fn single_cpr(prompt: &[u8], expected: &[u8]) -> Result<(usize, String), String> {
    let mut found = Vec::new();
    let mut i = 0;
    while let Some(p) = find_bytes(&prompt[i..], b"\x1b[") {
        let at = i + p;
        let t = &prompt[at + 2..];
        let d1 = t.iter().take_while(|b| b.is_ascii_digit()).count();
        let rest = &t[d1..];
        let ok = d1 > 0 && rest.first() == Some(&b';') && {
            let d2 = rest[1..].iter().take_while(|b| b.is_ascii_digit()).count();
            d2 > 0 && rest.get(1 + d2) == Some(&b'R')
        };
        if ok {
            let d2 = rest[1..].iter().take_while(|b| b.is_ascii_digit()).count();
            found.push((at, 2 + d1 + 1 + d2 + 1));
        }
        i = at + 2;
    }
    let [(at, len)] = found[..] else {
        return Err(format!("{} CPR replies in the prompt", found.len()));
    };
    let mut rest = prompt[..at].to_vec();
    rest.extend_from_slice(&prompt[at + len..]);
    if rest != expected {
        return Err("the prompt without the CPR reply differs from the payload".into());
    }
    Ok((
        at,
        String::from_utf8_lossy(&prompt[at..at + len]).into_owned(),
    ))
}

/// One B-10 child case (`non-reading-child` or `query-mid-paste`).
pub struct B10Run {
    pub gate: bool,
    pub paste_state: String,
    pub filler: Option<Filler>,
    pub survivors_empty: bool,
    pub prompt: Option<Vec<u8>>,
    pub expected: Vec<u8>,
    pub reply_after_paste: Option<bool>,
    pub harness_dsr_written: bool,
}

/// B-10 expectation per OS. macOS: today's checks. Windows (§3 conhost
/// facts): a paste that conhost accepted is followed by filler up to
/// 16 MiB; a blocked writer passes, all 16 MiB accepted is `not_exercised`
/// with the accepted bytes; a CPR reply must appear in the prompt exactly
/// once with no harness DSR write (`platform_fact` = its offset).
pub fn b10_expect(os_name: &str, case: &str, r: &B10Run) -> (Outcome, Value) {
    let mut ev = json!({"paste_state": r.paste_state, "filler": r.filler,
                        "reply_after_paste": r.reply_after_paste});
    if !r.survivors_empty {
        return (Outcome::platform("processes survived the stop"), ev);
    }
    let win = os_name == "windows";
    if case == "non-reading-child" {
        if !win || r.paste_state != "sent" {
            return (checked(r.gate && r.paste_state == "write_timeout"), ev);
        }
        let o = match &r.filler {
            None => Outcome::harness("no filler run after the accepted paste"),
            Some(f) if f.blocked => {
                ev["platform_fact"] = json!({"paste_accepted_by_conhost": true,
                                             "filler_accepted_bytes": f.accepted});
                checked(r.gate)
            }
            Some(f) if f.accepted >= FILLER_MAX => Outcome::not_exercised(format!(
                "conhost accepted the paste and all {} filler bytes; no blocked writer",
                f.accepted
            )),
            Some(f) => Outcome::harness(format!(
                "filler stopped: {}",
                f.error.as_deref().unwrap_or("unknown")
            )),
        };
        return (o, ev);
    }
    let today = r.prompt.as_deref() == Some(&r.expected[..]) && r.reply_after_paste == Some(true);
    if !win || today {
        return (checked(today), ev);
    }
    let Some(prompt) = &r.prompt else {
        return (checked(false), ev);
    };
    if r.harness_dsr_written {
        return (
            Outcome::platform("the harness wrote a DSR reply during the paste"),
            ev,
        );
    }
    match single_cpr(prompt, &r.expected) {
        Ok((at, reply)) => {
            let chars = String::from_utf8_lossy(&prompt[..at]).chars().count();
            ev["platform_fact"] = json!({"cpr_in_paste_body": reply, "offset_bytes": at,
                "offset_chars": chars, "excerpt": excerpt(prompt, at, reply.len())});
            (Outcome::Pass, ev)
        }
        Err(e) => (Outcome::platform(e), ev),
    }
}

/// B-10 cases whose child decides a typed outcome the parent passes on.
const TYPED_TEARDOWN: &[&str] = &["non-reading-child", "query-mid-paste"];

/// The `fake teardown` parent's verdict for one child case. The parent
/// still checks the child's elapsed time against its deadline, survivors,
/// and that it recorded owned processes (the run id is checked when the
/// report is read); a typed case then carries the child's own outcome
/// unchanged, the others compare the child's status with the expected one.
pub fn teardown_outcome(
    case: &str,
    want: &str,
    rep: &Report,
    within: bool,
    survivors_empty: bool,
    owned: usize,
) -> Outcome {
    let identity_ok = within && survivors_empty && owned > 0;
    if !TYPED_TEARDOWN.contains(&case) {
        return checked(rep.status == want && identity_ok);
    }
    if !identity_ok {
        return checked(false);
    }
    if !rep.errors.is_empty() {
        return Outcome::harness(format!("child errors: {:?}", rep.errors));
    }
    rep.checks
        .iter()
        .rev()
        .find(|c| c.id == "B-10" && c.case == case)
        .map(Outcome::from_check)
        .unwrap_or(Outcome::Missing)
}

/// B-2 over the child scenarios' statuses, typed: all pass → pass; a `fail`
/// → platform failure; an `error` or `not_run` → harness failure;
/// otherwise (an `inconclusive` child) not exercised.
pub fn b2_outcome(statuses: &[&str]) -> Outcome {
    if statuses.iter().all(|s| *s == "pass") {
        Outcome::Pass
    } else if statuses.contains(&"fail") {
        Outcome::platform("a fake scenario failed")
    } else if statuses.iter().any(|s| *s == "error" || *s == "not_run") {
        Outcome::harness("a fake scenario errored or did not run")
    } else {
        Outcome::not_exercised("a fake scenario was inconclusive")
    }
}

// ---------------- F-2c raw backend injections ----------------

/// Bound on a raw teardown (drain wait included).
const RAW_TEARDOWN_BOUND_MS: u64 = 5000;

/// One injected raw launch as F-2c sees it.
#[derive(Debug, Default, serde::Serialize)]
pub struct InjectRun {
    pub inject: String,
    pub spawn_error: Option<String>,
    pub branch: Option<String>,
    pub survivors: Vec<Value>,
    pub unknown_liveness: usize,
    pub teardown: Option<Value>,
    pub handles_before: Option<u32>,
    pub handles_after: Option<u32>,
}

/// F-2c: `force-suspended` must launch on the suspended branch with zero
/// escapees; every other injection must fail the launch and leave no
/// process alive, a teardown within its bound with the output drained or
/// the host read end closed, and the harness handle count back to its
/// pre-launch value.
pub fn raw_inject_outcome(r: &InjectRun) -> Outcome {
    let (Some(before), Some(after)) = (r.handles_before, r.handles_after) else {
        return Outcome::harness("handle count unavailable");
    };
    if r.inject == "force-suspended" {
        if let Some(e) = &r.spawn_error {
            return Outcome::platform(format!("suspended branch failed: {e}"));
        }
        if r.branch.as_deref() != Some("suspended") {
            return Outcome::harness(format!("injection not effective: branch {:?}", r.branch));
        }
    } else if r.spawn_error.is_none() {
        return Outcome::harness("the injection did not fail the launch");
    }
    if r.unknown_liveness > 0 {
        return Outcome::harness("liveness lookup failed");
    }
    if !r.survivors.is_empty() {
        return Outcome::platform("processes survived the teardown");
    }
    let Some(t) = &r.teardown else {
        return Outcome::harness("no teardown record");
    };
    if t["teardown_ms"]
        .as_u64()
        .is_none_or(|ms| ms > RAW_TEARDOWN_BOUND_MS)
    {
        return Outcome::platform(format!("teardown past its bound: {}", t["teardown_ms"]));
    }
    if t["drain_reached_eof"] != json!(true) && t["host_read_end_closed"] != json!(true) {
        return Outcome::platform("output neither drained to EOF nor its read end closed");
    }
    if after > before {
        return Outcome::platform(format!("handle count {after} > {before} before launch"));
    }
    Outcome::Pass
}

/// Samples pid -> (ppid, start) until stopped, so short-lived descendants
/// of a failed launch are still attributed.
fn sample_procs(
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<std::collections::HashMap<u32, (u32, u64)>> {
    std::thread::spawn(move || {
        let mut m = std::collections::HashMap::new();
        loop {
            for p in os::list_processes().unwrap_or_default() {
                m.entry(p.pid).or_insert((p.ppid, p.start_time));
            }
            if stop.load(std::sync::atomic::Ordering::SeqCst) {
                return m;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    })
}

/// Identities in `map` descending from one of `roots` (pid and start time).
fn descendants(
    map: &std::collections::HashMap<u32, (u32, u64)>,
    roots: &[(u32, u64)],
) -> Vec<(u32, u64)> {
    let mut out: Vec<(u32, u64)> = roots.to_vec();
    loop {
        let before = out.len();
        for (&pid, &(ppid, st)) in map {
            let parent = out.iter().find(|(p, _)| *p == ppid).copied();
            if parent.is_some_and(|(_, ps)| ps <= st) && !out.iter().any(|(p, _)| *p == pid) {
                out.push((pid, st));
            }
        }
        if out.len() == before {
            return out;
        }
    }
}

/// `fake raw-inject --raw-inject <x>` (F-2c, Windows, raw backend only):
/// one FakeCli `spawn-tree` launch with the injected failure.
fn raw_inject(ctx: &Ctx) {
    let inject = ctx.args.raw_inject.clone().unwrap_or_default();
    ctx.require("F-2c", &inject);
    if os::os_name() != "windows" || ctx.args.pty_backend != "raw" {
        ctx.error("raw-inject needs Windows and --pty-backend raw");
        return;
    }
    let mut r = InjectRun {
        inject: inject.clone(),
        handles_before: os::process_handle_count(),
        ..Default::default()
    };
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler = sample_procs(stop.clone());
    let opts = crate::session::SpawnOpts {
        argv: vec![
            fake_bin().into(),
            "--mode".into(),
            "spawn-tree".into(),
            "--max-life-secs".into(),
            SLEEPER_MAX_LIFE.into(),
        ],
        cwd: ctx.run_dir.clone(),
        env: Vec::new(),
        cols: ctx.args.cols,
        rows: ctx.args.rows,
        responder: true,
        out_file: ctx.out.join("pty-output-raw-inject.bin"),
        hold_assign: false,
        inject: None,
        backend: ctx.backend(),
    };
    let mut stop_report = None;
    match crate::session::Session::spawn(opts) {
        Ok(mut s) => {
            r.branch = s.launch["branch"].as_str().map(String::from);
            // Let the tree spawn before the stop.
            std::thread::sleep(Duration::from_secs(2));
            stop_report = Some(s.stop());
        }
        Err(e) => r.spawn_error = Some(e),
    }
    r.teardown = os::raw_last_teardown();
    let mut roots = crate::session::spawned_children();
    if let Some(t) = &r.teardown
        && let (Some(p), Some(st)) = (t["child_pid"].as_u64(), t["child_start"].as_u64())
    {
        roots.push((p as u32, st));
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let map = sampler.join().unwrap_or_default();
    let tree = descendants(&map, &roots);
    let end = Instant::now() + Duration::from_secs(3);
    let alive = loop {
        let l: Vec<((u32, u64), Option<bool>)> = tree
            .iter()
            .map(|&(p, st)| ((p, st), os::liveness(p, st)))
            .filter(|(_, l)| *l != Some(false))
            .collect();
        if l.is_empty() || Instant::now() >= end {
            break l;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    r.unknown_liveness = alive.iter().filter(|(_, l)| l.is_none()).count();
    r.survivors = alive
        .iter()
        .filter(|(_, l)| *l == Some(true))
        .map(|((p, st), _)| json!({"pid": p, "start_time": st}))
        .collect();
    // Handle count settles once released threads have exited.
    let end = Instant::now() + Duration::from_secs(2);
    loop {
        r.handles_after = os::process_handle_count();
        if r.handles_after <= r.handles_before || Instant::now() >= end {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if let Some(st) = &stop_report {
        r.survivors.extend(st.survivors.iter().map(|p| json!(p)));
    }
    let o = raw_inject_outcome(&r);
    ctx.check(
        "F-2c",
        &inject,
        o,
        json!({"run": r, "tree": tree.iter().map(|(p, s)| json!({"pid": p, "start_time": s})).collect::<Vec<_>>(),
               "stop": stop_report.as_ref().map(stop_evidence)}),
    );
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
    let statuses: Vec<&str> = child_status
        .iter()
        .map(|c| c["status"].as_str().unwrap_or("not_run"))
        .collect();
    ctx.check(
        "B-2",
        "all_fake_scenarios_pass",
        b2_outcome(&statuses),
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

    fn results(ok: &[(&str, bool)]) -> Value {
        let r: Vec<Value> = ok
            .iter()
            .map(|(k, o)| json!({"kind": k, "ok": o}))
            .collect();
        json!({"all_ok": ok.iter().all(|x| x.1), "results": r})
    }

    /// §3 Windows shape (`1b584fe6`): conhost answers DSR/DA itself, OSC
    /// 10/11 never reach the harness, kitty is harness-answered.
    fn windows_b5(responder: bool) -> B5Run {
        let kitty = responder;
        B5Run {
            exit: Some(1),
            fake: Some(results(&[
                ("dsr6", true),
                ("dsr5", true),
                ("da1", true),
                ("da2", true),
                ("osc10", false),
                ("osc11", false),
                ("kitty_keyboard", kitty),
            ])),
            log: if responder {
                vec![json!({"kind": "kitty_keyboard", "harness_wrote": true, "reply": "\x1b[?0u"})]
            } else {
                Vec::new()
            },
            output: Some(b"\x1b[?2004h\x1b[?u".to_vec()),
        }
    }

    /// F-3e: B-5 expectations from the recorded Windows shapes, and macOS
    /// keeps today's checks.
    #[test]
    fn b5_expect_windows_facts() {
        let (o, ev) = b5_expect("windows", true, &windows_b5(true));
        assert_eq!(o, Outcome::Pass, "{ev}");
        assert_eq!(
            ev["platform_fact"],
            json!({"answered_by_conhost": ["dsr6", "dsr5", "da1", "da2"],
                   "not_forwarded": ["osc10", "osc11"]})
        );
        assert_eq!(
            b5_expect("windows", false, &windows_b5(false)).0,
            Outcome::Pass
        );
        // Negative control: a kitty reply without the responder fails.
        let mut neg = windows_b5(false);
        neg.fake = windows_b5(true).fake;
        assert!(matches!(
            b5_expect("windows", false, &neg).0,
            Outcome::PlatformFail { .. }
        ));
        // Missing facts are harness failures.
        for f in [
            |r: &mut B5Run| r.output = None,
            |r: &mut B5Run| r.fake = None,
            |r: &mut B5Run| r.fake = Some(results(&[("dsr6", true)])),
        ] {
            let mut r = windows_b5(true);
            f(&mut r);
            assert!(matches!(
                b5_expect("windows", true, &r).0,
                Outcome::HarnessFail { .. }
            ));
        }
        // An OSC query that reached the PTY output is not `not_forwarded`.
        let mut r = windows_b5(true);
        r.output = Some(b"\x1b]10;?\x1b\\".to_vec());
        assert!(matches!(
            b5_expect("windows", true, &r).0,
            PlatformFail { .. }
        ));
        // Kitty must be harness-answered.
        let mut r = windows_b5(true);
        r.log.clear();
        assert!(matches!(
            b5_expect("windows", true, &r).0,
            PlatformFail { .. }
        ));
        // The same Windows shape on macOS fails today's check.
        assert!(matches!(
            b5_expect("macos", true, &windows_b5(true)).0,
            PlatformFail { .. }
        ));
        let mac = B5Run {
            exit: Some(0),
            fake: Some(json!({"all_ok": true})),
            log: Vec::new(),
            output: None,
        };
        assert_eq!(b5_expect("macos", true, &mac).0, Outcome::Pass);
        assert!(matches!(
            b5_expect("macos", false, &mac).0,
            PlatformFail { .. }
        ));
    }

    use Outcome::PlatformFail;

    fn b10(state: &str, filler: Option<Filler>) -> B10Run {
        B10Run {
            gate: true,
            paste_state: state.into(),
            filler,
            survivors_empty: true,
            prompt: None,
            expected: b"abc\rdef".to_vec(),
            reply_after_paste: None,
            harness_dsr_written: false,
        }
    }

    /// F-3e: B-10 `non-reading-child` and `query-mid-paste` per OS.
    #[test]
    fn b10_expect_windows_facts() {
        let f = |accepted, blocked| {
            Some(Filler {
                accepted,
                blocked,
                error: None,
            })
        };
        let nr = "non-reading-child";
        assert_eq!(
            b10_expect("windows", nr, &b10("sent", f(1 << 20, true))).0,
            Outcome::Pass
        );
        assert!(matches!(
            b10_expect("windows", nr, &b10("sent", f(FILLER_MAX, false))).0,
            Outcome::NotExercised { .. }
        ));
        assert!(matches!(
            b10_expect("windows", nr, &b10("sent", None)).0,
            Outcome::HarnessFail { .. }
        ));
        assert_eq!(
            b10_expect("macos", nr, &b10("write_timeout", None)).0,
            Outcome::Pass
        );
        assert!(matches!(
            b10_expect("macos", nr, &b10("sent", None)).0,
            PlatformFail { .. }
        ));
        let mut s = b10("write_timeout", None);
        s.survivors_empty = false;
        assert!(matches!(
            b10_expect("windows", nr, &s).0,
            PlatformFail { .. }
        ));
        // query-mid-paste: exactly one CPR inside the body.
        let q = "query-mid-paste";
        let mut r = b10("sent", None);
        r.prompt = Some(b"ab\x1b[1;1Rc\rdef".to_vec());
        let (o, ev) = b10_expect("windows", q, &r);
        assert_eq!(o, Outcome::Pass);
        assert_eq!(ev["platform_fact"]["offset_bytes"], json!(2));
        assert!(matches!(b10_expect("macos", q, &r).0, PlatformFail { .. }));
        for bad in [
            b"abc\rdef".to_vec(),
            b"ab\x1b[1;1Rc\rd\x1b[2;2Ref".to_vec(),
            b"ab\x1b[1;1Rxc\rdef".to_vec(),
        ] {
            r.prompt = Some(bad);
            assert!(matches!(
                b10_expect("windows", q, &r).0,
                PlatformFail { .. }
            ));
        }
        r.prompt = Some(b"ab\x1b[1;1Rc\rdef".to_vec());
        r.harness_dsr_written = true;
        assert!(matches!(
            b10_expect("windows", q, &r).0,
            PlatformFail { .. }
        ));
        // Today's shape still passes everywhere.
        let mut t = b10("sent", None);
        t.prompt = Some(t.expected.clone());
        t.reply_after_paste = Some(true);
        assert_eq!(b10_expect("windows", q, &t).0, Outcome::Pass);
        assert_eq!(b10_expect("macos", q, &t).0, Outcome::Pass);
    }

    /// F-3e: the `fake teardown` parent passes a child's typed
    /// `not_exercised` on unchanged, and still rejects a wrong run id, an
    /// elapsed time past the deadline, or a survivor.
    #[test]
    fn teardown_parent_keeps_the_childs_outcome() {
        let mut rep = Report::new("fake teardown-case non-reading-child", "run-1");
        rep.status = "inconclusive".into();
        rep.checks.push(crate::report::Check {
            id: "B-10".into(),
            case: "non-reading-child".into(),
            result: "not_exercised".into(),
            cause: Some("conhost accepted all 16 MiB".into()),
            evidence: Value::Null,
        });
        let nr = "non-reading-child";
        assert!(matches!(
            teardown_outcome(nr, "pass", &rep, true, true, 2),
            Outcome::NotExercised { .. }
        ));
        assert!(matches!(
            teardown_outcome(nr, "pass", &rep, false, true, 2),
            PlatformFail { .. }
        ));
        assert!(matches!(
            teardown_outcome(nr, "pass", &rep, true, false, 2),
            PlatformFail { .. }
        ));
        // Untyped cases still compare the status.
        assert!(matches!(
            teardown_outcome("pass", "pass", &rep, true, true, 2),
            PlatformFail { .. }
        ));
        // A report with another run id is never read as the child's.
        let dir = std::env::temp_dir().join(format!("chs-test-{}", util::run_id()));
        std::fs::create_dir(&dir).unwrap();
        let file = dir.join("report.json");
        std::fs::write(&file, serde_json::to_vec(&rep).unwrap()).unwrap();
        let (wrong, right) = (child_report(&dir, "run-2"), child_report(&dir, "run-1"));
        std::fs::remove_file(&file).unwrap();
        std::fs::remove_dir(&dir).unwrap();
        assert!(wrong.is_err());
        assert!(right.is_ok());
        // And `fake all` carries an inconclusive child as not exercised.
        assert!(matches!(
            b2_outcome(&["pass", "inconclusive"]),
            Outcome::NotExercised { .. }
        ));
        assert_eq!(b2_outcome(&["pass", "pass"]), Outcome::Pass);
        assert!(matches!(
            b2_outcome(&["fail", "inconclusive"]),
            PlatformFail { .. }
        ));
        assert!(matches!(
            b2_outcome(&["pass", "not_run"]),
            Outcome::HarnessFail { .. }
        ));
    }

    /// F-2c outcome rules.
    #[test]
    fn raw_inject_rules() {
        let td = json!({"teardown_ms": 120, "drain_reached_eof": true});
        let base = |inject: &str| InjectRun {
            inject: inject.into(),
            spawn_error: Some("injected".into()),
            teardown: Some(td.clone()),
            handles_before: Some(100),
            handles_after: Some(100),
            ..Default::default()
        };
        assert_eq!(raw_inject_outcome(&base("after-spawn")), Outcome::Pass);
        let mut r = base("after-spawn");
        r.spawn_error = None;
        assert!(matches!(
            raw_inject_outcome(&r),
            Outcome::HarnessFail { .. }
        ));
        let mut r = base("reader-start");
        r.survivors = vec![json!({"pid": 1})];
        assert!(matches!(raw_inject_outcome(&r), PlatformFail { .. }));
        let mut r = base("before-spawn");
        r.handles_after = Some(101);
        assert!(matches!(raw_inject_outcome(&r), PlatformFail { .. }));
        let mut r = base("resume-fail");
        r.teardown = Some(json!({"teardown_ms": 9000, "drain_reached_eof": true}));
        assert!(matches!(raw_inject_outcome(&r), PlatformFail { .. }));
        let mut r = base("resume-fail");
        r.teardown = Some(json!({"teardown_ms": 10, "drain_reached_eof": false}));
        assert!(matches!(raw_inject_outcome(&r), PlatformFail { .. }));
        r.teardown = Some(json!({"teardown_ms": 10, "drain_reached_eof": false,
                                 "host_read_end_closed": true}));
        assert_eq!(raw_inject_outcome(&r), Outcome::Pass);
        let mut r = base("before-spawn");
        r.handles_after = None;
        assert!(matches!(
            raw_inject_outcome(&r),
            Outcome::HarnessFail { .. }
        ));
        let mut r = base("force-suspended");
        r.spawn_error = None;
        r.branch = Some("suspended".into());
        assert_eq!(raw_inject_outcome(&r), Outcome::Pass);
        r.branch = Some("job_list".into());
        assert!(matches!(
            raw_inject_outcome(&r),
            Outcome::HarnessFail { .. }
        ));
    }

    #[test]
    fn descendants_follow_pid_and_start_time() {
        let m: std::collections::HashMap<u32, (u32, u64)> =
            [(2, (1, 10)), (3, (2, 11)), (4, (2, 5)), (5, (9, 12))].into();
        let mut d = descendants(&m, &[(1, 9)]);
        d.sort();
        // 4 started before its claimed parent: a reused pid, not ours.
        assert_eq!(d, vec![(1, 9), (2, 10), (3, 11)]);
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
