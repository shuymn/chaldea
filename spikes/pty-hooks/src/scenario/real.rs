//! Set A (and the real-CLI parts of set C): `chs real ...`, `chs print-launch`.
//! Launches the owner's real Claude Code / Codex; never run without
//! `--allow-api-usage`.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::hash::{Hash, Hasher};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{Ctx, Launch, LaunchSpec, cli_version, hook_command, launch, write_snapshot};
use crate::cli::{self, Args};
use crate::paste::{self, Newline, PasteOpts};
use crate::report::result_str;
use crate::session::{self, Session, SpawnOpts};
use crate::{os, util};

pub const SCENARIOS: &[&str] = &[
    "claude-hooks",
    "claude-paste",
    "claude-stall",
    "codex-hooks",
    "codex-paste",
    "codex-stall",
    "codex-warm",
    "tree-kill",
];

const CLAUDE_EVENTS: &[&str] = &["SessionStart", "UserPromptSubmit", "Stop", "SessionEnd"];
const CODEX_EVENTS: &[&str] = &["SessionStart", "UserPromptSubmit", "Stop"];
const REAL_UPS_WAIT: Duration = Duration::from_secs(30);
const REAL_STOP_WAIT: Duration = Duration::from_secs(120);

// ---------------- binaries ----------------

/// Resolves a CLI binary: explicit path (required on Windows; .cmd/.bat/.ps1 rejected) or PATH.
pub fn resolve_bin(name: &str, explicit: Option<&PathBuf>) -> Result<PathBuf, String> {
    if let Some(p) = explicit {
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ["cmd", "bat", "ps1"].contains(&ext.as_str()) {
            return Err(format!(
                "--{name}-bin {}: .{ext} wrappers cannot be launched directly; pass the native binary",
                p.display()
            ));
        }
        if !p.is_file() {
            return Err(format!("--{name}-bin {}: not a file", p.display()));
        }
        return std::fs::canonicalize(p).map_err(|e| e.to_string());
    }
    if os::os_name() == "windows" {
        return Err(format!(
            "--{name}-bin is required on Windows (native binary, not the npm wrapper)"
        ));
    }
    let file = util::bin_name(name);
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join(&file))
        .find(|p| p.is_file())
        .and_then(|p| std::fs::canonicalize(p).ok())
        .ok_or_else(|| format!("{name} not found on PATH; pass --{name}-bin PATH"))
}

fn needed_agents(which: &str, args: &Args) -> Result<Vec<&'static str>, String> {
    Ok(match which {
        w if w.starts_with("claude-") => vec!["claude"],
        w if w.starts_with("codex-") => vec!["codex"],
        "tree-kill" => match args.agent.as_deref() {
            Some("claude") => vec!["claude"],
            Some("codex") => vec!["codex"],
            _ => return Err("real tree-kill needs --agent claude|codex".into()),
        },
        _ => return Err(format!("unknown real scenario {which:?}\n{}", cli::USAGE)),
    })
}

pub struct Bins {
    pub claude: Option<PathBuf>,
    pub codex: Option<PathBuf>,
}

/// Validates flags and binaries before anything is created or launched.
pub fn preflight(args: &Args) -> Result<(String, Bins), String> {
    let which = args.positional.get(1).cloned().unwrap_or_default();
    let agents = needed_agents(&which, args)?;
    if !args.allow_api_usage {
        return Err(format!(
            "chs real {which}: refusing to launch real CLIs without --allow-api-usage \
             (they run under the owner's account and may use the API)"
        ));
    }
    if args.hook_source == "codex-home" && !args.i_approve_global_hooks {
        return Err(
            "--hook-source codex-home rewrites ~/.codex/hooks.json; add --i-approve-global-hooks"
                .into(),
        );
    }
    let mut bins = Bins {
        claude: None,
        codex: None,
    };
    for a in agents {
        match a {
            "claude" => bins.claude = Some(resolve_bin("claude", args.claude_bin.as_ref())?),
            _ => bins.codex = Some(resolve_bin("codex", args.codex_bin.as_ref())?),
        }
    }
    Ok((which, bins))
}

pub fn run(args: Args) -> i32 {
    let (which, bins) = match preflight(&args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let ctx = match Ctx::new(args, &format!("real {which}")) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("chs: {e}");
            return 3;
        }
    };
    let claude = bins.claude.as_ref().map(|b| Agent::claude(b));
    let codex = bins.codex.as_ref().map(|b| Agent::codex(b));
    ctx.sink.with(|r| {
        r.config["bins"] = json!({"claude": claude.as_ref().map(|a| &a.bin), "codex": codex.as_ref().map(|a| &a.bin)});
        r.config["versions"] =
            json!({"claude": claude.as_ref().map(|a| &a.version), "codex": codex.as_ref().map(|a| &a.version)});
        r.config["hook_source"] = json!(ctx.args.hook_source);
        r.config["codex_mode"] = json!(ctx.args.codex_mode);
    });
    match which.as_str() {
        "claude-hooks" => claude_hooks(&ctx, claude.as_ref().expect("resolved")),
        "claude-paste" => paste_sweep(&ctx, claude.as_ref().expect("resolved")),
        "claude-stall" => stall(&ctx, claude.as_ref().expect("resolved")),
        "codex-hooks" => codex_hooks(&ctx, codex.as_ref().expect("resolved")),
        "codex-paste" => paste_sweep(&ctx, codex.as_ref().expect("resolved")),
        "codex-stall" => stall(&ctx, codex.as_ref().expect("resolved")),
        "codex-warm" => codex_warm(&ctx, codex.as_ref().expect("resolved")),
        "tree-kill" => tree_kill(&ctx, claude.as_ref().or(codex.as_ref()).expect("resolved")),
        _ => unreachable!(),
    }
    ctx.finish()
}

