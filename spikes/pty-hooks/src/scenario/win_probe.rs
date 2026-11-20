//! Set C: `chs win-probe <paste|startup|kill|attach|console|all>` and
//! `attach-client`. Go/no-go screens for Windows architectural blockers
//! (§1 C, §4.8). Compiles everywhere; refuses to run off Windows.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{Ctx, LaunchSpec, emitter_path, launch, run_bounded};
use crate::cli::{self, Args};
use crate::http_client::{self, parse_local_url};
use crate::paste::{self, Newline, PasteOpts};
use crate::pty::WriterTx;
use crate::receiver::ProbeHandler;
use crate::report::{Probe, Report, read_report};
use crate::vt::Vt;
use crate::{os, util};

const PROBES: &[&str] = &["paste", "startup", "kill", "attach", "console"];

pub fn run(args: Args) -> i32 {
    let sub = args.positional.get(1).cloned().unwrap_or_default();
    if sub == "attach-client" {
        return attach_client(&args);
    }
    if sub != "all" && !PROBES.contains(&sub.as_str()) {
        eprintln!("chs win-probe: unknown probe {sub:?}\n{}", cli::USAGE);
        return 2;
    }
    let ctx = match Ctx::new(args, &format!("win-probe {sub}")) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("chs: {e}");
            return 3;
        }
    };
    if os::os_name() != "windows" {
        ctx.detail("reason", json!("windows_only"));
        ctx.error("windows_only");
        return ctx.finish();
    }
    ctx.sink.with(|r| {
        r.host["in_job"] = os::self_job_info();
        r.host["console_cp"] = json!(os::console_code_page());
    });
    match sub.as_str() {
        "all" => all(&ctx),
        "paste" => probe_paste(&ctx),
        "startup" => probe_startup(&ctx),
        "kill" => probe_kill(&ctx),
        "attach" => probe_attach(&ctx),
        "console" => probe_console(&ctx),
        _ => unreachable!(),
    }
    ctx.finish()
}

fn probe(ctx: &Ctx, id: &str, verdict: &str, why: &str, evidence: Value) {
    ctx.sink.with(|r| {
        r.probes.push(Probe {
            id: id.into(),
            verdict: verdict.into(),
            why: why.into(),
            evidence,
        });
    });
}

/// Runs `chs <args...>` as a child with its own out dir; returns its report.
fn child(
    ctx: &Ctx,
    name: &str,
    argv: &[&str],
    timeout: Duration,
    inherit_stdio: bool,
) -> (Option<Report>, PathBuf) {
    let out = ctx.out.join(util::sanitize_component(name));
    let mut cmd = Command::new(std::env::current_exe().unwrap_or_default());
    cmd.args(argv)
        .arg("--out")
        .arg(&out)
        .args(cli::forward_common(&ctx.args));
    let r = if inherit_stdio {
        cmd.stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        match cmd.spawn() {
            Ok(mut c) => {
                let end = Instant::now() + timeout;
                loop {
                    if let Ok(Some(_)) = c.try_wait() {
                        break;
                    }
                    if Instant::now() >= end {
                        let _ = c.kill();
                        let _ = c.wait();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Ok((0, String::new()))
            }
            Err(e) => Err(e.to_string()),
        }
    } else {
        run_bounded(&mut cmd, timeout)
    };
    if let Err(e) = r {
        ctx.detail(&format!("child-{name}-error"), json!(e));
    }
    (read_report(&out.join("report.json")), out)
}

fn checks_of<'a>(r: &'a Option<Report>, id: &str) -> Vec<&'a crate::report::Check> {
    r.as_ref()
        .map(|r| r.checks.iter().filter(|c| c.id == id).collect())
        .unwrap_or_default()
}

