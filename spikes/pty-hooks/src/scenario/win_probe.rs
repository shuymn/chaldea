//! Set C: `chs win-probe <paste|startup|kill|attach|console|all>` and
//! `attach-client`. Go/no-go screens for Windows architectural blockers
//! (§1 C, §4.8). Compiles everywhere; refuses to run off Windows.
//!
//! Every probe declares its required cases and gets its verdict from
//! `verdict::probe`: `inconclusive` when any required case is missing,
//! harness-failed, or not exercised; `go` when all passed; otherwise
//! `fail_observed`. The orchestrator, not the harness, decides whether an
//! observed failure is a blocker.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{Ctx, LaunchSpec, child_out, child_report, emitter_path, launch, run_bounded};
use crate::cli::{self, Args};
use crate::http_client::{self, parse_local_url};
use crate::paste::{self, Newline, PasteOpts};
use crate::pty::WriterTx;
use crate::receiver::{ProbeHandler, WitnessView};
use crate::report::{Check, Probe, Report};
use crate::verdict::{self, Outcome};
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
    fn put(&mut self, case: &str, outcome: Outcome, evidence: Value) {
        self.checks.push(Check {
            id: self.id.into(),
            case: case.into(),
            result: outcome.tag().into(),
            cause: outcome.cause(),
            evidence,
        });
    }
    fn finish(self, ctx: &Ctx, extra: Value) {
        let (v, why, eval) = verdict::probe(&self.required, &self.checks);
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

/// A child run's report, or why it is `not_run` (no fresh report carrying
/// the run id the parent assigned).
type ChildRep = Result<Report, String>;

/// A child run's status as one case: `fail` (a complete run with a platform
/// failure) is the only platform outcome; a child that did not run is a
/// harness failure.
fn child_outcome(r: &ChildRep) -> Outcome {
    match r {
        Err(e) => Outcome::harness(e.clone()),
        Ok(r) => match r.status.as_str() {
            "pass" => Outcome::Pass,
            "fail" => {
                Outcome::platform("the child run observed a platform failure (see its report)")
            }
            "error" => Outcome::harness(format!("child errors: {:?}", r.errors)),
            s => Outcome::not_exercised(format!("child status {s}")),
        },
    }
}

fn status_of(r: &ChildRep) -> Value {
    match r {
        Ok(r) => json!(r.status),
        Err(e) => json!(e),
    }
}

/// Runs `chs <args...>` as a child in a fresh, exclusively created out dir
/// with an assigned run id and `--deadline-secs` capped to what is left of
/// this run. Its report counts only if it carries that run id.
fn child(
    ctx: &Ctx,
    name: &str,
    argv: &[&str],
    budget: u64,
    inherit_stdio: bool,
    env: &[(&str, &str)],
) -> (ChildRep, PathBuf) {
    let left = ctx
        .cutoff
        .saturating_duration_since(Instant::now())
        .as_secs()
        .saturating_sub(CHILD_MARGIN);
    let deadline = budget.min(left);
    if deadline <= 20 {
        return (
            Err("not_run: no budget left before the cutoff".into()),
            ctx.out.clone(),
        );
    }
    let (out, run_id) = match child_out(&ctx.out, name) {
        Ok(v) => v,
        Err(e) => return (Err(format!("not_run: {e}")), ctx.out.clone()),
    };
    let mut cmd = Command::new(std::env::current_exe().unwrap_or_default());
    cmd.args(argv)
        .arg("--out")
        .arg(&out)
        .args(["--run-id", &run_id])
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
    if let Err(e) = &r {
        ctx.detail(&format!("child-{name}-error"), json!(e));
    }
    (child_report(&out, &run_id), out)
}

fn checks_of<'a>(r: &'a ChildRep, id: &str) -> Vec<&'a Check> {
    r.as_ref()
        .map(|r| r.checks.iter().filter(|c| c.id == id).collect())
        .unwrap_or_default()
}

fn find_check<'a>(r: &'a ChildRep, id: &str, case: &str) -> Option<&'a Check> {
    checks_of(r, id).into_iter().rev().find(|c| c.case == case)
}

