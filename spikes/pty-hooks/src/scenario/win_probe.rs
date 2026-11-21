//! Set C: `chs win-probe <paste|startup|kill|attach|console|all>` and
//! `attach-client`. Go/no-go screens for Windows architectural blockers
//! (§1 C, §4.8). The follow-up run (§1 D, §5 phase 3) adds `codex-input`
//! (F-1), `backend` (F-2 step 3), `followup`, and `f2-verdict`. Compiles
//! everywhere; refuses to run off Windows.
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
use crate::paste::{self, InputEncoding, Newline, NewlineKey, PasteOpts};
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

/// Follow-up run (§5 phase 3) probes, outside `all`, with their budgets
/// from the effective SessionStart timeout.
/// A probe budget from the effective SessionStart timeout.
type Budget = fn(u64) -> u64;

const F_PROBES: &[(&str, Budget)] = &[
    ("codex-input", codex_input_budget),
    ("backend", backend_budget),
    ("followup", followup_budget),
    ("rerun", rerun_budget),
    ("f2-verdict", |_| {
        // Step 5 waits for the detached step 4 run to end.
        console_budget(&F2_CONFIGS.map(String::from)) + 2 * CHILD_MARGIN + 120
    }),
];

/// Corpus F: verdict cases and recorded-only fixtures (a test checks both
/// against `paste::corpus_f`).
const F1_CASES: u64 = 18;
const F1_RECORDED: u64 = 3;
/// One `real codex-paste` F-1 child (one fresh session per case), sized so
/// a run in which every case first fails to start once (retried) and then
/// succeeds at its worst-case timing under the effective SessionStart
/// timeout `sst` fits.
const fn f1_case(sst: u64) -> u64 {
    super::real::keys_failed_start_secs(sst) + super::real::keys_case_worst_secs(sst)
}
const fn f1_child(sst: u64) -> u64 {
    F1_CASES * f1_case(sst) + CHILD_MARGIN
}
const fn f1_recorded_child(sst: u64) -> u64 {
    F1_RECORDED * f1_case(sst) + CHILD_MARGIN
}
fn codex_input_budget(sst: u64) -> u64 {
    2 * f1_child(sst) + f1_recorded_child(sst) + 3 * CHILD_MARGIN + 60
}
/// Per configuration: the comparison rows; plus the F-2c injections.
fn backend_budget(sst: u64) -> u64 {
    3 * (600 + 900 + 2400 + f1_child(sst) + 2 * 600 + 600 + 7 * CHILD_MARGIN)
        + 5 * (300 + CHILD_MARGIN)
        + 60
}
fn followup_budget(sst: u64) -> u64 {
    1800 + codex_input_budget(sst) + backend_budget(sst) + 1800 + 4 * CHILD_MARGIN + 60
}

/// Margin a parent gives a child beyond the child's own deadline.
const CHILD_MARGIN: u64 = 30;

pub fn run(mut args: Args) -> i32 {
    let sub = args.positional.get(1).cloned().unwrap_or_default();
    if sub == "attach-client" {
        return attach_client(&args);
    }
    let budget = match sub.as_str() {
        "all" => PROBES.iter().map(|p| p.1 + CHILD_MARGIN).sum::<u64>() + 60,
        s => match PROBES.iter().find(|p| p.0 == s).map(|p| p.1).or_else(|| {
            F_PROBES
                .iter()
                .find(|p| p.0 == s)
                .map(|p| (p.1)(args.session_start_timeout))
        }) {
            Some(b) => b,
            None => {
                eprintln!("chs win-probe: unknown probe {sub:?}\n{}", cli::USAGE);
                return 2;
            }
        },
    };
    let budget = match sub.as_str() {
        "console" => console_budget(&console_configs(&args)),
        _ => budget,
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
        "codex-input" => probe_codex_input(&ctx, &NewlineKey::ALL),
        "backend" => probe_backend(&ctx, ctx.args.f1_winner, None),
        "rerun" => rerun(&ctx),
        "followup" => followup(&ctx),
        "f2-verdict" => f2_verdict(&ctx),
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
    // Forwarded options first: a child's own flags (e.g. a configuration's
    // backend flags) come later and win.
    cmd.args(cli::forward_common(&ctx.args))
        .args(argv)
        .arg("--out")
        .arg(&out)
        .args(["--run-id", &run_id])
        .args(["--deadline-secs", &deadline.to_string()]);
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
        let enc = ctx.args.encoding_for(agent);
        carry_c1(&mut pc, agent, enc, &entries, &r);
        real_hosts.insert(
            agent.into(),
            json!({"status": status_of(&r), "encoding": enc.name(),
                   "host": r.as_ref().ok().map(|r| r.host.clone())}),
        );
    }
    // Provider evidence: each child samples the ConPTY module after its first PTY launch.
    // Real cases carry A-6 outcomes, judged against the same known CLI
    // transforms (`paste::match_prompt`) as on macOS.
    let extra = json!({"fake": fake, "real": real_hosts, "console_cp": os::console_code_page(),
                       "cli_transforms_checked": paste::CLI_TRANSFORMS});
    pc.finish(ctx, extra);
}

/// The A-6 cases a C-1 child must report for `agent` under the encoding it
/// actually uses: `<agent>/<mode>/c<chunk>/<entry>` for bracketed paste (all
/// newline modes, chunk 0 and 7), `<agent>/keys/<newline-key>/<entry>` for
/// `win_keys` (one newline key, no chunk sweep).
pub fn c1_child_cases(agent: &str, enc: InputEncoding, entries: &[String]) -> Vec<String> {
    match enc {
        InputEncoding::BracketedPaste => Newline::ALL
            .iter()
            .flat_map(|m| {
                [0usize, 7].into_iter().flat_map(move |c| {
                    entries
                        .iter()
                        .map(move |e| super::real::paste_case(agent, *m, c, e))
                })
            })
            .collect(),
        InputEncoding::WinKeys { newline } => entries
            .iter()
            .map(|e| super::real::keys_case(agent, newline, e))
            .collect(),
    }
}