fn all(ctx: &Ctx) {
    // Each probe in its own `chs` child; a failure in one never skips the others.
    for p in PROBES {
        let (rep, out) = child(
            ctx,
            &format!("probe-{p}"),
            &["win-probe", p],
            Duration::from_secs(1800),
            *p == "startup",
        );
        match rep {
            Some(r) => ctx.sink.with(|agg| {
                agg.probes.extend(r.probes.clone());
                agg.detail(
                    &format!("probe-{p}"),
                    json!({"status": r.status, "report": out.join("report.json")}),
                );
            }),
            None => probe(
                ctx,
                p,
                "inconclusive",
                "probe child wrote no report",
                json!({"out": out}),
            ),
        }
    }
}

// ---------------- C-1 ----------------

fn probe_paste(ctx: &Ctx) {
    let mut fake = Vec::new();
    for m in Newline::ALL {
        let (r, _) = child(
            ctx,
            &format!("fake-paste-{}", m.name()),
            &["fake", "paste", "--paste-newline", m.name()],
            Duration::from_secs(600),
            false,
        );
        let pass = r.as_ref().is_some_and(|r| r.status == "pass");
        fake.push(json!({"mode": m.name(), "pass": pass}));
    }
    let mut real = serde_json::Map::new();
    let mut real_ok = true;
    let mut any_mode_ok_per_agent = true;
    let mut all_modes_ok = true;
    let mut gate_failed = false;
    for agent in ["claude", "codex"] {
        let (r, _) = child(
            ctx,
            &format!("real-{agent}-paste"),
            &["real", &format!("{agent}-paste"), "--subset", "required"],
            Duration::from_secs(1800),
            false,
        );
        let checks = checks_of(&r, "A-6");
        if checks.is_empty() {
            real_ok = false;
        }
        gate_failed |= checks
            .iter()
            .any(|c| c.evidence.get("readiness_gate_failed").is_some());
        let mut mode_ok = std::collections::BTreeMap::new();
        for c in &checks {
            let mode = c.case.split('/').nth(1).unwrap_or("").to_string();
            let e = mode_ok.entry(mode).or_insert(true);
            *e &= c.result == "pass";
        }
        any_mode_ok_per_agent &= mode_ok.values().any(|v| *v);
        all_modes_ok &= !mode_ok.is_empty() && mode_ok.values().all(|v| *v);
        real.insert(
            agent.into(),
            json!({"status": r.as_ref().map(|r| r.status.clone()), "modes": mode_ok}),
        );
    }
    let (verdict, why) = if !real_ok {
        (
            "inconclusive",
            "real-CLI paste results missing; FakeCli-only results are inconclusive",
        )
    } else if !any_mode_ok_per_agent {
        (
            "blocker",
            "required subset corrupted or dropped in every newline mode for at least one CLI",
        )
    } else if !all_modes_ok {
        ("tuning", "some newline modes differ")
    } else {
        ("go", "required subset round-trips in every mode")
    };
    probe(
        ctx,
        "C-1",
        verdict,
        why,
        json!({"fake": fake, "real": real, "readiness_gate_failed_seen": gate_failed,
        "conpty_module": os::conpty_module(), "console_cp": os::console_code_page()}),
    );
}

// ---------------- C-2 ----------------

