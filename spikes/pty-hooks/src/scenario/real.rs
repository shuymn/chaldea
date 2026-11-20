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
use crate::session::{Session, SpawnOpts};
use crate::verdict::Outcome;
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
/// Codex fires no hook at startup: SessionStart, UserPromptSubmit and Stop
/// all fire with the first turn. Every Codex launch therefore passes this
/// first prompt as positional argv, so the session starts without a single
/// keystroke through the PTY (a key typed under a hook-review dialog can
/// approve hooks).
pub const CODEX_FIRST_PROMPT: &str = "Reply with the single word OK.";
/// Trust key Codex 0.170.0 records for the session-flag SessionStart hook
/// (cwd-independent; note the leading `/`).
const CODEX_SESSION_START_KEY: &str = "/<session-flags>/config.toml:session_start:0:0";
const REAL_STOP_WAIT: Duration = Duration::from_secs(120);

// ---------------- binaries ----------------

/// Example of the native Codex binary on macOS, for refusal messages and the runbook.
const CODEX_NATIVE_HINT: &str =
    "…/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex";

/// Resolves a CLI binary: explicit path (required on Windows) or PATH.
pub fn resolve_bin(name: &str, explicit: Option<&PathBuf>) -> Result<PathBuf, String> {
    resolve_in(name, explicit, std::env::var_os("PATH"))
}