/// Copies a child's check into the probe under `case` (absent = missing; a
/// child that did not run = harness failure).
fn carry(pc: &mut Pc, case: &str, r: &ChildRep, id: &str, child_case: &str) {
    pc.req(case);
    match r {
        Err(e) => pc.put(case, Outcome::harness(e.clone()), Value::Null),
        Ok(_) => {
            if let Some(c) = find_check(r, id, child_case) {
                pc.put(case, Outcome::from_check(c), c.evidence.clone());
            }
        }
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
                json!({"status": status_of(&rep), "report": out.join("report.json")}),
            );
            if got.is_empty() {
                agg.probes.push(Probe {
                    id: id.into(),
                    verdict: "inconclusive".into(),
                    why: match &rep {
                        Err(e) => e.clone(),
                        Ok(_) => "not_run: the probe child wrote no verdict".into(),
                    },
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
        pc.put(&case, child_outcome(&r), json!({"status": status_of(&r)}));
        fake.push(json!({"mode": m.name(), "host": r.as_ref().ok().map(|r| r.host.clone())}));
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
            json!({"status": status_of(&r), "host": r.as_ref().ok().map(|r| r.host.clone())}),
        );
    }
    // Provider evidence: each child samples the ConPTY module after its first PTY launch.
    // Real cases carry A-6 outcomes, judged against the same known CLI
    // transforms (`paste::match_prompt`) as on macOS.
    let extra = json!({"fake": fake, "real": real_hosts, "console_cp": os::console_code_page(),
                       "cli_transforms_checked": paste::CLI_TRANSFORMS});
    pc.finish(ctx, extra);
}

// ---------------- C-2 ----------------

/// Query case from a child's launch details: every logged query must have a
/// reply the harness wrote (the harness is the expected responder). None
/// when the child logged no launch.
fn query_outcome(r: &Report) -> Option<(Outcome, Value)> {
    let d = r.details.as_object()?;
    let mut bad = Vec::new();
    let mut any = false;
    for (k, v) in d {
        if !k.starts_with("launch-") {
            continue;
        }
        any = true;
        for q in v["query_log"].as_array().into_iter().flatten() {
            if q["reply"].is_null() || q["harness_wrote"] != json!(true) {
                bad.push(json!({"launch": k, "kind": q["kind"], "reply": q["reply"],
                                "harness_wrote": q["harness_wrote"]}));
            }
        }
    }
    if !any {
        return None;
    }
    Some(if bad.is_empty() {
        (Outcome::Pass, json!({"unanswered": []}))
    } else {
        (
            Outcome::harness("the harness did not write a reply to every query"),
            json!({"unanswered": bad}),
        )
    })
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
                // The child passes an attended case only when a relayed key
                // was written and SessionStart followed it.
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
        match &r {
            Err(e) => pc.put(&qcase, Outcome::harness(e.clone()), Value::Null),
            Ok(rep) => {
                if let Some((o, e)) = query_outcome(rep) {
                    pc.put(&qcase, o, e);
                }
            }
        }
        ev.insert(
            name.into(),
            json!({"status": status_of(&r), "errors": r.as_ref().ok().map(|r| r.errors.clone())}),
        );
    }
    pc.finish(ctx, Value::Object(ev));
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
        pc.put(
            &format!("{label}/status"),
            child_outcome(r),
            json!({"status": status_of(r)}),
        );
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
        real.insert(a.into(), status_of(&r));
    }
    // Race escapees: fallback kills under held assignment (positive evidence
    // from the child's B-7 checks).
    let race_escapees: Vec<Value> = checks_of(&held, "B-7")
        .iter()
        .filter(|c| c.evidence.pointer("/killed_by").and_then(Value::as_str) == Some("fallback"))
        .map(|c| json!(c.case))
        .collect();
    pc.req("held/race_free");
    let held_run = match &held {
        Err(e) => Some(Outcome::harness(e.clone())),
        Ok(_) => find_check(&held, "B-7", "tree-hold-assign/no_session_survivors")
            .map(Outcome::from_check),
    };
    if let Some(run) = held_run {
        // Fallback kills count as race escapees only when the held run
        // itself was exercised (assignment succeeded, liveness known).
        let o = match run {
            Outcome::Pass | Outcome::PlatformFail { .. } => Outcome::observed(
                race_escapees.is_empty(),
                "assign-after race escapees killed by the fallback",
            ),
            o => Outcome::harness(format!("held run not exercised: {}", o.tag())),
        };
        pc.put("held/race_free", o, json!({"race_escapees": race_escapees}));
    }
    let jf = job_first(ctx);
    for (case, o) in [
        ("job_first/launched", jf.launched),
        ("job_first/zero_escapees", jf.zero_escapees),
        ("job_first/no_survivors", jf.no_survivors),
    ] {
        pc.req(case);
        pc.put(case, o, Value::Null);
    }
    let extra = json!({"real": real, "job_first": jf.evidence, "harness_job": os::self_job_info()});
    pc.finish(ctx, extra);
}

