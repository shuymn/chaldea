//! Set C: `chs win-probe <paste|startup|kill|attach|console|all>` and
//! `attach-client`. Go/no-go screens for Windows architectural blockers
//! (§1 C, §4.8). Compiles everywhere; refuses to run off Windows.
//!
//! Every probe declares its required cases and gets its verdict from
//! `verdict::probe`: `go` only with every required case present and passed,
//! `blocker` only when the documented blocker condition is demonstrated, and
//! anything missing, setup-failed, readiness-failed, or timed out is
//! `inconclusive`.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{Ctx, LaunchSpec, emitter_path, launch, run_bounded};
use crate::cli::{self, Args};
use crate::http_client::{self, parse_local_url};
use crate::paste::{self, Newline, PasteOpts};
use crate::pty::WriterTx;
use crate::receiver::{ProbeHandler, WitnessView};
use crate::report::{Check, Probe, Report, read_report};
use crate::verdict::{self, Eval};
use crate::vt::Vt;
use crate::{os, util};

/// Probes in `all` order, each with its own budget in seconds.
const PROBES: &[(&str, u64)] = &[
    ("paste", 13200),
    ("startup", 7200),
    ("kill", 2400),
    ("attach", 600),
    ("console", 7200),
];

/// Margin a parent gives a child beyond the child's own deadline.
const CHILD_MARGIN: u64 = 30;