fn probe_startup(ctx: &Ctx) {
    let cwd = ctx
        .args
        .cwd
        .clone()
        .unwrap_or_else(|| ctx.args.root.join("work").join("日本 dir"));
    let cwd_s = cwd.display().to_string();
    let runs: Vec<(&str, Vec<&str>, bool)> = vec![
        (
            "claude-hooks",
            vec!["real", "claude-hooks", "--cwd", &cwd_s],
            false,
        ),
        (
            "codex-hooks",
            vec![
                "real",
                "codex-hooks",
                "--launches",
                "3",
                "--codex-mode",
                "both",
                "--cwd",
                &cwd_s,
            ],
            false,
        ),
        (
            "codex-warm",
            vec!["real", "codex-warm", "--cwd", &cwd_s],
            false,
        ),
        (
            "claude-attended",
            vec!["real", "claude-stall", "--attended"],
            true,
        ),
        (
            "codex-attended",
            vec!["real", "codex-stall", "--attended"],
            true,
        ),
    ];
    let mut ev = serde_json::Map::new();
    let mut started = true;
    for (name, argv, attended) in runs {
        let (r, _) = child(ctx, name, &argv, Duration::from_secs(1800), attended);
        let checks: Vec<Value> = r
            .as_ref()
            .map(|r| {
                r.checks
                    .iter()
                    .map(|c| json!({"id": c.id, "case": c.case, "result": c.result}))
                    .collect()
            })
            .unwrap_or_default();
        if name == "claude-hooks" {
            started &= checks_of(&r, "A-1").iter().all(|c| c.result == "pass")
                && !checks_of(&r, "A-1").is_empty();
        }
        if name == "codex-hooks" {
            started &= checks_of(&r, "A-3").iter().any(|c| c.result == "pass");
        }
        ev.insert(name.into(), json!({"status": r.as_ref().map(|r| r.status.clone()), "checks": checks, "errors": r.as_ref().map(|r| r.errors.clone())}));
    }
    let (verdict, why) = if started {
        (
            "go",
            "both CLIs delivered a bound SessionStart under ConPTY from a spaced non-ASCII cwd",
        )
    } else {
        (
            "inconclusive",
            "a CLI did not deliver SessionStart; the orchestrator classifies blocker vs tuning from the snapshots and emitter logs",
        )
    };
    probe(ctx, "C-2", verdict, why, Value::Object(ev));
}

// ---------------- C-3 ----------------

fn probe_kill(ctx: &Ctx) {
    let (held, _) = child(
        ctx,
        "fake-tree-kill-held",
        &["fake", "tree-kill", "--hold-assign-until-child"],
        Duration::from_secs(300),
        false,
    );
    let (plain, _) = child(
        ctx,
        "fake-tree-kill",
        &["fake", "tree-kill"],
        Duration::from_secs(300),
        false,
    );
    let mut real = serde_json::Map::new();
    for a in ["claude", "codex"] {
        let (r, _) = child(
            ctx,
            &format!("real-tree-kill-{a}"),
            &["real", "tree-kill", "--agent", a],
            Duration::from_secs(600),
            false,
        );
        real.insert(a.into(), json!(r.map(|r| (r.status, r.checks))));
    }
    let survivors = |r: &Option<Report>| {
        checks_of(r, "B-7")
            .iter()
            .any(|c| c.case.ends_with("no_session_survivors") && c.result == "fail")
    };
    let race_escapees = |r: &Option<Report>| -> usize {
        checks_of(r, "B-7")
            .iter()
            .filter(|c| {
                c.evidence.pointer("/killed_by").and_then(Value::as_str) == Some("fallback")
            })
            .count()
    };
    let jf = job_first(ctx);
    let jf_ok = jf
        .get("escapees")
        .and_then(Value::as_array)
        .is_some_and(|a| a.is_empty())
        && jf.get("error").is_none();
    let any_survivor = survivors(&held) || survivors(&plain);
    let (verdict, why) = if any_survivor {
        ("blocker", "owned processes survived job + fallback")
    } else if !jf_ok && race_escapees(&held) > 0 {
        (
            "blocker",
            "assign-after race escapees and job-first launch not feasible",
        )
    } else if race_escapees(&held) > 0 {
        ("tuning", "assign-after race escapees, job-first feasible")
    } else if jf_ok {
        ("go", "no survivors; job-first launch with zero escapees")
    } else {
        (
            "inconclusive",
            "no survivors but job-first launch did not complete",
        )
    };
    probe(
        ctx,
        "C-3",
        verdict,
        why,
        json!({
            "held": held.map(|r| r.checks), "plain": plain.map(|r| r.checks), "real": real,
            "job_first": jf, "harness_job": os::self_job_info(),
        }),
    );
}