// ---------------- launch construction ----------------

#[derive(Clone)]
pub struct Agent {
    pub name: &'static str,
    pub bin: PathBuf,
    pub version: String,
}

impl Agent {
    fn claude(bin: &Path) -> Agent {
        Agent {
            name: "claude",
            bin: bin.to_path_buf(),
            version: cli_version(bin),
        }
    }
    fn codex(bin: &Path) -> Agent {
        Agent {
            name: "codex",
            bin: bin.to_path_buf(),
            version: cli_version(bin),
        }
    }
    fn cli_names(&self) -> Vec<String> {
        let stem = self
            .bin
            .file_stem()
            .and_then(|s| s.to_str())
            .map(util::normalize_proc_name);
        let mut v = vec![self.name.to_string()];
        v.extend(stem);
        v
    }
}

pub fn claude_settings(root: &Path, hook_agent: &str) -> Value {
    let cmd = hook_command(root, hook_agent);
    let mut hooks = serde_json::Map::new();
    for ev in CLAUDE_EVENTS {
        hooks.insert(
            ev.to_string(),
            json!([{"hooks": [{"type": "command", "command": cmd, "timeout": 10}]}]),
        );
    }
    json!({"hooks": hooks})
}

fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `-c hooks.<Event>=[...]` arguments in fixed order (§4.3 session-flags).
pub fn codex_hook_args(cmd: &str) -> Vec<String> {
    let mut v = Vec::new();
    for ev in CODEX_EVENTS {
        v.push("-c".into());
        v.push(format!(
            "hooks.{ev}=[{{hooks=[{{type=\"command\",command={},timeout=10}}]}}]",
            toml_str(cmd)
        ));
    }
    v
}

pub fn codex_hooks_json(cmd: &str) -> Value {
    let mut hooks = serde_json::Map::new();
    for ev in CODEX_EVENTS {
        hooks.insert(
            ev.to_string(),
            json!([{"hooks": [{"type": "command", "command": cmd, "timeout": 10}]}]),
        );
    }
    json!({"hooks": hooks})
}

struct Plan {
    argv: Vec<OsString>,
    session_id: Option<String>,
}

/// Builds argv for one launch. `hook_agent` is the emitter argument (mutated for A-4).
fn plan(
    ctx: &Ctx,
    a: &Agent,
    label: &str,
    mode: Option<&str>,
    hook_agent: &str,
    extra: &[String],
) -> Result<Plan, String> {
    let mut argv: Vec<OsString> = vec![a.bin.clone().into()];
    if a.name == "claude" {
        let settings = ctx.run_dir.join(format!("claude-settings-{label}.json"));
        std::fs::write(
            &settings,
            serde_json::to_vec_pretty(&claude_settings(&ctx.args.root, hook_agent))
                .unwrap_or_default(),
        )
        .map_err(|e| e.to_string())?;
        os::set_private_file(&settings).map_err(|e| e.to_string())?;
        let sid = uuid::Uuid::new_v4().to_string();
        argv.extend([
            "--settings".into(),
            settings.into_os_string(),
            "--session-id".into(),
            sid.clone().into(),
        ]);
        argv.extend(extra.iter().map(OsString::from));
        return Ok(Plan {
            argv,
            session_id: Some(sid),
        });
    }
    if mode == Some("no-daemon") {
        argv.push("--no-daemon".into());
    }
    if ctx.args.hook_source == "session-flags" {
        argv.extend(
            codex_hook_args(&hook_command(&ctx.args.root, hook_agent))
                .into_iter()
                .map(OsString::from),
        );
    }
    argv.extend(extra.iter().map(OsString::from));
    Ok(Plan {
        argv,
        session_id: None,
    })
}

fn start(
    ctx: &Ctx,
    a: &Agent,
    label: &str,
    cwd: &Path,
    mode: Option<&str>,
    hook_agent: &str,
    extra: &[String],
) -> Result<Launch, String> {
    let p = plan(ctx, a, label, mode, hook_agent, extra)?;
    std::fs::create_dir_all(cwd).map_err(|e| format!("{}: {e}", cwd.display()))?;
    launch(
        ctx,
        LaunchSpec {
            label: label.into(),
            argv: p.argv,
            cwd: cwd.to_path_buf(),
            agent: a.name.into(),
            cli_version: a.version.clone(),
            session_id: p.session_id,
            cli_names: a.cli_names(),
            extra_env: Vec::new(),
            responder: true,
            hold_assign: false,
            inject: None,
        },
    )
}