pub fn run(mut args: Args) -> i32 {
    let sub = args.positional.get(1).cloned().unwrap_or_default();
    if sub == "attach-client" {
        return attach_client(&args);
    }
    let budget = match sub.as_str() {
        "all" => PROBES.iter().map(|p| p.1 + CHILD_MARGIN).sum::<u64>() + 60,
        s => match PROBES.iter().find(|p| p.0 == s) {
            Some(p) => p.1,
            None => {
                eprintln!("chs win-probe: unknown probe {sub:?}\n{}", cli::USAGE);
                return 2;
            }
        },
    };
    if !args.explicit_deadline {
        args.deadline_secs = budget;
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

/// One probe's declared cases and observed results.
struct Pc {
    id: &'static str,
    required: Vec<String>,
    checks: Vec<Check>,
}

impl Pc {
    fn new(id: &'static str) -> Pc {
        Pc {
            id,
            required: Vec::new(),
            checks: Vec::new(),
        }
    }
    fn req(&mut self, case: &str) {
        let k = verdict::key(self.id, case);
        if !self.required.contains(&k) {
            self.required.push(k);
        }
    }
    fn put(&mut self, case: &str, result: &str, evidence: Value) {
        self.checks.push(Check {
            id: self.id.into(),
            case: case.into(),
            result: result.into(),
            evidence,
        });
    }
    fn k(&self, case: &str) -> String {
        verdict::key(self.id, case)
    }
    fn finish(
        self,
        ctx: &Ctx,
        blocker: impl Fn(&Eval) -> Option<String>,
        tuning: impl Fn(&Eval) -> Option<String>,
        extra: Value,
    ) {
        let (v, why, eval) = verdict::probe(&self.required, &self.checks, blocker, tuning);
        ctx.sink.with(|r| {
            r.probes.push(Probe {
                id: self.id.into(),
                verdict: v.into(),
                why,
                evidence: json!({"eval": eval, "cases": self.checks, "extra": extra}),
            });
        });
    }
}

fn status_result(r: &Option<Report>) -> &'static str {
    match r.as_ref().map(|r| r.status.as_str()) {
        Some("pass") => "pass",
        Some("fail") => "fail",
        _ => "inconclusive",
    }
}

/// Runs `chs <args...>` as a child with its own out dir and `--deadline-secs`
/// capped to what is left of this run; returns its report (None if not run).
fn child(
    ctx: &Ctx,
    name: &str,
    argv: &[&str],
    budget: u64,
    inherit_stdio: bool,
    env: &[(&str, &str)],
) -> (Option<Report>, PathBuf) {
    let out = ctx.out.join(util::sanitize_component(name));
    let left = ctx
        .cutoff
        .saturating_duration_since(Instant::now())
        .as_secs()
        .saturating_sub(CHILD_MARGIN);
    let deadline = budget.min(left);
    if deadline <= 20 {
        ctx.detail(
            &format!("child-{name}-not-run"),
            json!("no budget left before the cutoff"),
        );
        return (None, out);
    }
    let mut cmd = Command::new(std::env::current_exe().unwrap_or_default());
    cmd.args(argv)
        .arg("--out")
        .arg(&out)
        .args(["--deadline-secs", &deadline.to_string()])
        .args(cli::forward_common(&ctx.args));
    for (k, v) in env {
        cmd.env(k, v);
    }
    let timeout = Duration::from_secs(deadline + CHILD_MARGIN);
    let r = if inherit_stdio {
        cmd.stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        match cmd.spawn() {
            Ok(mut c) => {
                let end = Instant::now() + timeout;
                loop {
                    if let Ok(Some(_)) = c.try_wait() {
                        break Ok((0, String::new()));
                    }
                    if Instant::now() >= end {
                        let _ = c.kill();
                        let _ = c.wait();
                        break Err("timeout".to_string());
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
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

fn checks_of<'a>(r: &'a Option<Report>, id: &str) -> Vec<&'a Check> {
    r.as_ref()
        .map(|r| r.checks.iter().filter(|c| c.id == id).collect())
        .unwrap_or_default()
}

fn find_check<'a>(r: &'a Option<Report>, id: &str, case: &str) -> Option<&'a Check> {
    checks_of(r, id).into_iter().rev().find(|c| c.case == case)
}

/// Copies a child's check into the probe under `case` (absent = missing).
fn carry(pc: &mut Pc, case: &str, r: &Option<Report>, id: &str, child_case: &str) {
    pc.req(case);
    if let Some(c) = find_check(r, id, child_case) {
        pc.put(case, &c.result, c.evidence.clone());
    }
}

fn all(ctx: &Ctx) {
    // Each probe in its own `chs` child with its own budget; a failure in one
    // never skips the others, and every planned probe gets a result.
    for (p, budget) in PROBES {
        let (rep, out) = child(
            ctx,
            &format!("probe-{p}"),
            &["win-probe", p],
            *budget,
            *p == "startup",
            &[],
        );
        let id = probe_id(p);
        let got: Vec<Probe> = rep
            .as_ref()
            .map(|r| r.probes.iter().filter(|x| x.id == id).cloned().collect())
            .unwrap_or_default();
        ctx.sink.with(|agg| {
            agg.detail(
                &format!("probe-{p}"),
                json!({"status": rep.as_ref().map(|r| r.status.clone()), "report": out.join("report.json")}),
            );
            if got.is_empty() {
                agg.probes.push(Probe {
                    id: id.into(),
                    verdict: "inconclusive".into(),
                    why: "not_run: the probe child wrote no verdict".into(),
                    evidence: json!({"out": out}),
                });
            } else {
                agg.probes.extend(got);
            }
        });
    }
}

fn probe_id(p: &str) -> &'static str {
    match p {
        "paste" => "C-1",
        "startup" => "C-2",
        "kill" => "C-3",
        "attach" => "C-4",
        _ => "C-5",
    }
}

// ---------------- C-1 ----------------

fn probe_paste(ctx: &Ctx) {
    let mut pc = Pc::new("C-1");
    let mut fake = Vec::new();
    for m in Newline::ALL {
        let case = format!("fake/{}", m.name());
        pc.req(&case);
        let (r, _) = child(
            ctx,
            &format!("fake-paste-{}", m.name()),
            &["fake", "paste", "--paste-newline", m.name()],
            900,
            false,
            &[],
        );
        if r.is_some() {
            pc.put(
                &case,
                status_result(&r),
                json!({"status": r.as_ref().map(|r| r.status.clone())}),
            );
        }
        fake.push(json!({"mode": m.name(), "host": r.as_ref().map(|r| r.host.clone())}));
    }
    let entries: Vec<String> = paste::corpus()
        .into_iter()
        .filter(|e| e.required)
        .map(|e| e.id)
        .collect();
    let mut real_hosts = serde_json::Map::new();
    for agent in ["claude", "codex"] {
        let (r, _) = child(
            ctx,
            &format!("real-{agent}-paste"),
            &["real", &format!("{agent}-paste"), "--subset", "required"],
            5400,
            false,
            &[],
        );
        for m in Newline::ALL {
            for chunk in [0usize, 7] {
                for e in &entries {
                    let child_case = super::real::paste_case(agent, m, chunk, e);
                    carry(
                        &mut pc,
                        &format!("real/{child_case}"),
                        &r,
                        "A-6",
                        &child_case,
                    );
                }
            }
        }
        real_hosts.insert(
            agent.into(),
            json!(r.as_ref().map(|r| (r.status.clone(), r.host.clone()))),
        );
    }
    // Blocker: for some CLI, every newline mode and write strategy has a
    // definitely corrupted or dropped required entry (readiness failures are
    // inconclusive and never count).
    let groups = |agent: &str| -> Vec<Vec<String>> {
        let mut g = Vec::new();
        for m in Newline::ALL {
            for chunk in [0usize, 7] {
                g.push(
                    entries
                        .iter()
                        .map(|e| {
                            pc.k(&format!(
                                "real/{}",
                                super::real::paste_case(agent, m, chunk, e)
                            ))
                        })
                        .collect(),
                );
            }
        }
        g
    };
    let (gc, gx) = (groups("claude"), groups("codex"));
    let blocker = |ev: &Eval| {
        [("claude", &gc), ("codex", &gx)]
            .iter()
            .find(|(_, gs)| {
                gs.iter().all(|g| {
                    g.iter().all(|k| matches!(ev.result_of(k), "pass" | "fail"))
                        && g.iter().any(|k| ev.result_of(k) == "fail")
                })
            })
            .map(|(a, _)| format!("{a}: required subset corrupted or dropped in every newline mode and write strategy"))
    };
    let tuning = |ev: &Eval| {
        (!ev.failed.is_empty())
            .then(|| format!("some modes or write strategies differ: {:?}", ev.failed))
    };
    let extra = json!({"fake": fake, "real": real_hosts,
        "conpty_module": os::conpty_module(), "console_cp": os::console_code_page()});
    pc.finish(ctx, blocker, tuning, extra);
}

// ---------------- C-2 ----------------

/// Unanswered queries in a child's launch details (query-log check).
fn unanswered(r: &Option<Report>) -> Option<Vec<Value>> {
    let d = r.as_ref()?.details.as_object()?;
    let mut out = Vec::new();
    let mut any = false;
    for (k, v) in d {
        if !k.starts_with("launch-") {
            continue;
        }
        any = true;
        for q in v["query_log"].as_array().into_iter().flatten() {
            if q["reply"].is_null() {
                out.push(json!({"launch": k, "kind": q["kind"]}));
            }
        }
    }
    any.then_some(out)
}

fn probe_startup(ctx: &Ctx) {
    let mut pc = Pc::new("C-2");
    let cwd = ctx
        .args
        .cwd
        .clone()
        .unwrap_or_else(|| super::real::c2_cwd(&ctx.args.root));
    let cwd_s = cwd.display().to_string();
    let runs: Vec<(&str, Vec<&str>, bool, u64)> = vec![
        (
            "claude-hooks",
            vec!["real", "claude-hooks", "--cwd", &cwd_s],
            false,
            600,
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
            2400,
        ),
        (
            "codex-warm",
            vec!["real", "codex-warm", "--cwd", &cwd_s],
            false,
            900,
        ),
        (
            "claude-attended",
            vec!["real", "claude-stall", "--attended"],
            true,
            1500,
        ),
        (
            "codex-attended",
            vec!["real", "codex-stall", "--attended"],
            true,
            1500,
        ),
    ];
    let mut ev = serde_json::Map::new();
    for (name, argv, attended, budget) in runs {
        let (r, _) = child(ctx, name, &argv, budget, attended, &[]);
        match name {
            "claude-hooks" => carry(&mut pc, "claude/A-1", &r, "A-1", "session_start_bound"),
            "codex-hooks" => {
                for mode in ["no-daemon", "default"] {
                    for i in 0..3 {
                        let c = format!("{mode}/{mode}-{i}");
                        carry(&mut pc, &format!("codex/A-3 {c}"), &r, "A-3", &c);
                    }
                }
            }
            "codex-warm" => {
                for c in ["hooks_in_our_tree", "unrelated_survives_our_stop"] {
                    carry(&mut pc, &format!("codex/A-9 {c}"), &r, "A-9", c);
                }
            }
            _ => {
                let agent = if name.starts_with("claude") {
                    "claude"
                } else {
                    "codex"
                };
                let c = format!("attended/{agent}");
                carry(&mut pc, &c, &r, "C-2", &c);
            }
        }
        let qcase = format!("queries/{name}");
        pc.req(&qcase);
        match unanswered(&r) {
            Some(u) if u.is_empty() => pc.put(&qcase, "pass", json!({"unanswered": []})),
            // An unanswered query is evidence for the orchestrator, not a verdict by itself.
            Some(u) => pc.put(&qcase, "inconclusive", json!({"unanswered": u})),
            None => {}
        }
        ev.insert(
            name.into(),
            json!({"status": r.as_ref().map(|r| r.status.clone()), "errors": r.as_ref().map(|r| r.errors.clone())}),
        );
    }
    // Blocker: a CLI never starts under ConPTY: every startup case for it
    // (hook launches and the attended launch) is present and failed.
    let mut claude_keys = vec![pc.k("claude/A-1"), pc.k("attended/claude")];
    claude_keys.dedup();
    let mut codex_keys: Vec<String> = Vec::new();
    for mode in ["no-daemon", "default"] {
        for i in 0..3 {
            codex_keys.push(pc.k(&format!("codex/A-3 {mode}/{mode}-{i}")));
        }
    }
    codex_keys.push(pc.k("attended/codex"));
    let blocker = |e: &Eval| {
        if e.all_present_and_failed(&claude_keys) {
            Some("claude: no launch delivered SessionStart under ConPTY, attended included".into())
        } else if e.all_present_and_failed(&codex_keys) {
            Some("codex: no launch delivered SessionStart under ConPTY, attended included".into())
        } else {
            None
        }
    };
    let tuning = |e: &Eval| (!e.failed.is_empty()).then(|| format!("failed: {:?}", e.failed));
    pc.finish(ctx, blocker, tuning, Value::Object(ev));
}

// ---------------- C-3 ----------------

fn probe_kill(ctx: &Ctx) {
    let mut pc = Pc::new("C-3");
    let (held, _) = child(
        ctx,
        "fake-tree-kill-held",
        &["fake", "tree-kill", "--hold-assign-until-child"],
        300,
        false,
        &[],
    );
    let (plain, _) = child(
        ctx,
        "fake-tree-kill",
        &["fake", "tree-kill"],
        300,
        false,
        &[],
    );
    for (label, r, prefix) in [
        ("held", &held, "tree-hold-assign"),
        ("plain", &plain, "tree"),
    ] {
        pc.req(&format!("{label}/status"));
        if r.is_some() {
            pc.put(
                &format!("{label}/status"),
                status_result(r),
                json!({"status": r.as_ref().map(|r| r.status.clone())}),
            );
        }
        carry(
            &mut pc,
            &format!("{label}/no_session_survivors"),
            r,
            "B-7",
            &format!("{prefix}/no_session_survivors"),
        );
    }
    let mut real = serde_json::Map::new();
    for a in ["claude", "codex"] {
        let (r, _) = child(
            ctx,
            &format!("real-tree-kill-{a}"),
            &["real", "tree-kill", "--agent", a],
            600,
            false,
            &[],
        );
        let c = format!("{a}_mcp_witness_tree");
        carry(&mut pc, &format!("real/{c}"), &r, "A-8", &c);
        real.insert(a.into(), json!(r.map(|r| r.status)));
    }
    let race_escapees: Vec<Value> = checks_of(&held, "B-7")
        .iter()
        .filter(|c| c.evidence.pointer("/killed_by").and_then(Value::as_str) == Some("fallback"))
        .map(|c| json!(c.case))
        .collect();
    let jf = job_first(ctx);
    for (case, v) in [
        ("job_first/launched", &jf["launched"]),
        ("job_first/zero_escapees", &jf["zero_escapees"]),
        ("job_first/no_survivors", &jf["no_survivors"]),
    ] {
        pc.req(case);
        if let Some(r) = v.as_str() {
            pc.put(case, r, Value::Null);
        }
    }
    let survivor_keys: Vec<String> = [
        "held/no_session_survivors",
        "plain/no_session_survivors",
        "real/claude_mcp_witness_tree",
        "real/codex_mcp_witness_tree",
        "job_first/no_survivors",
    ]
    .iter()
    .map(|c| pc.k(c))
    .collect();
    // Race escapees (fallback kills under held assignment) keep C-3 from `go`.
    pc.req("held/race_free");
    if held.is_some() {
        pc.put(
            "held/race_free",
            if race_escapees.is_empty() {
                "pass"
            } else {
                "fail"
            },
            json!({"race_escapees": race_escapees}),
        );
    }
    let (launched_k, zero_k, race_k) = (
        pc.k("job_first/launched"),
        pc.k("job_first/zero_escapees"),
        pc.k("held/race_free"),
    );
    let blocker = |e: &Eval| {
        if let Some(k) = survivor_keys.iter().find(|k| e.result_of(k) == "fail") {
            Some(format!("owned processes survived job + fallback ({k})"))
        } else if e.result_of(&race_k) == "fail"
            && (e.result_of(&launched_k) == "fail" || e.result_of(&zero_k) == "fail")
        {
            Some("assign-after race escapees and job-first launch not feasible".into())
        } else {
            None
        }
    };
    let tuning = |e: &Eval| {
        (e.result_of(&race_k) == "fail"
            && e.result_of(&launched_k) == "pass"
            && e.result_of(&zero_k) == "pass")
            .then(|| "assign-after race escapees, job-first feasible".to_string())
    };
    let extra = json!({"real": real, "job_first": jf, "harness_job": os::self_job_info()});
    pc.finish(ctx, blocker, tuning, extra);
}

/// One job-first launch of FakeCli `spawn-tree` (§4.6). Kill authority comes
/// only from the job, the retained process handle, and descendants recorded
/// with verified lineage during the wait; witnesses are only checked for
/// survival. Verification and fallback run before ConPTY is closed.
fn job_first(ctx: &Ctx) -> Value {
    let rx = match crate::receiver::Receiver::start(crate::receiver::Config {
        token: util::token(),
        thread_id: uuid::Uuid::new_v4().to_string(),
        agent: "fake".into(),
        cli_version: util::FAKE_CLI_VERSION.into(),
        launch_id: "job-first".into(),
        rule: crate::receiver::BindRule::Ancestor,
        fixtures_dir: ctx.out.join("fixtures-job-first"),
        run_id: ctx.run_id.clone(),
        cli_names: vec!["chs-fake-cli".into()],
    }) {
        Ok(r) => r,
        Err(e) => return json!({"launched": "inconclusive", "error": e.to_string()}),
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
        "--max-life-secs".into(),
        "300".into(),
    ];
    let remove: Vec<String> = std::env::vars_os()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .filter(|k| crate::session::scrub_env_name(k))
        .collect();
    let (mut jf, branch) = match os::JobFirst::launch(&argv, &env, &remove, &ctx.run_dir) {
        Ok(v) => v,
        Err(e) => return json!({"launched": "fail", "error": e}),
    };
    let mut want: Vec<&str> = os::spawn_tree_roles().to_vec();
    want.push("spawn-tree");
    // Record the kill set while waiting: job members + verified lineage.
    let mut recorded: std::collections::BTreeMap<(u32, u64), os::ProcInfo> = Default::default();
    let mut record = |jf: &os::JobFirst| {
        let Ok(procs) = os::list_processes() else {
            return;
        };
        let mut roots = vec![(jf.pid(), jf.start_time())];
        roots.extend(recorded.keys().copied());
        let mut found = crate::session::owned_by(jf.pid(), jf.start_time(), &roots, &procs);
        for m in jf.members() {
            found.extend(procs.iter().filter(|p| p.pid == m).cloned());
        }
        for p in found {
            recorded.entry((p.pid, p.start_time)).or_insert(p);
        }
    };
    let end = ctx.until(Duration::from_secs(10));
    let mut seen = false;
    while Instant::now() < end {
        record(&jf);
        let ws = rx.witnesses();
        if want.iter().all(|r| ws.iter().any(|w| w.role == *r)) {
            seen = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    record(&jf);
    let members = jf.members();
    let ws = rx.witnesses();
    let escapees: Vec<Value> = ws
        .iter()
        .filter(|w| w.session && !members.contains(&w.pid) && os::is_alive(w.pid, w.start_time))
        .map(|w| json!({"pid": w.pid, "role": w.role}))
        .collect();
    let steps = jf.terminate_job();
    std::thread::sleep(Duration::from_millis(300));
    let alive_recorded = |r: &std::collections::BTreeMap<(u32, u64), os::ProcInfo>| -> Vec<u32> {
        r.values()
            .filter(|p| os::is_alive(p.pid, p.start_time))
            .map(|p| p.pid)
            .collect()
    };
    let escaped_job = alive_recorded(&recorded);
    let fallback: Vec<u32> = recorded
        .values()
        .filter(|p| os::force_kill(p.pid, p.start_time))
        .map(|p| p.pid)
        .collect();
    let witness_alive = |ws: &[crate::receiver::WitnessRecord]| -> Vec<u32> {
        ws.iter()
            .filter(|w| w.session && os::is_alive(w.pid, w.start_time))
            .map(|w| w.pid)
            .collect()
    };
    let vend = Instant::now() + Duration::from_secs(5);
    let mut pre_close: Vec<u32> = alive_recorded(&recorded);
    pre_close.extend(witness_alive(&ws));
    while !pre_close.is_empty() && Instant::now() < vend {
        std::thread::sleep(Duration::from_millis(100));
        pre_close = alive_recorded(&recorded);
        pre_close.extend(witness_alive(&ws));
    }
    jf.close_conpty();
    let post_close = witness_alive(&ws);
    let mut rx = rx;
    rx.stop();
    json!({
        "launched": "pass", "branch": branch, "pid": jf.pid(), "all_witnesses_seen": seen,
        "zero_escapees": if !seen { "inconclusive" } else if escapees.is_empty() { "pass" } else { "fail" },
        "no_survivors": if pre_close.is_empty() { "pass" } else { "fail" },
        "job_members": members, "escapees": escapees, "kill_steps": steps,
        "recorded": recorded.values().collect::<Vec<_>>(), "escaped_job": escaped_job,
        "killed_by_fallback": fallback, "survivors_before_conpty_close": pre_close,
        "survivors_after_conpty_close": post_close,
    })
}

// ---------------- C-4 / C-5 attach ----------------

/// Probe routes for `attachable`. Input is gated (readiness + DECSET 2004);
/// keys are ungated `write_key`. Each request is attributed to the child by
/// waiting for its next report to show exactly that request's effect;
/// otherwise the handler answers 504.
struct AttachProbe {
    credential: String,
    vt: Arc<Mutex<Vt>>,
    tx: WriterTx,
    ready: Arc<dyn Fn() -> bool + Send + Sync>,
    witnesses: WitnessView,
    child_pid: u32,
    t0: Instant,
    seq: AtomicU64,
    log: Mutex<Vec<Value>>,
    confirmed_ms: Mutex<Option<u64>>,
}

impl AttachProbe {
    fn report(&self) -> Option<Value> {
        self.witnesses
            .witnesses()
            .into_iter()
            .rev()
            .find(|w| w.role == "attachable-report" && w.pid == self.child_pid)
            .map(|w| w.report)
    }
    fn wait_report(&self, deadline: Instant, pred: impl Fn(&Value) -> bool) -> Option<Value> {
        loop {
            if let Some(r) = self.report().filter(|r| pred(r)) {
                return Some(r);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn len_of(&self, field: &str) -> usize {
        self.report()
            .and_then(|r| r[field].as_array().map(Vec::len))
            .unwrap_or(0)
    }
    fn record(&self, v: Value) {
        self.log.lock().unwrap_or_else(|e| e.into_inner()).push(v);
    }
    fn now_ms(&self) -> u64 {
        self.t0.elapsed().as_millis() as u64
    }
    fn log(&self) -> Vec<Value> {
        self.log.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl ProbeHandler for AttachProbe {
    fn credential(&self) -> String {
        self.credential.clone()
    }
    fn screen(&self, _deadline: Instant) -> Vec<u8> {
        let s = self.vt.lock().unwrap_or_else(|e| e.into_inner()).snapshot();
        s.lines.join("\n").into_bytes()
    }
    fn input(&self, body: &[u8], deadline: Instant) -> u16 {
        let id = self.seq.fetch_add(1, Ordering::SeqCst);
        let nonce = String::from_utf8_lossy(body).into_owned();
        let before = self.len_of("nonces");
        let ready = self.ready.clone();
        let o = paste::paste(
            &self.tx,
            body,
            &PasteOpts {
                mode: Newline::Cr,
                chunk: 0,
                enter_delay: Duration::from_millis(300),
                write_deadline: deadline,
            },
            move || ready(),
        );
        let attributed = o.state == "sent"
            && self
                .wait_report(deadline, |r| {
                    r["nonces"].as_array().is_some_and(|a| {
                        a.len() > before && a[before..].iter().any(|n| n == &json!(nonce))
                    })
                })
                .is_some();
        self.record(
            json!({"id": id, "kind": "input", "nonce": nonce, "state": o.state,
                           "attributed": attributed, "at_ms": self.now_ms()}),
        );
        match (o.state.as_str(), attributed) {
            (_, true) => 200,
            ("sent", false) => 504,
            _ => 409,
        }
    }
    fn key(&self, body: &[u8], deadline: Instant) -> u16 {
        let id = self.seq.fetch_add(1, Ordering::SeqCst);
        let name = String::from_utf8_lossy(body).trim().to_string();
        let Some(bytes) = paste::key_bytes(&name) else {
            return 400;
        };
        let before = self.len_of("keys");
        let wrote = self.tx.write_wait(vec![bytes], deadline).is_ok();
        let base = name.trim_start_matches("w-").to_string();
        let attributed = wrote
            && self
                .wait_report(deadline, |r| {
                    r["keys"].as_array().is_some_and(|a| {
                        a.len() > before
                            && a[before..].iter().any(|k| {
                                k.as_str()
                                    .is_some_and(|k| k.starts_with(&format!("{base}:")))
                            })
                    })
                })
                .is_some();
        self.record(json!({"id": id, "kind": "key", "key": name, "wrote": wrote,
                           "attributed": attributed, "at_ms": self.now_ms()}));
        match (wrote, attributed) {
            (_, true) => 200,
            (true, false) => 504,
            _ => 409,
        }
    }
    fn confirm(&self, body: &[u8], _deadline: Instant) -> u16 {
        let what = String::from_utf8_lossy(body).trim().to_string();
        if what != "terminal_closed" {
            return 400;
        }
        let ms = self.now_ms();
        *self.confirmed_ms.lock().unwrap_or_else(|e| e.into_inner()) = Some(ms);
        self.record(json!({"kind": "confirm", "what": what, "at_ms": ms}));
        200
    }
}

/// Starts FakeCli `attachable` with probe routes; returns the launch, URL base,
/// and the probe (for its request log).
fn start_attachable(
    ctx: &Ctx,
    credential: String,
) -> Result<(super::Launch, String, Arc<AttachProbe>), String> {
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
    let child_pid = l
        .receiver
        .wait_witness(ctx.until(Duration::from_secs(5)), |w| {
            w.role == "attachable"
        })
        .map(|w| w.pid)
        .ok_or("attachable never attested")?;
    let io = l.session.io();
    let bound = l.receiver.bound();
    let p = Arc::new(AttachProbe {
        credential,
        vt: io.vt.clone(),
        tx: io.tx.clone(),
        ready: Arc::new(move || bound.is_some()),
        witnesses: l.receiver.witness_view(),
        child_pid,
        t0: Instant::now(),
        seq: AtomicU64::new(0),
        log: Mutex::new(Vec::new()),
        confirmed_ms: Mutex::new(None),
    });
    l.receiver.set_probe(Some(p.clone()));
    let base = format!("http://127.0.0.1:{}/probe", l.receiver.port());
    Ok((l, base, p))
}

/// Result of the request(s) of `kind` matching `pred`: pass if one was
/// attributed, fail if one was made and none attributed, None if never made.
fn attributed(log: &[Value], kind: &str, pred: impl Fn(&Value) -> bool) -> Option<&'static str> {
    let made: Vec<&Value> = log
        .iter()
        .filter(|v| v["kind"] == kind && pred(v))
        .collect();
    if made.is_empty() {
        None
    } else if made.iter().any(|v| v["attributed"] == true) {
        Some("pass")
    } else {
        Some("fail")
    }
}

fn probe_attach(ctx: &Ctx) {
    let mut pc = Pc::new("C-4");
    for c in [
        "viewerless_30s",
        "marker",
        "nonce",
        "key/down",
        "key/enter",
        "key/esc",
    ] {
        pc.req(c);
    }
    let (l, base, p) = match start_attachable(ctx, util::token()) {
        Ok(v) => v,
        Err(e) => {
            return pc.finish(ctx, |_| None, |_| None, json!({"setup_error": e}));
        }
    };
    // Viewerless for the full 30 s; a cutoff that cuts it short is inconclusive.
    let want = Duration::from_secs(30);
    let t = Instant::now();
    std::thread::sleep(want.min(ctx.cutoff.saturating_duration_since(Instant::now())));
    let slept = t.elapsed();
    pc.put(
        "viewerless_30s",
        if slept >= want {
            "pass"
        } else {
            "inconclusive"
        },
        json!({"slept_ms": slept.as_millis() as u64}),
    );
    let mut cmd = Command::new(std::env::current_exe().unwrap_or_default());
    cmd.args(["win-probe", "attach-client", "--url", &base]);
    let client = run_client(&mut cmd, &p.credential, Duration::from_secs(60));
    let log = p.log();
    let rep = p.report();
    let emitted: Vec<u64> = rep
        .as_ref()
        .and_then(|r| r["markers_emitted"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_u64)
        .collect();
    match (client["screen_status"].as_u64(), client["marker"].as_u64()) {
        (Some(200), Some(m)) => pc.put(
            "marker",
            if emitted.contains(&m) { "pass" } else { "fail" },
            json!({"marker": m}),
        ),
        (Some(200), None) => pc.put("marker", "fail", json!({"marker": null})),
        _ => {}
    }
    let nonce = client["nonce"].as_str().unwrap_or("").to_string();
    if let Some(r) = attributed(&log, "input", |v| v["nonce"] == json!(nonce)) {
        pc.put("nonce", r, json!({"nonce": nonce}));
    }
    for k in ["down", "enter", "esc"] {
        if let Some(r) = attributed(&log, "key", |v| v["key"] == json!(k)) {
            pc.put(&format!("key/{k}"), r, Value::Null);
        }
    }
    l.receiver.set_probe(None);
    let (stop, _) = l.finish();
    let (mk, nk) = (pc.k("marker"), pc.k("nonce"));
    let keys: Vec<String> = ["key/down", "key/enter", "key/esc"]
        .iter()
        .map(|c| pc.k(c))
        .collect();
    let blocker = |e: &Eval| {
        if e.result_of(&mk) == "fail" {
            Some("no viewerless progress visible to another process".into())
        } else if e.result_of(&nk) == "fail" && e.all_present_and_failed(&keys) {
            Some("input and keys from another process were not delivered".into())
        } else {
            None
        }
    };
    let tuning = |e: &Eval| (!e.failed.is_empty()).then(|| format!("failed: {:?}", e.failed));
    pc.finish(
        ctx,
        blocker,
        tuning,
        json!({"client": client, "requests": log, "survivors": stop.survivors}),
    );
}

/// Runs an attach-client child with the credential on stdin; returns its JSON line.
fn run_client(cmd: &mut Command, cred: &str, timeout: Duration) -> Value {
    match cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut c) => {
            if let Some(mut si) = c.stdin.take() {
                let _ = writeln!(si, "{cred}");
            }
            let so = c.stdout.take();
            let reader = std::thread::spawn(move || {
                let mut out = String::new();
                if let Some(o) = so {
                    let _ = std::io::Read::read_to_string(
                        &mut std::io::Read::take(o, 1 << 20),
                        &mut out,
                    );
                }
                out
            });
            let end = Instant::now() + timeout;
            while Instant::now() < end && !matches!(c.try_wait(), Ok(Some(_))) {
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = c.kill();
            let _ = c.wait();
            let out = reader.join().unwrap_or_default();
            serde_json::from_str::<Value>(out.trim()).unwrap_or(json!({"raw": out}))
        }
        Err(e) => json!({"spawn_error": e.to_string()}),
    }
}

/// `chs win-probe attach-client --url U [--confirm-close]`; credential on stdin.
fn attach_client(args: &Args) -> i32 {
    let Some(url) = args.url.as_deref() else {
        eprintln!("attach-client needs --url http://127.0.0.1:<port>/probe");
        return 2;
    };
    let Some(base) = parse_local_url(url, "/probe") else {
        eprintln!("attach-client: --url must be http://127.0.0.1:<port>/probe");
        return 2;
    };
    let mut lines = std::io::stdin().lock().lines();
    let cred = lines.next().and_then(Result::ok).unwrap_or_default();
    if cred.trim().is_empty() {
        eprintln!("attach-client: paste the per-run credential on stdin");
        return 2;
    }
    let auth = format!("Bearer {}", cred.trim());
    let h = [("Authorization", auth.as_str())];
    let dl = || Instant::now() + Duration::from_secs(5);
    let post = |path: &str, body: &[u8]| {
        http_client::request(
            &base.with_path(path),
            "POST",
            &h,
            Some(body),
            Duration::from_secs(1),
            dl(),
        )
        .map(|r| r.0)
        .ok()
    };
    let mut confirm = None;
    if args.confirm_close {
        eprintln!(
            "Did you close the terminal that started `chs win-probe console --detached`? [y/N]"
        );
        let ans = lines.next().and_then(Result::ok).unwrap_or_default();
        let yes = matches!(ans.trim().to_ascii_lowercase().as_str(), "y" | "yes");
        confirm = Some(
            json!({"answer": ans.trim(), "status": if yes { post("/probe/confirm", b"terminal_closed") } else { None }}),
        );
        if !yes {
            println!("{}", json!({"confirm": confirm}));
            return 1;
        }
    }
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
    let input = post("/probe/input", nonce.as_bytes());
    let mut keys = Vec::new();
    if !args.confirm_close {
        for k in ["down", "enter", "esc"] {
            keys.push(json!({"key": k, "status": post("/probe/key", k.as_bytes())}));
        }
    }
    let out = json!({"screen_status": screen.as_ref().map(|r| r.0).ok(), "marker": marker, "nonce": nonce,
                     "input_status": input, "keys": keys, "confirm": confirm});
    println!("{out}");
    if marker.is_some() { 0 } else { 1 }
}

// ---------------- C-5 console ----------------

/// Records pid -> ppid every 300 ms; window events also carry their own
/// ppid captured at event time, which attributes short-lived owners.
struct PpidRecorder {
    map: Arc<Mutex<std::collections::HashMap<u32, u32>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl PpidRecorder {
    fn start() -> PpidRecorder {
        let map = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (m, s) = (map.clone(), stop.clone());
        std::thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
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

    /// True when the shown window's owner (via its event-time ppid, then the
    /// sampled chain, conhost / OpenConsole included) reaches `root`.
    fn attributed(&self, shown: &(u32, Option<u32>), root: u32) -> bool {
        let m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if shown.0 == root {
            return true;
        }
        let mut cur = shown.1.or_else(|| m.get(&shown.0).copied());
        for _ in 0..16 {
            match cur {
                Some(c) if c == root => return true,
                Some(c) => cur = m.get(&c).copied().filter(|pp| *pp != c),
                None => break,
            }
        }
        false
    }
}

impl Drop for PpidRecorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn probe_console(ctx: &Ctx) {
    if ctx.args.detached && std::env::var("CHS_DETACHED_CHILD").is_err() {
        return relaunch_detached(ctx);
    }
    let mut pc = Pc::new("C-5");
    for c in [
        "detached",
        "calibration",
        "startup_launch",
        "paste_launch",
        "no_flash",
        "survival",
    ] {
        pc.req(c);
    }
    let detached = std::env::var("CHS_DETACHED_CHILD").is_ok();
    let console_window = os::console_window_present();
    pc.put(
        "detached",
        if detached && console_window == Some(false) {
            "pass"
        } else {
            "inconclusive"
        },
        json!({"detached": detached, "console_window_present": console_window}),
    );
    let tree = PpidRecorder::start();
    let me = std::process::id();
    let obs = match os::WindowObserver::start() {
        Ok(o) => o,
        Err(e) => return pc.finish(ctx, |_| None, |_| None, json!({"observer_error": e})),
    };
    // Calibration: a short-lived console exe (no CREATE_NO_WINDOW) must be seen and attributed.
    let cal = Command::new("cmd.exe").args(["/c", "exit"]).spawn();
    let cal_pid = cal.as_ref().map(|c| c.id()).ok();
    if let Ok(mut c) = cal {
        let _ = c.wait();
    }
    std::thread::sleep(Duration::from_millis(500));
    let shown_cal = obs.stop();
    let calibrated = cal_pid.is_some_and(|p| shown_cal.iter().any(|s| tree.attributed(s, p)));
    pc.put(
        "calibration",
        if calibrated { "pass" } else { "inconclusive" },
        json!({"pid": cal_pid, "events": shown_cal.len()}),
    );
    let obs = match os::WindowObserver::start() {
        Ok(o) => o,
        Err(e) => return pc.finish(ctx, |_| None, |_| None, json!({"observer_error": e})),
    };
    let (startup, _) = child(
        ctx,
        "console-startup",
        &["win-probe", "startup"],
        7200,
        false,
        &[],
    );
    let (fpaste, _) = child(
        ctx,
        "console-fake-paste",
        &["fake", "paste", "--paste-newline", "cr"],
        900,
        false,
        &[],
    );
    let shown = obs.stop();
    // A flash judgment needs launches that actually ran in scope.
    let in_scope = |r: &Option<Report>| {
        if status_result(r) == "pass" {
            "pass"
        } else {
            "inconclusive"
        }
    };
    pc.put(
        "startup_launch",
        in_scope(&startup),
        json!({"status": startup.as_ref().map(|r| r.status.clone())}),
    );
    pc.put(
        "paste_launch",
        in_scope(&fpaste),
        json!({"status": fpaste.as_ref().map(|r| r.status.clone())}),
    );
    let ours: Vec<(u32, Option<u32>)> = shown
        .iter()
        .copied()
        .filter(|s| tree.attributed(s, me))
        .collect();
    pc.put(
        "no_flash",
        if ours.is_empty() { "pass" } else { "fail" },
        json!({"shown_from_our_tree": ours}),
    );
    if !ours.is_empty() {
        // Mitigation re-run in the same session: GUI-subsystem emitter for
        // FakeCli and for a real hook delivery.
        for c in ["rerun_gui_no_flash", "rerun_gui_delivery"] {
            pc.req(c);
        }
        let obs2 = os::WindowObserver::start().ok();
        let gui = [("CHS_EMITTER_VARIANT", "gui")];
        let (rp, _) = child(
            ctx,
            "console-rerun-gui-paste",
            &["fake", "paste", "--paste-newline", "cr"],
            900,
            false,
            &gui,
        );
        let (rh, _) = child(
            ctx,
            "console-rerun-gui-claude",
            &["real", "claude-hooks"],
            600,
            false,
            &gui,
        );
        let shown2 = obs2.map(|o| o.stop()).unwrap_or_default();
        let ours2: Vec<(u32, Option<u32>)> = shown2
            .iter()
            .copied()
            .filter(|s| tree.attributed(s, me))
            .collect();
        let delivered = find_check(&rh, "A-1", "session_start_bound").map(|c| c.result.clone());
        if status_result(&rp) == "pass" && delivered.as_deref() == Some("pass") {
            pc.put(
                "rerun_gui_no_flash",
                if ours2.is_empty() { "pass" } else { "fail" },
                json!({"shown": ours2}),
            );
        }
        if let Some(d) = delivered {
            pc.put("rerun_gui_delivery", &d, Value::Null);
        }
    }
    let survival = if detached {
        terminal_close_survival(ctx)
    } else {
        json!({"result": null, "why": "run `chs win-probe console --detached`"})
    };
    if let Some(r) = survival["result"].as_str() {
        pc.put("survival", r, survival.clone());
    }
    let (nf, rr, sv) = (
        pc.k("no_flash"),
        pc.k("rerun_gui_no_flash"),
        pc.k("survival"),
    );
    let blocker = |e: &Eval| {
        if e.result_of(&nf) == "fail" && e.result_of(&rr) == "fail" {
            Some("repeatable window flash not removed by the GUI-subsystem emitter".into())
        } else if e.result_of(&sv) == "fail" {
            Some("the session died on terminal close".into())
        } else {
            None
        }
    };
    let tuning = |e: &Eval| {
        (e.result_of(&nf) == "fail" && e.result_of(&rr) == "pass")
            .then(|| "flash removed by the GUI-subsystem emitter".to_string())
    };
    pc.finish(
        ctx,
        blocker,
        tuning,
        json!({"breakaway": std::env::var("CHS_BREAKAWAY").ok(), "harness_job": os::self_job_info()}),
    );
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
    let mut pc = Pc::new("C-5-launch");
    pc.req("detached_launch");
    match os::spawn_detached(&mut cmd) {
        Ok((mut c, breakaway)) => {
            if let Some(mut si) = c.stdin.take() {
                let _ = writeln!(si, "{cred}");
            }
            let end = ctx.until(Duration::from_secs(900));
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
            println!(
                "  chs win-probe attach-client --confirm-close --url {}",
                url.trim()
            );
            println!("then paste this credential on its stdin: {cred}");
            println!("(its report: {})", ctx.args.root.join("out").display());
            pc.put(
                "detached_launch",
                "inconclusive",
                json!({"breakaway_permitted": breakaway, "breakaway_denied": !breakaway,
                       "parent_job": os::self_job_info(), "note": "the detached child reports C-5"}),
            );
        }
        Err(e) => pc.put(
            "detached_launch",
            "inconclusive",
            json!({"error": e, "parent_job": os::self_job_info()}),
        ),
    }
    pc.finish(ctx, |_| None, |_| None, Value::Null);
}

/// Detached child: FakeCli `attachable` waits for an owner-confirmed terminal
/// close (`/probe/confirm`) and then a fresh nonce from the same child.
fn terminal_close_survival(ctx: &Ctx) -> Value {
    let mut cred = String::new();
    let _ = std::io::stdin().lock().read_line(&mut cred);
    let credential = if cred.trim().is_empty() {
        util::token()
    } else {
        cred.trim().to_string()
    };
    let (l, base, p) = match start_attachable(ctx, credential) {
        Ok(v) => v,
        Err(e) => return json!({"result": null, "error": e}),
    };
    if let Ok(f) = std::env::var("CHS_ATTACH_URL_FILE") {
        let _ = std::fs::write(Path::new(&f), &base);
    }
    let end = ctx.until(Duration::from_secs(1800));
    let after_confirm = |log: &[Value], c: u64| {
        log.iter().any(|v| {
            v["kind"] == "input"
                && v["attributed"] == true
                && v["at_ms"].as_u64().is_some_and(|t| t >= c)
        })
    };
    let mut res: Option<&str> = None;
    while Instant::now() < end {
        let confirmed = *p.confirmed_ms.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = confirmed {
            if after_confirm(&p.log(), c) {
                res = Some("pass");
                break;
            }
            let child_alive = p
                .witnesses
                .witnesses()
                .iter()
                .find(|w| w.role == "attachable" && w.pid == p.child_pid)
                .is_some_and(|w| os::is_alive(w.pid, w.start_time));
            if !child_alive {
                res = Some("fail");
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let log = p.log();
    l.receiver.set_probe(None);
    let (stop, _) = l.finish();
    json!({"result": res, "requests": log, "child_pid": p.child_pid, "survivors": stop.survivors})
}