/// One job-first launch of FakeCli `spawn-tree` (§4.6); escapees = session witnesses outside the job.
fn job_first(ctx: &Ctx) -> Value {
    let rx = match crate::receiver::Receiver::start(crate::receiver::Config {
        token: util::token(),
        thread_id: uuid::Uuid::new_v4().to_string(),
        agent: "fake".into(),
        cli_version: util::FAKE_CLI_VERSION.into(),
        rule: crate::receiver::BindRule::Ancestor,
        fixtures_dir: ctx.out.join("fixtures-job-first"),
        run_id: ctx.run_id.clone(),
        cli_names: vec!["chs-fake-cli".into()],
    }) {
        Ok(r) => r,
        Err(e) => return json!({"error": e.to_string()}),
    };
    let cfg = rx.config().clone();
    let env = vec![
        ("CHALDEA_HOOK_URL".to_string(), rx.url()),
        ("CHALDEA_HOOK_TOKEN".to_string(), cfg.token.clone()),
        ("CHALDEA_THREAD_ID".to_string(), cfg.thread_id.clone()),
    ];
    let argv = vec![
        util::sibling_bin("chs-fake-cli").into_os_string(),
        "--mode".into(),
        "spawn-tree".into(),
        "--emitter".into(),
        emitter_path(&ctx.args.root).into_os_string(),
    ];
    let remove: Vec<String> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| crate::session::scrub_env_name(k))
        .collect();
    let (mut jf, branch) = match os::JobFirst::launch(&argv, &env, &remove, &ctx.run_dir) {
        Ok(v) => v,
        Err(e) => return json!({"error": e}),
    };
    let mut want: Vec<&str> = os::spawn_tree_roles().to_vec();
    want.push("spawn-tree");
    let seen = rx.wait_witnesses(ctx.until(Duration::from_secs(10)), |ws| {
        want.iter().all(|r| ws.iter().any(|w| w.role == *r))
    });
    let members = jf.members();
    let ws = rx.witnesses();
    let escapees: Vec<Value> = ws
        .iter()
        .filter(|w| w.session && !members.contains(&w.pid) && os::is_alive(w.pid, w.start_time))
        .map(|w| json!({"pid": w.pid, "role": w.role}))
        .collect();
    jf.terminate();
    for w in &ws {
        if w.session && os::is_alive(w.pid, w.start_time) {
            os::force_kill(w.pid, w.start_time);
        }
    }
    json!({"branch": branch, "pid": jf.pid(), "all_witnesses_seen": seen, "job_members": members, "escapees": escapees})
}

// ---------------- C-4 attach ----------------

struct AttachProbe {
    credential: String,
    vt: Arc<std::sync::Mutex<Vt>>,
    tx: WriterTx,
    gate: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl ProbeHandler for AttachProbe {
    fn credential(&self) -> String {
        self.credential.clone()
    }
    fn screen(&self) -> Vec<u8> {
        let s = self.vt.lock().unwrap_or_else(|e| e.into_inner()).snapshot();
        s.lines.join("\n").into_bytes()
    }
    fn input(&self, body: &[u8]) -> u16 {
        let o = paste::paste(
            &self.tx,
            body,
            &PasteOpts {
                mode: Newline::Cr,
                chunk: 0,
                enter_delay: Duration::from_millis(300),
                write_deadline: Instant::now() + Duration::from_secs(3),
            },
            || (self.gate)(),
        );
        if o.state == "sent" { 200 } else { 400 }
    }
    fn key(&self, body: &[u8]) -> u16 {
        match paste::key_bytes(String::from_utf8_lossy(body).trim()) {
            Some(b) => match self
                .tx
                .write_wait(vec![b], Instant::now() + Duration::from_secs(3))
            {
                Ok(()) => 200,
                Err(_) => 400,
            },
            None => 400,
        }
    }
}

/// Starts FakeCli `attachable` with probe routes; returns the launch, URL base and credential.
fn start_attachable(ctx: &Ctx) -> Result<(super::Launch, String, String), String> {
    let argv = vec![
        util::sibling_bin("chs-fake-cli").into_os_string(),
        "--mode".into(),
        "attachable".into(),
        "--emitter".into(),
        emitter_path(&ctx.args.root).into_os_string(),
    ];
    let l = launch(
        ctx,
        LaunchSpec {
            label: "attachable".into(),
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
        },
    )?;
    if !l.wait_gate(ctx.until(Duration::from_secs(ctx.args.session_start_timeout))) {
        return Err("attachable never opened the input gate".into());
    }
    let credential = util::token();
    let io = l.session.io();
    let bound = l.receiver.bound();
    let vt = io.vt.clone();
    let vt2 = io.vt.clone();
    let gate: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
        bound.is_some()
            && vt2
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .bracketed_paste()
    });
    l.receiver.set_probe(Some(Arc::new(AttachProbe {
        credential: credential.clone(),
        vt,
        tx: io.tx.clone(),
        gate,
    })));
    let base = format!("http://127.0.0.1:{}/probe", l.receiver.port());
    Ok((l, base, credential))
}