/// Identity is checked on the file the symlink chain ends at, followed hop by
/// hop, never on a version-manager shim's link name: that file must be named
/// `name` (minus `.exe`) and must not be a script (shebang, `.js`) or a
/// `.cmd` / `.bat` / `.ps1` wrapper. Returns that exact file.
fn resolve_in(
    name: &str,
    explicit: Option<&PathBuf>,
    path_var: Option<OsString>,
) -> Result<PathBuf, String> {
    let ask = |what: &str| {
        let hint = if name == "codex" {
            format!(" (on macOS: {CODEX_NATIVE_HINT})")
        } else {
            String::new()
        };
        format!("{what}; pass --{name}-bin with the native {name} binary{hint}")
    };
    let found = match explicit {
        Some(p) => p.clone(),
        None if os::os_name() == "windows" => {
            return Err(ask(&format!("--{name}-bin is required on Windows")));
        }
        None => {
            let file = util::bin_name(name);
            path_var
                .iter()
                .flat_map(std::env::split_paths)
                .map(|d| d.join(&file))
                .find(|p| p.is_file())
                .ok_or_else(|| ask(&format!("{name} not found on PATH")))?
        }
    };
    let target = symlink_target(&found).map_err(|e| ask(&format!("{}: {e}", found.display())))?;
    let refuse = |why: &str| {
        ask(&format!(
            "{} resolves to {}, {why}",
            found.display(),
            target.display()
        ))
    };
    let file_name = target
        .file_name()
        .map(|f| f.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let ext = target
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if ["cmd", "bat", "ps1", "js", "mjs", "cjs"].contains(&ext.as_str()) {
        return Err(refuse(&format!(
            "a .{ext} wrapper that cannot be launched directly"
        )));
    }
    if file_name.strip_suffix(".exe").unwrap_or(&file_name) != name {
        return Err(refuse(&format!(
            "which is not the {name} binary (a version-manager shim?)"
        )));
    }
    let mut head = [0u8; 2];
    let n = std::fs::File::open(&target)
        .and_then(|mut f| std::io::Read::read(&mut f, &mut head))
        .map_err(|e| refuse(&e.to_string()))?;
    if head[..n] == *b"#!" {
        return Err(refuse("a script"));
    }
    Ok(target)
}

/// Follows `p`'s symlink chain one hop at a time (bounded) to a regular file.
fn symlink_target(p: &Path) -> Result<PathBuf, String> {
    let mut cur = p.to_path_buf();
    for _ in 0..16 {
        let m = std::fs::symlink_metadata(&cur).map_err(|e| e.to_string())?;
        if m.file_type().is_symlink() {
            let t = std::fs::read_link(&cur).map_err(|e| e.to_string())?;
            let base = cur.parent().map(Path::to_path_buf).unwrap_or_default();
            cur = base.join(t);
        } else if m.is_file() {
            // `cur` is no longer a link; this only normalizes its directories.
            return std::fs::canonicalize(&cur).map_err(|e| e.to_string());
        } else {
            return Err("not a file".into());
        }
    }
    Err("too many symlinks".into())
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

/// Default `--deadline-secs` per real scenario (paste sweeps run longest).
pub fn default_deadline(which: &str, args: &Args) -> u64 {
    match which {
        "claude-paste" | "codex-paste" => 5400,
        "codex-hooks" => 2400,
        "claude-stall" | "codex-stall" if args.attended => 1800,
        "codex-warm" => 900,
        _ => 600,
    }
}

pub fn run(mut args: Args) -> i32 {
    let which = args.positional.get(1).cloned().unwrap_or_default();
    if !args.explicit_deadline {
        args.deadline_secs = default_deadline(&which, &args);
    }
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
        r.config["session_start"] = json!({"claude": claude.as_ref().map(|a| a.session_start()),
                                           "codex": codex.as_ref().map(|a| a.session_start())});
        r.config["codex_first_prompt"] = json!(CODEX_FIRST_PROMPT);
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
    /// Adapter capability: when the CLI fires SessionStart. `eager`: at
    /// startup (Claude). `first_turn`: only with the first user turn
    /// (Codex), so binding and readiness come from that turn.
    pub fn session_start(&self) -> &'static str {
        if self.name == "codex" {
            "first_turn"
        } else {
            "eager"
        }
    }
    /// User turns the launch itself submits (Codex's positional first prompt).
    fn launch_turns(&self) -> usize {
        usize::from(self.name == "codex")
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
    // The first turn is the positional prompt; nothing is typed before binding.
    argv.push(CODEX_FIRST_PROMPT.into());
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

fn submit(ctx: &Ctx, l: &Launch, text: &str, mode: Newline, chunk: usize) -> paste::Outcome {
    paste::paste(
        &l.session.io().tx,
        text.as_bytes(),
        &PasteOpts {
            mode,
            chunk,
            enter_delay: Duration::from_millis(ctx.args.enter_delay_ms),
            write_deadline: ctx.until(Duration::from_secs(10)),
        },
        || l.ready(),
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
    /// `absent`, `unreadable: <kind>`, or the content fingerprint.
    fp: String,
    readable: bool,
    keys: BTreeSet<String>,
}

fn codex_config() -> ConfigState {
    match std::fs::read(codex_home().join("config.toml")) {
        Ok(b) => ConfigState {
            fp: fingerprint(&b),
            readable: true,
            keys: trust_keys(&b),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ConfigState {
            fp: "absent".into(),
            readable: true,
            keys: BTreeSet::new(),
        },
        Err(e) => ConfigState {
            fp: format!("unreadable: {}", e.kind()),
            readable: false,
            keys: BTreeSet::new(),
        },
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

/// Hook fingerprints of Claude's global config files; the flag is false when
/// any file is unreadable or malformed (C1 is then inconclusive).
fn claude_global_hooks() -> (Value, bool) {
    let home = std::env::home_dir().unwrap_or_default();
    let mut out = serde_json::Map::new();
    let mut ok = true;
    for f in [
        home.join(".claude").join("settings.json"),
        home.join(".claude.json"),
    ] {
        let state = match std::fs::read(&f) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!("absent"),
            Err(e) => {
                ok = false;
                json!(format!("unreadable: {}", e.kind()))
            }
            Ok(b) => match serde_json::from_slice::<Value>(&b) {
                Ok(v) => json!(
                    v.get("hooks")
                        .map(|h| fingerprint(h.to_string().as_bytes()))
                        .unwrap_or_else(|| "no hooks key".into())
                ),
                Err(_) => {
                    ok = false;
                    json!("malformed")
                }
            },
        };
        out.insert(f.display().to_string(), state);
    }
    (Value::Object(out), ok)
}

// ---------------- scenarios ----------------
//
// Every scenario declares its required cases up front (`ctx.require`). A
// case is a `PlatformFail` only with positive evidence of what the CLI or OS
// did; an absent hook is `Missing`, and setup, gate, writer, or lookup
// failures are `HarnessFail` or `NotExercised`. Launch verdicts are taken from
// the receiver's final summary after it stopped and drained.

fn not_run(ctx: &Ctx, cases: &[(&str, String)], why: &str) {
    for (id, case) in cases {
        ctx.check(id, case, Outcome::harness(why), json!({"not_run": why}));
    }
}

/// A paste that was not sent: a readiness-gate failure was never exercised;
/// any other gate, writer, or validator outcome is a harness failure.
fn unsent(o: &paste::Outcome) -> Outcome {
    match o.state.as_str() {
        "readiness_gate_failed" => Outcome::not_exercised("readiness gate never opened"),
        s => Outcome::harness(format!("paste not sent: {s}")),
    }
}

/// The bound session id from the final receiver summary; ambiguity voids it.
fn final_bound(fin: &Value) -> Option<String> {
    let r = &fin["receiver"];
    if r["ambiguous"].as_array().is_some_and(|a| !a.is_empty()) {
        return None;
    }
    r["bound_session"].as_str().map(str::to_string)
}

fn final_ambiguous(fin: &Value) -> bool {
    fin["receiver"]["ambiguous"]
        .as_array()
        .is_some_and(|a| !a.is_empty())
}

/// Readiness outcome from the final summary: bound → pass, ambiguous → not
/// exercised, otherwise the hook is missing.
fn bound_outcome(fin: &Value) -> Outcome {
    if final_ambiguous(fin) {
        Outcome::not_exercised("binding became ambiguous")
    } else if final_bound(fin).is_some() {
        Outcome::Pass
    } else {
        Outcome::Missing
    }
}

/// Reads one line from stdin with a deadline (owner prompts).
fn ask_owner(question: &str, deadline: Instant) -> Option<String> {
    println!("{question}");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        if std::io::stdin()
            .lock()
            .read_line(&mut s)
            .is_ok_and(|n| n > 0)
        {
            let _ = tx.send(s.trim().to_string());
        }
    });
    rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()
}

fn claude_hooks(ctx: &Ctx, a: &Agent) {
    ctx.require("A-1", "session_start_bound");
    if ctx.args.prompt {
        ctx.require("A-2", "user_prompt_submit");
    }
    ctx.require("C1", "no_global_hook_edit");
    ctx.require("A-5", "claude_registered_vs_observed");
    let (before, before_ok) = claude_global_hooks();
    let cwd = default_cwd(ctx, a);
    let l = match start(ctx, a, "claude-hooks", &cwd, None, "claude", &[]) {
        Ok(l) => l,
        Err(e) => return not_run(ctx, &[("A-1", "session_start_bound".into())], &e),
    };
    let (bound, ms, snap) = wait_start(ctx, &l);
    let mut a2 = None;
    if ctx.args.prompt && bound.is_some() {
        a2 = Some(if !l.wait_gate(ctx.until(Duration::from_secs(10))) {
            (
                Outcome::not_exercised("readiness gate never opened"),
                json!({}),
            )
        } else {
            let o = submit(ctx, &l, "Reply with the single word OK.", Newline::Cr, 0);
            if o.state != "sent" {
                (unsent(&o), json!({"paste": o}))
            } else {
                let ups = nth_bound(&l, "UserPromptSubmit", 1, ctx.until(REAL_UPS_WAIT));
                let prompt = ups
                    .as_ref()
                    .and_then(|e| e.json())
                    .and_then(|v| v.get("prompt").and_then(Value::as_str).map(str::to_string));
                (
                    if prompt.is_some() {
                        Outcome::Pass
                    } else {
                        Outcome::Missing
                    },
                    json!({"paste": o, "fixture": ups.map(|e| e.fixture)}),
                )
            }
        });
    }
    let obs = observed(&l);
    let fin = finish(ctx, l);
    ctx.check(
        "A-1",
        "session_start_bound",
        bound_outcome(&fin),
        json!({"session_id": final_bound(&fin), "latency_ms": ms, "snapshot": snap,
               "ambiguous": fin["receiver"]["ambiguous"]}),
    );
    if let Some((o, ev)) = a2 {
        let o = if final_ambiguous(&fin) {
            Outcome::not_exercised("binding became ambiguous")
        } else {
            o
        };
        ctx.check("A-2", "user_prompt_submit", o, ev);
    }
    let (after, after_ok) = claude_global_hooks();
    ctx.check(
        "C1",
        "no_global_hook_edit",
        if before_ok && after_ok {
            Outcome::observed(before == after, "global Claude hook config changed")
        } else {
            Outcome::harness("global Claude config unreadable or malformed")
        },
        json!({"before": before, "after": after}),
    );
    ctx.check(
        "A-5",
        "claude_registered_vs_observed",
        Outcome::Pass,
        json!({"registered": CLAUDE_EVENTS, "observed": obs}),
    );
}

/// Write strategies for the real paste sweep: C-1's required subset runs
/// whole and 7-byte chunked writes; otherwise `--chunk`.
fn paste_chunks(ctx: &Ctx) -> Vec<usize> {
    if ctx.args.subset.as_deref() == Some("required") {
        vec![0, 7]
    } else {
        vec![ctx.args.chunk]
    }
}

pub fn paste_case(agent: &str, mode: Newline, chunk: usize, entry: &str) -> String {
    format!("{agent}/{}/c{chunk}/{entry}", mode.name())
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
    let chunks = paste_chunks(ctx);
    for mode in &modes {
        for chunk in &chunks {
            for e in &corpus {
                ctx.require("A-6", &paste_case(a.name, *mode, *chunk, &e.id));
            }
        }
    }
    let cwd = default_cwd(ctx, a);
    for mode in modes {
        for &chunk in &chunks {
            if ctx.past_cutoff() {
                return;
            }
            let cases: Vec<(&str, String)> = corpus
                .iter()
                .map(|e| ("A-6", paste_case(a.name, mode, chunk, &e.id)))
                .collect();
            let label = format!("{}-paste-{}-c{chunk}", a.name, mode.name());
            let l = match start(ctx, a, &label, &cwd, Some("no-daemon"), a.name, &[]) {
                Ok(l) => l,
                Err(e) => {
                    not_run(ctx, &cases, &e);
                    continue;
                }
            };
            let (bound, ms, snap) = wait_start(ctx, &l);
            // Results are held until the receiver drained: a late ambiguous
            // candidate voids them.
            let mut held: Vec<(String, Outcome, Value)> = Vec::new();
            if bound.is_none() {
                for (_, case) in &cases {
                    held.push((
                        case.clone(),
                        Outcome::not_exercised("no bound SessionStart"),
                        json!({"latency_ms": ms, "snapshot": snap}),
                    ));
                }
            }
            // Codex's positional first prompt is turn 1: wait for it to end
            // before the first paste, and count pasted turns after it.
            let mut n = a.launch_turns();
            if n > 0 && bound.is_some() {
                let _ = nth_bound(&l, "Stop", n, ctx.until(REAL_STOP_WAIT));
            }
            for (e, (_, case)) in corpus.iter().zip(&cases).filter(|_| bound.is_some()) {
                if ctx.past_cutoff() {
                    break;
                }
                if !l.wait_gate(ctx.until(Duration::from_secs(15))) {
                    held.push((
                        case.clone(),
                        Outcome::not_exercised("readiness gate never opened"),
                        json!({"readiness_gate_failed": true}),
                    ));
                    continue;
                }
                let o = submit(ctx, &l, &e.text, mode, chunk);
                if o.state != "sent" {
                    held.push((case.clone(), unsent(&o), json!({"paste": o})));
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
                        let fixture = ups.as_ref().map(|e| &e.fixture);
                        // A known CLI input transform is not PTY corruption.
                        match paste::match_prompt(&want, &gotn) {
                            paste::PromptMatch::Exact => {
                                (Outcome::Pass, json!({"equal": true, "fixture": fixture}))
                            }
                            paste::PromptMatch::Transformed(t) => (
                                Outcome::Pass,
                                json!({"equal": false, "cli_transform": t,
                                       "first_diff": util::first_diff(&want, &gotn), "fixture": fixture}),
                            ),
                            paste::PromptMatch::Differs(d) => (
                                Outcome::platform(format!(
                                    "prompt bytes differ at offset {d}, not explained by {:?}",
                                    paste::CLI_TRANSFORMS
                                )),
                                json!({"equal": false, "first_diff": d, "fixture": fixture,
                                       "transforms_checked": paste::CLI_TRANSFORMS}),
                            ),
                        }
                    }
                    // No prompt bytes to compare: hook evidence is missing.
                    None => (Outcome::Missing, json!({"ups": "missing"})),
                };
                held.push((case.clone(), res, ev));
                // Let the turn finish before the next paste.
                let _ = nth_bound(&l, "Stop", n, ctx.until(REAL_STOP_WAIT));
            }
            let fin = finish(ctx, l);
            let void = final_ambiguous(&fin);
            for (case, o, ev) in held {
                let o = if void {
                    Outcome::not_exercised("binding became ambiguous")
                } else {
                    o
                };
                ctx.check("A-6", &case, o, ev);
            }
        }
    }
}

/// A-7 (and C-2's attended launch with `--attended`).
fn stall(ctx: &Ctx, a: &Agent) {
    let a7 = format!("{}_untrusted_folder_snapshot", a.name);
    let c2 = format!("attended/{}", a.name);
    ctx.require("A-7", &a7);
    if ctx.args.attended {
        ctx.require("C-2", &c2);
    }
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
        Err(e) => return not_run(ctx, &[("A-7", a7), ("C-2", c2)], &e),
    };
    let (bound, ms, snap) = wait_start(ctx, &l);
    let mut attended = None;
    if ctx.args.attended {
        attended = Some(if bound.is_some() {
            (
                None,
                json!({"why": "SessionStart arrived without a prompt; nothing to clear"}),
            )
        } else {
            attend(ctx, &l)
        });
    }
    let fin = finish(ctx, l);
    if let Some((first_key, ev)) = attended {
        // Pass only when a relayed key was written while unbound and the
        // SessionStart followed it.
        let bound_at = fin["receiver"]["bound_at_ms"].as_u64();
        let o = match (first_key, bound_at, final_ambiguous(&fin)) {
            (_, _, true) => Outcome::not_exercised("binding became ambiguous"),
            (Some(k), Some(b), false) if b > k => Outcome::Pass,
            (Some(_), _, false) => {
                Outcome::not_exercised("no SessionStart followed the relayed keys")
            }
            (None, _, false) => Outcome::not_exercised("no key was written through the relay"),
        };
        ctx.check(
            "C-2",
            &c2,
            o,
            json!({"attended": ev, "first_key_ms": first_key, "bound_at_ms": bound_at}),
        );
    }
    ctx.check(
        "A-7",
        &a7,
        Outcome::not_exercised("owner judgment required"),
        json!({"timed_out": bound.is_none(), "latency_ms": ms, "snapshot": snap}),
    );
}

/// Shows the snapshot and relays the owner's keys as ungated `write_key` (C-2).
/// Returns the receiver-relative time at which the writer finished the first
/// key written while unbound.
fn attend(ctx: &Ctx, l: &Launch) -> (Option<u64>, Value) {
    let mut used = Vec::new();
    let mut first: Option<u64> = None;
    loop {
        for line in l.snapshot().lines.iter().filter(|s| !s.is_empty()) {
            println!("| {line}");
        }
        let Some(cmd) = ask_owner(
            "keys: down enter esc w-down w-enter w-esc | done",
            ctx.until(Duration::from_secs(300)),
        ) else {
            break;
        };
        if cmd == "done" || ctx.past_cutoff() {
            break;
        }
        if let Some(b) = paste::key_bytes(&cmd) {
            let unbound = l.receiver.bound().is_none();
            let requested = l.receiver.now_ms();
            let r = l
                .session
                .io()
                .tx
                .write_wait(vec![b], ctx.until(Duration::from_secs(5)));
            // Taken once the writer reported the write done, so it is never
            // earlier than the write itself.
            let written = r.is_ok().then(|| l.receiver.now_ms());
            if let (Some(w), true) = (written, unbound) {
                first.get_or_insert(w);
            }
            used.push(
                json!({"key": cmd, "requested_ms": requested, "written_ms": written,
                             "encoding": if cmd.starts_with("w-") { "win32" } else { "vt" }}),
            );
        }
        if l.receiver
            .wait_bound(ctx.until(Duration::from_secs(2)))
            .is_some()
        {
            break;
        }
    }
    (
        first,
        json!({"keys": used, "csi_9001h_seen": l.session.io().saw_win32_input_mode.load(std::sync::atomic::Ordering::SeqCst)}),
    )
}

fn codex_modes(ctx: &Ctx) -> Vec<&'static str> {
    match ctx.args.codex_mode.as_str() {
        "both" => vec!["no-daemon", "default"],
        "default" => vec!["default"],
        _ => vec!["no-daemon"],
    }
}

/// The exact trust key of the hook under test: the session-flag
/// SessionStart hook this harness injects, compared verbatim with its
/// leading `/` (other keys are ignored, so a concurrent change to them does
/// not affect C3).
fn hook_keys(keys: &BTreeSet<String>) -> BTreeSet<String> {
    keys.iter()
        .filter(|k| k.trim_matches('"') == CODEX_SESSION_START_KEY)
        .cloned()
        .collect()
}

/// C3 (second cwd or a fresh, empty cwd): the hook delivered (SessionStart
/// bound, so no review dialog held the first turn), the key under test was
/// already trusted from the first cwd, and the launch added no new key.
fn c3_outcome(
    config_ok: bool,
    bound: Outcome,
    first: &BTreeSet<String>,
    before: &BTreeSet<String>,
    after: &BTreeSet<String>,
) -> Outcome {
    if !config_ok {
        return Outcome::harness("config.toml unreadable");
    }
    if first.is_empty() {
        return Outcome::not_exercised("no session-flag trust key recorded in the first cwd");
    }
    match bound {
        Outcome::Pass => Outcome::observed(
            after.difference(before).count() == 0 && after == first,
            "this cwd added or changed the session-flag trust key",
        ),
        // No SessionStart: a review dialog may be holding the first turn;
        // the timeout snapshot is the evidence.
        o => o,
    }
}

/// A-4: events this launch's receiver bound to the launch. `raw_unknown`
/// and foreign-session events do not show that the mutated hook delivered.
fn bound_events(fin: &Value) -> usize {
    fin["receiver"]["events"]
        .as_array()
        .map(|a| a.iter().filter(|e| e["class"] == "bound").count())
        .unwrap_or(0)
}

fn codex_hooks(ctx: &Ctx, a: &Agent) {
    let cwd = default_cwd(ctx, a);
    let mut plan: Vec<(&str, PathBuf, String, &str, String)> = Vec::new();
    for mode in codex_modes(ctx) {
        for i in 0..ctx.args.launches.max(1) {
            let tag = format!("{mode}-{i}");
            plan.push((
                mode,
                cwd.clone(),
                tag.clone(),
                "A-3",
                format!("{mode}/{tag}"),
            ));
        }
        if let Some(c2) = &ctx.args.cwd2 {
            plan.push((
                mode,
                c2.clone(),
                format!("{mode}-cwd2"),
                "C3",
                format!("{mode}/second_cwd_same_key"),
            ));
        }
        // A brand-new, empty (untrusted) folder: trust must still hold.
        plan.push((
            mode,
            ctx.run_dir.join(format!("fresh-cwd-{mode}")),
            format!("{mode}-fresh-cwd"),
            "C3",
            format!("{mode}/fresh_cwd_no_review"),
        ));
        if ctx.args.mutate_command {
            plan.push((
                mode,
                cwd.clone(),
                format!("{mode}-mutated"),
                "A-4",
                format!("{mode}/mutated_not_delivered"),
            ));
            plan.push((
                mode,
                cwd.clone(),
                format!("{mode}-restored"),
                "A-4",
                format!("{mode}/restored_delivers"),
            ));
        }
    }
    for (_, _, _, id, case) in &plan {
        ctx.require(id, case);
        if *id == "A-3" {
            ctx.require("A-5", case);
        }
    }
    let mut first_keys: std::collections::BTreeMap<&str, BTreeSet<String>> = Default::default();
    for (mode, dir, tag, id, case) in plan {
        if ctx.past_cutoff() {
            break;
        }
        let is_mutated = tag.ends_with("-mutated");
        // A-4 mutates one byte of the session-flag command string (the emitter argument).
        let hook_agent = if is_mutated { "codez" } else { "codex" };
        let before = codex_config();
        let label = format!("codex-hooks-{tag}");
        let l = match start(ctx, a, &label, &dir, Some(mode), hook_agent, &[]) {
            Ok(l) => l,
            Err(e) => {
                not_run(ctx, &[(id, case)], &e);
                continue;
            }
        };
        let (bound, ms, snap) = wait_start(ctx, &l);
        // The positional first prompt is the turn that fires every hook.
        let mut ups_seen = None;
        if bound.is_some() {
            ups_seen =
                Some(nth_bound(&l, "UserPromptSubmit", 1, ctx.until(REAL_UPS_WAIT)).is_some());
            let _ = nth_bound(&l, "Stop", 1, ctx.until(REAL_STOP_WAIT));
        }
        let mut owner = Value::Null;
        if is_mutated && bound.is_none() {
            for line in l.snapshot().lines.iter().filter(|s| !s.is_empty()) {
                println!("| {line}");
            }
            let ans = ask_owner(
                "A-4: does the hooks review entry above name the mutated command (ending in `codez`)? [y/n]",
                ctx.until(Duration::from_secs(300)),
            );
            owner = json!({"prompt": "review entry names the mutated command", "answer": ans});
        }
        let obs = observed(&l);
        // Finalized after the receiver stopped and drained.
        let fin = finish(ctx, l);
        let delivered = bound_events(&fin) > 0;
        let after = codex_config();
        let delta = config_delta(&before, &after);
        let base = json!({"mode": mode, "cwd": dir, "session_id": final_bound(&fin), "latency_ms": ms,
                          "snapshot": snap, "registered": CODEX_EVENTS, "observed": obs, "ups_seen": ups_seen,
                          "config": delta, "ambiguous": fin["receiver"]["ambiguous"],
                          "observed_on": format!("codex {}, source session-flags, mode {mode}", a.version)});
        let config_ok = before.readable && after.readable;
        match id {
            "A-4" if is_mutated => {
                let o = if delivered {
                    Outcome::platform("the mutated hook delivered without review")
                } else {
                    match owner["answer"].as_str().map(|s| s.to_ascii_lowercase()) {
                        Some(s) if s == "y" || s == "yes" => Outcome::Pass,
                        Some(s) if s == "n" || s == "no" => Outcome::platform(
                            "owner: the review entry does not name the mutated command",
                        ),
                        _ => Outcome::not_exercised("no owner confirmation"),
                    }
                };
                ctx.check(
                    id,
                    &case,
                    o,
                    json!({"base": base, "delivered_from_mutated": delivered, "owner": owner}),
                );
            }
            "A-4" => ctx.check(id, &case, bound_outcome(&fin), base),
            "C3" => {
                let first = first_keys.get(mode).cloned().unwrap_or_default();
                let (prior, now) = (hook_keys(&before.keys), hook_keys(&after.keys));
                let o = c3_outcome(config_ok, bound_outcome(&fin), &first, &prior, &now);
                ctx.check(
                    id,
                    &case,
                    o,
                    json!({"base": base, "first_cwd_keys": first, "keys_before": prior,
                           "keys_after": now}),
                );
            }
            _ => {
                let o = bound_outcome(&fin);
                if o == Outcome::Pass && config_ok {
                    let k = hook_keys(&after.keys);
                    if !k.is_empty() {
                        first_keys.entry(mode).or_insert(k);
                    }
                }
                ctx.check(id, &case, o, base.clone());
                ctx.check(
                    "A-5",
                    &case,
                    if config_ok {
                        Outcome::Pass
                    } else {
                        Outcome::harness("config.toml unreadable")
                    },
                    base,
                );
            }
        }
    }
}

fn is_codex(p: &os::ProcInfo) -> bool {
    util::normalize_proc_name(&p.name) == "codex" && !p.zombie
}

/// A-9: our default-mode Codex next to an unrelated, already-running Codex.
fn codex_warm(ctx: &Ctx, a: &Agent) {
    let cases = [
        ("A-9", "hooks_in_our_tree".to_string()),
        ("A-9", "unrelated_survives_our_stop".to_string()),
    ];
    for (id, c) in &cases {
        ctx.require(id, c);
    }
    let cwd = default_cwd(ctx, a);
    // The unrelated Codex: our own PTY, no CHALDEA_* env, default mode.
    let unrelated = Session::spawn(SpawnOpts {
        // Codex starts only with a first turn: the positional prompt.
        argv: vec![a.bin.clone().into(), CODEX_FIRST_PROMPT.into()],
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
        Err(e) => return not_run(ctx, &cases, &e),
    };
    std::thread::sleep(
        Duration::from_secs(10).min(ctx.cutoff.saturating_duration_since(Instant::now())),
    );
    // The warm server must be the warm fixture's own process: a Codex
    // recorded in its tree by verified lineage, or whose ancestry reaches the
    // fixture's CLI.
    unrelated.owned.record_scan();
    let fixture = unrelated.owned.child_pid;
    let mut warm: Vec<os::ProcInfo> = unrelated
        .owned
        .recorded()
        .into_iter()
        .filter(|p| p.pid != fixture && is_codex(p))
        .collect();
    for p in os::list_processes().unwrap_or_default() {
        if p.pid != fixture
            && is_codex(&p)
            && !warm.iter().any(|w| w.pid == p.pid)
            && os::ancestry(p.pid, 16).iter().any(|(q, _)| *q == fixture)
        {
            warm.push(p);
        }
    }
    // Only servers positively alive (pid + start time) right before the
    // measured launch count; a lookup failure is a harness failure.
    let pre_launch: Vec<(u32, Option<bool>)> = warm
        .iter()
        .map(|p| (p.pid, os::liveness(p.pid, p.start_time)))
        .collect();
    if pre_launch.iter().any(|(_, l)| l.is_none()) {
        for (id, c) in &cases {
            ctx.check(
                id,
                c,
                Outcome::harness("warm server liveness lookup failed"),
                json!({"pre_launch": pre_launch}),
            );
        }
        let s = unrelated.stop();
        ctx.detail("unrelated_stop", json!(s));
        return;
    }
    warm.retain(|p| pre_launch.contains(&(p.pid, Some(true))));
    let warm_id: Vec<Value> = warm
        .iter()
        .map(|p| json!({"pid": p.pid, "start_time": p.start_time, "ancestry": os::ancestry(p.pid, 8)}))
        .collect();
    ctx.detail(
        "warm_server",
        json!({"fixture_pid": fixture, "servers": warm_id}),
    );
    if warm.is_empty() {
        for (id, c) in &cases {
            ctx.check(
                id,
                c,
                Outcome::not_exercised("no warm server from the fixture identified before launch"),
                json!({}),
            );
        }
        let s = unrelated.stop();
        ctx.detail("unrelated_stop", json!(s));
        return;
    }
    let l = match start(ctx, a, "codex-warm", &cwd, Some("default"), "codex", &[]) {
        Ok(l) => l,
        Err(e) => {
            unrelated.stop();
            return not_run(ctx, &cases, &e);
        }
    };
    let (_, ms, snap) = wait_start(ctx, &l);
    let ours = l.session.owned.child_pid;
    let warm_pids: Vec<u32> = warm.iter().map(|p| p.pid).collect();
    let from_warm: Vec<u64> = l
        .receiver
        .events()
        .iter()
        .filter(|e| e.ancestry.iter().any(|(p, _)| warm_pids.contains(p)))
        .map(|e| e.seq)
        .collect();
    let fin = finish(ctx, l);
    let bound = final_bound(&fin);
    ctx.check(
        "A-9",
        "hooks_in_our_tree",
        bound_outcome(&fin),
        json!({"bound": bound, "our_cli_pid": ours, "latency_ms": ms, "snapshot": snap, "warm_server": warm_id,
               "events_from_warm_tree": from_warm,
               "interpretation": if bound.is_none() && !from_warm.is_empty() { "adapter requires --no-daemon" }
                                 else if bound.is_none() { "delivery failure (no hook arrived)" } else { "ok" }}),
    );
    // Survival needs positive evidence either way; a failed lookup is a harness failure.
    let mut lives: Vec<(u32, Option<bool>)> =
        vec![(fixture, os::liveness(fixture, unrelated.owned.child_start))];
    lives.extend(
        warm.iter()
            .map(|p| (p.pid, os::liveness(p.pid, p.start_time))),
    );
    let o = if lives.iter().any(|(_, l)| l.is_none()) {
        Outcome::harness("process lookup failed")
    } else {
        Outcome::observed(
            lives.iter().all(|(_, l)| *l == Some(true)),
            "stopping our session ended the unrelated Codex or its warm server",
        )
    };
    ctx.check(
        "A-9",
        "unrelated_survives_our_stop",
        o,
        json!({"liveness": lives, "warm_before": warm_pids}),
    );
    let s = unrelated.stop();
    ctx.detail("unrelated_stop", json!(s));
}

/// A-8: stop after the MCP sleeper attested; nothing owned survives 5 s later.
fn tree_kill(ctx: &Ctx, a: &Agent) {
    let case = format!("{}_mcp_witness_tree", a.name);
    ctx.require("A-8", &case);
    let fake = util::sibling_bin("chs-fake-cli");
    let cwd = default_cwd(ctx, a);
    let extra: Vec<String> = if a.name == "claude" {
        let cfg = ctx.run_dir.join("mcp.json");
        let v = json!({"mcpServers": {"chs": {"command": fake, "args": ["--mode", "sleeper", "--role", "mcp"]}}});
        if let Err(e) = std::fs::write(&cfg, v.to_string()) {
            return not_run(ctx, &[("A-8", case)], &e.to_string());
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
        Err(e) => return not_run(ctx, &[("A-8", case)], &e),
    };
    let (bound, _, snap) = wait_start(ctx, &l);
    let w = l
        .receiver
        .wait_witness(ctx.until(Duration::from_secs(60)), |w| {
            w.role == "mcp-sleeper"
        });
    let witnesses = l.receiver.witnesses();
    let fin = finish(ctx, l);
    std::thread::sleep(
        Duration::from_secs(5).min(ctx.cutoff.saturating_duration_since(Instant::now())),
    );
    let lives: Vec<(u32, Option<bool>)> = witnesses
        .iter()
        .filter(|w| w.session)
        .map(|w| (w.pid, os::liveness(w.pid, w.start_time)))
        .collect();
    let owned_survivors = fin["stop"]["survivors"].clone();
    let owned_clear = owned_survivors.as_array().is_some_and(|a| a.is_empty());
    let o = if w.is_none() {
        Outcome::not_exercised("the MCP sleeper never attested")
    } else if lives.iter().any(|(_, l)| l.is_none()) {
        Outcome::harness("process lookup failed")
    } else {
        Outcome::observed(
            owned_clear && lives.iter().all(|(_, l)| *l == Some(false)),
            "owned processes or session witnesses survived the stop",
        )
    };
    ctx.check(
        "A-8",
        &case,
        o,
        json!({"bound": bound, "snapshot": snap, "witness": w, "witness_liveness": lives,
               "owned_survivors": owned_survivors}),
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
        argv.push(CODEX_FIRST_PROMPT.into());
    }
    let work = args.root.join("work");
    let mut cwds = vec![work.join(name)];
    if name == "codex" {
        cwds.push(work.join("codex2"));
    }
    cwds.push(c2_cwd(&args.root));
    println!(
        "pre-trust: run the command below once in each of these folders and accept folder trust:"
    );
    for c in &cwds {
        println!("  {}", c.display());
    }
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
    println!("then (real CLIs run only with --allow-api-usage):");
    let follow: &[&str] = if name == "claude" {
        &[
            "chs real claude-hooks --prompt --allow-api-usage",
            "chs real tree-kill --agent claude --allow-api-usage",
            "chs real claude-paste --allow-api-usage",
            "chs real claude-stall --allow-api-usage",
        ]
    } else {
        &[
            "chs real codex-hooks --launches 3 --cwd2 <work>/codex2 --codex-mode both --allow-api-usage",
            "chs real codex-hooks --mutate-command --allow-api-usage",
            "chs real codex-warm --allow-api-usage",
            "chs real tree-kill --agent codex --allow-api-usage",
            "chs real codex-paste --allow-api-usage",
            "chs real codex-stall --allow-api-usage",
        ]
    };
    // Every follow-up names the exact binary resolved above.
    let bin_flag = format!("--{name}-bin {}", shell_quote(&bin.display().to_string()));
    for f in follow {
        println!(
            "  {} {bin_flag}",
            f.replace(
                "<work>/codex2",
                &shell_quote(&work.join("codex2").display().to_string())
            )
        );
    }
    let other = if name == "claude" {
        "--codex-bin …"
    } else {
        "--claude-bin …"
    };
    println!(
        "Windows set C: chs win-probe all {bin_flag} {other} --allow-api-usage (C-2 uses {})",
        c2_cwd(&args.root).display()
    );
    0
}

/// C-2's spaced, non-ASCII cwd (`C:\chs\work\日本 dir` on Windows).
pub fn c2_cwd(root: &Path) -> PathBuf {
    root.join("work").join("日本 dir")
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

    /// r3 A-4: only an event bound to the mutated launch counts as delivery.
    #[test]
    fn a4_counts_only_bound_events() {
        let fin = json!({"receiver": {"events": [
            {"class": "raw_unknown", "reason": "foreign_session"},
            {"class": "raw_unknown", "reason": "unparseable"}
        ]}});
        assert_eq!(bound_events(&fin), 0);
        let fin = json!({"receiver": {"events": [{"class": "bound"}]}});
        assert_eq!(bound_events(&fin), 1);
    }

    /// r3 C3: only the SessionStart session-flag key of the hook under test.
    #[test]
    fn c3_compares_the_hook_under_test() {
        // The real 0.170.0 format has a leading `/`; the old form without it
        // must not match.
        let k = trust_keys(
            b"[hooks.state.\"/<session-flags>/config.toml:session_start:0:0\"]\n\
              [hooks.state.\"/<session-flags>/config.toml:stop:0:0\"]\n\
              [hooks.state.\"<session-flags>/config.toml:session_start:0:0\"]\n\
              [hooks.state.\"/x/hooks.json:session_start:0:0\"]\n",
        );
        assert_eq!(
            hook_keys(&k).into_iter().collect::<Vec<_>>(),
            vec!["\"/<session-flags>/config.toml:session_start:0:0\"".to_string()]
        );
    }

    /// C3 fresh cwd: delivered, no review, and no new key.
    #[test]
    fn c3_fresh_cwd_needs_delivery_and_no_new_key() {
        let key: BTreeSet<String> = [format!("\"{CODEX_SESSION_START_KEY}\"")].into();
        let none = BTreeSet::new();
        assert_eq!(
            c3_outcome(true, Outcome::Pass, &key, &key, &key),
            Outcome::Pass
        );
        let extra: BTreeSet<String> = [key.iter().next().unwrap().clone(), "\"x\"".into()].into();
        assert!(matches!(
            c3_outcome(true, Outcome::Pass, &key, &key, &extra),
            Outcome::PlatformFail { .. }
        ));
        assert_eq!(
            c3_outcome(true, Outcome::Missing, &key, &key, &key),
            Outcome::Missing
        );
        assert!(matches!(
            c3_outcome(true, Outcome::Pass, &none, &none, &key),
            Outcome::NotExercised { .. }
        ));
    }

    /// Every Codex launch starts with the positional first prompt, last in argv.
    #[test]
    fn codex_capability_is_first_turn() {
        let a = Agent {
            name: "codex",
            bin: PathBuf::from("/x/codex"),
            version: "0.170.0".into(),
        };
        assert_eq!(a.session_start(), "first_turn");
        assert_eq!(a.launch_turns(), 1);
        let c = Agent {
            name: "claude",
            ..a
        };
        assert_eq!(c.session_start(), "eager");
        assert_eq!(c.launch_turns(), 0);
    }

    #[test]
    fn trust_keys_parsed() {
        let k = trust_keys(b"[x]\n[hooks.state.\"/<session-flags>/config.toml:session_start:0:0\"]\ntrusted_hash = \"sha256:1\"\n");
        assert_eq!(
            k.into_iter().collect::<Vec<_>>(),
            vec!["\"/<session-flags>/config.toml:session_start:0:0\"".to_string()]
        );
    }

    #[test]
    fn wrappers_rejected() {
        let d = std::env::temp_dir().join(format!("chs-bin-test-{}", util::run_id()));
        std::fs::create_dir_all(&d).unwrap();
        let cmd = d.join("codex.cmd");
        std::fs::write(&cmd, b"@echo off").unwrap();
        let e = resolve_bin("codex", Some(&cmd)).unwrap_err();
        assert!(e.contains(".cmd"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The orchestrator's Mac: PATH has a mise shim `codex` that links to the
    /// `mise` binary. Resolution must refuse it rather than launch mise, and
    /// must return the exact native file behind a chain of `codex` links.
    #[cfg(unix)]
    #[test]
    fn symlink_chain_resolves_to_the_named_binary() {
        use std::os::unix::fs::symlink;
        let d = std::env::temp_dir().join(format!("chs-bin-test-{}", util::run_id()));
        let mk = |p: &str, body: &[u8]| {
            let f = d.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(&f, body).unwrap();
            f
        };
        mk("nix/bin/mise", b"\x7fELF");
        let native = mk("vendor/bin/codex", b"\xcf\xfa\xed\xfe");
        mk("npm/codex.js", b"#!/usr/bin/env node\n");
        mk("sh/codex", b"#!/bin/sh\n");
        for dir in ["shims", "brew", "lib", "npmbin"] {
            std::fs::create_dir_all(d.join(dir)).unwrap();
        }
        symlink("../nix/bin/mise", d.join("shims/codex")).unwrap();
        symlink("../vendor/bin/codex", d.join("lib/codex")).unwrap();
        symlink(d.join("lib/codex"), d.join("brew/codex")).unwrap();
        symlink("../npm/codex.js", d.join("npmbin/codex")).unwrap();
        let path =
            |dirs: &[&str]| Some(std::env::join_paths(dirs.iter().map(|x| d.join(x))).unwrap());
        let e = resolve_in("codex", None, path(&["shims", "brew"])).unwrap_err();
        assert!(e.contains("mise") && e.contains("--codex-bin"), "{e}");
        let got = resolve_in("codex", None, path(&["brew", "shims"])).unwrap();
        assert_eq!(got, std::fs::canonicalize(&native).unwrap());
        let e = resolve_in("codex", None, path(&["npmbin"])).unwrap_err();
        assert!(e.contains(".js"), "{e}");
        let e = resolve_in("codex", Some(&d.join("sh/codex")), None).unwrap_err();
        assert!(e.contains("script"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