fn carry_c1(pc: &mut Pc, agent: &str, enc: InputEncoding, entries: &[String], r: &ChildRep) {
    for c in c1_child_cases(agent, enc, entries) {
        carry(pc, &format!("real/{c}"), r, "A-6", &c);
    }
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

/// What the C-4 resize step observed.
pub struct ResizeSeen {
    pub resized: Result<(), String>,
    /// The child reported the new console size.
    pub child_reported: bool,
    pub report_present: bool,
    /// The harness screen's `(cols, rows)` after the resize.
    pub screen: (usize, usize),
    pub want: (u16, u16),
    /// The marker on screen at the resize and after it.
    pub marker_before: Option<u64>,
    pub marker_after: Option<u64>,
    /// The first marker the child printed after it observed the new size.
    pub size_marker: Option<u64>,
}

/// C-4 resize: the child must report the new size, the harness's screen
/// must have the new geometry (a stale grid is the harness's own failure),
/// and that screen must show the first marker the child printed after it
/// observed the new size, or a later one (also one printed after the
/// child's last report).
pub fn resize_outcome(r: &ResizeSeen, emitted: &[u64]) -> Outcome {
    match (&r.resized, r.child_reported) {
        (Err(e), _) => return Outcome::harness(format!("resize failed: {e}")),
        (Ok(()), true) => {}
        (Ok(()), false) if !r.report_present => {
            return Outcome::harness("the child sent no report after the resize");
        }
        (Ok(()), false) => return Outcome::platform("the child never reported the new size"),
    }
    let want = (r.want.0 as usize, r.want.1 as usize);
    if r.screen != want {
        return Outcome::harness(format!(
            "the harness screen is {:?} after a resize to {want:?}",
            r.screen
        ));
    }
    if r.marker_before.is_none() {
        return Outcome::harness("no marker on screen at the resize");
    }
    let Some(sm) = r.size_marker else {
        return Outcome::harness("the child recorded no marker after it saw the new size");
    };
    // Markers are monotonic: one newer than the last report still counts.
    match r.marker_after {
        Some(m) if m >= sm => Outcome::Pass,
        _ if emitted.iter().any(|e| *e >= sm) => Outcome::platform(
            "the resized screen shows no marker drawn after the child saw the new size",
        ),
        _ => Outcome::harness("the fixture emitted no marker after the resize"),
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
        "resize",
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
    // One resize (F-2): 120x40 → 100x30, which the child must report, and
    // after which the harness's screen has the new geometry and shows a
    // marker the child drew after the resize.
    let (cols, rows) = (100u16, 30u16);
    let at_resize = marker_of(&String::from_utf8_lossy(
        &p.screen(Instant::now() + Duration::from_secs(1)),
    ));
    let resized = l.session.io().resize(cols, rows);
    let want = json!({"cols": cols, "rows": rows});
    let seen = resized.is_ok()
        && p.wait_report(ctx.until(Duration::from_secs(10)), |r| {
            r["console_size"] == want
        })
        .is_some();
    // The first marker printed after the child observed the new size.
    let size_marker = p
        .wait_report(ctx.until(Duration::from_secs(2)), |r| {
            r["size_marker"]["cols"] == want["cols"] && r["size_marker"]["rows"] == want["rows"]
        })
        .and_then(|r| r["size_marker"]["marker"].as_u64());
    // Two marker ticks after the size report, so the resized screen is drawn.
    std::thread::sleep(
        Duration::from_millis(1200).min(ctx.cutoff.saturating_duration_since(Instant::now())),
    );
    let snap = l.snapshot();
    let after = marker_of(&snap.lines.join("\n"));
    let emitted: Vec<u64> = p
        .report()
        .and_then(|r| r["markers_emitted"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_u64)
        .collect();
    let rs = ResizeSeen {
        resized: resized.clone(),
        child_reported: seen,
        report_present: p.report().is_some(),
        screen: (snap.cols, snap.rows),
        want: (cols, rows),
        marker_before: at_resize,
        marker_after: after,
        size_marker,
    };
    pc.put(
        "resize",
        resize_outcome(&rs, &emitted),
        json!({"want": {"cols": cols, "rows": rows}, "reported": p.report().map(|r| r["console_size"].clone()),
               "screen": {"cols": snap.cols, "rows": snap.rows}, "marker_at_resize": at_resize,
               "marker_after": after, "size_marker": size_marker}),
    );
    l.receiver.set_probe(None);
    let (stop, _) = l.finish();
    pc.finish(
        ctx,
        json!({"client": client, "requests": log, "survivors": stop.survivors,
               "launch": stop.launch}),
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
    let marker = screen
        .as_ref()
        .ok()
        .and_then(|(_, b)| marker_of(&String::from_utf8_lossy(b)));
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

// ---------------- F-1 / F-2 follow-up (§4.8, §5 phase 3) ----------------

/// Completeness of one case set: every case passed; complete with a
/// platform failure; or incomplete (missing, harness failure, not exercised).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sel {
    Pass,
    Fail,
    Incomplete,
}

pub fn sel_of(e: &verdict::Eval) -> Sel {
    if !e.complete() || (e.passed.is_empty() && e.platform_fail.is_empty()) {
        Sel::Incomplete
    } else if e.platform_fail.is_empty() {
        Sel::Pass
    } else {
        Sel::Fail
    }
}

/// F-1b: `csi-u` wins whenever it passes; else `w32` if it passes. Both
/// complete with failures → `fail_observed`; anything else `inconclusive`.
pub fn f1_select(csi_u: Sel, w32: Sel) -> (&'static str, Option<NewlineKey>) {
    match (csi_u, w32) {
        (Sel::Pass, _) => ("go", Some(NewlineKey::CsiU)),
        (_, Sel::Pass) => ("go", Some(NewlineKey::W32)),
        (Sel::Fail, Sel::Fail) => ("fail_observed", None),
        _ => ("inconclusive", None),
    }
}

/// F-2b: candidates are judged only against a complete, passing `P`; the
/// first selectable of `R0`, `R6` wins, and one candidate's result never
/// affects the other's eligibility; both complete with failures →
/// `fail_observed`; anything else `inconclusive`.
pub fn f2_select(p: Sel, r0: Sel, r6: Sel) -> (&'static str, Option<&'static str>) {
    if p != Sel::Pass {
        return ("inconclusive", None);
    }
    match (r0, r6) {
        (Sel::Pass, _) => ("go", Some("R0")),
        (_, Sel::Pass) => ("go", Some("R6")),
        (Sel::Fail, Sel::Fail) => ("fail_observed", None),
        _ => ("inconclusive", None),
    }
}

fn f1_entries() -> Vec<String> {
    paste::corpus_f()
        .into_iter()
        .filter(|e| !e.recorded)
        .map(|e| e.id)
        .collect()
}

/// F-1 (`codex-input`): the corpus F sweep once per newline key in `keys`,
/// each case in its own fresh Codex session; recorded fixtures once with
/// the winner (or the first key), outside the verdict. A key not run
/// counts as incomplete for the selection.
fn probe_codex_input(ctx: &Ctx, keys: &[NewlineKey]) {
    let entries = f1_entries();
    let mut checks = Vec::new();
    let mut variants = serde_json::Map::new();
    let mut sels = [Sel::Incomplete; 2];
    for &nk in keys {
        let (r, out) = child(
            ctx,
            &format!("codex-input-{}", nk.name()),
            &[
                "real",
                "codex-paste",
                "--subset",
                "f1",
                "--newline-key",
                nk.name(),
            ],
            f1_child(ctx.args.session_start_timeout),
            false,
            &[],
        );
        let mut pc = Pc::new("F-1");
        for e in &entries {
            let c = super::real::keys_case("codex", nk, e);
            carry(&mut pc, &c, &r, "A-6", &c);
        }
        let e = verdict::evaluate(&pc.required, &pc.checks);
        let sel = sel_of(&e);
        sels[NewlineKey::ALL.iter().position(|k| *k == nk).unwrap_or(0)] = sel;
        variants.insert(
            nk.name().into(),
            json!({"sel": format!("{sel:?}"), "eval": e.json(), "status": status_of(&r),
                   "report": out.join("report.json")}),
        );
        checks.extend(pc.checks);
    }
    let (v, winner) = f1_select(sels[0], sels[1]);
    let rec_key = winner.unwrap_or(keys[0]);
    let (r, out) = child(
        ctx,
        "codex-input-recorded",
        &[
            "real",
            "codex-paste",
            "--subset",
            "f1-recorded",
            "--newline-key",
            rec_key.name(),
        ],
        f1_recorded_child(ctx.args.session_start_timeout),
        false,
        &[],
    );
    let recorded = json!({"newline_key": rec_key.name(), "status": status_of(&r),
        "report": out.join("report.json"),
        "entries": r.as_ref().ok().map(|r| r.details["f1_recorded"].clone())});
    let w = winner.map(|k| k.name());
    ctx.detail("f1_winner", json!(w));
    ctx.sink.with(|rep| {
        rep.probes.push(Probe {
            id: "F-1".into(),
            verdict: v.into(),
            why: format!("csi-u {:?}, w32 {:?}; winner {w:?}", sels[0], sels[1]),
            evidence: json!({"variants": variants, "cases": checks, "winner": w,
                             "extra": {"recorded": recorded}}),
        });
    });
}

/// One F-2a comparison row: its name, child argv (before the
/// configuration's flags; `None` = not runnable), budget, and the child
/// check ids that decide it (empty: the child's C-4 probe verdict).
struct Row {
    name: &'static str,
    argv: Option<Vec<String>>,
    budget: u64,
    ids: &'static [&'static str],
}

fn backend_rows(winner: Option<NewlineKey>, sst: u64) -> Vec<Row> {
    let v = |a: &[&str]| Some(a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    let row = |name, argv, budget, ids| Row {
        name,
        argv,
        budget,
        ids,
    };
    vec![
        row(
            "claude_hooks",
            v(&["real", "claude-hooks", "--prompt"]),
            600,
            &["A-1", "A-2"],
        ),
        row(
            "codex_hooks",
            v(&[
                "real",
                "codex-hooks",
                "--launches",
                "1",
                "--codex-mode",
                "no-daemon",
            ]),
            900,
            &["A-3"],
        ),
        row(
            "claude_paste",
            v(&[
                "real",
                "claude-paste",
                "--subset",
                "required",
                "--paste-newline",
                "cr",
            ]),
            2400,
            &["A-6"],
        ),
        row(
            "codex_input",
            winner.and_then(|k| {
                v(&[
                    "real",
                    "codex-paste",
                    "--subset",
                    "f1",
                    "--newline-key",
                    k.name(),
                ])
            }),
            f1_child(sst),
            &["A-6"],
        ),
        row(
            "tree_kill_claude",
            v(&["real", "tree-kill", "--agent", "claude"]),
            600,
            &["A-8"],
        ),
        row(
            "tree_kill_codex",
            v(&["real", "tree-kill", "--agent", "codex"]),
            600,
            &["A-8"],
        ),
        row("attach", v(&["win-probe", "attach"]), 600, &[]),
    ]
}

/// A child's outcome over its own required cases with the given check ids
/// (empty ids: the child's C-4 probe verdict).
fn child_cases_outcome(r: &ChildRep, ids: &[&str]) -> (Outcome, Value) {
    let rep = match r {
        Err(e) => return (Outcome::harness(e.clone()), Value::Null),
        Ok(rep) => rep,
    };
    let st = json!({"status": rep.status, "errors": rep.errors});
    if ids.is_empty() {
        let p = rep.probes.iter().find(|p| p.id == "C-4");
        let o = match p.map(|p| (p.verdict.as_str(), p.why.clone())) {
            Some(("go", _)) => Outcome::Pass,
            Some(("fail_observed", why)) => Outcome::platform(why),
            Some((_, why)) => Outcome::not_exercised(why),
            None => Outcome::Missing,
        };
        return (o, st);
    }
    let in_ids = |k: &str| ids.iter().any(|i| k.starts_with(&format!("{i} ")));
    let req: Vec<String> = rep.required.iter().filter(|k| in_ids(k)).cloned().collect();
    let checks: Vec<Check> = rep
        .checks
        .iter()
        .filter(|c| ids.contains(&c.id.as_str()))
        .cloned()
        .collect();
    let e = verdict::evaluate(&req, &checks);
    let o = if !rep.errors.is_empty() {
        Outcome::harness(format!("child errors: {:?}", rep.errors))
    } else if req.is_empty() {
        Outcome::harness("the child declared no cases for this row")
    } else if let Some((k, c)) = e.harness_fail.first() {
        Outcome::harness(format!("{k}: {c}"))
    } else if !e.missing.is_empty() {
        Outcome::Missing
    } else if let Some((k, c)) = e.not_exercised.first() {
        Outcome::not_exercised(format!("{k}: {c}"))
    } else if let Some((k, c)) = e.platform_fail.first() {
        Outcome::platform(format!("{k}: {c}"))
    } else {
        Outcome::Pass
    };
    (o, json!({"status": st, "eval": e.json()}))
}

/// Every `launch` record (backend, branch, ConPTY module) in a report.
fn launch_records(v: &Value, out: &mut Vec<Value>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                if k == "launch" && x.get("backend").is_some() {
                    out.push(x.clone());
                } else {
                    launch_records(x, out);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| launch_records(x, out)),
        _ => {}
    }
}

/// Every launch record in a child's report (details and probe evidence).
fn child_launches(r: &ChildRep) -> Vec<Value> {
    let mut launches = Vec::new();
    if let Ok(rep) = r {
        launch_records(&rep.details, &mut launches);
        for p in &rep.probes {
            launch_records(&p.evidence, &mut launches);
        }
    }
    launches
}

/// The backend and ConPTY flags a configuration's launches must carry.
fn config_launch(config: &str) -> Option<(&'static str, Option<u64>)> {
    match config {
        "P" => Some(("portable", None)),
        "R0" => Some(("raw", Some(0))),
        "R6" => Some(("raw", Some(6))),
        _ => None,
    }
}

/// F-2b launch rule for a row of `config`: every launch the child made
/// must carry that configuration's backend and flags, and a candidate counts
/// only on the JOB_LIST branch. A positively observed JOB_LIST rejection (the
/// `CreateProcessW` call with the job list failed) is a branch failure, also
/// when the fallback failed too; any other launch off the JOB_LIST branch
/// (e.g. an attribute-setup failure), a mismatched launch, or no launch
/// record (a failed lookup) is a harness failure. A raw launch's teardown
/// record (ordinary or failed launch) with a hung close is a branch
/// failure; one whose drain ended on a read error is a harness failure.
pub fn branch_gate(config: &str, o: Outcome, launches: &[Value]) -> Outcome {
    let Some((backend, flags)) = config_launch(config) else {
        return Outcome::harness(format!("unknown configuration {config}"));
    };
    // Every launch must carry this configuration's backend and flags before
    // any of them can say something about it (a JOB_LIST rejection included).
    for l in launches {
        if l["backend"] != json!(backend) || flags.is_some_and(|f| l["conpty_flags"] != json!(f)) {
            return Outcome::harness(format!("launch {l} does not match configuration {config}"));
        }
    }
    if backend == "raw"
        && launches
            .iter()
            .any(|l| l["job_list_rejected"] == json!(true))
    {
        return Outcome::platform("JOB_LIST rejected: CreateProcessW with the job list failed");
    }
    if matches!(o, Outcome::Missing | Outcome::HarnessFail { .. }) {
        return o;
    }
    if launches.is_empty() {
        return Outcome::harness("branch lookup failed: no launch record");
    }
    for l in launches {
        if backend == "raw" && l["branch"] != json!("job_list") {
            return Outcome::harness(format!(
                "launch on branch {} without an observed JOB_LIST rejection",
                l["branch"]
            ));
        }
    }
    // A raw launch's teardown: the close must return in bound and the
    // drain must not end on a read error.
    for t in launches.iter().map(|l| &l["teardown"]) {
        if t["close_hung"] == json!(true) {
            return Outcome::platform("ClosePseudoConsole did not return within its bound");
        }
        if let Some(e) = t["drain_end"].as_str().filter(|e| *e != "eof") {
            return Outcome::harness(format!("the output drain ended on a read {e}"));
        }
    }
    o
}

const F2_CONFIGS: [&str; 3] = ["P", "R0", "R6"];
const F2C_INJECTS: [&str; 5] = [
    "force-suspended",
    "before-spawn",
    "after-spawn",
    "reader-start",
    "resume-fail",
];

/// F-2 step 3 (`backend`): the comparison rows (all, or those named in
/// `only`) under every configuration as cases `<row>/<config>`, the Codex
/// input row with `f1_winner`; F-2c injections under `R0` as their own probe.
fn probe_backend(ctx: &Ctx, f1_winner: Option<NewlineKey>, only: Option<&[&str]>) {
    let configs: Vec<String> = if ctx.args.configs.is_empty() {
        F2_CONFIGS.iter().map(|c| c.to_string()).collect()
    } else {
        ctx.args.configs.clone()
    };
    let winner = f1_winner.map(|k| k.name());
    let mut rows = backend_rows(f1_winner, ctx.args.session_start_timeout);
    if let Some(only) = only {
        rows.retain(|r| only.contains(&r.name));
    }
    let mut pc = Pc::new("F-2a");
    for cfg in &configs {
        for row in &rows {
            pc.req(&format!("{}/{cfg}", row.name));
        }
    }
    for cfg in &configs {
        let flags = cli::config_flags(cfg).unwrap_or_default();
        for row in &rows {
            let case = format!("{}/{cfg}", row.name);
            let Some(argv) = &row.argv else {
                pc.put(
                    &case,
                    Outcome::not_exercised("no F-1 winner"),
                    json!({"config": cfg}),
                );
                continue;
            };
            let mut a: Vec<&str> = argv.iter().map(String::as_str).collect();
            a.extend(flags.iter().map(String::as_str));
            let (r, out) = child(
                ctx,
                &format!("backend-{cfg}-{}", row.name),
                &a,
                row.budget,
                false,
                &[],
            );
            let (o, ev) = child_cases_outcome(&r, row.ids);
            let launches = child_launches(&r);
            let o = branch_gate(cfg, o, &launches);
            pc.put(
                &case,
                o,
                json!({"config": cfg, "child": ev, "launches": launches,
                       "report": out.join("report.json")}),
            );
        }
    }
    ctx.sink.with(|r| {
        r.config["configs"] = json!(configs);
        r.config["f1_winner"] = json!(winner);
    });
    pc.finish(ctx, json!({"f1_winner": winner}));
    // F-2c: FakeCli `spawn-tree` under R0 with injected failures.
    let mut fc = Pc::new("F-2c");
    let r0 = cli::config_flags("R0").unwrap_or_default();
    for inj in F2C_INJECTS {
        fc.req(inj);
        let mut a = vec!["fake", "raw-inject", "--raw-inject", inj];
        a.extend(r0.iter().map(String::as_str));
        let (r, out) = child(ctx, &format!("f2c-{inj}"), &a, 300, false, &[]);
        let (o, ev) = child_cases_outcome(&r, &["F-2c"]);
        fc.put(
            inj,
            o,
            json!({"child": ev, "report": out.join("report.json")}),
        );
    }
    fc.finish(ctx, Value::Null);
}

/// Step 3 (`followup`): `fake all`, `codex-input`, `backend` with F-1's
/// winner, and C-2's Codex part, each a child with its own budget; then
/// prints the step 4 command.
fn followup(ctx: &Ctx) {
    let mut steps = serde_json::Map::new();
    let mut take = |name: &str, r: &ChildRep, out: &Path, probe: Option<&str>| {
        steps.insert(
            name.into(),
            json!({"status": status_of(r), "report": out.join("report.json")}),
        );
        ctx.sink.with(|agg| match (r, probe) {
            (Ok(rep), _) => agg.probes.extend(rep.probes.iter().cloned()),
            (Err(e), Some(id)) => agg.probes.push(Probe {
                id: id.into(),
                verdict: "inconclusive".into(),
                why: e.clone(),
                evidence: json!({"out": out}),
            }),
            (Err(_), None) => {}
        });
    };
    let (r, out) = child(ctx, "fake-all", &["fake", "all"], 1800, false, &[]);
    take("fake_all", &r, &out, None);
    let (r, out) = child(
        ctx,
        "codex-input",
        &["win-probe", "codex-input"],
        codex_input_budget(ctx.args.session_start_timeout),
        false,
        &[],
    );
    let winner = r
        .as_ref()
        .ok()
        .and_then(|rep| rep.details["f1_winner"].as_str())
        .and_then(NewlineKey::parse);
    take("codex_input", &r, &out, Some("F-1"));
    let w = winner.map_or("none", |k| k.name());
    let (r, backend_out) = child(
        ctx,
        "backend",
        &["win-probe", "backend", "--f1-winner", w],
        backend_budget(ctx.args.session_start_timeout),
        false,
        &[],
    );
    let join = r.as_ref().ok().map(|rep| rep.run_id.clone());
    take("backend", &r, &backend_out, Some("F-2a"));
    let cwd = super::real::c2_cwd(&ctx.args.root).display().to_string();
    let (r, out) = child(
        ctx,
        "codex-hooks-c2",
        &[
            "real",
            "codex-hooks",
            "--launches",
            "3",
            "--codex-mode",
            "both",
            "--cwd",
            &cwd,
        ],
        1800,
        false,
        &[],
    );
    take("codex_hooks_c2", &r, &out, None);
    let exe = std::env::current_exe().unwrap_or_default();
    let next = join
        .as_ref()
        .map(|j| step4_command(&exe, j, winner, &ctx.args));
    ctx.detail("steps", json!(steps));
    ctx.detail("f1_winner", json!(winner.map(|k| k.name())));
    ctx.detail("step3_report_dir", json!(backend_out));
    ctx.detail("step4_command", json!(next));
    match next {
        Some(c) => {
            println!("step 3 done. F-1 winner: {w}. Step 3 report dir (f2-verdict --from):");
            println!("  {}", backend_out.display());
            println!("step 4, from this terminal:\n  {c}");
        }
        None => println!("step 3: the backend child wrote no report; no step 4 command"),
    }
}

/// Rows of the narrow rerun (§5 phase 3b).
const RERUN_ROWS: [&str; 3] = ["tree_kill_claude", "tree_kill_codex", "codex_input"];

/// Narrow rerun budget: F-1 `w32` (corpus and recorded), the rerun rows
/// under every configuration, and the F-2c injections.
fn rerun_budget(sst: u64) -> u64 {
    f1_child(sst)
        + f1_recorded_child(sst)
        + 3 * (600 + 600 + f1_child(sst) + 3 * CHILD_MARGIN)
        + 5 * (300 + CHILD_MARGIN)
        + 2 * CHILD_MARGIN
        + 60
}

/// `rerun` (§5 phase 3b): F-1 with the `w32` newline key only, F-2a's
/// `tree_kill_claude`, `tree_kill_codex`, and `codex_input` (`w32`) under
/// `P`, `R0`, `R6`, and F-2c. No detached console step and no F-2 verdict:
/// the orchestrator reads the raw reports. Ends with the bundle command.
fn rerun(ctx: &Ctx) {
    println!("rerun: about 40 minutes; leave the PC idle until it prints the bundle command");
    probe_codex_input(ctx, &[NewlineKey::W32]);
    probe_backend(ctx, Some(NewlineKey::W32), Some(&RERUN_ROWS));
    let exe = std::env::current_exe().unwrap_or_default();
    println!(
        "rerun done in {} min (estimated about 40 min). Report dir:\n  {}",
        ctx.t0.elapsed().as_secs() / 60,
        ctx.out.display()
    );
    println!(
        "next: {}  (writes C:\\chs\\chs-out-windows.zip; review it, then send it back)",
        ps_invocation(&exe, &["bundle".to_string()])
    );
}

/// The step 4 command `followup` prints: `console --detached` over every
/// configuration, joined to step 3's run id, with F-1's winning newline key.
pub fn step4_command(exe: &Path, join: &str, winner: Option<NewlineKey>, a: &Args) -> String {
    let mut v: Vec<String> = [
        "win-probe",
        "console",
        "--detached",
        "--configs",
        "P,R0,R6",
        "--join-run-id",
        join,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Some(k) = winner {
        v.extend(["--newline-key".into(), k.name().into()]);
    }
    for (f, b) in [
        ("--claude-bin", &a.claude_bin),
        ("--codex-bin", &a.codex_bin),
    ] {
        if let Some(b) = b {
            v.extend([f.into(), b.display().to_string()]);
        }
    }
    v.push("--allow-api-usage".into());
    ps_invocation(exe, &v)
}

/// A PowerShell-valid invocation of `exe`: the call operator and every
/// argument that needs it single-quoted (literal in PowerShell).
pub fn ps_invocation(exe: &Path, args: &[String]) -> String {
    let q = |s: &str| {
        if !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.,:/\\=".contains(c))
        {
            s.to_string()
        } else {
            format!("'{}'", s.replace('\'', "''"))
        }
    };
    let mut s = format!("& '{}'", exe.display().to_string().replace('\'', "''"));
    for a in args {
        s.push(' ');
        s.push_str(&q(a));
    }
    s
}

/// The cases F-2b needs for one configuration: step 3's rows, and from
/// step 4 the detached run's observer health (detached, calibration, both
/// controls) and the C-5 observation of Claude and Codex (`--no-daemon`).
fn f2_required(config: &str) -> Vec<String> {
    let mut v: Vec<String> = backend_rows(None, cli::SESSION_START_DEFAULT)
        .iter()
        .map(|r| verdict::key("F-2a", &format!("{}/{config}", r.name)))
        .collect();
    for c in CONSOLE_BASE.iter().filter(|c| **c != "survival") {
        v.push(verdict::key("C-5", c));
    }
    for name in ["claude-hooks", "codex-hooks-no-daemon"] {
        v.push(verdict::key("C-5", &format!("launch/{config}/{name}")));
        v.push(verdict::key("C-5", &format!("no_flash/{config}/{name}")));
    }
    v
}

fn probe_cases(r: &Report, id: &str) -> Vec<Check> {
    r.probes
        .iter()
        .filter(|p| p.id == id)
        .flat_map(|p| {
            serde_json::from_value::<Vec<Check>>(p.evidence["cases"].clone()).unwrap_or_default()
        })
        .collect()
}

/// F-2b over the joined step 3 (`backend`) and step 4 (`console`) reports.
/// They join only when step 4's `join_run_id` is step 3's run id; a case
/// missing from either leaves its configuration incomplete.
pub fn f2_evaluate(step3: &Report, step4: &Report) -> (&'static str, Option<&'static str>, Value) {
    let joined = step4.config["join_run_id"].as_str() == Some(step3.run_id.as_str());
    let all: Vec<Check> = if joined {
        probe_cases(step3, "F-2a")
            .into_iter()
            .chain(probe_cases(step4, "C-5"))
            .collect()
    } else {
        Vec::new()
    };
    let mut per = serde_json::Map::new();
    let mut sels = Vec::new();
    for cfg in F2_CONFIGS {
        let req = f2_required(cfg);
        let checks: Vec<Check> = all
            .iter()
            .filter(|c| req.contains(&verdict::key(&c.id, &c.case)))
            .cloned()
            .collect();
        let e = verdict::evaluate(&req, &checks);
        let sel = sel_of(&e);
        sels.push(sel);
        per.insert(
            cfg.into(),
            json!({"sel": format!("{sel:?}"), "eval": e.json()}),
        );
    }
    let (v, selected) = f2_select(sels[0], sels[1], sels[2]);
    (
        v,
        selected,
        json!({"joined": joined, "step3_run_id": step3.run_id,
               "step4_join_run_id": step4.config["join_run_id"], "configs": per}),
    )
}

/// Step 5 (`f2-verdict --from <step 3 dir> --from <step 4 dir>`).
fn f2_verdict(ctx: &Ctx) {
    let read = |i: usize| -> Result<Report, String> {
        let d = ctx
            .args
            .from
            .get(i)
            .ok_or("needs --from <step 3 dir> --from <step 4 dir>")?;
        crate::report::read_report(&d.join("report.json"))
            .ok_or_else(|| format!("{}: no readable report.json", d.display()))
    };
    // The detached step 4 run writes its report only when it ends.
    let step4 = match ctx.args.from.get(1) {
        None => Err("needs --from <step 3 dir> --from <step 4 dir>".to_string()),
        Some(d) => wait_final_report(
            d,
            ctx.cutoff,
            Duration::from_secs(2),
            Duration::from_secs(60),
            |m| println!("{m}"),
        ),
    };
    if let Err(e) = &step4 {
        println!("f2-verdict: {e}");
        ctx.detail("from", json!(ctx.args.from));
        ctx.error(format!("step 4 report: {e}"));
        return;
    }
    let (v, why, sel, ev) = match (read(0), step4) {
        (Ok(s3), Ok(s4)) => {
            let (v, sel, ev) = f2_evaluate(&s3, &s4);
            (v, format!("selected: {sel:?}"), sel, ev)
        }
        (a, b) => (
            "inconclusive",
            format!("not joined: {:?} {:?}", a.err(), b.err()),
            None,
            Value::Null,
        ),
    };
    ctx.detail("from", json!(ctx.args.from));
    ctx.sink.with(|r| {
        r.probes.push(Probe {
            id: "F-2".into(),
            verdict: v.into(),
            why,
            evidence: json!({"selected": sel, "evaluation": ev}),
        });
    });
}

// ---------------- C-5 console ----------------

/// Records pid -> (ppid, start time) every 300 ms; window events also carry
/// their owner's ppid and start time captured at event time, which
/// attributes short-lived owners.
struct PpidRecorder {
    map: Arc<Mutex<std::collections::HashMap<u32, (u32, u64)>>>,
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
                        .insert(p.pid, (p.ppid, p.start_time));
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        });
        PpidRecorder { map, stop }
    }

    fn snapshot(&self) -> std::collections::HashMap<u32, (u32, u64)> {
        self.map.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Drop for PpidRecorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attr {
    Ours,
    Ambiguous,
    Other,
}

/// The observed launch: its root `(pid, start time)` (the process the
/// harness created inside the observation job).
#[derive(Debug, Clone, Copy)]
pub struct LaunchId {
    pub root: (u32, u64),
}

const CONSOLE_HOSTS: &[&str] = &["conhost.exe", "openconsole.exe", "windowsterminal.exe"];

fn image_name(e: &os::WindowEvent) -> Option<String> {
    e.image.as_ref().map(|i| {
        i.rsplit(['\\', '/'])
            .next()
            .unwrap_or(i)
            .to_ascii_lowercase()
    })
}

/// C-5 classification of one window event during a launch (§4.8), for
/// `OBJID_WINDOW` / `CHILDID_SELF` events only (others: `None`, ignored).
/// `ours`: the owner was in the observation job at callback time, or its
/// ancestry, matched by pid and start time, reaches the launch root.
/// `ambiguous`: a console host / terminal owner that is not provably ours,
/// an unresolved owner (no pid, process gone, broken ancestry), or an hwnd
/// whose owner at callback time is not the process (or thread) that raised
/// the event (a reused handle). An owner that started before the launch
/// root cannot be its descendant (`other`).
pub fn classify(
    e: &os::WindowEvent,
    launch: &LaunchId,
    samples: &std::collections::HashMap<u32, (u32, u64)>,
) -> Option<Attr> {
    if !e.is_window() {
        return None;
    }
    if !e.owner_consistent() {
        return Some(Attr::Ambiguous);
    }
    if e.in_job == Some(true) {
        return Some(Attr::Ours);
    }
    let (Some(start), Some(ppid)) = (e.start_time, e.ppid) else {
        return Some(Attr::Ambiguous);
    };
    if e.owner_pid == 0 {
        return Some(Attr::Ambiguous);
    }
    let is_host = image_name(e).is_some_and(|n| CONSOLE_HOSTS.contains(&n.as_str()));
    // Walk up by (pid, start time); a parent never starts after its child.
    let (mut pid, mut st, mut parent) = (e.owner_pid, start, ppid);
    let mut broken = false;
    for _ in 0..64 {
        if (pid, st) == launch.root {
            return Some(Attr::Ours);
        }
        if parent == 0 || parent == pid || st < launch.root.1 {
            break;
        }
        match samples.get(&parent) {
            Some(&(pp, ps)) if ps <= st => {
                (pid, st, parent) = (parent, ps, pp);
            }
            _ => {
                broken = true;
                break;
            }
        }
    }
    Some(if is_host || (broken && start >= launch.root.1) {
        Attr::Ambiguous
    } else {
        Attr::Other
    })
}

/// Window events of one observed launch, classified.
#[derive(Default)]
struct Flash {
    observer_error: Option<String>,
    ours: Vec<os::WindowEvent>,
    ambiguous: Vec<os::WindowEvent>,
    all: usize,
}

impl Flash {
    fn json(&self) -> Value {
        json!({"observer_error": self.observer_error, "ours": self.ours,
               "ambiguous": self.ambiguous, "events": self.all})
    }
}

/// `no_flash`: an `ours` event is a flash (`platform_fail`); an `ambiguous`
/// one leaves the case not exercised; an observer failure (start, quit,
/// join, message loop) is a harness failure; an uncalibrated observer or a
/// launch out of scope proves nothing.
fn flash_outcome(f: &Flash, launched: bool, calibrated: bool) -> Outcome {
    if let Some(e) = &f.observer_error {
        return Outcome::harness(format!("window observer failed: {e}"));
    }
    if !f.ours.is_empty() {
        return Outcome::platform("a window was shown from the observed launch");
    }
    if !f.ambiguous.is_empty() {
        return Outcome::not_exercised("ambiguous window events");
    }
    if !calibrated {
        return Outcome::not_exercised("the observer is not calibrated");
    }
    if !launched {
        return Outcome::not_exercised("the launch did not complete in scope");
    }
    Outcome::Pass
}

/// Calibration passes only on an `OBJID_WINDOW` show event whose hwnd is the
/// one the helper reported and whose owner pid and start time are the
/// helper's; the caret control must be recorded and never counts.
pub fn calibration_outcome(
    events: &[os::WindowEvent],
    helper: Option<(u32, u64, u64)>,
) -> (Outcome, Outcome) {
    let Some((pid, start, hwnd)) = helper else {
        let o = Outcome::not_exercised("the helper reported no window");
        return (o.clone(), o);
    };
    let window = events.iter().any(|e| {
        e.is_window()
            && e.owner_consistent()
            && e.hwnd == hwnd
            && e.owner_pid == pid
            && e.start_time == Some(start)
    });
    let caret = events
        .iter()
        .any(|e| e.id_object == OBJID_CARET && e.owner_pid == pid);
    (
        if window {
            Outcome::Pass
        } else {
            Outcome::not_exercised("no show event for the helper's window")
        },
        if caret {
            Outcome::Pass
        } else {
            Outcome::not_exercised("the helper's caret event was not recorded")
        },
    )
}

const OBJID_CARET: i32 = -8;

/// Runs `cmd` as an observed launch: the observer is armed first, the child
/// starts with `CREATE_NO_WINDOW`, is assigned to its own observation job,
/// and only then gets `go` on stdin. Returns the exit code and the root
/// identity.
fn run_observed(
    cmd: &mut Command,
    job: &os::ObservationJob,
    timeout: Duration,
) -> Result<(Option<i32>, LaunchId), String> {
    os::no_window(cmd);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut c = cmd.spawn().map_err(|e| e.to_string())?;
    let root = (
        c.id(),
        os::proc_info(c.id()).map(|p| p.start_time).unwrap_or(0),
    );
    if let Err(e) = job.assign(&c) {
        let _ = c.kill();
        let _ = c.wait();
        return Err(format!("observation job assignment failed: {e}"));
    }
    if let Some(mut si) = c.stdin.take() {
        let _ = writeln!(si, "go");
    }
    let end = Instant::now() + timeout;
    let code = loop {
        if let Ok(Some(st)) = c.try_wait() {
            break st.code();
        }
        if Instant::now() >= end {
            let _ = c.kill();
            let _ = c.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Ok((code, LaunchId { root }))
}

/// Classifies an observer's events for one launch.
fn classify_all(
    events: Vec<os::WindowEvent>,
    health: Result<(), String>,
    launch: Option<&LaunchId>,
    tree: &PpidRecorder,
) -> Flash {
    let samples = tree.snapshot();
    let mut f = Flash {
        observer_error: health.err(),
        all: events.len(),
        ..Default::default()
    };
    for e in events {
        let attr = match launch {
            Some(l) => classify(&e, l, &samples),
            None => e.is_window().then_some(Attr::Ambiguous),
        };
        match attr {
            Some(Attr::Ours) => f.ours.push(e),
            Some(Attr::Ambiguous) => f.ambiguous.push(e),
            _ => {}
        }
    }
    f
}

/// One `chs` child under its own observer and observation job; its report
/// counts only with the run id assigned here.
fn observed_child(
    ctx: &Ctx,
    tree: &PpidRecorder,
    name: &str,
    argv: &[String],
    budget: u64,
    env: &[(&str, &str)],
) -> (ChildRep, Flash) {
    let not_run = |e: String| {
        (
            Err(format!("not_run: {e}")),
            Flash {
                observer_error: Some(e),
                ..Default::default()
            },
        )
    };
    let job = match os::ObservationJob::new() {
        Ok(j) => j,
        Err(e) => return not_run(format!("observation job: {e}")),
    };
    let (out, run_id) = match child_out(&ctx.out, name) {
        Ok(v) => v,
        Err(e) => return not_run(e),
    };
    let left = ctx
        .cutoff
        .saturating_duration_since(Instant::now())
        .as_secs()
        .saturating_sub(CHILD_MARGIN);
    let deadline = budget.min(left);
    if deadline <= 20 {
        return not_run("no budget left before the cutoff".into());
    }
    let obs = match os::WindowObserver::start(Some(&job)) {
        Ok(o) => o,
        Err(e) => return not_run(format!("window observer failed: {e}")),
    };
    let mut cmd = Command::new(std::env::current_exe().unwrap_or_default());
    // Forwarded options first: a child's own flags (e.g. a configuration's
    // backend flags) come later and win.
    cmd.args(cli::forward_common(&ctx.args))
        .args(argv)
        .arg("--out")
        .arg(&out)
        .args(["--run-id", &run_id])
        .args(["--deadline-secs", &deadline.to_string()])
        .env("CHS_WAIT_GO", "1");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let r = run_observed(&mut cmd, &job, Duration::from_secs(deadline + CHILD_MARGIN));
    let (events, health) = obs.stop();
    let launch = r.as_ref().ok().map(|(_, l)| *l);
    if let Err(e) = &r {
        ctx.detail(&format!("child-{name}-error"), json!(e));
    }
    let f = classify_all(events, health, launch.as_ref(), tree);
    (child_report(&out, &run_id), f)
}

/// One calibration helper run; a launch failure (spawn, job assignment)
/// lands in `health`.
struct Calib {
    events: Vec<os::WindowEvent>,
    health: Result<(), String>,
    /// `(pid, start time, hwnd)` the helper reported.
    helper: Option<(u32, u64, u64)>,
    launch: Option<LaunchId>,
}

/// Runs the calibration helper once (`at_exit`: window only in its last
/// 50 ms) under a fresh observer and observation job.
fn calibrate(ctx: &Ctx, at_exit: bool) -> Calib {
    let failed = |e: String| Calib {
        events: Vec::new(),
        health: Err(e),
        helper: None,
        launch: None,
    };
    let job = match os::ObservationJob::new() {
        Ok(j) => j,
        Err(e) => return failed(format!("observation job: {e}")),
    };
    let report = ctx.out.join(format!(
        "calibration{}.json",
        if at_exit { "-at-exit" } else { "" }
    ));
    let obs = match os::WindowObserver::start(Some(&job)) {
        Ok(o) => o,
        Err(e) => return failed(e),
    };
    let mut cmd = Command::new(util::sibling_bin("chs-fake-cli"));
    cmd.args(["--mode", "window-calibrate", "--report-file"])
        .arg(&report);
    if at_exit {
        cmd.arg("--at-exit");
    }
    let r = run_observed(&mut cmd, &job, Duration::from_secs(15));
    let (events, health) = obs.stop();
    let helper = std::fs::read(&report)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| {
            Some((
                v["pid"].as_u64()? as u32,
                v["start_time"].as_u64()?,
                v["hwnd"].as_u64()?,
            ))
        });
    let (health, launch) = match r {
        Ok((_, l)) => (health, Some(l)),
        Err(e) => (Err(e), None),
    };
    Calib {
        events,
        health,
        helper,
        launch,
    }
}

/// A launch's own hook-delivery case decides `launch/<config>/<name>`:
/// Claude A-1, Codex the first A-3 case, fake paste every chunk 0 B-3 case.
fn delivery_outcome(name: &str, r: &ChildRep) -> Outcome {
    let rep = match r {
        Err(e) => return Outcome::harness(e.clone()),
        Ok(rep) => rep,
    };
    let find = |id: &str, case: &str| {
        rep.checks
            .iter()
            .rev()
            .find(|c| c.id == id && c.case == case)
            .map(Outcome::from_check)
            .unwrap_or(Outcome::Missing)
    };
    match name {
        "claude-hooks" => find("A-1", "session_start_bound"),
        "codex-hooks-no-daemon" => find("A-3", "no-daemon/no-daemon-0"),
        "codex-hooks-default" => find("A-3", "default/default-0"),
        _ => {
            let c0: Vec<Outcome> = rep
                .checks
                .iter()
                .filter(|c| c.id == "B-3" && c.case.ends_with("/chunk0"))
                .map(Outcome::from_check)
                .collect();
            if c0.is_empty() {
                Outcome::Missing
            } else {
                c0.into_iter()
                    .find(|o| *o != Outcome::Pass)
                    .unwrap_or(Outcome::Pass)
            }
        }
    }
}

/// `launch/<config>/<name>`: the launch's own delivery case, with the
/// launch records it ran on.
pub fn console_launch(config: &str, name: &str, r: &ChildRep) -> (Outcome, Vec<Value>) {
    let launches = child_launches(r);
    (
        branch_gate(config, delivery_outcome(name, r), &launches),
        launches,
    )
}

/// Whether a flashed console launch is re-run with the GUI-subsystem
/// emitter. Never for Codex: its hook trust is keyed by source and position
/// with one trusted hash per key, so the GUI emitter's command at the same
/// position cannot be trusted next to the console emitter's (pre-trusting
/// it marks the console command `Modified`). Claude (`--settings` hooks) and
/// FakeCli have no hook trust.
pub fn gui_mitigation(name: &str) -> bool {
    !name.starts_with("codex")
}

/// The pre-trusted, unattended matrix for one configuration (no attended
/// steps): Claude and Codex `--no-daemon` under every configuration; under
/// `P` also Codex default mode and fake paste.
/// Budget of one console matrix launch.
fn console_launch_budget(name: &str) -> u64 {
    if name == "claude-hooks" { 600 } else { 900 }
}

/// How long the detached survival step waits for the owner's attach-client.
const SURVIVAL_WAIT: Duration = Duration::from_secs(1800);

/// A console run's budget over `configs`, so a run in which every wait
/// ends at its bound fits: the survival fixture's start, both
/// calibrations, every matrix launch (and its GUI-emitter rerun where one
/// applies), and the survival wait.
pub fn console_budget(configs: &[String]) -> u64 {
    let matrix: u64 = configs
        .iter()
        .flat_map(|c| console_names(c))
        .map(|n| (console_launch_budget(n) + CHILD_MARGIN) * (1 + gui_mitigation(n) as u64))
        .sum();
    120 + 2 * (15 + CHILD_MARGIN) + matrix + SURVIVAL_WAIT.as_secs() + 60
}

fn console_matrix(ctx: &Ctx, config: &str) -> Vec<(&'static str, Vec<String>, u64)> {
    let cwd = ctx
        .args
        .cwd
        .clone()
        .unwrap_or_else(|| super::real::c2_cwd(&ctx.args.root))
        .display()
        .to_string();
    let flags = cli::config_flags(config).unwrap_or_default();
    let v = |a: &[&str]| {
        let mut x: Vec<String> = a.iter().map(|s| s.to_string()).collect();
        x.extend(flags.iter().cloned());
        x
    };
    let mut m = vec![
        (
            "claude-hooks",
            v(&["real", "claude-hooks", "--cwd", &cwd]),
            console_launch_budget("claude-hooks"),
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
            console_launch_budget("codex-hooks-no-daemon"),
        ),
    ];
    if config == "P" {
        m.push((
            "codex-hooks-default",
            v(&[
                "real",
                "codex-hooks",
                "--codex-mode",
                "default",
                "--cwd",
                &cwd,
            ]),
            console_launch_budget("codex-hooks-default"),
        ));
        m.push((
            "fake-paste",
            v(&["fake", "paste", "--paste-newline", "cr"]),
            console_launch_budget("fake-paste"),
        ));
    }
    m
}

/// The matrix launches of one configuration (names only).
fn console_names(config: &str) -> Vec<&'static str> {
    let mut v = vec!["claude-hooks", "codex-hooks-no-daemon"];
    if config == "P" {
        v.extend(["codex-hooks-default", "fake-paste"]);
    }
    v
}

/// C-5 cases every observed run needs before any launch counts.
const CONSOLE_BASE: [&str; 5] = [
    "detached",
    "calibration",
    "calibration/caret_control",
    "calibration/at_exit_control",
    "survival",
];

/// Every required C-5 case of a console run over `configs`.
pub fn console_cases(configs: &[String]) -> Vec<String> {
    let mut v: Vec<String> = CONSOLE_BASE.iter().map(|c| c.to_string()).collect();
    for cfg in configs {
        for name in console_names(cfg) {
            v.push(format!("launch/{cfg}/{name}"));
            v.push(format!("no_flash/{cfg}/{name}"));
        }
    }
    v
}

/// The configurations a console run observes (`P` unless `--configs`).
pub fn console_configs(a: &Args) -> Vec<String> {
    if a.configs.is_empty() {
        vec!["P".into()]
    } else {
        a.configs.clone()
    }
}

/// The report config a console run writes; `f2-verdict` joins on it.
pub fn console_config(a: &Args) -> Value {
    json!({"join_run_id": a.join_run_id, "configs": console_configs(a)})
}

/// The detached console child's arguments: the common options plus, named
/// explicitly, the configurations, the step 3 run id it joins, its cwd, and
/// the report dir and run id the parent allocated and printed.
pub fn detached_child_argv(a: &Args, out: &Path, run_id: &str) -> Vec<String> {
    let mut v: Vec<String> = ["win-probe", "console", "--detached"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    v.extend(cli::forward_common(a));
    if !a.configs.is_empty() {
        v.extend(["--configs".into(), a.configs.join(",")]);
    }
    if let Some(j) = &a.join_run_id {
        v.extend(["--join-run-id".into(), j.clone()]);
    }
    if let Some(c) = &a.cwd {
        v.extend(["--cwd".into(), c.display().to_string()]);
    }
    v.extend([
        "--out".into(),
        out.display().to_string(),
        "--run-id".into(),
        run_id.into(),
    ]);
    v
}

/// Applies `f` to the detached run's handoff and rewrites it.
fn publish(ctx: &Ctx, h: &mut Option<Handoff>, f: impl FnOnce(&mut Handoff)) {
    if let Some(h) = h {
        f(h);
        if let Err(e) = write_handoff(h) {
            ctx.error(format!("handoff not written: {e}"));
        }
    }
}

fn probe_console(ctx: &Ctx) {
    if ctx.args.detached && std::env::var("CHS_DETACHED_CHILD").is_err() {
        return relaunch_detached(ctx);
    }
    let mut pc = Pc::new("C-5");
    let configs = console_configs(&ctx.args);
    for c in console_cases(&configs) {
        pc.req(&c);
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
    // Detached: the survival fixture starts before anything is observed, so
    // its attach URL (or why there is none) is published at once; the
    // launching process tells the owner to close the terminal only after
    // the matrix (`close_step_lines`).
    let mut hand = detached.then(|| Handoff {
        run_id: ctx.run_id.clone(),
        report_dir: ctx.out.clone(),
        phase: "starting the survival fixture".into(),
        deadline_unix_ms: unix_ms()
            + ctx
                .cutoff
                .saturating_duration_since(Instant::now())
                .as_millis() as u64
            + 10_000,
        ..Default::default()
    });
    publish(ctx, &mut hand, |_| {});
    let attach = detached.then(|| {
        let mut cred = String::new();
        let _ = std::io::stdin().lock().read_line(&mut cred);
        let credential = if cred.trim().is_empty() {
            util::token()
        } else {
            cred.trim().to_string()
        };
        start_attachable(ctx, credential)
    });
    publish(ctx, &mut hand, |h| {
        match &attach {
            Some(Ok((_, base, _))) => h.url = Some(base.clone()),
            Some(Err(e)) => h.error = Some(e.clone()),
            None => {}
        }
        h.phase = "calibration".into();
    });
    let tree = PpidRecorder::start();
    // Calibration: a helper of ours shows its own top-level window inside an
    // observation job, with the observer armed first (deliberately visible).
    let Calib {
        events,
        health,
        helper,
        ..
    } = calibrate(ctx, false);
    let calibrated = match &health {
        Err(e) => {
            let o = Outcome::harness(format!("window observer failed: {e}"));
            pc.put("calibration", o.clone(), Value::Null);
            pc.put("calibration/caret_control", o, Value::Null);
            false
        }
        Ok(()) => {
            let (win, caret) = calibration_outcome(&events, helper);
            let ok = win == Outcome::Pass;
            let ev = json!({"helper": helper, "events": events});
            pc.put("calibration", win, ev.clone());
            pc.put("calibration/caret_control", caret, ev);
            ok
        }
    };
    // A flash at the very end of a launch must still come out `ours`.
    let c = calibrate(ctx, true);
    let (helper, launch) = (c.helper, c.launch);
    let f = classify_all(c.events, c.health, launch.as_ref(), &tree);
    let hit = helper.is_some_and(|(_, _, h)| f.ours.iter().any(|e| e.hwnd == h));
    pc.put(
        "calibration/at_exit_control",
        match (&f.observer_error, hit) {
            (Some(e), _) => Outcome::harness(format!("window observer failed: {e}")),
            (None, true) => Outcome::Pass,
            (None, false) => Outcome::not_exercised("the at-exit window was not classified ours"),
        },
        json!({"helper": helper, "flash": f.json()}),
    );
    let mut launches = serde_json::Map::new();
    let total: usize = configs.iter().map(|c| console_names(c).len()).sum();
    let mut i = 0;
    for cfg in &configs {
        for (name, argv, budget) in console_matrix(ctx, cfg) {
            i += 1;
            publish(ctx, &mut hand, |h| {
                h.phase = format!("matrix {i}/{total}: {cfg}/{name}")
            });
            let (r, f) = observed_child(
                ctx,
                &tree,
                &format!("console-{cfg}-{name}"),
                &argv,
                budget,
                &[],
            );
            let (delivered, ev) = console_launch(cfg, name, &r);
            let ok = delivered == Outcome::Pass;
            let tag = json!({"config": cfg, "status": status_of(&r), "launches": ev});
            pc.put(&format!("launch/{cfg}/{name}"), delivered, tag.clone());
            let nf = flash_outcome(&f, ok, calibrated);
            let flashed = matches!(nf, Outcome::PlatformFail { .. });
            pc.put(
                &format!("no_flash/{cfg}/{name}"),
                nf,
                json!({"config": cfg, "flash": f.json()}),
            );
            let mut entry = json!({"baseline": f.json()});
            if flashed && gui_mitigation(name) {
                // Mitigation: rerun the SAME launch, cwd, and hook delivery with
                // the GUI-subsystem emitter.
                let (dk, fk) = (
                    format!("mitigated/{cfg}/{name}/delivery"),
                    format!("mitigated/{cfg}/{name}/no_flash"),
                );
                pc.req(&dk);
                pc.req(&fk);
                let gui = [("CHS_EMITTER_VARIANT", "gui")];
                let (r2, f2) = observed_child(
                    ctx,
                    &tree,
                    &format!("console-{cfg}-{name}-gui"),
                    &argv,
                    budget,
                    &gui,
                );
                let (delivered, ev2) = console_launch(cfg, name, &r2);
                let ok2 = delivered == Outcome::Pass;
                pc.put(
                    &dk,
                    delivered,
                    json!({"config": cfg, "status": status_of(&r2), "launches": ev2}),
                );
                pc.put(
                    &fk,
                    flash_outcome(&f2, ok2, calibrated),
                    json!({"config": cfg, "flash": f2.json()}),
                );
                entry["mitigated"] = f2.json();
            }
            launches.insert(format!("{cfg}/{name}"), entry);
        }
    }
    publish(ctx, &mut hand, |h| {
        h.matrix_done = true;
        h.phase = "survival: waiting for attach-client".into();
    });
    if let Some(a) = attach {
        let (o, ev) = terminal_close_survival(ctx, a);
        pc.put("survival", o, ev);
        publish(ctx, &mut hand, |h| h.phase = "writing the report".into());
    } else {
        pc.put(
            "survival",
            Outcome::not_exercised("run `chs win-probe console --detached`"),
            Value::Null,
        );
    }
    ctx.sink.with(|r| {
        if let (Value::Object(c), Value::Object(add)) = (&mut r.config, console_config(&ctx.args)) {
            c.extend(add);
        }
    });
    pc.finish(
        ctx,
        json!({"launches": launches, "breakaway": std::env::var("CHS_BREAKAWAY").ok(),
               "harness_job": os::self_job_info(), "join_run_id": ctx.args.join_run_id,
               "default_terminal": os::default_terminal_delegation()}),
    );
}

/// The detached console child's handoff, `<report dir>/handoff.json`,
/// rewritten atomically as it advances: published before the matrix with
/// the attach URL (or why there is none), then the phase, and
/// `matrix_done` once every observed launch has ended.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Handoff {
    pub run_id: String,
    pub report_dir: PathBuf,
    pub url: Option<String>,
    pub error: Option<String>,
    pub phase: String,
    pub matrix_done: bool,
    /// The child's hard deadline (its report is written by then).
    pub deadline_unix_ms: u64,
}

const HANDOFF: &str = "handoff.json";

/// How long the launching process waits for the detached child's attach
/// URL (published before the matrix, after the survival fixture started).
const ATTACH_URL_WAIT: Duration = Duration::from_secs(120);

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// `Instant` for a wall-clock time in unix milliseconds.
fn instant_at(unix: u64) -> Instant {
    Instant::now() + Duration::from_millis(unix.saturating_sub(unix_ms()))
}

/// Writes the handoff (retried briefly: a reader holding the file open can
/// make the rename fail on Windows).
pub fn write_handoff(h: &Handoff) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(h).unwrap_or_default();
    let mut last = String::new();
    for _ in 0..20 {
        match util::write_atomic(&h.report_dir.join(HANDOFF), &bytes) {
            Ok(()) => return Ok(()),
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(last)
}

pub fn read_handoff(dir: &Path) -> Option<Handoff> {
    serde_json::from_slice(&std::fs::read(dir.join(HANDOFF)).ok()?).ok()
}

/// Polls `f` every `poll` until it yields or `end` passes; calls `progress`
/// every `every` meanwhile.
fn poll_until<T>(
    end: Instant,
    poll: Duration,
    every: Duration,
    mut f: impl FnMut() -> Option<T>,
    mut progress: impl FnMut(),
) -> Option<T> {
    let mut next = Instant::now() + every;
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        let now = Instant::now();
        if now >= end {
            return None;
        }
        if now >= next {
            progress();
            next = now + every;
        }
        std::thread::sleep(poll.min(end - now));
    }
}

/// Why the launcher stopped waiting: the detached run exited with
/// `status` before it got that far.
fn exited_early(dir: &Path, status: &str) -> String {
    format!(
        "the detached run exited early ({status}); its report, if it wrote one: {}",
        dir.join("report.json").display()
    )
}

/// The attach URL the detached child published for this run (its run id
/// and report dir), validated as a local probe URL; or why there is none
/// (also as soon as `exited` reports the child's exit status).
pub fn await_attach_url(
    dir: &Path,
    run_id: &str,
    end: Instant,
    mut exited: impl FnMut() -> Option<String>,
) -> Result<String, String> {
    let published = || {
        read_handoff(dir).filter(|h| h.run_id == run_id && (h.url.is_some() || h.error.is_some()))
    };
    let h = poll_until(
        end,
        Duration::from_millis(200),
        Duration::from_secs(3600),
        || match published() {
            Some(h) => Some(Ok(h)),
            None => exited().map(|s| published().ok_or_else(|| exited_early(dir, &s))),
        },
        || {},
    )
    .ok_or("the detached run published no attach URL in time")??;
    if h.report_dir != dir {
        return Err(format!(
            "handoff names report dir {}",
            h.report_dir.display()
        ));
    }
    match (h.url, h.error) {
        (Some(u), _) if parse_local_url(&u, "/probe").is_some() => Ok(u),
        (Some(u), _) => Err(format!("invalid attach URL {u:?}")),
        (None, e) => Err(e.unwrap_or_default()),
    }
}

/// Waits until the detached run's handoff says `matrix_done`; stops at
/// `end` or as soon as `exited` reports the child's exit status (a handoff
/// with `matrix_done` still counts after the exit). Progress every minute.
pub fn await_matrix(
    dir: &Path,
    end: Instant,
    poll: Duration,
    mut exited: impl FnMut() -> Option<String>,
    mut progress: impl FnMut(),
) -> Result<(), String> {
    let done = || read_handoff(dir).is_some_and(|h| h.matrix_done);
    poll_until(
        end,
        poll,
        Duration::from_secs(60),
        || {
            if done() {
                return Some(Ok(()));
            }
            exited().map(|s| {
                if done() {
                    Ok(())
                } else {
                    Err(exited_early(dir, &s))
                }
            })
        },
        &mut progress,
    )
    .unwrap_or_else(|| Err("the observation matrix did not finish by its deadline".into()))
}

/// The terminal-close step the launching process prints: only with a
/// valid attach URL and once the observation matrix has ended (a terminal
/// opened during an observed launch would be an `ambiguous` window);
/// otherwise why not and how to proceed.
pub fn close_step_lines(
    exe: &Path,
    url: &Result<String, String>,
    matrix: &Result<(), String>,
    cred: &str,
) -> Vec<String> {
    match (url, matrix) {
        (Err(e), _) => vec![
            format!("no attach URL from the detached run: {e}"),
            "do not close this terminal for the survival check (C-5 survival stays not_exercised);"
                .into(),
            "run step 5 below; it waits for the detached report.".into(),
        ],
        (Ok(_), Err(e)) => vec![
            format!("{e}; do not close this terminal;"),
            "run step 5 below; it waits for the detached report.".into(),
        ],
        (Ok(u), Ok(())) => {
            let client: Vec<String> = ["win-probe", "attach-client", "--confirm-close", "--url"]
                .iter()
                .map(|s| s.to_string())
                .chain([u.clone()])
                .collect();
            vec![
                "now close this terminal, open a new one, and run:".into(),
                format!("  {}", ps_invocation(exe, &client)),
                format!("then paste this credential on its stdin: {cred}"),
                "when it has reported, run step 5 below.".into(),
            ]
        }
    }
}

/// Step 5's wait for the detached run's final report: `report.json`
/// (written once, when the run ends) in the step 4 dir, polled until the
/// deadline its handoff names (capped at `cap`), with progress. A dir with
/// neither a report nor a handoff, or a report of another run, is refused.
pub fn wait_final_report(
    dir: &Path,
    cap: Instant,
    poll: Duration,
    every: Duration,
    mut progress: impl FnMut(&str),
) -> Result<Report, String> {
    let path = dir.join("report.json");
    let first = read_handoff(dir);
    if first.is_none() && !path.exists() {
        return Err(format!(
            "{}: neither a report nor a detached-run handoff",
            dir.display()
        ));
    }
    let end = first.as_ref().map_or(cap, |h| {
        instant_at(h.deadline_unix_ms + CHILD_MARGIN * 1000).min(cap)
    });
    let r = poll_until(
        end,
        poll,
        every,
        || crate::report::read_report(&path),
        || {
            let phase = read_handoff(dir).map(|h| h.phase).unwrap_or_default();
            let left = end.saturating_duration_since(Instant::now()).as_secs() / 60;
            progress(&format!(
                "waiting for the step 4 report ({phase}); at most {left} min left"
            ));
        },
    )
    .ok_or_else(|| {
        format!(
            "the step 4 report did not appear in {} by its deadline; nothing was evaluated; \
             rerun f2-verdict once the detached run has written report.json",
            dir.display()
        )
    })?;
    match first {
        Some(h) if h.run_id != r.run_id => Err(format!(
            "report run id {} is not the detached run's {}",
            r.run_id, h.run_id
        )),
        _ => Ok(r),
    }
}

/// Relaunches the harness detached into a report dir allocated here, and
/// prints that dir and the owner's next steps.
fn relaunch_detached(ctx: &Ctx) {
    let mut pc = Pc::new("C-5-launch");
    pc.req("detached_launch");
    let (child_dir, child_id) = match child_out(&ctx.out, "console-detached") {
        Ok(v) => v,
        Err(e) => {
            pc.put(
                "detached_launch",
                Outcome::harness(format!("child report dir: {e}")),
                Value::Null,
            );
            return pc.finish(ctx, Value::Null);
        }
    };
    let exe = std::env::current_exe().unwrap_or_default();
    let mut cmd = Command::new(&exe);
    cmd.args(detached_child_argv(&ctx.args, &child_dir, &child_id));
    cmd.env("CHS_DETACHED_CHILD", "1");
    let cred = util::token();
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    ctx.detail("step4_report_dir", json!(child_dir));
    match os::spawn_detached(&mut cmd) {
        Ok((mut c, breakaway)) => {
            if let Some(mut si) = c.stdin.take() {
                let _ = writeln!(si, "{cred}");
            }
            println!(
                "detached harness pid {} (breakaway {})",
                c.id(),
                if breakaway { "permitted" } else { "denied" }
            );
            println!("step 4 report dir (f2-verdict --from), run id {child_id}:");
            println!("  {}", child_dir.display());
            let verdict: Vec<String> = [
                "win-probe".to_string(),
                "f2-verdict".into(),
                "--from".into(),
                "<step 3 report dir>".into(),
                "--from".into(),
                child_dir.display().to_string(),
            ]
            .into();
            let step5 = format!("  {}", ps_invocation(&exe, &verdict));
            println!("step 5 (waits for the detached report, then evaluates F-2):");
            println!("{step5}");
            let mut exited = || c.try_wait().ok().flatten().map(|s| s.to_string());
            // The child publishes its attach URL before the matrix.
            let url = await_attach_url(
                &child_dir,
                &child_id,
                ctx.until(ATTACH_URL_WAIT),
                &mut exited,
            );
            let matrix = match &url {
                Err(e) => Err(e.clone()),
                Ok(_) => {
                    println!(
                        "the detached run is observing launches; leave the machine idle and keep \
                         this terminal open until told (progress every minute)"
                    );
                    let end = read_handoff(&child_dir).map_or(ctx.cutoff, |h| {
                        instant_at(h.deadline_unix_ms).min(ctx.cutoff)
                    });
                    await_matrix(&child_dir, end, Duration::from_secs(1), &mut exited, || {
                        let phase = read_handoff(&child_dir)
                            .map(|h| h.phase)
                            .unwrap_or_default();
                        println!("  step 4: {phase}");
                    })
                }
            };
            let matrix_done = matrix.is_ok();
            for line in close_step_lines(&exe, &url, &matrix, &cred) {
                println!("{line}");
            }
            println!("step 5 (waits for the detached report, then evaluates F-2):");
            println!("{step5}");
            pc.put(
                "detached_launch",
                Outcome::not_exercised("the detached child reports C-5"),
                json!({"breakaway_permitted": breakaway, "breakaway_denied": !breakaway,
                       "parent_job": os::self_job_info(), "child_report_dir": child_dir,
                       "child_run_id": child_id, "attach_url": url, "matrix_done": matrix_done}),
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
fn terminal_close_survival(
    ctx: &Ctx,
    started: Result<(super::Launch, String, Arc<AttachProbe>), String>,
) -> (Outcome, Value) {
    let (l, _, p) = match started {
        Ok(v) => v,
        Err(e) => return (Outcome::harness(e.clone()), json!({"error": e})),
    };
    let start_time = p
        .witnesses
        .witnesses()
        .iter()
        .find(|w| w.role == "attachable" && w.pid == p.child_pid)
        .map(|w| w.start_time);
    let end = ctx.until(SURVIVAL_WAIT);
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

    fn ev(
        pid: u32,
        image: &str,
        ppid: Option<u32>,
        start: Option<u64>,
        in_job: Option<bool>,
    ) -> os::WindowEvent {
        os::WindowEvent {
            time_ms: 0,
            hwnd: 0x100,
            id_object: os::OBJID_WINDOW,
            id_child: os::CHILDID_SELF,
            class: "C".into(),
            owner_pid: pid,
            image: Some(format!("C:\\Windows\\{image}")),
            ppid,
            start_time: start,
            in_job,
            event_ms: 0,
            event_thread: 7,
            event_pid: Some(pid),
            owner_thread: 7,
        }
    }

    /// M5: an hwnd that no longer belongs to the process (or thread) that
    /// raised the event is not attributable, even when its current owner
    /// is in the observation job; calibration needs a consistent owner too.
    #[test]
    fn reused_hwnd_owner_is_ambiguous() {
        let l = LaunchId { root: (100, 1000) };
        let samples = std::collections::HashMap::new();
        let mut e = ev(100, "a.exe", Some(1), Some(1000), Some(true));
        assert_eq!(classify(&e, &l, &samples), Some(Attr::Ours));
        e.event_pid = Some(555);
        assert_eq!(classify(&e, &l, &samples), Some(Attr::Ambiguous));
        // Unresolved event process: the owning thread must be the event's.
        e.event_pid = None;
        assert_eq!(classify(&e, &l, &samples), Some(Attr::Ours));
        e.owner_thread = 8;
        assert_eq!(classify(&e, &l, &samples), Some(Attr::Ambiguous));
        let mut w = ev(42, "chs-fake-cli.exe", Some(1), Some(7), Some(true));
        w.event_pid = Some(43);
        let (o, _) = calibration_outcome(&[w], Some((42, 7, 0x100)));
        assert!(matches!(o, Outcome::NotExercised { .. }), "{o:?}");
    }

    /// F-3a: §4.8 classification of window events during a launch.
    #[test]
    fn classify_rules() {
        let l = LaunchId { root: (100, 1000) };
        // root 100 (start 1000) -> 200 (1100) -> conhost 300 (1200)
        let samples: std::collections::HashMap<u32, (u32, u64)> =
            [(100, (1, 1000)), (200, (100, 1100)), (1, (0, 10))].into();
        let c = |e: &os::WindowEvent| classify(e, &l, &samples);
        // In the observation job at callback time.
        assert_eq!(c(&ev(9, "x.exe", None, None, Some(true))), Some(Attr::Ours));
        // Ancestry by pid and start time; a console host under our client.
        assert_eq!(
            c(&ev(300, "conhost.exe", Some(200), Some(1200), Some(false))),
            Some(Attr::Ours)
        );
        assert_eq!(
            c(&ev(100, "a.exe", Some(1), Some(1000), None)),
            Some(Attr::Ours)
        );
        // Same pid as the root, different start time: not ours.
        assert_ne!(
            c(&ev(100, "a.exe", Some(1), Some(5000), None)),
            Some(Attr::Ours)
        );
        // A parent pid reused by a later process breaks the ancestry.
        let reused: std::collections::HashMap<u32, (u32, u64)> =
            [(100, (1, 1000)), (200, (100, 9000))].into();
        assert_eq!(
            classify(
                &ev(300, "x.exe", Some(200), Some(1200), Some(false)),
                &l,
                &reused
            ),
            Some(Attr::Ambiguous)
        );
        // Console hosts / terminals not provably ours.
        for host in ["conhost.exe", "OpenConsole.exe", "WindowsTerminal.exe"] {
            assert_eq!(
                c(&ev(400, host, Some(1), Some(50), Some(false))),
                Some(Attr::Ambiguous)
            );
        }
        // Unresolved owner: no start time, no pid, broken ancestry.
        assert_eq!(
            c(&ev(500, "x.exe", Some(1), None, None)),
            Some(Attr::Ambiguous)
        );
        assert_eq!(
            c(&ev(0, "x.exe", Some(1), Some(1500), None)),
            Some(Attr::Ambiguous)
        );
        assert_eq!(
            c(&ev(501, "x.exe", Some(777), Some(1500), None)),
            Some(Attr::Ambiguous)
        );
        // Unrelated owners.
        assert_eq!(
            c(&ev(600, "explorer.exe", Some(1), Some(20), Some(false))),
            Some(Attr::Other)
        );
        assert_eq!(
            c(&ev(601, "x.exe", Some(1), Some(1500), Some(false))),
            Some(Attr::Other)
        );
        // Non-window events (the caret) are ignored.
        let mut caret = ev(9, "x.exe", None, None, Some(true));
        caret.id_object = OBJID_CARET;
        assert_eq!(c(&caret), None);
    }

    /// F-3a: calibration needs the reported hwnd plus the helper's pid and
    /// start time; the caret alone never satisfies it.
    #[test]
    fn calibration_needs_the_helpers_window() {
        let helper = Some((42, 7, 0x100));
        let win = ev(42, "chs-fake-cli.exe", Some(1), Some(7), Some(true));
        let mut caret = win.clone();
        caret.id_object = OBJID_CARET;
        assert_eq!(
            calibration_outcome(&[win.clone(), caret.clone()], helper),
            (Outcome::Pass, Outcome::Pass)
        );
        let (w, c) = calibration_outcome(std::slice::from_ref(&caret), helper);
        assert!(matches!(w, Outcome::NotExercised { .. }), "{w:?}");
        assert_eq!(c, Outcome::Pass);
        for bad in [
            os::WindowEvent {
                hwnd: 0x200,
                ..win.clone()
            },
            os::WindowEvent {
                owner_pid: 43,
                ..win.clone()
            },
            os::WindowEvent {
                start_time: Some(8),
                ..win.clone()
            },
        ] {
            let (w, _) = calibration_outcome(&[bad], helper);
            assert!(matches!(w, Outcome::NotExercised { .. }), "{w:?}");
        }
        let (w, _) = calibration_outcome(&[win], None);
        assert!(matches!(w, Outcome::NotExercised { .. }), "{w:?}");
    }

    /// F-3a: an event recorded during the shutdown drain is classified, and
    /// the no-flash rules follow it.
    #[test]
    fn flash_rules() {
        let l = LaunchId { root: (100, 1000) };
        let tree = PpidRecorder {
            map: Arc::new(Mutex::new([(100, (1, 1000))].into())),
            stop: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let mut late = ev(100, "a.exe", Some(1), Some(1000), Some(false));
        late.time_ms = 60_000;
        let f = classify_all(vec![late], Ok(()), Some(&l), &tree);
        assert_eq!(f.ours.len(), 1);
        assert!(matches!(
            flash_outcome(&f, true, true),
            Outcome::PlatformFail { .. }
        ));
        let amb = classify_all(
            vec![ev(400, "conhost.exe", Some(1), Some(50), Some(false))],
            Ok(()),
            Some(&l),
            &tree,
        );
        assert!(matches!(
            flash_outcome(&amb, true, true),
            Outcome::NotExercised { .. }
        ));
        let none = classify_all(Vec::new(), Ok(()), Some(&l), &tree);
        assert_eq!(flash_outcome(&none, true, true), Outcome::Pass);
        assert!(matches!(
            flash_outcome(&none, true, false),
            Outcome::NotExercised { .. }
        ));
        assert!(matches!(
            flash_outcome(&none, false, true),
            Outcome::NotExercised { .. }
        ));
        let broken = classify_all(Vec::new(), Err("WM_QUIT".into()), Some(&l), &tree);
        assert!(matches!(
            flash_outcome(&broken, true, true),
            Outcome::HarnessFail { .. }
        ));
    }

    /// M4: the F-1 budgets follow the effective SessionStart timeout and
    /// include the 10 s write bound (one case: SessionStart, bootstrap 120 s,
    /// write 10 s, fixture 120 s, settle 5 s, launch and teardown 20 s),
    /// plus one failed start (SessionStart timeout, launch and teardown)
    /// retried per case.
    #[test]
    fn f1_budget_follows_the_effective_timeout() {
        for sst in [20, 60, 120] {
            let case = (sst + 20) + (sst + 120 + 10 + 120 + 5 + 20);
            let child = f1_child(sst);
            assert!(
                child >= F1_CASES * case + CHILD_MARGIN,
                "sst {sst}: {child}"
            );
            assert!(f1_recorded_child(sst) >= F1_RECORDED * case + CHILD_MARGIN);
            assert!(codex_input_budget(sst) >= 2 * child + f1_recorded_child(sst));
            assert!(backend_budget(sst) >= 3 * child);
            assert!(followup_budget(sst) >= codex_input_budget(sst) + backend_budget(sst));
        }
    }

    /// M1: an F-1 child in which every case fails to start once and then
    /// succeeds at its worst-case timing fits the child's budget.
    #[test]
    fn f1_child_budget_fits_a_retried_run() {
        let n = f1_entries().len() as u64;
        assert_eq!(n, F1_CASES);
        let recorded = paste::corpus_f().iter().filter(|e| e.recorded).count() as u64;
        assert_eq!(recorded, F1_RECORDED);
        // Every case fails to start once, then succeeds at its worst case.
        let worst = super::super::real::keys_failed_start_secs(20)
            + super::super::real::keys_case_worst_secs(20);
        assert!(f1_recorded_child(20) >= recorded * worst + CHILD_MARGIN);
        assert!(f1_child(20) >= n * worst + CHILD_MARGIN);
    }

    /// The narrow rerun names existing rows, all runnable with `w32`, and
    /// its budget covers each of its children.
    #[test]
    fn rerun_rows_exist() {
        let rows = backend_rows(Some(NewlineKey::W32), 20);
        for name in RERUN_ROWS {
            let r = rows.iter().find(|r| r.name == name);
            assert!(r.is_some_and(|r| r.argv.is_some()), "{name}");
        }
        let children: u64 = rows
            .iter()
            .filter(|r| RERUN_ROWS.contains(&r.name))
            .map(|r| r.budget + CHILD_MARGIN)
            .sum();
        assert!(rerun_budget(20) >= f1_child(20) + f1_recorded_child(20) + 3 * children + 5 * 300);
    }

    /// F-1b selection.
    #[test]
    fn f1_select_rules() {
        use Sel::*;
        assert_eq!(f1_select(Pass, Pass), ("go", Some(NewlineKey::CsiU)));
        assert_eq!(f1_select(Pass, Fail), ("go", Some(NewlineKey::CsiU)));
        assert_eq!(f1_select(Pass, Incomplete), ("go", Some(NewlineKey::CsiU)));
        assert_eq!(f1_select(Fail, Pass), ("go", Some(NewlineKey::W32)));
        assert_eq!(f1_select(Incomplete, Pass), ("go", Some(NewlineKey::W32)));
        assert_eq!(f1_select(Fail, Fail), ("fail_observed", None));
        assert_eq!(f1_select(Incomplete, Fail), ("inconclusive", None));
        assert_eq!(f1_select(Fail, Incomplete), ("inconclusive", None));
        assert_eq!(f1_select(Incomplete, Incomplete), ("inconclusive", None));
    }

    fn chk(id: &str, case: &str, o: Outcome) -> Check {
        Check {
            id: id.into(),
            case: case.into(),
            result: o.tag().into(),
            cause: o.cause(),
            evidence: Value::Null,
        }
    }

    /// Step 3 and step 4 reports where every F-2 case of every config passes
    /// except those `over` replaces.
    fn f2_reports(over: &[(&str, &str, Outcome)], drop: &[&str]) -> (Report, Report) {
        let mut s3 = Report::new("win-probe backend", "run-3");
        let mut s4 = Report::new("win-probe console", "run-4");
        s4.config = json!({"join_run_id": "run-3"});
        let (mut c3, mut c4) = (Vec::new(), Vec::new());
        for cfg in F2_CONFIGS {
            for k in f2_required(cfg) {
                let (id, case) = k.split_once(' ').unwrap();
                if drop.contains(&case) {
                    continue;
                }
                let o = over
                    .iter()
                    .find(|(i, c, _)| *i == id && *c == case)
                    .map(|x| x.2.clone())
                    .unwrap_or(Outcome::Pass);
                let c = chk(id, case, o);
                if id == "F-2a" { c3.push(c) } else { c4.push(c) }
            }
        }
        let probe = |id: &str, cases: Vec<Check>| Probe {
            id: id.into(),
            verdict: "go".into(),
            why: String::new(),
            evidence: json!({"cases": cases}),
        };
        s3.probes.push(probe("F-2a", c3));
        s4.probes.push(probe("C-5", c4));
        (s3, s4)
    }

    /// F-2b selection over joined step 3 and step 4 reports.
    #[test]
    fn f2_select_over_joined_reports() {
        let fail = || Outcome::platform("x");
        let (a, b) = f2_reports(&[], &[]);
        assert_eq!(f2_evaluate(&a, &b).0, "go");
        assert_eq!(f2_evaluate(&a, &b).1, Some("R0"));
        // R6 selectable when R0 fails, and vice versa.
        let (a, b) = f2_reports(&[("F-2a", "claude_hooks/R0", fail())], &[]);
        assert_eq!(f2_evaluate(&a, &b).1, Some("R6"));
        let (a, b) = f2_reports(&[("C-5", "no_flash/R6/claude-hooks", fail())], &[]);
        assert_eq!(f2_evaluate(&a, &b).1, Some("R0"));
        // Both candidates complete with a failure each.
        let (a, b) = f2_reports(
            &[
                ("F-2a", "attach/R0", fail()),
                ("F-2a", "codex_hooks/R6", fail()),
            ],
            &[],
        );
        assert_eq!(f2_evaluate(&a, &b).0, "fail_observed");
        // P evidence missing, or a config missing from step 4.
        let (a, b) = f2_reports(&[], &["tree_kill_codex/P"]);
        assert_eq!(
            f2_evaluate(&a, &b),
            ("inconclusive", None, f2_evaluate(&a, &b).2)
        );
        let (a, b) = f2_reports(
            &[("F-2a", "claude_hooks/R0", fail())],
            &["launch/R6/claude-hooks", "no_flash/R6/claude-hooks"],
        );
        assert_eq!(f2_evaluate(&a, &b).0, "inconclusive");
        // Mixed: R0 failed, R6 incomplete (an ambiguous window observation).
        let (a, b) = f2_reports(
            &[
                ("F-2a", "claude_paste/R0", fail()),
                (
                    "C-5",
                    "no_flash/R6/codex-hooks-no-daemon",
                    Outcome::not_exercised("ambiguous window events"),
                ),
            ],
            &[],
        );
        assert_eq!(f2_evaluate(&a, &b).0, "inconclusive");
        // No F-1 winner: the Codex input row is not exercised everywhere.
        let none: Vec<(&str, String, Outcome)> = F2_CONFIGS
            .iter()
            .map(|c| {
                (
                    "F-2a",
                    format!("codex_input/{c}"),
                    Outcome::not_exercised("no F-1 winner"),
                )
            })
            .collect();
        let none: Vec<(&str, &str, Outcome)> = none
            .iter()
            .map(|(i, c, o)| (*i, c.as_str(), o.clone()))
            .collect();
        let (a, b) = f2_reports(&none, &[]);
        let (v, sel, ev) = f2_evaluate(&a, &b);
        assert_eq!((v, sel), ("inconclusive", None));
        assert_eq!(
            ev["configs"]["R0"]["eval"]["passed"]
                .as_array()
                .unwrap()
                .len(),
            // 6 other rows + 4 launch cases + 4 observer-health cases.
            14
        );
        // Mismatched run ids: not joined.
        let (a, mut b) = f2_reports(&[], &[]);
        b.config["join_run_id"] = json!("other");
        let (v, _, ev) = f2_evaluate(&a, &b);
        assert_eq!((v, ev["joined"].clone()), ("inconclusive", json!(false)));
    }

    fn strs(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// Splits a printed PowerShell command line into its arguments (single
    /// and double quotes; `''` inside single quotes is one quote).
    fn ps_tokens(s: &str) -> Vec<String> {
        let cs: Vec<char> = s.chars().collect();
        let (mut out, mut cur, mut q, mut quoted) = (Vec::new(), String::new(), None, false);
        let mut i = 0;
        while i < cs.len() {
            let c = cs[i];
            match q {
                Some('\'') if c == '\'' && cs.get(i + 1) == Some(&'\'') => {
                    cur.push('\'');
                    i += 1;
                }
                Some(qc) if c == qc => q = None,
                Some(_) => cur.push(c),
                None if c == '\'' || c == '"' => {
                    q = Some(c);
                    quoted = true;
                }
                None if c == ' ' => {
                    if quoted || !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                        quoted = false;
                    }
                }
                None => cur.push(c),
            }
            i += 1;
        }
        if quoted || !cur.is_empty() {
            out.push(cur);
        }
        out
    }

    fn followup_parent() -> Args {
        cli::parse(&strs(&[
            "win-probe",
            "followup",
            "--claude-bin",
            r"C:\Program Files\Claude\claude.exe",
            "--codex-bin",
            r"C:\chs\npm\codex.exe",
            "--allow-api-usage",
        ]))
        .unwrap()
    }

    /// H1: the step 4 command `followup` prints, the detached relaunch it
    /// starts, and the report config the detached child writes carry the
    /// join id and every configuration, so `f2-verdict` joins step 3 with
    /// step 4 and selects a candidate.
    #[test]
    fn relaunch_reaches_the_f2_join() {
        let (mut step3, _) = f2_reports(&[], &[]);
        step3.run_id = "a1b2c3d4".into();
        let exe = Path::new(r"C:\chs\t\release\chs.exe");
        let cmd = step4_command(
            exe,
            &step3.run_id,
            Some(NewlineKey::W32),
            &followup_parent(),
        );
        let toks = ps_tokens(&cmd);
        let at = toks
            .iter()
            .position(|t| t == exe.to_str().unwrap())
            .unwrap();
        let step4 = cli::parse(&toks[at + 1..]).unwrap();
        assert!(step4.detached, "{cmd}");
        let out = PathBuf::from(r"C:\chs\out\5e6f7a8b\console-detached-0a0b0c0d");
        let child = cli::parse(&detached_child_argv(&step4, &out, "0a0b0c0d")).unwrap();
        let mut step4_rep = Report::new("win-probe console", "0a0b0c0d");
        step4_rep.config = console_config(&child);
        let cases: Vec<Check> = console_cases(&console_configs(&child))
            .iter()
            .map(|c| chk("C-5", c, Outcome::Pass))
            .collect();
        step4_rep.probes.push(Probe {
            id: "C-5".into(),
            verdict: "go".into(),
            why: String::new(),
            evidence: json!({"cases": cases}),
        });
        let (v, sel, ev) = f2_evaluate(&step3, &step4_rep);
        assert_eq!((v, sel), ("go", Some("R0")), "{ev}");
        // The child writes its report where the parent printed it would.
        assert_eq!(child.out.as_deref(), Some(out.as_path()));
        assert_eq!(child.run_id.as_deref(), Some("0a0b0c0d"));
        assert_eq!(child.newline_key, NewlineKey::W32);
    }

    /// L4: the printed step 4 command is a PowerShell invocation (call
    /// operator, quoted executable path).
    #[test]
    fn step4_command_is_powershell_valid() {
        let exe = Path::new(r"C:\chs dir\t\release\chs.exe");
        let cmd = step4_command(exe, "a1b2c3d4", None, &followup_parent());
        assert!(cmd.starts_with("& "), "{cmd}");
        let toks = ps_tokens(&cmd);
        assert_eq!(toks[1], exe.to_str().unwrap(), "{cmd}");
        assert!(toks.contains(&r"C:\Program Files\Claude\claude.exe".to_string()));
    }

    /// M2b (r3): a raw launch's teardown counts: a hung close or a drain
    /// that ended on a read error prevents a pass (also on a failed
    /// launch's record); a clean teardown passes.
    #[test]
    fn branch_gate_needs_a_clean_raw_teardown() {
        let with = |td: Value| {
            let mut l = raw_launch(0, "job_list");
            l["teardown"] = td;
            l
        };
        let clean = json!({"close_hung": false, "drain_end": "eof"});
        assert_eq!(
            branch_gate("R0", Outcome::Pass, &[with(clean.clone())]),
            Outcome::Pass
        );
        let hung = json!({"close_hung": true, "drain_end": "eof"});
        let o = branch_gate("R0", Outcome::Pass, &[with(hung)]);
        assert!(matches!(o, Outcome::PlatformFail { .. }), "{o:?}");
        let err = json!({"close_hung": false, "drain_end": "error: broken pipe"});
        let o = branch_gate("R0", Outcome::Pass, &[with(clean), with(err)]);
        assert!(matches!(o, Outcome::HarnessFail { .. }), "{o:?}");
    }

    /// F-2b branch rule: suspended-branch passes are branch failures; a
    /// failed lookup is a harness failure (so the candidate is incomplete).
    #[test]
    fn branch_gate_rules() {
        let jl = raw_launch(0, "job_list");
        let sus = json!({"backend": "raw", "conpty_flags": 0, "branch": "suspended",
                         "job_list_rejected": true, "job_list_error": "denied"});
        assert_eq!(
            branch_gate("R0", Outcome::Pass, std::slice::from_ref(&jl)),
            Outcome::Pass
        );
        assert!(matches!(
            branch_gate("R0", Outcome::Pass, &[jl.clone(), sus.clone()]),
            Outcome::PlatformFail { .. }
        ));
        assert!(matches!(
            branch_gate("R6", Outcome::Pass, &[]),
            Outcome::HarnessFail { .. }
        ));
        let p = json!({"backend": "portable", "branch": "assign_after"});
        assert_eq!(branch_gate("P", Outcome::Pass, &[p]), Outcome::Pass);
        // The gated outcome makes the candidate not selectable.
        let (a, b) = f2_reports(
            &[(
                "F-2a",
                "claude_hooks/R0",
                branch_gate("R0", Outcome::Pass, &[sus]),
            )],
            &[],
        );
        assert_eq!(f2_evaluate(&a, &b).1, Some("R6"));
        let (a, b) = f2_reports(
            &[
                (
                    "F-2a",
                    "claude_hooks/R0",
                    branch_gate("R0", Outcome::Pass, &[]),
                ),
                (
                    "F-2a",
                    "claude_hooks/R6",
                    branch_gate("R6", Outcome::Pass, &[]),
                ),
            ],
            &[],
        );
        assert_eq!(f2_evaluate(&a, &b).0, "inconclusive");
    }

    /// H1: a flashed Codex launch is not re-run with the GUI emitter (its
    /// trust would collide with the console emitter's); Claude and FakeCli
    /// launches still are.
    #[test]
    fn gui_mitigation_skips_codex() {
        for n in [
            "claude-hooks",
            "codex-hooks-no-daemon",
            "codex-hooks-default",
            "fake-paste",
        ] {
            assert_eq!(gui_mitigation(n), !n.starts_with("codex"), "{n}");
        }
    }

    fn handoff_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("chs-handoff-test-{}", util::run_id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn handoff(dir: &Path, url: Option<&str>, deadline_ms: u64) -> Handoff {
        Handoff {
            run_id: "r4".into(),
            report_dir: dir.to_path_buf(),
            url: url.map(String::from),
            phase: "matrix 1/7".into(),
            deadline_unix_ms: unix_ms() + deadline_ms,
            ..Default::default()
        }
    }

    /// H2: the attach URL published after the parent started waiting is
    /// still taken; none in time is an error, never an empty URL.
    #[test]
    fn late_attach_url_is_taken() {
        let d = handoff_dir();
        let h = handoff(&d, Some("http://127.0.0.1:4321/probe"), 60_000);
        let w = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            write_handoff(&h).unwrap();
        });
        let end = Instant::now() + Duration::from_secs(5);
        assert_eq!(
            await_attach_url(&d, "r4", end, || None),
            Ok("http://127.0.0.1:4321/probe".into())
        );
        w.join().unwrap();
        let other = handoff_dir();
        let e = await_attach_url(
            &other,
            "r4",
            Instant::now() + Duration::from_millis(300),
            || None,
        );
        assert!(e.is_err(), "{e:?}");
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&other);
    }

    /// H2 (r3): a detached run that exits early ends the launcher's waits
    /// at once, with its exit status and report path, instead of waiting
    /// out the run's budget; a finished matrix still counts after exit.
    #[test]
    fn early_detached_exit_ends_the_wait() {
        let short_lived = || {
            #[cfg(windows)]
            let c = Command::new("cmd").args(["/c", "exit 3"]).spawn();
            #[cfg(not(windows))]
            let c = Command::new("sh").args(["-c", "exit 3"]).spawn();
            c.unwrap()
        };
        let d = handoff_dir();
        let end = Instant::now() + Duration::from_secs(60);
        let t0 = Instant::now();
        let mut c = short_lived();
        let mut exited = || c.try_wait().ok().flatten().map(|s| s.to_string());
        let e = await_attach_url(&d, "r4", end, &mut exited).unwrap_err();
        assert!(e.contains("exited early") && e.contains("3"), "{e}");
        assert!(
            e.contains(&d.join("report.json").display().to_string()),
            "{e}"
        );
        let m = await_matrix(&d, end, Duration::from_millis(50), &mut exited, || {});
        assert_eq!(m, Err(e.clone()));
        assert!(t0.elapsed() < Duration::from_secs(10), "{:?}", t0.elapsed());
        let lines = close_step_lines(Path::new("chs.exe"), &Ok("u".into()), &m, "c");
        assert!(lines.iter().any(|l| l.contains(&e)), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("close this terminal,")));
        let mut h = handoff(&d, Some("http://127.0.0.1:1/probe"), 60_000);
        h.matrix_done = true;
        write_handoff(&h).unwrap();
        let mut c = short_lived();
        let _ = c.wait();
        let mut exited = || c.try_wait().ok().flatten().map(|s| s.to_string());
        assert_eq!(
            await_matrix(&d, end, Duration::from_millis(50), &mut exited, || {}),
            Ok(())
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// H2: the terminal-close instruction is printed only with a valid URL
    /// and after the matrix; otherwise the owner is told how to proceed.
    #[test]
    fn close_step_needs_a_url_and_the_matrix() {
        let exe = Path::new(r"C:\chs\t\release\chs.exe");
        let url: Result<String, String> = Ok("http://127.0.0.1:4321/probe".into());
        let has_close = |l: &[String]| l.iter().any(|l| l.contains("close this terminal,"));
        let ok = close_step_lines(exe, &url, &Ok(()), "c");
        assert!(has_close(&ok), "{ok:?}");
        assert!(
            ok.iter()
                .any(|l| l.contains("--url http://127.0.0.1:4321/probe"))
        );
        let none = close_step_lines(exe, &Err("no URL".into()), &Ok(()), "c");
        assert!(!has_close(&none), "{none:?}");
        assert!(none.iter().any(|l| l.contains("step 5")), "{none:?}");
        assert!(!none.iter().any(|l| l.contains("--url")), "{none:?}");
        let late = Err("not finished".into());
        assert!(!has_close(&close_step_lines(exe, &url, &late, "c")));
    }

    /// H2: step 5 waits for the detached run's final report (it is written
    /// only when the run ends) and evaluates it once it appears.
    #[test]
    fn final_report_appearing_during_the_wait_is_read() {
        let d = handoff_dir();
        write_handoff(&handoff(&d, Some("http://127.0.0.1:1/probe"), 60_000)).unwrap();
        let path = d.join("report.json");
        let w = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            let r = Report::new("win-probe console", "r4");
            util::write_atomic(&path, &serde_json::to_vec(&r).unwrap()).unwrap();
        });
        let mut ticks = 0;
        let r = wait_final_report(
            &d,
            Instant::now() + Duration::from_secs(10),
            Duration::from_millis(20),
            Duration::from_millis(100),
            |_| ticks += 1,
        );
        w.join().unwrap();
        assert_eq!(r.map(|r| r.run_id), Ok("r4".into()));
        assert!(ticks > 0, "progress was printed");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// H2: a report still absent at the handoff's deadline is reported as
    /// such, within that deadline (no verdict is persisted from it).
    #[test]
    fn absent_final_report_is_an_error_at_the_deadline() {
        let d = handoff_dir();
        let mut h = handoff(&d, None, 0);
        h.deadline_unix_ms = unix_ms().saturating_sub(CHILD_MARGIN * 1000) + 300;
        write_handoff(&h).unwrap();
        let t = Instant::now();
        let r = wait_final_report(
            &d,
            Instant::now() + Duration::from_secs(10),
            Duration::from_millis(20),
            Duration::from_secs(60),
            |_| {},
        );
        assert!(
            r.as_ref().is_err_and(|e| e.contains("did not appear")),
            "{:?}",
            r.map(|r| r.run_id)
        );
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        let empty = handoff_dir();
        assert!(
            wait_final_report(
                &empty,
                Instant::now(),
                Duration::ZERO,
                Duration::ZERO,
                |_| {}
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&empty);
    }

    /// H2: F-2 needs C-5's detached and observer-health cases (calibration
    /// and both controls); a broken control leaves every candidate
    /// incomplete even when each launch passed.
    #[test]
    fn f2_needs_detached_observer_health() {
        for case in [
            "detached",
            "calibration",
            "calibration/caret_control",
            "calibration/at_exit_control",
        ] {
            let (a, mut b) = f2_reports(&[], &[]);
            let mut cases: Vec<Check> =
                serde_json::from_value(b.probes[0].evidence["cases"].clone()).unwrap();
            cases.retain(|c| c.case != case);
            cases.push(chk("C-5", case, Outcome::harness("observer failed")));
            b.probes[0].evidence["cases"] = json!(cases);
            b.probes[0].verdict = "inconclusive".into();
            assert_eq!(f2_evaluate(&a, &b).0, "inconclusive", "{case}");
        }
    }

    fn raw_launch(flags: u64, branch: &str) -> Value {
        json!({"backend": "raw", "conpty_flags": flags, "branch": branch,
               "job_list_rejected": false})
    }

    /// H2 / M9: a row counts only on a launch with its configuration's own
    /// backend and flags; only a failed JOB_LIST creation call is a branch
    /// failure, and it stays one when both branches failed.
    #[test]
    fn branch_gate_validates_config_and_provenance() {
        let harness = |o: Outcome| matches!(o, Outcome::HarnessFail { .. });
        let portable = json!({"backend": "portable", "branch": "assign_after"});
        assert!(harness(branch_gate(
            "R0",
            Outcome::Pass,
            &[raw_launch(6, "job_list")]
        )));
        assert!(harness(branch_gate(
            "R6",
            Outcome::Pass,
            std::slice::from_ref(&portable)
        )));
        assert!(harness(branch_gate(
            "P",
            Outcome::Pass,
            &[raw_launch(0, "job_list")]
        )));
        assert!(harness(branch_gate("P", Outcome::Pass, &[])));
        // Suspended after an attribute-setup failure: no JOB_LIST rejection.
        let attr = json!({"backend": "raw", "conpty_flags": 0, "branch": "suspended",
            "job_list_rejected": false, "job_list_error": "attr job_list: denied",
            "attempts": [{"branch": "job_list", "stage": "attr_job_list", "error": "denied"},
                         {"branch": "suspended", "stage": "created"}]});
        assert!(harness(branch_gate("R0", Outcome::Pass, &[attr])));
        // Both branches failed after a JOB_LIST creation rejection.
        let both = json!({"backend": "raw", "conpty_flags": 0, "branch": "none",
            "job_list_rejected": true,
            "attempts": [{"branch": "job_list", "stage": "create_process", "error": "87"},
                         {"branch": "suspended", "stage": "create_process", "error": "5"}]});
        assert!(matches!(
            branch_gate("R0", Outcome::harness("launch failed"), &[both]),
            Outcome::PlatformFail { .. }
        ));
        assert_eq!(
            branch_gate("R6", Outcome::Pass, &[raw_launch(6, "job_list")]),
            Outcome::Pass
        );
        assert_eq!(branch_gate("P", Outcome::Pass, &[portable]), Outcome::Pass);
        // L2: a JOB_LIST rejection on a launch of another configuration's
        // flags is a mismatched launch (harness), not R0's branch failure.
        let other = json!({"backend": "raw", "conpty_flags": 6, "branch": "none",
            "job_list_rejected": true});
        assert!(harness(branch_gate(
            "R0",
            Outcome::harness("launch failed"),
            std::slice::from_ref(&other)
        )));
        assert!(harness(branch_gate("P", Outcome::Pass, &[other])));
    }

    /// H2: a console launch counts only on its configuration's own JOB_LIST
    /// launch (a raw launch that fell back to `suspended` under the
    /// observer's job never supplies passing C-5 evidence).
    #[test]
    fn console_launch_validates_the_launch() {
        let mut rep = Report::new("real claude-hooks", "r");
        rep.check("A-1", "session_start_bound", Outcome::Pass, Value::Null);
        rep.detail(
            "launch-x",
            json!({"stop": {"launch": raw_launch(0, "suspended")}}),
        );
        let o = console_launch("R0", "claude-hooks", &Ok(rep.clone())).0;
        assert!(!matches!(o, Outcome::Pass), "{o:?}");
        rep.detail(
            "launch-x",
            json!({"stop": {"launch": raw_launch(0, "job_list")}}),
        );
        assert_eq!(
            console_launch("R0", "claude-hooks", &Ok(rep.clone())).0,
            Outcome::Pass
        );
        let o = console_launch("R6", "claude-hooks", &Ok(rep)).0;
        assert!(!matches!(o, Outcome::Pass), "{o:?}");
    }

    /// Row outcomes read the child's own required cases.
    #[test]
    fn child_rows_follow_required_cases() {
        let mut r = Report::new("real tree-kill", "x");
        r.required = vec![
            verdict::key("A-8", "codex_mcp_witness_tree"),
            verdict::key("A-8", "codex_kill_proof"),
        ];
        r.checks = vec![chk("A-8", "codex_mcp_witness_tree", Outcome::Pass)];
        let rep: ChildRep = Ok(r.clone());
        assert_eq!(child_cases_outcome(&rep, &["A-8"]).0, Outcome::Missing);
        r.checks
            .push(chk("A-8", "codex_kill_proof", Outcome::platform("escapee")));
        assert!(matches!(
            child_cases_outcome(&Ok(r.clone()), &["A-8"]).0,
            Outcome::PlatformFail { .. }
        ));
        r.checks.push(chk("A-8", "codex_kill_proof", Outcome::Pass));
        assert_eq!(child_cases_outcome(&Ok(r), &["A-8"]).0, Outcome::Pass);
        assert!(matches!(
            child_cases_outcome(&Err("not_run".into()), &["A-8"]).0,
            Outcome::HarnessFail { .. }
        ));
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

    /// M6: the resize passes only when the harness's own screen took the
    /// new geometry and shows a marker the child drew after the resize.
    #[test]
    fn resize_needs_the_resized_screen() {
        let ok = || ResizeSeen {
            resized: Ok(()),
            child_reported: true,
            report_present: true,
            screen: (100, 30),
            want: (100, 30),
            marker_before: Some(4),
            marker_after: Some(6),
            size_marker: Some(6),
        };
        let emitted = [3, 4, 5, 6];
        assert_eq!(resize_outcome(&ok(), &emitted), Outcome::Pass);
        // The reviewer's shape: the child saw 100x30, the grid stayed 120x40.
        let stale = ResizeSeen {
            screen: (120, 40),
            ..ok()
        };
        let o = resize_outcome(&stale, &emitted);
        assert!(matches!(o, Outcome::HarnessFail { .. }), "{o:?}");
        // No new content drawn on the resized screen.
        let same = ResizeSeen {
            marker_after: Some(4),
            ..ok()
        };
        assert!(matches!(
            resize_outcome(&same, &emitted),
            Outcome::PlatformFail { .. }
        ));
        let none = ResizeSeen {
            marker_after: None,
            ..ok()
        };
        assert!(matches!(
            resize_outcome(&none, &emitted),
            Outcome::PlatformFail { .. }
        ));
        // L1: a marker printed after the resize call but before the child
        // observed the new size is not resized content.
        let early = ResizeSeen {
            marker_after: Some(5),
            ..ok()
        };
        let o = resize_outcome(&early, &emitted);
        assert!(matches!(o, Outcome::PlatformFail { .. }), "{o:?}");
        // The child never recorded a marker after the new size.
        let unknown = ResizeSeen {
            size_marker: None,
            ..ok()
        };
        let o = resize_outcome(&unknown, &emitted);
        assert!(matches!(o, Outcome::HarnessFail { .. }), "{o:?}");
    }

    /// L1 (r3): markers are monotonic, so the screen may show a marker
    /// newer than the last report (which ends at the size marker N): N and
    /// N+2 pass, N-1 does not.
    #[test]
    fn resize_accepts_markers_after_the_last_report() {
        let seen = |after: u64| ResizeSeen {
            resized: Ok(()),
            child_reported: true,
            report_present: true,
            screen: (100, 30),
            want: (100, 30),
            marker_before: Some(4),
            marker_after: Some(after),
            size_marker: Some(6),
        };
        let emitted = [3, 4, 5, 6];
        assert_eq!(resize_outcome(&seen(6), &emitted), Outcome::Pass);
        assert_eq!(resize_outcome(&seen(8), &emitted), Outcome::Pass);
        let o = resize_outcome(&seen(5), &emitted);
        assert!(matches!(o, Outcome::PlatformFail { .. }), "{o:?}");
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

    /// C-1: the required child cases follow the encoding in use; a child
    /// report of either shape aggregates with no missing case, and removing
    /// one row yields exactly that case as missing.
    #[test]
    fn c1_aggregates_both_report_shapes() {
        let entries: Vec<String> = vec!["japanese".into(), "astral".into()];
        let bracketed = c1_child_cases("claude", InputEncoding::BracketedPaste, &entries);
        assert_eq!(bracketed.len(), 3 * 2 * 2);
        assert!(bracketed.contains(&"claude/crlf/c7/astral".to_string()));
        let keys = c1_child_cases(
            "codex",
            InputEncoding::WinKeys {
                newline: NewlineKey::W32,
            },
            &entries,
        );
        assert_eq!(
            keys,
            vec!["codex/keys/w32/japanese", "codex/keys/w32/astral"]
        );
        for (agent, enc, cases) in [
            ("claude", InputEncoding::BracketedPaste, bracketed),
            (
                "codex",
                InputEncoding::WinKeys {
                    newline: NewlineKey::W32,
                },
                keys,
            ),
        ] {
            let mut rep = Report::new("real x-paste", "r");
            for c in &cases {
                rep.check("A-6", c, Outcome::Pass, Value::Null);
            }
            let mut pc = Pc::new("C-1");
            carry_c1(&mut pc, agent, enc, &entries, &Ok(rep.clone()));
            let e = verdict::evaluate(&pc.required, &pc.checks);
            assert!(e.complete() && e.missing.is_empty(), "{agent}: {e:?}");
            let gone = cases[1].clone();
            rep.checks.retain(|c| c.case != gone);
            let mut pc = Pc::new("C-1");
            carry_c1(&mut pc, agent, enc, &entries, &Ok(rep));
            let e = verdict::evaluate(&pc.required, &pc.checks);
            assert_eq!(
                e.missing,
                vec![verdict::key("C-1", &format!("real/{gone}"))]
            );
        }
    }
}