struct JobFirstResult {
    launched: Outcome,
    zero_escapees: Outcome,
    no_survivors: Outcome,
    evidence: Value,
}

/// C-3 escapees. One SUCCESSFUL job membership snapshot is taken first; then
/// every session witness that is positively live and not in it is an
/// escapee. An enumeration failure is a harness failure, never an empty job.
fn escapee_outcome(
    all_seen: bool,
    witnesses: &[(u32, u64)],
    enumerate: impl FnOnce() -> Result<Vec<u32>, String>,
    live: impl Fn(u32, u64) -> Option<bool>,
) -> (Outcome, Value) {
    let members = match enumerate() {
        Ok(m) => m,
        Err(e) => {
            return (
                Outcome::harness(format!("job enumeration failed: {e}")),
                json!({"enumeration_error": e}),
            );
        }
    };
    if !all_seen {
        return (
            Outcome::not_exercised("not every spawn-tree witness attested"),
            json!({"members": members}),
        );
    }
    let (mut escapees, mut unknown, mut exited, mut counted) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for &(pid, st) in witnesses {
        match live(pid, st) {
            Some(true) if members.contains(&pid) => counted.push(pid),
            Some(true) => escapees.push(pid),
            Some(false) => exited.push(pid),
            None => unknown.push(pid),
        }
    }
    let ev = json!({"members": members, "counted": counted, "escapees": escapees,
                    "exited_before_snapshot": exited, "lookup_failed": unknown});
    // A required witness counts only if it is positively alive and in the
    // job at this pre-stop snapshot.
    let o = if !unknown.is_empty() {
        Outcome::harness("witness liveness lookup failed")
    } else if !escapees.is_empty() {
        Outcome::platform("live session witnesses outside the job")
    } else if !exited.is_empty() {
        Outcome::not_exercised("a required witness exited before the membership snapshot")
    } else {
        Outcome::Pass
    };
    (o, ev)
}

/// Survivors among `procs`: platform failure only with positive liveness;
/// a failed lookup is a harness failure.
fn survivors_outcome(procs: &[(u32, u64)], what: &str) -> (Outcome, Value) {
    let lives: Vec<(u32, Option<bool>)> = procs
        .iter()
        .map(|&(p, st)| (p, os::liveness(p, st)))
        .collect();
    let o = if lives.iter().any(|(_, l)| l.is_none()) {
        Outcome::harness("process lookup failed")
    } else {
        Outcome::observed(
            lives.iter().all(|(_, l)| *l == Some(false)),
            format!("{what} survived"),
        )
    };
    (o, json!(lives))
}

/// One job-first launch of FakeCli `spawn-tree` (§4.6). Kill authority comes
/// only from the job, the retained process handle, and descendants recorded
/// with verified lineage during the wait; witnesses are only checked for
/// survival. Verification and fallback run before ConPTY is closed.
fn job_first(ctx: &Ctx) -> JobFirstResult {
    let not_run = |why: &str, o: Outcome| JobFirstResult {
        launched: o,
        zero_escapees: Outcome::not_exercised(why),
        no_survivors: Outcome::not_exercised(why),
        evidence: json!({"error": why}),
    };
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
        Err(e) => {
            let why = format!("receiver: {e}");
            return not_run(&why, Outcome::harness(why.clone()));
        }
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
        // The OS refused the job-first launch: positive evidence.
        Err(e) => {
            let why = format!("job-first launch failed: {e}");
            return not_run(&why, Outcome::platform(why.clone()));
        }
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
        if let Ok(ms) = jf.members() {
            for m in ms {
                found.extend(procs.iter().filter(|p| p.pid == m).cloned());
            }
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
    let ws = rx.witnesses();
    let session_ws: Vec<(u32, u64)> = ws
        .iter()
        .filter(|w| w.session)
        .map(|w| (w.pid, w.start_time))
        .collect();
    let (zero_escapees, esc_ev) = escapee_outcome(seen, &session_ws, || jf.members(), os::liveness);
    let steps = jf.terminate_job();
    std::thread::sleep(Duration::from_millis(300));
    let rec: Vec<(u32, u64)> = recorded.keys().copied().collect();
    let escaped_job: Vec<u32> = rec
        .iter()
        .filter(|(p, st)| os::is_alive(*p, *st))
        .map(|(p, _)| *p)
        .collect();
    let fallback: Vec<u32> = recorded
        .values()
        .filter(|p| os::force_kill(p.pid, p.start_time))
        .map(|p| p.pid)
        .collect();
    let mut watched = rec.clone();
    watched.extend(session_ws.iter().copied());
    let vend = Instant::now() + Duration::from_secs(5);
    let mut pre = survivors_outcome(&watched, "owned processes or session witnesses");
    while pre.0 != Outcome::Pass && Instant::now() < vend {
        std::thread::sleep(Duration::from_millis(100));
        pre = survivors_outcome(&watched, "owned processes or session witnesses");
    }
    jf.close_conpty();
    let (_, post_close) = survivors_outcome(&session_ws, "session witnesses");
    let mut rx = rx;
    rx.stop();
    JobFirstResult {
        launched: Outcome::Pass,
        zero_escapees,
        no_survivors: pre.0,
        evidence: json!({
            "branch": branch, "pid": jf.pid(), "all_witnesses_seen": seen, "escapees": esc_ev,
            "kill_steps": steps, "recorded": recorded.values().collect::<Vec<_>>(),
            "escaped_job": escaped_job, "killed_by_fallback": fallback,
            "liveness_before_conpty_close": pre.1, "witness_liveness_after_conpty_close": post_close,
        }),
    }
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
        let started_ms = self.now_ms();
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
                           "attributed": attributed, "started_ms": started_ms, "at_ms": self.now_ms()}),
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
            before_spawn: None,
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