fn attach_verdict(l: &super::Launch, client: &Value) -> (bool, Value) {
    let rep = l
        .receiver
        .witnesses()
        .into_iter()
        .rev()
        .find(|w| w.role == "attachable-report")
        .map(|w| w.report);
    let marker = client["marker"].as_u64();
    let nonce = client["nonce"].as_str().unwrap_or("");
    let emitted = rep
        .as_ref()
        .and_then(|r| r["markers_emitted"].as_array().cloned())
        .unwrap_or_default();
    let nonces = rep
        .as_ref()
        .and_then(|r| r["nonces"].as_array().cloned())
        .unwrap_or_default();
    let keys: Vec<String> = rep
        .as_ref()
        .and_then(|r| r["keys"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|k| k.as_str().map(str::to_string))
        .collect();
    let marker_ok = marker.is_some_and(|m| emitted.iter().any(|e| e.as_u64() == Some(m)));
    let nonce_ok = !nonce.is_empty() && nonces.iter().any(|n| n.as_str() == Some(nonce));
    let keys_ok = ["down", "enter", "esc"]
        .iter()
        .all(|k| keys.iter().any(|x| x.starts_with(k)));
    (
        marker_ok && nonce_ok && keys_ok,
        json!({"marker_ok": marker_ok, "nonce_ok": nonce_ok, "keys_ok": keys_ok, "keys": keys, "client": client}),
    )
}

fn probe_attach(ctx: &Ctx) {
    let (l, base, cred) = match start_attachable(ctx) {
        Ok(v) => v,
        Err(e) => {
            return probe(
                ctx,
                "C-4",
                "inconclusive",
                "attachable did not start",
                json!({"error": e}),
            );
        }
    };
    // Viewerless for 30 s.
    std::thread::sleep(
        Duration::from_secs(30).min(ctx.cutoff.saturating_duration_since(Instant::now())),
    );
    let mut cmd = Command::new(std::env::current_exe().unwrap_or_default());
    cmd.args(["win-probe", "attach-client", "--url", &base]);
    let client = match cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut c) => {
            if let Some(mut si) = c.stdin.take() {
                let _ = writeln!(si, "{cred}");
            }
            let mut out = String::new();
            if let Some(o) = c.stdout.take() {
                let _ =
                    std::io::Read::read_to_string(&mut std::io::Read::take(o, 1 << 20), &mut out);
            }
            let end = Instant::now() + Duration::from_secs(30);
            while Instant::now() < end && !matches!(c.try_wait(), Ok(Some(_))) {
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = c.kill();
            let _ = c.wait();
            serde_json::from_str::<Value>(out.trim()).unwrap_or(json!({"raw": out}))
        }
        Err(e) => json!({"spawn_error": e.to_string()}),
    };
    std::thread::sleep(Duration::from_secs(1));
    let (ok, ev) = attach_verdict(&l, &client);
    l.receiver.set_probe(None);
    let (stop, _) = l.finish();
    probe(
        ctx,
        "C-4",
        if ok { "go" } else { "blocker" },
        if ok {
            "viewerless progress and relayed input/keys reached the same child"
        } else {
            "marker, nonce, or keys did not round-trip"
        },
        json!({"check": ev, "survivors": stop.survivors}),
    );
}