fn default_cwd(ctx: &Ctx, a: &Agent) -> PathBuf {
    ctx.args
        .cwd
        .clone()
        .unwrap_or_else(|| ctx.args.root.join("work").join(a.name))
}

/// Waits for the bound SessionStart; on timeout writes the snapshot (decided_by hook_timeout).
fn wait_start(ctx: &Ctx, l: &Launch) -> (Option<String>, u64, Option<PathBuf>) {
    let t = Instant::now();
    let bound = l
        .receiver
        .wait_bound(ctx.until(Duration::from_secs(ctx.args.session_start_timeout)));
    let ms = t.elapsed().as_millis() as u64;
    if bound.is_some() {
        return (bound, ms, None);
    }
    let mut s = l.snapshot();
    s.decided_by = Some("hook_timeout".into());
    let p = write_snapshot(ctx, &l.label, &s);
    (None, ms, Some(p))
}

fn observed(l: &Launch) -> Vec<String> {
    let set: BTreeSet<String> = l
        .receiver
        .events()
        .into_iter()
        .filter(|e| e.is_bound())
        .map(|e| e.event)
        .collect();
    set.into_iter().collect()
}

fn finish(ctx: &Ctx, l: Launch) -> Value {
    let label = l.label.clone();
    let q = l.query_log();
    let saw_9001 = l
        .session
        .io()
        .saw_win32_input_mode
        .load(std::sync::atomic::Ordering::SeqCst);
    let (stop, summary) = l.finish();
    let v = json!({"stop": stop, "receiver": summary, "query_log": q, "csi_9001h_seen": saw_9001});
    ctx.detail(&format!("launch-{label}"), v.clone());
    v
}

fn submit(ctx: &Ctx, l: &Launch, text: &str, mode: Newline) -> paste::Outcome {
    paste::paste(
        &l.session.io().tx,
        text.as_bytes(),
        &PasteOpts {
            mode,
            chunk: ctx.args.chunk,
            enter_delay: Duration::from_millis(ctx.args.enter_delay_ms),
            write_deadline: ctx.until(Duration::from_secs(10)),
        },
        || l.gate_open(),
    )
}

fn nth_bound(
    l: &Launch,
    event: &str,
    n: usize,
    deadline: Instant,
) -> Option<crate::receiver::EventRecord> {
    loop {
        let v: Vec<_> = l
            .receiver
            .events()
            .into_iter()
            .filter(|e| e.event == event && e.is_bound())
            .collect();
        if v.len() >= n {
            return v.into_iter().nth(n - 1);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ---------------- global config fingerprints (A-5, C1) ----------------

fn fingerprint(bytes: &[u8]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    format!("siphash:{:016x}:len{}", h.finish(), bytes.len())
}

pub fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::home_dir().unwrap_or_default().join(".codex"))
}

fn trust_keys(config: &[u8]) -> BTreeSet<String> {
    String::from_utf8_lossy(config)
        .lines()
        .filter_map(|l| {
            l.trim()
                .strip_prefix("[hooks.state.")
                .and_then(|r| r.strip_suffix(']'))
                .map(str::to_string)
        })
        .collect()
}

#[derive(Clone)]
struct ConfigState {
    fp: Option<String>,
    keys: BTreeSet<String>,
}

fn codex_config() -> ConfigState {
    let b = std::fs::read(codex_home().join("config.toml")).ok();
    ConfigState {
        fp: b.as_deref().map(fingerprint),
        keys: b.as_deref().map(trust_keys).unwrap_or_default(),
    }
}

fn config_delta(before: &ConfigState, after: &ConfigState) -> Value {
    json!({
        "config_toml_before": before.fp,
        "config_toml_after": after.fp,
        "hooks_state_keys_added": after.keys.difference(&before.keys).collect::<Vec<_>>(),
        "hooks_state_keys_removed": before.keys.difference(&after.keys).collect::<Vec<_>>(),
    })
}

fn claude_global_hooks() -> Value {
    let home = std::env::home_dir().unwrap_or_default();
    let mut out = serde_json::Map::new();
    for f in [
        home.join(".claude").join("settings.json"),
        home.join(".claude.json"),
    ] {
        let v = std::fs::read(&f)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        let hooks = v
            .as_ref()
            .and_then(|v| v.get("hooks"))
            .map(|h| fingerprint(h.to_string().as_bytes()));
        out.insert(f.display().to_string(), json!(hooks));
    }
    Value::Object(out)
}

// ---------------- codex-home hooks.json backup / restore (§4.3) ----------------

fn meta_path(home: &Path) -> PathBuf {
    home.join("hooks.json.chs-backup.json")
}

/// Backs up `<home>/hooks.json`, records metadata next to it, then writes ours.
fn install_global_hooks(home: &Path, run_id: &str, content: &[u8]) -> Result<(), String> {
    let target = home.join("hooks.json");
    let meta = meta_path(home);
    if meta.exists() {
        return Err(format!(
            "{} exists; a previous run has not been restored",
            meta.display()
        ));
    }
    if std::fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!("{} is a symlink; refusing", target.display()));
    }
    let backup = home.join(format!("hooks.json.chs-backup-{run_id}"));
    let had = target.exists();
    let mode = os::file_mode(&target);
    if had {
        std::fs::copy(&target, &backup).map_err(|e| format!("backup: {e}"))?;
    }
    let me = os::proc_info(std::process::id())
        .map(|p| p.start_time)
        .unwrap_or(0);
    let m = json!({
        "run_id": run_id, "harness_pid": std::process::id(), "harness_start": me,
        "backup_path": had.then(|| backup.display().to_string()), "mode": mode,
        "written": String::from_utf8_lossy(content),
    });
    util::write_atomic(&meta, m.to_string().as_bytes()).map_err(|e| format!("meta: {e}"))?;
    let h = home.to_path_buf();
    session::watchdog_cleanup(
        "codex-home hooks.json",
        Box::new(move || restore_global_hooks(&h, false)),
    );
    util::write_atomic(&target, content).map_err(|e| format!("write hooks.json: {e}"))?;
    Ok(())
}