/// Outcome of the relayed request(s) of `kind` matching `pred`: pass if one
/// was attributed; a request that was made but never attributed (relay, gate,
/// writer, or witness timeout) is a harness failure; None if never made.
fn relayed(log: &[Value], kind: &str, pred: impl Fn(&Value) -> bool) -> Option<Outcome> {
    let made: Vec<&Value> = log
        .iter()
        .filter(|v| v["kind"] == kind && pred(v))
        .collect();
    if made.is_empty() {
        None
    } else if made.iter().any(|v| v["attributed"] == true) {
        Some(Outcome::Pass)
    } else {
        Some(Outcome::harness(format!(
            "relayed {kind} never attributed: {:?}",
            made.iter()
                .map(|v| (v["state"].clone(), v["wrote"].clone()))
                .collect::<Vec<_>>()
        )))
    }
}

/// `MARKER-nnnnnn` on a screen dump.
fn marker_of(screen: &str) -> Option<u64> {
    let i = screen.find("MARKER-")?;
    screen.get(i + 7..i + 13)?.parse().ok()
}

/// Marker advancement across the viewerless interval: the marker another
/// process reads must be one the fixture emitted, and later than the one on
/// screen when the interval started. A stopped PTY reader is a harness
/// failure (the screen it serves is stale).
fn marker_outcome(
    before: Option<u64>,
    reader_ok: bool,
    client: &Value,
    emitted: &[u64],
) -> Outcome {
    let Some(m0) = before else {
        return Outcome::harness("no marker on screen before the viewerless interval");
    };
    if !reader_ok {
        return Outcome::harness("the PTY reader stopped");
    }
    if client["screen_status"].as_u64() != Some(200) {
        return Outcome::harness("screen relay failed");
    }
    let Some(m) = client["marker"].as_u64() else {
        return Outcome::platform("the screen read by another process had no marker");
    };
    if !emitted.contains(&m) {
        return Outcome::harness("marker not in the fixture's emitted list");
    }
    if m > m0 {
        Outcome::Pass
    } else if emitted.iter().any(|e| *e > m0) {
        Outcome::platform("the screen did not advance across the viewerless interval")
    } else {
        Outcome::harness("the fixture emitted no marker during the interval")
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
            pc.put("viewerless_30s", Outcome::harness(e.clone()), Value::Null);
            return pc.finish(ctx, json!({"setup_error": e}));
        }
    };
    // The interval starts only once the first marker is on screen.
    let first_by = ctx.until(Duration::from_secs(10));
    let before = loop {
        let m = marker_of(&String::from_utf8_lossy(
            &p.screen(Instant::now() + Duration::from_secs(1)),
        ));
        if m.is_some() || Instant::now() >= first_by {
            break m;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let reader_eof = l.session.io().reader_eof.clone();
    // Viewerless for the full 30 s; a cutoff that cuts it short is not exercised.
    let want = Duration::from_secs(30);
    let t = Instant::now();
    std::thread::sleep(want.min(ctx.cutoff.saturating_duration_since(Instant::now())));
    let slept = t.elapsed();
    pc.put(
        "viewerless_30s",
        if slept >= want {
            Outcome::Pass
        } else {
            Outcome::not_exercised("cut short by the run cutoff")
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
    if client.get("spawn_error").is_none() {
        pc.put(
            "marker",
            marker_outcome(
                before,
                !reader_eof.load(Ordering::SeqCst),
                &client,
                &emitted,
            ),
            json!({"before": before, "marker": client["marker"]}),
        );
    }
    let nonce = client["nonce"].as_str().unwrap_or("").to_string();
    if let Some(o) = relayed(&log, "input", |v| v["nonce"] == json!(nonce)) {
        pc.put("nonce", o, json!({"nonce": nonce}));
    }
    for k in ["down", "enter", "esc"] {
        if let Some(o) = relayed(&log, "key", |v| v["key"] == json!(k)) {
            pc.put(&format!("key/{k}"), o, Value::Null);
        }
    }
    l.receiver.set_probe(None);
    let (stop, _) = l.finish();
    pc.finish(
        ctx,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attr {
    Ours,
    Other,
    /// The owner could not be identified at callback time (no pid, or no
    /// parent known for it).
    Unattributed,
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

    /// Whether the shown window's owner (via its event-time ppid, then the
    /// sampled chain, conhost / OpenConsole included) reaches `root`.
    fn classify(&self, shown: &(u32, Option<u32>), root: u32) -> Attr {
        let m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        classify(&m, shown, root)
    }
}

fn classify(
    m: &std::collections::HashMap<u32, u32>,
    shown: &(u32, Option<u32>),
    root: u32,
) -> Attr {
    if shown.0 == 0 {
        return Attr::Unattributed;
    }
    if shown.0 == root {
        return Attr::Ours;
    }
    let mut cur = match shown.1.or_else(|| m.get(&shown.0).copied()) {
        Some(c) => c,
        None => return Attr::Unattributed,
    };
    // Walk to the top of the chain; a parent missing from the samples breaks
    // the chain, and a broken chain may have passed through `root`.
    for _ in 0..32 {
        if cur == root {
            return Attr::Ours;
        }
        if cur == 0 {
            return Attr::Other;
        }
        match m.get(&cur) {
            None => return Attr::Unattributed,
            Some(&pp) if pp == cur || pp == 0 => return Attr::Other,
            Some(&pp) => cur = pp,
        }
    }
    Attr::Unattributed
}

impl Drop for PpidRecorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Window events seen while one launch ran under its own observer.
#[derive(Default)]
struct Flash {
    observer_error: Option<String>,
    ours: Vec<(u32, Option<u32>)>,
    unattributed: Vec<(u32, Option<u32>)>,
}

impl Flash {
    fn json(&self) -> Value {
        json!({"observer_error": self.observer_error, "shown_from_our_tree": self.ours,
               "unattributed": self.unattributed})
    }
}

/// No-flash judgment for one launch. A window attributed to our tree is
/// positive evidence; an observer failure (start, quit, join, or message
/// loop) is a harness failure; any unattributed event (no owner pid, or a
/// broken parent chain) leaves the case not exercised.
fn flash_outcome(f: &Flash, launched: bool) -> Outcome {
    if let Some(e) = &f.observer_error {
        return Outcome::harness(format!("window observer failed: {e}"));
    }
    if !f.ours.is_empty() {
        return Outcome::platform("a window was shown from the harness tree");
    }
    if !launched {
        return Outcome::not_exercised("the launch did not complete in scope");
    }
    if !f.unattributed.is_empty() {
        return Outcome::not_exercised("unattributed window events");
    }
    Outcome::Pass
}

/// Runs one launch child under its own window observer.
fn observed_child(
    ctx: &Ctx,
    tree: &PpidRecorder,
    name: &str,
    argv: &[String],
    budget: u64,
    env: &[(&str, &str)],
) -> (ChildRep, Flash) {
    let obs = match os::WindowObserver::start() {
        Ok(o) => o,
        Err(e) => {
            return (
                Err(format!("not_run: window observer failed: {e}")),
                Flash {
                    observer_error: Some(e),
                    ..Default::default()
                },
            );
        }
    };
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let (r, _) = child(ctx, name, &argv, budget, false, env);
    let (shown, health) = obs.stop();
    let me = std::process::id();
    let mut f = Flash {
        observer_error: health.err(),
        ..Default::default()
    };
    for s in shown {
        match tree.classify(&s, me) {
            Attr::Ours => f.ours.push(s),
            Attr::Unattributed => f.unattributed.push(s),
            Attr::Other => {}
        }
    }
    (r, f)
}

/// The pre-trusted, unattended baseline matrix (no attended steps).
fn console_matrix(ctx: &Ctx) -> Vec<(&'static str, Vec<String>, u64)> {
    let cwd = ctx
        .args
        .cwd
        .clone()
        .unwrap_or_else(|| super::real::c2_cwd(&ctx.args.root))
        .display()
        .to_string();
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    vec![
        (
            "claude-hooks",
            v(&["real", "claude-hooks", "--cwd", &cwd]),
            600,
        ),
        (
            "codex-hooks-no-daemon",
            v(&[
                "real",
                "codex-hooks",
                "--codex-mode",
                "no-daemon",
                "--cwd",
                &cwd,
            ]),
            900,
        ),
        (
            "codex-hooks-default",
            v(&[
                "real",
                "codex-hooks",
                "--codex-mode",
                "default",
                "--cwd",
                &cwd,
            ]),
            900,
        ),
        (
            "fake-paste",
            v(&["fake", "paste", "--paste-newline", "cr"]),
            900,
        ),
    ]
}

fn probe_console(ctx: &Ctx) {
    if ctx.args.detached && std::env::var("CHS_DETACHED_CHILD").is_err() {
        return relaunch_detached(ctx);
    }
    let mut pc = Pc::new("C-5");
    let matrix = console_matrix(ctx);
    for c in ["detached", "calibration", "survival"] {
        pc.req(c);
    }
    for (name, _, _) in &matrix {
        pc.req(&format!("launch/{name}"));
        pc.req(&format!("no_flash/{name}"));
    }
    let detached = std::env::var("CHS_DETACHED_CHILD").is_ok();
    let console_window = os::console_window_present();
    pc.put(
        "detached",
        match (detached, console_window) {
            (false, _) => Outcome::not_exercised("run `chs win-probe console --detached`"),
            (true, Some(false)) => Outcome::Pass,
            (true, Some(true)) => Outcome::harness("the detached harness still has a console"),
            (true, None) => Outcome::harness("console window lookup failed"),
        },
        json!({"detached": detached, "console_window_present": console_window}),
    );
    let tree = PpidRecorder::start();
    // Calibration: a short-lived console exe, deliberately visible (no
    // CREATE_NO_WINDOW), must be seen and attributed.
    let calibrated = match os::WindowObserver::start() {
        Err(e) => {
            pc.put(
                "calibration",
                Outcome::harness(format!("window observer failed: {e}")),
                Value::Null,
            );
            false
        }
        Ok(obs) => {
            let cal = Command::new("cmd.exe").args(["/c", "exit"]).spawn();
            let cal_pid = cal.as_ref().map(|c| c.id()).ok();
            if let Ok(mut c) = cal {
                let _ = c.wait();
            }
            std::thread::sleep(Duration::from_millis(500));
            let (shown, health) = obs.stop();
            let ok = health.is_ok()
                && cal_pid.is_some_and(|p| shown.iter().any(|s| tree.classify(s, p) == Attr::Ours));
            pc.put(
                "calibration",
                match (&health, cal_pid, ok) {
                    (Err(e), _, _) => Outcome::harness(format!("window observer failed: {e}")),
                    (_, None, _) => Outcome::harness("calibration launch failed"),
                    (_, _, true) => Outcome::Pass,
                    (_, _, false) => Outcome::not_exercised("calibration window not attributed"),
                },
                json!({"pid": cal_pid, "events": shown.len(), "observer_error": health.err()}),
            );
            ok
        }
    };
    let mut launches = serde_json::Map::new();
    for (name, argv, budget) in &matrix {
        let (r, f) = observed_child(ctx, &tree, &format!("console-{name}"), argv, *budget, &[]);
        let launched = child_outcome(&r);
        let ok = launched == Outcome::Pass;
        pc.put(
            &format!("launch/{name}"),
            match launched {
                Outcome::Pass | Outcome::HarnessFail { .. } => launched,
                o => Outcome::not_exercised(format!("launch out of scope: {}", o.tag())),
            },
            json!({"status": status_of(&r)}),
        );
        let nf = flash_outcome(&f, ok && calibrated);
        let flashed = matches!(nf, Outcome::PlatformFail { .. });
        pc.put(&format!("no_flash/{name}"), nf, f.json());
        let mut entry = json!({"baseline": f.json()});
        if flashed {
            // Mitigation: rerun the SAME launch, cwd, and hook delivery with
            // the GUI-subsystem emitter.
            let (dk, fk) = (
                format!("mitigated/{name}/delivery"),
                format!("mitigated/{name}/no_flash"),
            );
            pc.req(&dk);
            pc.req(&fk);
            let gui = [("CHS_EMITTER_VARIANT", "gui")];
            let (r2, f2) = observed_child(
                ctx,
                &tree,
                &format!("console-{name}-gui"),
                argv,
                *budget,
                &gui,
            );
            let delivered = child_outcome(&r2);
            let ok2 = delivered == Outcome::Pass;
            pc.put(&dk, delivered, json!({"status": status_of(&r2)}));
            pc.put(&fk, flash_outcome(&f2, ok2 && calibrated), f2.json());
            entry["mitigated"] = f2.json();
        }
        launches.insert((*name).into(), entry);
    }
    if detached {
        let (o, ev) = terminal_close_survival(ctx);
        pc.put("survival", o, ev);
    } else {
        pc.put(
            "survival",
            Outcome::not_exercised("run `chs win-probe console --detached`"),
            Value::Null,
        );
    }
    pc.finish(
        ctx,
        json!({"launches": launches, "breakaway": std::env::var("CHS_BREAKAWAY").ok(),
               "harness_job": os::self_job_info()}),
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
                Outcome::not_exercised("the detached child reports C-5"),
                json!({"breakaway_permitted": breakaway, "breakaway_denied": !breakaway,
                       "parent_job": os::self_job_info()}),
            );
        }
        Err(e) => pc.put(
            "detached_launch",
            Outcome::harness(format!("detached launch failed: {e}")),
            json!({"error": e, "parent_job": os::self_job_info()}),
        ),
    }
    pc.finish(ctx, Value::Null);
}

/// Detached child: FakeCli `attachable` waits for an owner-confirmed terminal
/// close (`/probe/confirm`) and then a fresh nonce from the same child. A
/// platform failure needs positive death evidence for that child.
fn terminal_close_survival(ctx: &Ctx) -> (Outcome, Value) {
    let mut cred = String::new();
    let _ = std::io::stdin().lock().read_line(&mut cred);
    let credential = if cred.trim().is_empty() {
        util::token()
    } else {
        cred.trim().to_string()
    };
    let (l, base, p) = match start_attachable(ctx, credential) {
        Ok(v) => v,
        Err(e) => return (Outcome::harness(e.clone()), json!({"error": e})),
    };
    if let Ok(f) = std::env::var("CHS_ATTACH_URL_FILE") {
        let _ = std::fs::write(Path::new(&f), &base);
    }
    let start_time = p
        .witnesses
        .witnesses()
        .iter()
        .find(|w| w.role == "attachable" && w.pid == p.child_pid)
        .map(|w| w.start_time);
    let end = ctx.until(Duration::from_secs(1800));
    // The nonce must come from a request that started after the confirmation.
    let after_confirm = |log: &[Value], c: u64| {
        log.iter().any(|v| {
            v["kind"] == "input"
                && v["attributed"] == true
                && v["started_ms"].as_u64().is_some_and(|t| t > c)
        })
    };
    let mut res: Option<Outcome> = None;
    let mut last_lookup: Option<bool> = None;
    let mut confirmed = None;
    while Instant::now() < end {
        confirmed = *p.confirmed_ms.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = confirmed {
            if after_confirm(&p.log(), c) {
                res = Some(Outcome::Pass);
                break;
            }
            last_lookup = start_time.and_then(|st| os::liveness(p.child_pid, st));
            if last_lookup == Some(false) {
                // A death during the harness's own teardown is not evidence.
                res = Some(if crate::session::teardown_in_progress() {
                    Outcome::harness("harness teardown in progress when the child died")
                } else {
                    Outcome::platform("the attachable child died after the terminal closed")
                });
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let res = res.unwrap_or_else(|| match (confirmed, last_lookup) {
        (None, _) => Outcome::not_exercised("the owner never confirmed the terminal close"),
        (Some(_), None) => Outcome::harness("child liveness lookup failed"),
        (Some(_), Some(_)) => Outcome::not_exercised("no fresh input after the confirmed close"),
    });
    let log = p.log();
    l.receiver.set_probe(None);
    let (stop, _) = l.finish();
    (
        res,
        json!({"requests": log, "child_pid": p.child_pid, "survivors": stop.survivors}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// r2 C-3: an enumeration failure is a harness failure, never an empty
    /// job that turns every live witness into an escapee.
    #[test]
    fn enumeration_failure_is_harness_fail() {
        let ws = [(10, 1), (11, 1)];
        let (o, _) = escapee_outcome(true, &ws, || Err("denied".into()), |_, _| Some(true));
        assert!(matches!(o, Outcome::HarnessFail { .. }), "{o:?}");
        // A successful snapshot that misses a live witness is an escapee.
        let (o, _) = escapee_outcome(true, &ws, || Ok(vec![10]), |_, _| Some(true));
        assert!(matches!(o, Outcome::PlatformFail { .. }), "{o:?}");
        // A failed lookup is harness.
        let (o, _) = escapee_outcome(true, &ws, || Ok(vec![10]), |_, _| None);
        assert!(matches!(o, Outcome::HarnessFail { .. }), "{o:?}");
        // Every witness live and in the job: pass.
        let (o, _) = escapee_outcome(true, &ws, || Ok(vec![10, 11]), |_, _| Some(true));
        assert_eq!(o, Outcome::Pass);
    }

    /// r3 C-3: a required witness that exited before the membership snapshot
    /// does not count, so the case is not exercised rather than a false `go`.
    #[test]
    fn exited_witness_is_not_counted() {
        let ws = [(10, 1), (11, 1)];
        let (o, _) = escapee_outcome(true, &ws, || Ok(vec![10]), |p, _| Some(p == 10));
        assert!(matches!(o, Outcome::NotExercised { .. }), "{o:?}");
    }

    #[test]
    fn unattributed_events_block_no_flash() {
        let mut f = Flash {
            unattributed: vec![(0, None)],
            ..Default::default()
        };
        assert!(matches!(
            flash_outcome(&f, true),
            Outcome::NotExercised { .. }
        ));
        f.unattributed.clear();
        assert_eq!(flash_outcome(&f, true), Outcome::Pass);
        f.observer_error = Some("message loop ended on an error".into());
        assert!(matches!(
            flash_outcome(&f, true),
            Outcome::HarnessFail { .. }
        ));
        let m = std::collections::HashMap::from([(5, 4), (4, 1), (1, 0)]);
        assert_eq!(classify(&m, &(0, None), 1), Attr::Unattributed);
        assert_eq!(classify(&m, &(9, None), 1), Attr::Unattributed);
        assert_eq!(classify(&m, &(5, None), 1), Attr::Ours);
        assert_eq!(classify(&m, &(5, None), 2), Attr::Other);
        // r3: a parent missing from the samples breaks the chain.
        assert_eq!(classify(&m, &(8, Some(7)), 2), Attr::Unattributed);
    }

    #[test]
    fn relay_failures_are_harness_fail() {
        let log = vec![json!({"kind": "key", "key": "esc", "wrote": false, "attributed": false})];
        assert!(matches!(
            relayed(&log, "key", |v| v["key"] == "esc"),
            Some(Outcome::HarnessFail { .. })
        ));
        assert!(relayed(&log, "key", |v| v["key"] == "down").is_none());
    }

    #[test]
    fn marker_must_advance_across_the_interval() {
        let c = |m: u64| json!({"screen_status": 200, "marker": m});
        assert_eq!(marker_outcome(Some(3), true, &c(9), &[1, 9]), Outcome::Pass);
        assert!(matches!(
            marker_outcome(Some(3), true, &c(3), &[3, 9]),
            Outcome::PlatformFail { .. }
        ));
        // r3: a stopped reader serves a stale screen.
        assert!(matches!(
            marker_outcome(Some(3), false, &c(3), &[3, 9]),
            Outcome::HarnessFail { .. }
        ));
        assert!(matches!(
            marker_outcome(Some(3), true, &c(3), &[3]),
            Outcome::HarnessFail { .. }
        ));
        assert!(matches!(
            marker_outcome(None, true, &c(9), &[9]),
            Outcome::HarnessFail { .. }
        ));
    }

    #[test]
    fn queries_need_a_written_reply() {
        let mut r = Report::new("x", "y");
        r.detail(
            "launch-a",
            json!({"query_log": [{"kind": "da1", "reply": "x", "harness_wrote": false}]}),
        );
        let o = query_outcome(&r).map(|x| x.0);
        assert!(matches!(o, Some(Outcome::HarnessFail { .. })), "{o:?}");
        r.detail(
            "launch-a",
            json!({"query_log": [{"kind": "da1", "reply": "x", "harness_wrote": true}]}),
        );
        assert_eq!(query_outcome(&r).map(|x| x.0), Some(Outcome::Pass));
    }
}