/// `chs win-probe attach-client --url U`; credential on stdin. Prints a JSON result.
fn attach_client(args: &Args) -> i32 {
    let Some(url) = args.url.as_deref() else {
        eprintln!("attach-client needs --url http://127.0.0.1:<port>/probe");
        return 2;
    };
    let Some(base) = url
        .strip_suffix("/probe")
        .and_then(|u| parse_local_url(&format!("{u}/probe"), "/probe"))
    else {
        eprintln!("attach-client: --url must be http://127.0.0.1:<port>/probe");
        return 2;
    };
    let mut cred = String::new();
    if std::io::stdin().lock().read_line(&mut cred).is_err() || cred.trim().is_empty() {
        eprintln!("attach-client: paste the per-run credential on stdin");
        return 2;
    }
    let auth = format!("Bearer {}", cred.trim());
    let h = [("Authorization", auth.as_str())];
    let dl = || Instant::now() + Duration::from_secs(5);
    let screen = http_client::request(
        &base.with_path("/probe/screen"),
        "GET",
        &h,
        None,
        Duration::from_secs(1),
        dl(),
    );
    let marker = screen.as_ref().ok().and_then(|(_, b)| {
        let s = String::from_utf8_lossy(b);
        s.find("MARKER-")
            .and_then(|i| s.get(i + 7..i + 13))
            .and_then(|d| d.parse::<u64>().ok())
    });
    let nonce = util::token()[..16].to_string();
    let input = http_client::request(
        &base.with_path("/probe/input"),
        "POST",
        &h,
        Some(nonce.as_bytes()),
        Duration::from_secs(1),
        dl(),
    );
    let mut keys = Vec::new();
    for k in ["down", "enter", "esc"] {
        let r = http_client::request(
            &base.with_path("/probe/key"),
            "POST",
            &h,
            Some(k.as_bytes()),
            Duration::from_secs(1),
            dl(),
        );
        keys.push(json!({"key": k, "status": r.map(|r| r.0).ok()}));
        std::thread::sleep(Duration::from_millis(200));
    }
    let out = json!({"screen_status": screen.as_ref().map(|r| r.0).ok(), "marker": marker, "nonce": nonce,
                     "input_status": input.map(|r| r.0).ok(), "keys": keys});
    println!("{out}");
    if marker.is_some() { 0 } else { 1 }
}

// ---------------- C-5 console ----------------