/// Restores the backed-up hooks.json if it still holds exactly what we wrote.
/// With `only_if_dead`, a metadata file from a still-running harness is left alone.
pub fn restore_global_hooks(home: &Path, only_if_dead: bool) -> String {
    let meta = meta_path(home);
    let Ok(raw) = std::fs::read(&meta) else {
        return "no metadata".into();
    };
    let Ok(m) = serde_json::from_slice::<Value>(&raw) else {
        return "metadata unreadable; left in place".into();
    };
    let pid = m["harness_pid"].as_u64().unwrap_or(0) as u32;
    let start = m["harness_start"].as_u64().unwrap_or(0);
    if only_if_dead && os::is_alive(pid, start) {
        return "owning harness still running".into();
    }
    let target = home.join("hooks.json");
    let written = m["written"].as_str().unwrap_or("").as_bytes().to_vec();
    match std::fs::read(&target) {
        Ok(cur) if cur == written => {}
        Ok(_) => {
            return format!(
                "{} changed since we wrote it; refusing to restore",
                target.display()
            );
        }
        Err(_) => return format!("{} missing; refusing to restore", target.display()),
    }
    let res = match m["backup_path"].as_str() {
        Some(b) => std::fs::copy(b, &target)
            .map(|_| ())
            .and_then(|_| os::set_file_mode(&target, m["mode"].as_u64().map(|v| v as u32)))
            .and_then(|_| std::fs::remove_file(b)),
        None => std::fs::remove_file(&target),
    };
    match res {
        Ok(()) => {
            let _ = std::fs::remove_file(&meta);
            "restored".into()
        }
        Err(e) => format!("restore failed: {e}"),
    }
}

/// Called at every `chs` start (§4.3).
pub fn recover_leftover() {
    let home = codex_home();
    if meta_path(&home).exists() {
        let r = restore_global_hooks(&home, true);
        eprintln!("chs: leftover codex-home hooks.json backup: {r}");
    }
}