fn probe_console(ctx: &Ctx) {
    if ctx.args.detached && std::env::var("CHS_DETACHED_CHILD").is_err() {
        return relaunch_detached(ctx);
    }
    let detached = std::env::var("CHS_DETACHED_CHILD").is_ok();
    let console_window = os::console_window_present();
    let tree = PpidRecorder::start();
    let obs = match os::WindowObserver::start() {
        Ok(o) => o,
        Err(e) => {
            return probe(
                ctx,
                "C-5",
                "inconclusive",
                "observer did not start",
                json!({"error": e}),
            );
        }
    };
    // Calibration: a console exe started without CREATE_NO_WINDOW must be seen and attributed.
    let cal = Command::new("cmd.exe")
        .args(["/c", "ping -n 3 127.0.0.1 >nul"])
        .spawn();
    let cal_pid = cal.as_ref().map(|c| c.id()).ok();
    if let Ok(mut c) = cal {
        let _ = c.wait();
    }
    std::thread::sleep(Duration::from_millis(500));
    let shown_cal = obs.stop();
    let calibrated = cal_pid.is_some_and(|p| tree.attributed(&shown_cal, p));
    let obs = match os::WindowObserver::start() {
        Ok(o) => o,
        Err(e) => {
            return probe(
                ctx,
                "C-5",
                "inconclusive",
                "observer restart failed",
                json!({"error": e}),
            );
        }
    };
    let (startup, _) = child(
        ctx,
        "console-startup",
        &["win-probe", "startup"],
        Duration::from_secs(1800),
        false,
    );
    let (fpaste, _) = child(
        ctx,
        "console-fake-paste",
        &["fake", "paste", "--paste-newline", "cr"],
        Duration::from_secs(600),
        false,
    );
    let shown = obs.stop();
    let me = std::process::id();
    let ours: Vec<u32> = shown
        .iter()
        .copied()
        .filter(|p| tree.attributed(&[*p], me))
        .collect();
    let mut rerun = Value::Null;
    if !ours.is_empty() {
        // Mitigation re-run in the same session with the GUI-subsystem emitter.
        let obs2 = os::WindowObserver::start().ok();
        let mut cmd = Command::new(std::env::current_exe().unwrap_or_default());
        cmd.env("CHS_EMITTER_VARIANT", "gui")
            .args(["fake", "paste", "--paste-newline", "cr", "--out"])
            .arg(ctx.out.join("console-rerun-gui"))
            .args(cli::forward_common(&ctx.args));
        let _ = run_bounded(&mut cmd, Duration::from_secs(600));
        let shown2 = obs2.map(|o| o.stop()).unwrap_or_default();
        rerun = json!({"shown": shown2.iter().filter(|p| tree.attributed(&[**p], me)).collect::<Vec<_>>()});
    }
    let survival = if detached {
        terminal_close_survival(ctx)
    } else {
        json!({"skipped": "run with --detached"})
    };
    let survived = survival["nonce_round_trip"].as_bool() == Some(true);
    let (verdict, why) = if !calibrated {
        ("inconclusive", "observer not calibrated")
    } else if !ours.is_empty() && rerun["shown"].as_array().is_some_and(|a| !a.is_empty()) {
        (
            "blocker",
            "repeatable window flash not removed by the GUI-subsystem emitter",
        )
    } else if !ours.is_empty() {
        ("tuning", "flash removed by a verified launch variant")
    } else if detached && !survived {
        ("blocker", "session did not survive terminal close")
    } else if !detached {
        (
            "inconclusive",
            "no flash; terminal-close survival needs --detached",
        )
    } else {
        (
            "go",
            "no window from our tree; session survived terminal close",
        )
    };
    probe(
        ctx,
        "C-5",
        verdict,
        why,
        json!({
            "detached": detached, "console_window": console_window, "calibration": {"pid": cal_pid, "seen": calibrated},
            "shown_from_our_tree": ours, "rerun_gui_emitter": rerun, "startup": startup.map(|r| r.status),
            "fake_paste": fpaste.map(|r| r.status), "survival": survival,
            "breakaway": std::env::var("CHS_BREAKAWAY").ok(), "harness_job": os::self_job_info(),
        }),
    );
}