/// `project` source: `<cwd>/.codex/hooks.json`, created only if absent.
fn install_project_hooks(cwd: &Path, content: &[u8]) -> Result<PathBuf, String> {
    let dir = cwd.join(".codex");
    let f = dir.join("hooks.json");
    if f.exists() {
        return Err(format!(
            "{} already exists; refusing to overwrite",
            f.display()
        ));
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(&f, content).map_err(|e| e.to_string())?;
    Ok(f)
}

fn remove_project_hooks(f: &Path, content: &[u8]) {
    if std::fs::read(f).is_ok_and(|c| c == content) {
        let _ = std::fs::remove_file(f);
    }
}

// ---------------- scenarios ----------------

fn claude_hooks(ctx: &Ctx, a: &Agent) {
    let before = claude_global_hooks();
    let cwd = default_cwd(ctx, a);
    let l = match start(ctx, a, "claude-hooks", &cwd, None, "claude", &[]) {
        Ok(l) => l,
        Err(e) => return ctx.error(e),
    };
    let (bound, ms, snap) = wait_start(ctx, &l);
    ctx.check(
        "A-1",
        "session_start_bound",
        result_str(bound.is_some()),
        json!({"session_id": bound, "latency_ms": ms, "snapshot": snap}),
    );
    if ctx.args.prompt && bound.is_some() {
        let gate = l.wait_gate(ctx.until(Duration::from_secs(10)));
        let o = submit(ctx, &l, "Reply with the single word OK.", Newline::Cr);
        let ups = nth_bound(&l, "UserPromptSubmit", 1, ctx.until(REAL_UPS_WAIT));
        let has_prompt = ups
            .as_ref()
            .and_then(|e| e.json())
            .is_some_and(|v| v.get("prompt").is_some_and(Value::is_string));
        ctx.check(
            "A-2",
            "user_prompt_submit",
            if gate { result_str(has_prompt) } else { "fail" },
            json!({"paste": o, "gate": gate, "fixture": ups.map(|e| e.fixture)}),
        );
    }
    let obs = observed(&l);
    finish(ctx, l);
    let after = claude_global_hooks();
    ctx.check(
        "C1",
        "no_global_hook_edit",
        result_str(before == after),
        json!({"before": before, "after": after}),
    );
    ctx.check(
        "A-5",
        "claude_registered_vs_observed",
        "pass",
        json!({"registered": CLAUDE_EVENTS, "observed": obs}),
    );
}

fn paste_sweep(ctx: &Ctx, a: &Agent) {
    let corpus: Vec<_> = paste::corpus()
        .into_iter()
        .filter(|e| !e.fake_only && e.text.len() <= 8 * 1024)
        .filter(|e| ctx.args.subset.as_deref() != Some("required") || e.required)
        .collect();
    let _ = std::fs::write(
        ctx.out.join("corpus.json"),
        serde_json::to_vec_pretty(&corpus).unwrap_or_default(),
    );
    let modes: Vec<Newline> = ctx
        .args
        .paste_newline
        .map(|m| vec![m])
        .unwrap_or(Newline::ALL.to_vec());
    let cwd = default_cwd(ctx, a);
    for mode in modes {
        let label = format!("{}-paste-{}", a.name, mode.name());
        let l = match start(ctx, a, &label, &cwd, Some("no-daemon"), a.name, &[]) {
            Ok(l) => l,
            Err(e) => {
                ctx.error(e);
                continue;
            }
        };
        let (bound, ms, snap) = wait_start(ctx, &l);
        if bound.is_none() {
            ctx.check(
                "A-6",
                &format!("{}/{}", a.name, mode.name()),
                "fail",
                json!({"session_start": "timeout", "latency_ms": ms, "snapshot": snap}),
            );
            finish(ctx, l);
            continue;
        }
        let mut n = 0;
        for e in &corpus {
            if ctx.past_cutoff() {
                break;
            }
            let case = format!("{}/{}/{}", a.name, mode.name(), e.id);
            if !l.wait_gate(ctx.until(Duration::from_secs(15))) {
                ctx.check(
                    "A-6",
                    &case,
                    "inconclusive",
                    json!({"readiness_gate_failed": true}),
                );
                continue;
            }
            let o = submit(ctx, &l, &e.text, mode);
            if o.state != "sent" {
                ctx.check("A-6", &case, "fail", json!({"paste": o}));
                continue;
            }
            n += 1;
            let ups = nth_bound(&l, "UserPromptSubmit", n, ctx.until(REAL_UPS_WAIT));
            let got = ups
                .as_ref()
                .and_then(|e| e.json())
                .and_then(|v| v["prompt"].as_str().map(str::to_string));
            let want = paste::normalize_lf(e.text.as_bytes());
            let (res, ev) = match &got {
                Some(g) => {
                    let gotn = paste::normalize_lf(g.as_bytes());
                    let d = util::first_diff(&want, &gotn);
                    (
                        result_str(d.is_none()),
                        json!({"equal": d.is_none(), "first_diff": d, "fixture": ups.as_ref().map(|e| &e.fixture)}),
                    )
                }
                None => ("fail", json!({"ups": "missing"})),
            };
            ctx.check("A-6", &case, res, ev);
            // Let the turn finish before the next paste.
            let _ = nth_bound(&l, "Stop", n, ctx.until(REAL_STOP_WAIT));
        }
        finish(ctx, l);
    }
}

/// A-7 (and C-2's attended launch with `--attended`).
fn stall(ctx: &Ctx, a: &Agent) {
    let cwd = ctx.args.cwd.clone().unwrap_or_else(|| {
        ctx.args
            .root
            .join("work")
            .join(format!("stall-{}-{}", a.name, ctx.run_id))
    });
    let l = match start(
        ctx,
        a,
        &format!("{}-stall", a.name),
        &cwd,
        Some("no-daemon"),
        a.name,
        &[],
    ) {
        Ok(l) => l,
        Err(e) => return ctx.error(e),
    };
    let (bound, ms, snap) = wait_start(ctx, &l);
    let mut attended = Value::Null;
    if bound.is_none() && ctx.args.attended {
        attended = attend(ctx, &l);
    }
    ctx.check(
        "A-7",
        &format!("{}_untrusted_folder_snapshot", a.name),
        "inconclusive",
        json!({"owner_judgment_required": true, "timed_out": bound.is_none(), "latency_ms": ms, "snapshot": snap,
               "attended": attended, "bound_after_keys": l.receiver.bound()}),
    );
    finish(ctx, l);
}

/// Shows the snapshot and relays the owner's keys as ungated `write_key` (C-2).
fn attend(ctx: &Ctx, l: &Launch) -> Value {
    let mut used = Vec::new();
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        for line in l.snapshot().lines.iter().filter(|s| !s.is_empty()) {
            println!("| {line}");
        }
        println!("keys: down enter esc w-down w-enter w-esc | done");
        let Some(Ok(cmd)) = lines.next() else { break };
        let cmd = cmd.trim().to_string();
        if cmd == "done" || ctx.past_cutoff() {
            break;
        }
        if let Some(b) = paste::key_bytes(&cmd) {
            let r = l
                .session
                .io()
                .tx
                .write_wait(vec![b], ctx.until(Duration::from_secs(5)));
            used.push(json!({"key": cmd, "ok": r.is_ok()}));
        }
        if l.receiver
            .wait_bound(ctx.until(Duration::from_secs(2)))
            .is_some()
        {
            break;
        }
    }
    json!({"keys": used, "csi_9001h_seen": l.session.io().saw_win32_input_mode.load(std::sync::atomic::Ordering::SeqCst)})
}

fn codex_modes(ctx: &Ctx) -> Vec<&'static str> {
    match ctx.args.codex_mode.as_str() {
        "both" => vec!["no-daemon", "default"],
        "default" => vec!["default"],
        _ => vec!["no-daemon"],
    }
}

fn codex_hooks(ctx: &Ctx, a: &Agent) {
    let cwd = default_cwd(ctx, a);
    let cmd = hook_command(&ctx.args.root, "codex");
    let content = serde_json::to_vec_pretty(&codex_hooks_json(&cmd)).unwrap_or_default();
    let mut project_file = None;
    match ctx.args.hook_source.as_str() {
        "codex-home" => {
            if let Err(e) = install_global_hooks(&codex_home(), &ctx.run_id, &content) {
                return ctx.error(e);
            }
        }
        "project" => match install_project_hooks(&cwd, &content) {
            Ok(f) => project_file = Some(f),
            Err(e) => return ctx.error(e),
        },
        _ => {}
    }
    let mut runs = Vec::new();
    for mode in codex_modes(ctx) {
        let mut launches: Vec<(PathBuf, String)> = (0..ctx.args.launches.max(1))
            .map(|i| (cwd.clone(), format!("{mode}-{i}")))
            .collect();
        if let Some(c2) = &ctx.args.cwd2 {
            launches.push((c2.clone(), format!("{mode}-cwd2")));
        }
        if ctx.args.mutate_command {
            launches.push((cwd.clone(), format!("{mode}-mutated")));
            launches.push((cwd.clone(), format!("{mode}-restored")));
        }
        for (dir, tag) in launches {
            if ctx.past_cutoff() {
                break;
            }
            let mutated = tag.ends_with("-mutated");
            let hook_agent = if mutated { "codez" } else { "codex" };
            let before = codex_config();
            let label = format!("codex-hooks-{tag}");
            let l = match start(ctx, a, &label, &dir, Some(mode), hook_agent, &[]) {
                Ok(l) => l,
                Err(e) => {
                    ctx.error(e);
                    continue;
                }
            };
            let (bound, ms, snap) = wait_start(ctx, &l);
            let mut ups_seen = None;
            if ctx.args.prompt && bound.is_some() && l.wait_gate(ctx.until(Duration::from_secs(10)))
            {
                let _ = submit(ctx, &l, "Reply with the single word OK.", Newline::Cr);
                ups_seen =
                    Some(nth_bound(&l, "UserPromptSubmit", 1, ctx.until(REAL_UPS_WAIT)).is_some());
                let _ = nth_bound(&l, "Stop", 1, ctx.until(REAL_STOP_WAIT));
            }
            let mentions = snap
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .map(|s| s.contains("codez"));
            let any_event = !l.receiver.events().is_empty();
            let obs = observed(&l);
            finish(ctx, l);
            let after = codex_config();
            let delta = config_delta(&before, &after);
            let base = json!({"mode": mode, "cwd": dir, "session_id": bound, "latency_ms": ms, "snapshot": snap,
                              "registered": CODEX_EVENTS, "observed": obs, "ups_seen": ups_seen, "config": delta,
                              "observed_on": format!("codex {}, source {}, mode {mode}", a.version, ctx.args.hook_source)});
            if mutated {
                ctx.check(
                    "A-4",
                    &format!("{mode}/mutated_not_delivered"),
                    result_str(!any_event),
                    json!({"base": base, "snapshot_mentions_mutated_command": mentions}),
                );
            } else if tag.ends_with("-restored") {
                ctx.check(
                    "A-4",
                    &format!("{mode}/restored_delivers"),
                    result_str(bound.is_some()),
                    base,
                );
            } else if tag.ends_with("-cwd2") {
                let added = after.keys.difference(&before.keys).count();
                ctx.check(
                    "C3",
                    &format!("{mode}/second_cwd_same_key"),
                    result_str(bound.is_some() && added == 0),
                    base,
                );
            } else {
                ctx.check(
                    "A-3",
                    &format!("{mode}/{tag}"),
                    result_str(bound.is_some()),
                    base.clone(),
                );
                ctx.check("A-5", &format!("{mode}/{tag}"), "pass", base);
            }
            runs.push(tag);
        }
    }
    if let Some(f) = project_file {
        remove_project_hooks(&f, &content);
    }
    if ctx.args.hook_source == "codex-home" {
        let r = restore_global_hooks(&codex_home(), false);
        session::watchdog_remove_cleanup("codex-home hooks.json");
        ctx.detail("codex_home_restore", json!(r));
    }
}