/// Records pid -> ppid every 300 ms so short-lived console hosts can still be attributed.
struct PpidRecorder {
    map: Arc<std::sync::Mutex<std::collections::HashMap<u32, u32>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl PpidRecorder {
    fn start() -> PpidRecorder {
        let map = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (m, s) = (map.clone(), stop.clone());
        std::thread::spawn(move || {
            while !s.load(std::sync::atomic::Ordering::SeqCst) {
                for p in os::list_processes().unwrap_or_default() {
                    m.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(p.pid, p.ppid);
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        });
        PpidRecorder { map, stop }
    }

    /// True when a shown pid's recorded parent chain (conhost / OpenConsole included) reaches `root`.
    fn attributed(&self, shown: &[u32], root: u32) -> bool {
        let m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        shown.iter().any(|&p| {
            let mut cur = Some(p);
            for _ in 0..16 {
                match cur {
                    Some(c) if c == root => return true,
                    Some(c) => cur = m.get(&c).copied().filter(|pp| *pp != c),
                    None => break,
                }
            }
            false
        })
    }
}

impl Drop for PpidRecorder {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Relaunches the harness detached and prints the owner's next steps.
fn relaunch_detached(ctx: &Ctx) {
    let mut cmd = Command::new(std::env::current_exe().unwrap_or_default());
    cmd.args(["win-probe", "console", "--detached"])
        .args(cli::forward_common(&ctx.args));
    cmd.env("CHS_DETACHED_CHILD", "1");
    let cred = util::token();
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let url_file = ctx.out.join("attach-url.txt");
    cmd.env("CHS_ATTACH_URL_FILE", &url_file);
    match os::spawn_detached(&mut cmd) {
        Ok((mut c, breakaway)) => {
            if let Some(mut si) = c.stdin.take() {
                let _ = writeln!(si, "{cred}");
            }
            let end = Instant::now() + Duration::from_secs(900);
            while !url_file.exists() && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(500));
            }
            let url = std::fs::read_to_string(&url_file).unwrap_or_default();
            println!(
                "detached harness pid {} (breakaway {})",
                c.id(),
                if breakaway { "permitted" } else { "denied" }
            );
            println!("now close this terminal, open a new one, and run:");
            println!("  chs win-probe attach-client --url {}", url.trim());
            println!("then paste this credential on its stdin: {cred}");
            probe(
                ctx,
                "C-5-launch",
                "inconclusive",
                "detached run continues in the background",
                json!({"breakaway_permitted": breakaway, "breakaway_denied": !breakaway, "parent_job": os::self_job_info()}),
            );
        }
        Err(e) => probe(
            ctx,
            "C-5-launch",
            "inconclusive",
            "detached launch failed",
            json!({"error": e, "parent_job": os::self_job_info()}),
        ),
    }
}

/// Detached child: FakeCli `attachable` waits for an attach-client run from a new terminal.
fn terminal_close_survival(ctx: &Ctx) -> Value {
    let mut cred = String::new();
    let _ = std::io::stdin().lock().read_line(&mut cred);
    let (l, base, own) = match start_attachable(ctx) {
        Ok(v) => v,
        Err(e) => return json!({"error": e}),
    };
    let credential = if cred.trim().is_empty() {
        own
    } else {
        cred.trim().to_string()
    };
    // Re-register with the credential the owner holds.
    let io = l.session.io();
    let bound = l.receiver.bound();
    let vt2 = io.vt.clone();
    let gate: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
        bound.is_some()
            && vt2
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .bracketed_paste()
    });
    l.receiver.set_probe(Some(Arc::new(AttachProbe {
        credential,
        vt: io.vt.clone(),
        tx: io.tx.clone(),
        gate,
    })));
    if let Ok(f) = std::env::var("CHS_ATTACH_URL_FILE") {
        let _ = std::fs::write(Path::new(&f), &base);
    }
    let before = l.receiver.witnesses().len();
    let end = ctx.until(Duration::from_secs(1800));
    let mut nonce_seen = false;
    while Instant::now() < end {
        let ws = l.receiver.witnesses();
        if ws.len() > before
            && ws
                .iter()
                .rev()
                .find(|w| w.role == "attachable-report")
                .is_some_and(|w| w.report["nonces"].as_array().is_some_and(|a| !a.is_empty()))
        {
            nonce_seen = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    l.receiver.set_probe(None);
    let (stop, _) = l.finish();
    json!({"nonce_round_trip": nonce_seen, "survivors": stop.survivors})
}