fn codex_procs() -> Vec<os::ProcInfo> {
    os::list_processes()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| util::normalize_proc_name(&p.name) == "codex" && !p.zombie)
        .collect()
}

/// A-9: our default-mode Codex next to an unrelated, already-running Codex.
fn codex_warm(ctx: &Ctx, a: &Agent) {
    let cwd = default_cwd(ctx, a);
    let pre: Vec<u32> = codex_procs().iter().map(|p| p.pid).collect();
    // The unrelated Codex: our own PTY, no CHALDEA_* env, default mode.
    let unrelated = Session::spawn(SpawnOpts {
        argv: vec![a.bin.clone().into()],
        cwd: cwd.clone(),
        env: Vec::new(),
        cols: ctx.args.cols,
        rows: ctx.args.rows,
        responder: true,
        out_file: ctx.out.join("pty-output-codex-unrelated.bin"),
        hold_assign: false,
        inject: None,
    });
    let mut unrelated = match unrelated {
        Ok(s) => s,
        Err(e) => return ctx.error(e),
    };
    std::thread::sleep(
        Duration::from_secs(10).min(ctx.cutoff.saturating_duration_since(Instant::now())),
    );
    let warm: Vec<os::ProcInfo> = codex_procs()
        .into_iter()
        .filter(|p| !pre.contains(&p.pid) && p.pid != unrelated.owned.child_pid)
        .collect();
    let l = match start(ctx, a, "codex-warm", &cwd, Some("default"), "codex", &[]) {
        Ok(l) => l,
        Err(e) => {
            unrelated.stop();
            return ctx.error(e);
        }
    };
    let (bound, ms, snap) = wait_start(ctx, &l);
    let ours = l.session.owned.child_pid;
    let warm_pids: Vec<u32> = warm.iter().map(|p| p.pid).collect();
    let from_warm: Vec<u64> = l
        .receiver
        .events()
        .iter()
        .filter(|e| e.ancestry.iter().any(|(p, _)| warm_pids.contains(p)))
        .map(|e| e.seq)
        .collect();
    finish(ctx, l);
    let warm_alive: Vec<u32> = warm
        .iter()
        .filter(|p| os::is_alive(p.pid, p.start_time))
        .map(|p| p.pid)
        .collect();
    let unrelated_alive = os::is_alive(unrelated.owned.child_pid, unrelated.owned.child_start);
    let verdict = result_str(bound.is_some());
    ctx.check(
        "A-9",
        "hooks_in_our_tree",
        verdict,
        json!({"bound": bound, "our_cli_pid": ours, "latency_ms": ms, "snapshot": snap, "warm_server_pids": warm_pids,
               "events_from_warm_tree": from_warm,
               "interpretation": if bound.is_none() && !from_warm.is_empty() { "adapter requires --no-daemon" }
                                 else if bound.is_none() { "delivery failure (no hook arrived)" } else { "ok" }}),
    );
    ctx.check(
        "A-9",
        "unrelated_survives_our_stop",
        result_str(unrelated_alive && warm_alive.len() == warm.len()),
        json!({"unrelated_cli_alive": unrelated_alive, "warm_alive": warm_alive, "warm_before": warm_pids}),
    );
    let s = unrelated.stop();
    ctx.detail("unrelated_stop", json!(s));
}

/// A-8: stop after the MCP sleeper attested; nothing owned survives 5 s later.
fn tree_kill(ctx: &Ctx, a: &Agent) {
    let fake = util::sibling_bin("chs-fake-cli");
    let cwd = default_cwd(ctx, a);
    let extra: Vec<String> = if a.name == "claude" {
        let cfg = ctx.run_dir.join("mcp.json");
        let v = json!({"mcpServers": {"chs": {"command": fake, "args": ["--mode", "sleeper", "--role", "mcp"]}}});
        if let Err(e) = std::fs::write(&cfg, v.to_string()) {
            return ctx.error(e.to_string());
        }
        vec!["--mcp-config".into(), cfg.display().to_string()]
    } else {
        vec![
            "-c".into(),
            format!("mcp_servers.chs.command={}", toml_str(&fake.display().to_string())),
            "-c".into(),
            "mcp_servers.chs.args=[\"--mode\",\"sleeper\",\"--role\",\"mcp\"]".into(),
            "-c".into(),
            "mcp_servers.chs.env_vars=[\"CHALDEA_HOOK_URL\",\"CHALDEA_HOOK_TOKEN\",\"CHALDEA_THREAD_ID\"]".into(),
        ]
    };
    let l = match start(
        ctx,
        a,
        &format!("{}-tree-kill", a.name),
        &cwd,
        Some("no-daemon"),
        a.name,
        &extra,
    ) {
        Ok(l) => l,
        Err(e) => return ctx.error(e),
    };
    let (bound, _, snap) = wait_start(ctx, &l);
    let w = l
        .receiver
        .wait_witness(ctx.until(Duration::from_secs(60)), |w| {
            w.role == "mcp-sleeper"
        });
    let witnesses = l.receiver.witnesses();
    finish(ctx, l);
    std::thread::sleep(
        Duration::from_secs(5).min(ctx.cutoff.saturating_duration_since(Instant::now())),
    );
    let alive: Vec<_> = witnesses
        .iter()
        .filter(|w| w.session && os::is_alive(w.pid, w.start_time))
        .map(|w| w.pid)
        .collect();
    let res = if w.is_none() {
        "inconclusive"
    } else {
        result_str(alive.is_empty())
    };
    ctx.check(
        "A-8",
        &format!("{}_mcp_witness_tree", a.name),
        res,
        json!({"bound": bound, "snapshot": snap, "witness": w, "survivors": alive}),
    );
}

// ---------------- print-launch ----------------

fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-=:".contains(&b))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

pub fn print_launch(args: &Args) -> i32 {
    let which = args.positional.get(1).map(String::as_str).unwrap_or("");
    let (name, explicit) = match which {
        "claude" => ("claude", args.claude_bin.as_ref()),
        "codex" => ("codex", args.codex_bin.as_ref()),
        _ => {
            eprintln!("usage: chs print-launch <claude|codex>");
            return 2;
        }
    };
    let bin = match resolve_bin(name, explicit) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let mut argv = vec![bin.display().to_string()];
    if name == "claude" {
        let dir = args.root.join("print-launch");
        if let Err(e) =
            os::ensure_private_dir(&args.root).and_then(|_| std::fs::create_dir_all(&dir))
        {
            eprintln!("{e}");
            return 3;
        }
        let f = dir.join("claude-settings.json");
        if std::fs::write(
            &f,
            serde_json::to_vec_pretty(&claude_settings(&args.root, "claude")).unwrap_or_default(),
        )
        .is_err()
        {
            return 3;
        }
        argv.extend([
            "--settings".into(),
            f.display().to_string(),
            "--session-id".into(),
            uuid::Uuid::new_v4().to_string(),
        ]);
    } else {
        if args.codex_mode == "no-daemon" {
            argv.push("--no-daemon".into());
        }
        argv.extend(codex_hook_args(&hook_command(&args.root, "codex")));
    }
    println!("cwd: {}", args.root.join("work").join(name).display());
    println!("env (names only; values are per launch):");
    println!("  CHALDEA_HOOK_URL=http://127.0.0.1:<port>/hook");
    println!("  CHALDEA_HOOK_TOKEN=<64-hex, fresh per launch>");
    println!("  CHALDEA_THREAD_ID=<uuid, fresh per launch>");
    println!("command:");
    println!(
        "  {}",
        argv.iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ")
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_flag_is_valid_inline_toml_shape() {
        let a = codex_hook_args("/x/chs-hook-emit codex");
        assert_eq!(a.len(), 6);
        assert_eq!(
            a[1],
            "hooks.SessionStart=[{hooks=[{type=\"command\",command=\"/x/chs-hook-emit codex\",timeout=10}]}]"
        );
    }

    #[test]
    fn trust_keys_parsed() {
        let k = trust_keys(b"[x]\n[hooks.state.\"<session-flags>/config.toml:session_start:0:0\"]\ntrusted_hash = \"sha256:1\"\n");
        assert_eq!(
            k.into_iter().collect::<Vec<_>>(),
            vec!["\"<session-flags>/config.toml:session_start:0:0\"".to_string()]
        );
    }

    fn tmp_home() -> PathBuf {
        let d = std::env::temp_dir().join(format!("chs-codex-home-{}", util::run_id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn global_hooks_backup_restores_original() {
        let h = tmp_home();
        std::fs::write(h.join("hooks.json"), b"original").unwrap();
        install_global_hooks(&h, "r1", b"ours").unwrap();
        assert_eq!(std::fs::read(h.join("hooks.json")).unwrap(), b"ours");
        assert!(
            install_global_hooks(&h, "r2", b"ours").is_err(),
            "second install must refuse"
        );
        // Our own harness is alive: a start-time recovery must leave it alone.
        assert_eq!(
            restore_global_hooks(&h, true),
            "owning harness still running"
        );
        assert_eq!(restore_global_hooks(&h, false), "restored");
        assert_eq!(std::fs::read(h.join("hooks.json")).unwrap(), b"original");
        assert!(!meta_path(&h).exists());
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn global_hooks_restore_refuses_concurrent_change() {
        let h = tmp_home();
        install_global_hooks(&h, "r1", b"ours").unwrap();
        std::fs::write(h.join("hooks.json"), b"someone else").unwrap();
        assert!(restore_global_hooks(&h, false).contains("refusing"));
        assert_eq!(
            std::fs::read(h.join("hooks.json")).unwrap(),
            b"someone else"
        );
        assert!(meta_path(&h).exists());
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn global_hooks_absent_original_is_removed() {
        let h = tmp_home();
        install_global_hooks(&h, "r1", b"ours").unwrap();
        assert_eq!(restore_global_hooks(&h, false), "restored");
        assert!(!h.join("hooks.json").exists());
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn wrappers_rejected() {
        let e = resolve_bin("codex", Some(&PathBuf::from("C:/x/codex.cmd"))).unwrap_err();
        assert!(e.contains(".cmd"));
    }
}
