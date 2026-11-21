//! Set A (and the real-CLI parts of set C): `chs real ...`, `chs pretrust`,
//! `chs print-launch`.
//! Launches the user's real Claude Code / Codex; never run without
//! `--allow-api-usage`.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::hash::{Hash, Hasher};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{
    Ctx, Launch, LaunchSpec, cli_version, hook_command, hook_command_for, launch, write_snapshot,
};
use crate::cli::{self, Args};
use crate::paste::{self, InputEncoding, Lossy, Newline, NewlineKey};
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
             (they run under the user's account and may use the API)"
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
        r.config["hook_source"] = json!(cli::HOOK_SOURCE);
        r.config["session_start"] = json!({"claude": claude.as_ref().map(|a| a.session_start()),
                                           "codex": codex.as_ref().map(|a| a.session_start())});
        r.config["codex_first_prompt"] = json!(CODEX_FIRST_PROMPT);
        r.config["codex_mode"] = json!(ctx.args.codex_mode);
        r.config["pty_backend"] = json!(ctx.args.pty_backend);
        r.config["conpty_flags"] = json!(ctx.args.conpty_flags);
        r.config["input_encoding"] = json!({
            "claude": ctx.args.encoding_for("claude").name(),
            "codex": ctx.args.encoding_for("codex").name()});
        r.config["newline_key"] = json!(ctx.args.newline_key.name());
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

/// Low reasoning effort for every harness Codex launch, so the user's
/// default (e.g. max) does not stretch the bootstrap turn past minutes
/// (codex-rs 0.171.0 `config/src/config_toml.rs:414`). Hook trust is keyed
/// by source path, event, and group/handler index
/// (`hooks/src/engine/discovery.rs:473,821`); a non-hook `-c` changes none.
pub const CODEX_EFFORT_ARGS: [&str; 2] = ["-c", "model_reasoning_effort=\"low\""];

/// Codex launch arguments after the binary: the reasoning effort, the
/// session-flag hooks for `hook_cmd`, `extra`, then the positional first
/// prompt (the first turn, so nothing is typed before binding).
pub fn codex_launch_args(hook_cmd: &str, no_daemon: bool, extra: &[String]) -> Vec<String> {
    let mut v = Vec::new();
    if no_daemon {
        v.push("--no-daemon".into());
    }
    v.extend(CODEX_EFFORT_ARGS.map(String::from));
    v.extend(codex_hook_args(hook_cmd));
    v.extend(extra.iter().cloned());
    v.push(CODEX_FIRST_PROMPT.into());
    v
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
    argv.extend(
        codex_launch_args(
            &hook_command(&ctx.args.root, hook_agent),
            mode == Some("no-daemon"),
            extra,
        )
        .into_iter()
        .map(OsString::from),
    );
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
    start_with(ctx, a, label, cwd, mode, hook_agent, extra, None)
}

#[allow(clippy::too_many_arguments)]
fn start_with(
    ctx: &Ctx,
    a: &Agent,
    label: &str,
    cwd: &Path,
    mode: Option<&str>,
    hook_agent: &str,
    extra: &[String],
    before_spawn: Option<super::BeforeSpawn>,
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
            responder: true,
            hold_assign: false,
            inject: None,
            before_spawn,
        },
    )
}

/// The full argv of a Codex `win_keys` case launch for the arm in `args`:
/// `--no-daemon`, the session flags, and the paste-burst override,
/// explicit both ways and after the hook flags (which it never changes).
pub fn keys_launch_argv(args: &Args, bin: &Path, hook_cmd: &str) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec![bin.into()];
    argv.extend(
        codex_launch_args(hook_cmd, true, &args.codex_paste_burst.codex_args())
            .into_iter()
            .map(OsString::from),
    );
    argv
}

/// Launches `argv` (built by the caller) for `a` in `cwd`.
fn start_argv(
    ctx: &Ctx,
    a: &Agent,
    label: &str,
    cwd: &Path,
    argv: Vec<OsString>,
) -> Result<Launch, String> {
    std::fs::create_dir_all(cwd).map_err(|e| format!("{}: {e}", cwd.display()))?;
    launch(
        ctx,
        LaunchSpec {
            label: label.into(),
            argv,
            cwd: cwd.to_path_buf(),
            agent: a.name.into(),
            cli_version: a.version.clone(),
            session_id: None,
            cli_names: a.cli_names(),
            responder: true,
            hold_assign: false,
            inject: None,
            before_spawn: None,
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
    wait_start_by(ctx, l, ctx.cutoff)
}

/// `wait_start` ending no later than `end`.
fn wait_start_by(ctx: &Ctx, l: &Launch, end: Instant) -> (Option<String>, u64, Option<PathBuf>) {
    let t = Instant::now();
    let bound = l.receiver.wait_bound(
        ctx.until(Duration::from_secs(ctx.args.session_start_timeout))
            .min(end),
    );
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

/// Polls `cond` every 50 ms until it holds or `end` passes (checked once
/// more at the end).
fn wait_until(end: Instant, cond: impl Fn() -> bool) -> bool {
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn nth_bound(
    l: &Launch,
    event: &str,
    n: usize,
    deadline: Instant,
) -> Option<crate::receiver::EventRecord> {
    l.wait_nth_bound(event, n, deadline, Duration::from_millis(50))
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

/// Every `hooks.state` key of a parsed `config.toml`, however it is
/// written (table or dotted form); hash values are never read.
fn state_keys(t: &toml::Table) -> BTreeSet<String> {
    t.get("hooks")
        .and_then(toml::Value::as_table)
        .and_then(|h| h.get("state"))
        .and_then(toml::Value::as_table)
        .map(|st| st.keys().cloned().collect())
        .unwrap_or_default()
}

/// The session-flag SessionStart trust key (F-3b), found structurally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyResult {
    /// `config.toml` is not valid UTF-8 TOML.
    Unreadable(String),
    /// No persisted entry (absent file, empty `hooks.state`, or no
    /// session-flag SessionStart key).
    None,
    /// A `<session-flags>` key that does not split as
    /// `<source>:<event>:<group>:<index>` or has non-numeric indices.
    Unrecognized(Vec<String>),
    /// More than one matching key.
    Ambiguous(Vec<String>),
    Found {
        key: String,
        raw: Vec<String>,
    },
}

impl KeyResult {
    pub fn tag(&self) -> &'static str {
        match self {
            KeyResult::Unreadable(_) => "unreadable",
            KeyResult::None => "none",
            KeyResult::Unrecognized(_) => "unrecognized",
            KeyResult::Ambiguous(_) => "ambiguous",
            KeyResult::Found { .. } => "found",
        }
    }
    pub fn key(&self) -> Option<&str> {
        match self {
            KeyResult::Found { key, .. } => Some(key),
            _ => None,
        }
    }
}

/// Splits a trust key from the right into `(source, event, group, index)`;
/// the source may itself hold a drive colon (`C:\…`).
fn split_key(k: &str) -> Option<(&str, &str, &str, &str)> {
    let mut it = k.rsplitn(4, ':');
    let (i, g, e, s) = (it.next()?, it.next()?, it.next()?, it.next()?);
    Some((s, e, g, i))
}

fn is_session_flags_source(src: &str) -> bool {
    let comps: Vec<&str> = src.split(['/', '\\']).filter(|c| !c.is_empty()).collect();
    comps.len() >= 2 && comps[comps.len() - 2..] == ["<session-flags>", "config.toml"]
}

fn is_index(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// The trust key of the session-flag SessionStart hook under test: the one
/// `hooks.state` key whose source path ends in `<session-flags>/config.toml`
/// (either separator), whose event is `session_start`, and whose group and
/// handler indices are non-negative integers; any unrecognized
/// `<session-flags>` candidate takes precedence (`Unrecognized`). `raw`
/// lists every key containing `session-flags` exactly as written (keys
/// only).
pub fn session_flag_key(config: &[u8]) -> KeyResult {
    let text = match std::str::from_utf8(config) {
        Ok(t) => t,
        Err(e) => return KeyResult::Unreadable(format!("not UTF-8: {e}")),
    };
    let table: toml::Table = match toml::from_str(text) {
        Ok(t) => t,
        Err(e) => return KeyResult::Unreadable(e.message().to_string()),
    };
    let raw: Vec<String> = state_keys(&table)
        .into_iter()
        .filter(|k| k.contains("session-flags"))
        .collect();
    let mut found = Vec::new();
    let mut unrecognized = Vec::new();
    for k in &raw {
        match split_key(k) {
            Some((src, ev, g, i)) if is_session_flags_source(src) => {
                if !is_index(g) || !is_index(i) {
                    unrecognized.push(k.clone());
                } else if ev == "session_start" {
                    found.push(k.clone());
                }
            }
            Some(_) => {}
            None if k.contains("<session-flags>") => unrecognized.push(k.clone()),
            None => {}
        }
    }
    // An unrecognized candidate means the key under test is not established,
    // whatever else matched.
    if !unrecognized.is_empty() {
        return KeyResult::Unrecognized(raw);
    }
    match found.len() {
        0 => KeyResult::None,
        1 => KeyResult::Found {
            key: found.remove(0),
            raw,
        },
        _ => KeyResult::Ambiguous(raw),
    }
}

impl KeyResult {
    fn raw(&self) -> Vec<String> {
        match self {
            KeyResult::Unrecognized(r)
            | KeyResult::Ambiguous(r)
            | KeyResult::Found { raw: r, .. } => r.clone(),
            _ => Vec::new(),
        }
    }
    fn json(&self) -> Value {
        json!({"result": self.tag(), "key": self.key(), "trust_keys_raw": self.raw(),
               "error": match self { KeyResult::Unreadable(e) => Some(e), _ => None }})
    }
}

#[derive(Clone)]
struct ConfigState {
    /// `absent`, `unreadable: <kind>`, or the content fingerprint.
    fp: String,
    readable: bool,
    /// Every `hooks.state` key (A-5).
    keys: BTreeSet<String>,
    /// The session-flag SessionStart key (F-3b, C3).
    key: KeyResult,
}

fn codex_config() -> ConfigState {
    match std::fs::read(codex_home().join("config.toml")) {
        Ok(b) => {
            let key = session_flag_key(&b);
            let keys = std::str::from_utf8(&b)
                .ok()
                .and_then(|t| toml::from_str::<toml::Table>(t).ok())
                .map(|t| state_keys(&t))
                .unwrap_or_default();
            ConfigState {
                fp: fingerprint(&b),
                // Unreadable TOML is a harness failure like an unreadable file.
                readable: !matches!(key, KeyResult::Unreadable(_)),
                keys,
                key,
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ConfigState {
            fp: "absent".into(),
            readable: true,
            keys: BTreeSet::new(),
            key: KeyResult::None,
        },
        Err(e) => ConfigState {
            fp: format!("unreadable: {}", e.kind()),
            readable: false,
            keys: BTreeSet::new(),
            key: KeyResult::Unreadable(e.kind().to_string()),
        },
    }
}

fn config_delta(before: &ConfigState, after: &ConfigState) -> Value {
    json!({
        "config_toml_before": before.fp,
        "config_toml_after": after.fp,
        "hooks_state_keys_added": after.keys.difference(&before.keys).collect::<Vec<_>>(),
        "hooks_state_keys_removed": before.keys.difference(&after.keys).collect::<Vec<_>>(),
        "session_flag_key_before": before.key.json(),
        "session_flag_key_after": after.key.json(),
        "trust_keys_raw": after.key.raw(),
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

/// Reads one line from stdin with a deadline (prompts to a person at the keyboard).
fn ask_person(question: &str, deadline: Instant) -> Option<String> {
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
            let o = l.submit(ctx, "Reply with the single word OK.", Newline::Cr, 0);
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

/// Case id of a `win_keys` input case (§4.8).
pub fn keys_case(agent: &str, key: NewlineKey, entry: &str) -> String {
    format!("{agent}/keys/{}/{entry}", key.name())
}

fn paste_sweep(ctx: &Ctx, a: &Agent) {
    if let InputEncoding::WinKeys { newline } = ctx.args.encoding_for(a.name) {
        return keys_sweep(ctx, a, newline);
    }
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
                let o = l.submit(ctx, &e.text, mode, chunk);
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

/// Settle interval after a `win_keys` case's `Stop`: the receiver and Codex
/// stay up so a late second submit still fails the case.
const KEYS_SETTLE: Duration = Duration::from_secs(5);
/// Launch plus teardown of one `win_keys` case session (spawn, kill grace,
/// job death wait, verification, thread joins, receiver stop).
const KEYS_OVERHEAD: Duration = Duration::from_secs(20);
/// The bootstrap turn's budget: its `Stop` after the bound SessionStart.
const KEYS_BOOTSTRAP: Duration = REAL_STOP_WAIT;
/// One deadline for the fixture's `UserPromptSubmit` and its turn's `Stop`.
const KEYS_FIXTURE: Duration = Duration::from_secs(120);
/// Bound on writing one case's input (the writer's deadline).
pub const KEYS_WRITE: Duration = Duration::from_secs(10);

/// Worst-case seconds of one `win_keys` case in which every wait succeeds
/// at its bound: SessionStart, the bootstrap turn, the input write, the
/// fixture, the full settle interval, and launch plus teardown.
pub const fn keys_case_worst_secs(session_start_timeout: u64) -> u64 {
    session_start_timeout
        + KEYS_BOOTSTRAP.as_secs()
        + KEYS_WRITE.as_secs()
        + KEYS_FIXTURE.as_secs()
        + KEYS_SETTLE.as_secs()
        + KEYS_OVERHEAD.as_secs()
}

/// Seconds of one failed `win_keys` start (no bound SessionStart, retried
/// once): the SessionStart timeout plus launch and teardown.
pub const fn keys_failed_start_secs(session_start_timeout: u64) -> u64 {
    session_start_timeout + KEYS_OVERHEAD.as_secs()
}

/// The fixture's submit: the first bound event named `event` after the
/// bootstrap fence.
pub fn first_bound_after<'a>(
    events: &'a [KeyEvent],
    event: &str,
    fence: u64,
) -> Option<&'a KeyEvent> {
    events
        .iter()
        .find(|e| e.bound && e.event == event && e.seq > fence)
}

/// A case decided after a settle interval the cutoff cut short is never a
/// pass: a late second submit could have been missed.
pub fn keys_settled(o: Outcome, settled: bool) -> Outcome {
    if o == Outcome::Pass && !settled {
        Outcome::harness("the settle interval was cut short by the run cutoff")
    } else {
        o
    }
}

/// One drained hook event, reduced for the `win_keys` oracle.
#[derive(Debug, Clone)]
pub struct KeyEvent {
    pub seq: u64,
    pub event: String,
    pub bound: bool,
    pub prompt: Option<String>,
    pub received_ms: u64,
    /// The payload's `turn_id` (Codex sends it on submit and Stop).
    pub turn_id: Option<String>,
}

impl KeyEvent {
    fn of(e: &crate::receiver::EventRecord) -> KeyEvent {
        KeyEvent {
            seq: e.seq,
            event: e.event.clone(),
            bound: e.is_bound(),
            prompt: e
                .json()
                .and_then(|v| v["prompt"].as_str().map(str::to_string)),
            received_ms: e.received_ms,
            turn_id: e
                .json()
                .and_then(|v| v["turn_id"].as_str().map(str::to_string)),
        }
    }
}

/// The `win_keys` oracle over one case's drained events (§4.4): bound
/// events after the bootstrap fence (`seq > fence`) only; exactly one
/// `UserPromptSubmit` equal to `expected` byte for byte, followed by a
/// `Stop` of the same `turn_id`, passes. A second submit (also after the
/// `Stop`) or a different prompt is a platform failure, classified
/// `csi_u_not_reassembled` when the prompt holds more literal `[13;2u` than
/// the expected form (a marker the payload itself holds is text); a
/// matching submit without its turn's `Stop`, or no submit at all, is
/// missing.
pub fn win_keys_case(
    events: &[KeyEvent],
    fence: u64,
    expected: &str,
    write_ms: Option<u64>,
) -> (Outcome, Value) {
    let after: Vec<&KeyEvent> = events.iter().filter(|e| e.bound && e.seq > fence).collect();
    let ups: Vec<&&KeyEvent> = after
        .iter()
        .filter(|e| e.event == "UserPromptSubmit")
        .collect();
    let prompts: Vec<Option<&str>> = ups.iter().map(|e| e.prompt.as_deref()).collect();
    let Some(first) = ups.first() else {
        return (
            Outcome::Missing,
            json!({"ups": 0, "why": "no UserPromptSubmit after the bootstrap turn",
                   "ack": "missing", "fidelity": "not_exercised", "completion": "not_exercised"}),
        );
    };
    let got = first.prompt.clone().unwrap_or_default();
    let latency = write_ms.map(|w| first.received_ms.saturating_sub(w));
    let same_turn = |e: &&KeyEvent| e.turn_id.is_some() && e.turn_id == first.turn_id;
    let completion = if after
        .iter()
        .any(|e| e.event == "Stop" && e.seq > first.seq && same_turn(e))
    {
        "ok"
    } else {
        "missing"
    };
    // The oracle split (§4.4): acknowledgment = the bound submit after the
    // fence; fidelity = its prompt against the expected form; completion =
    // the Stop of its turn. Each is recorded whatever the case outcome.
    let mut ev = json!({"ups": ups.len(), "prompts": prompts, "expected": expected,
                        "first_diff": util::first_diff(expected.as_bytes(), got.as_bytes()),
                        "write_to_ups_ms": latency, "ack": "ok",
                        "fidelity": if got == expected { "ok" } else { "differs" },
                        "completion": completion});
    let markers = |s: &str| s.matches("[13;2u").count();
    let want_markers = markers(expected);
    if prompts.iter().flatten().any(|p| markers(p) > want_markers) {
        ev["classification"] = json!("csi_u_not_reassembled");
        return (
            Outcome::platform("a literal [13;2u reached the prompt (csi_u_not_reassembled)"),
            ev,
        );
    }
    if ups.len() > 1 {
        return (
            Outcome::platform("more than one UserPromptSubmit for one input"),
            ev,
        );
    }
    if got != expected {
        return (
            Outcome::platform("the prompt differs from the expected form"),
            ev,
        );
    }
    ev["turn_id"] = json!(first.turn_id);
    if completion != "ok" {
        // `ack_ok, completion_missing`: fidelity stands; the case is
        // incomplete, not failed.
        ev["why"] = json!("matching prompt without a Stop of its turn");
        return (Outcome::Missing, ev);
    }
    (Outcome::Pass, ev)
}

/// One `win_keys` send and its wait: the input is written once (exactly
/// one CR, never resent), then the first bound submit after the fence and
/// the `Stop` of its turn are awaited until `end`; an acknowledged send is
/// followed by the settle interval (cut short by `cutoff` = not settled).
pub struct KeysSend {
    pub sent: paste::Outcome,
    /// `acked` (a bound submit after the fence), `unconfirmed` (sent, or a
    /// CR write whose completion was not reported in time, with no submit
    /// by the deadline; never resent), or the unsent state.
    pub state: String,
    pub write_ms: u64,
    pub completed: bool,
    pub settled: bool,
    /// The input and its CR were written and reported complete (`sent`).
    pub write_completed: bool,
    /// The planned observation window (from the write's return to
    /// `window` or the slot's end, whichever came first) in milliseconds.
    pub window_ms: u64,
    /// The wait ran to the end of the planned window without the run's
    /// hard cutoff cutting it short (set when the evidence did not end it
    /// earlier).
    pub window_elapsed: bool,
    /// The CLI was still running when the observation window ended (only
    /// checked for a case that ended without its submit or its `Stop`).
    pub cli_running_at_end: bool,
}

/// Sends one case and waits for its hooks. The planned observation window
/// ends at `window` after the write returns or at `slot_end`, whichever is
/// first; the run's `hard` cutoff can cut it short (then it did not
/// elapse). `running` reports whether the CLI is still running. The
/// settle may run into the slot's teardown reserve.
#[allow(clippy::too_many_arguments)]
pub fn send_keys_case(
    type_keys: impl FnOnce() -> paste::Outcome,
    now_ms: impl Fn() -> u64,
    events: impl Fn() -> Vec<KeyEvent>,
    running: impl Fn() -> bool,
    fence: u64,
    window: Duration,
    slot_end: Instant,
    hard: Instant,
    settle: Duration,
) -> KeysSend {
    let sent = type_keys();
    let write_ms = now_ms();
    // The fixture's wait starts once the write has returned.
    let t = Instant::now();
    let planned = (t + window).min(slot_end);
    let end = planned.min(hard);
    let elapsed = || planned <= hard && Instant::now() >= planned;
    let mut r = KeysSend {
        state: sent.state.clone(),
        write_completed: sent.state == "sent",
        sent,
        write_ms,
        completed: false,
        settled: false,
        window_ms: planned.saturating_duration_since(t).as_millis() as u64,
        window_elapsed: false,
        cli_running_at_end: false,
    };
    if !submit_may_have_reached(&r.sent) {
        return r;
    }
    wait_until(end, || {
        first_bound_after(&events(), "UserPromptSubmit", fence).is_some()
    });
    let Some(turn) =
        first_bound_after(&events(), "UserPromptSubmit", fence).map(|u| u.turn_id.clone())
    else {
        r.state = "unconfirmed".into();
        r.window_elapsed = elapsed();
        r.cli_running_at_end = running();
        return r;
    };
    r.state = "acked".into();
    r.completed = turn.is_some()
        && wait_until(end, || {
            events()
                .iter()
                .any(|e| e.bound && e.event == "Stop" && e.seq > fence && e.turn_id == turn)
        });
    if !r.completed {
        r.window_elapsed = elapsed();
        r.cli_running_at_end = running();
    }
    let settle_end = hard.min(slot_end + settle);
    let left = settle_end.saturating_duration_since(Instant::now());
    std::thread::sleep(settle.min(left));
    r.settled = left >= settle;
    r
}

/// The shortest live observation window in which "no submit arrived" is
/// an observed result (phase 3b's slowest Windows `w32` submit took about
/// 5.8 s after the write).
pub const NO_SUBMIT_MIN_WINDOW: Duration = Duration::from_secs(10);

/// A case whose write completed, whose planned observation window of at
/// least `NO_SUBMIT_MIN_WINDOW` fully elapsed with no `UserPromptSubmit`,
/// and whose CLI was still running at the window's end is an observed
/// no-submit (the control outcome phase 3b recorded); skipped, cut-short,
/// short-window, early-exit, or uncertain-write cases are not.
pub fn no_submit_observed(result: &str, ev: &Value) -> bool {
    let b = &ev["base"];
    result == "missing"
        && ev["ack"] == json!("missing")
        && b["write_completed"] == json!(true)
        && b["window_elapsed"] == json!(true)
        && b["cli_running_at_end"] == json!(true)
        && b["window_ms"]
            .as_u64()
            .is_some_and(|ms| ms >= NO_SUBMIT_MIN_WINDOW.as_millis() as u64)
}

/// The send's state and observation flags for a case's `base` evidence.
pub fn send_flags(s: Option<&KeysSend>) -> Value {
    json!({"send_state": s.map(|s| s.state.clone()),
           "write_completed": s.is_some_and(|s| s.write_completed),
           "window_ms": s.map(|s| s.window_ms),
           "window_elapsed": s.is_some_and(|s| s.window_elapsed),
           "cli_running_at_end": s.is_some_and(|s| s.cli_running_at_end)})
}

/// The oracle for a case whose input may have reached the CLI (`None`
/// otherwise): ack, fidelity and completion from the drained events, a
/// pass cut by the run cutoff's settle downgraded.
pub fn keys_sent_outcome(
    send: &KeysSend,
    evs: &[KeyEvent],
    fence: u64,
    text: &str,
    allow: &[Lossy],
) -> Option<(Outcome, Value)> {
    if !submit_may_have_reached(&send.sent) {
        return None;
    }
    let expected = paste::expected_win_keys(text, allow);
    let (o, mut ev) = win_keys_case(evs, fence, &expected, Some(send.write_ms));
    if allow.contains(&Lossy::Tab) && text.contains('\t') {
        ev["tab_outcome"] = json!(tab_outcome(evs, fence, &expected));
    }
    Some((keys_settled(o, send.settled), ev))
}

/// The input and its CR were written, or the CR write may have reached
/// the PTY (`unconfirmed`): the bound hooks decide either way.
pub fn submit_may_have_reached(o: &paste::Outcome) -> bool {
    o.state == "sent" || o.state == "unconfirmed"
}

/// F-1d `lossy_tab` under the paste-burst override (informational, never a
/// pass/fail input): `literal_tab` (the expected prompt with its TAB),
/// `submitted_early` (the first submit is the text before the TAB), or
/// `other`.
pub fn tab_outcome(events: &[KeyEvent], fence: u64, expected: &str) -> &'static str {
    let Some(first) = first_bound_after(events, "UserPromptSubmit", fence) else {
        return "other";
    };
    let got = first.prompt.as_deref().unwrap_or("");
    let before_tab = expected.split('\t').next().unwrap_or("").trim();
    if got == expected {
        "literal_tab"
    } else if !before_tab.is_empty() && got.trim() == before_tab {
        "submitted_early"
    } else {
        "other"
    }
}

/// F-1 (and C-1's Codex part on Windows): one fresh Codex session per case.
/// Launch with the positional first prompt, wait for that bootstrap turn's
/// `Stop` (the fence), type the one case, wait for its submit and `Stop`
/// (or the timeout), keep everything up for the settle interval, stop, and
/// decide from the drained receiver. Recorded entries go to
/// `details.f1_recorded`, never into the verdict.
fn keys_sweep(ctx: &Ctx, a: &Agent, newline: NewlineKey) {
    // `f1`: the verdict corpus; `f1-recorded`: the recorded fixtures only
    // (run once with F-1's winner).
    let entries: Vec<paste::FEntry> =
        if let Some(rec @ ("f1" | "f1-recorded")) = ctx.args.subset.as_deref() {
            // With `--f1-entries`, entries are chosen by id from the whole
            // corpus F (recorded ones stay recorded).
            paste::corpus_f()
                .into_iter()
                .filter(|e| !ctx.args.f1_entries.is_empty() || e.recorded == (rec == "f1-recorded"))
                .collect()
        } else {
            paste::corpus()
                .into_iter()
                .filter(|e| !e.fake_only && e.text.len() <= 8 * 1024)
                .filter(|e| ctx.args.subset.as_deref() != Some("required") || e.required)
                .map(|e| paste::FEntry {
                    id: e.id,
                    text: e.text,
                    recorded: false,
                    allow: Vec::new(),
                })
                .collect()
        };
    let entries = select_entries(entries, &ctx.args.f1_entries);
    let _ = std::fs::write(
        ctx.out.join("corpus.json"),
        serde_json::to_vec_pretty(&entries).unwrap_or_default(),
    );
    ctx.sink.with(|r| {
        r.config["input_encoding"] = json!("win_keys");
        r.config["newline_key"] = json!(newline.name());
        r.config["unicode_records"] = json!(ctx.args.unicode_records.name());
        r.config["codex_paste_burst"] = json!(ctx.args.codex_paste_burst.name());
        r.config["codex_paste_burst_args"] = json!(ctx.args.codex_paste_burst.codex_args());
        r.config["f1_entries"] = json!(ctx.args.f1_entries);
    });
    for e in entries.iter().filter(|e| !e.recorded) {
        ctx.require("A-6", &keys_case(a.name, newline, &e.id));
    }
    // Every planned row starts as not exercised; each fixture's row replaces
    // its own and the report is persisted, so a later hard stop keeps the
    // completed rows.
    let record = |e: &paste::FEntry, o: Outcome, ev: Value| {
        let case = keys_case(a.name, newline, &e.id);
        ctx.sink.with(|r| record_keys_row(r, &case, e, o, ev));
        let _ = ctx.sink.write();
    };
    for e in &entries {
        record(e, Outcome::not_exercised(KEYS_NOT_REACHED), json!({}));
    }
    let min_start = Duration::from_secs(ctx.args.session_start_timeout) + KEYS_MIN_INPUT;
    sweep_slots(
        entries.len(),
        Instant::now,
        ctx.cutoff,
        min_start,
        |i, end| {
            let e = &entries[i];
            // Recorded entries run with their own allow set; others with the CLI's.
            let allow: Vec<Lossy> = if e.recorded {
                e.allow.clone()
            } else {
                ctx.args.allow_lossy.clone()
            };
            let (mut o, mut ev) = keys_case_run(ctx, a, newline, e, &allow, "", end);
            ev["slot_end_ms_left"] =
                json!(end.saturating_duration_since(Instant::now()).as_millis() as u64);
            if startup_retry(&o) && end.saturating_duration_since(Instant::now()) >= min_start {
                // Codex failed to start (e.g. `account/read` timed out): once
                // more in a fresh session within the same slot; both attempts
                // are recorded.
                let first = json!({"result": o.tag(), "cause": o.cause(), "evidence": ev});
                (o, ev) = keys_case_run(ctx, a, newline, e, &allow, "-retry", end);
                ev["attempts"] = json!([first, {"result": o.tag(), "cause": o.cause()}]);
            }
            record(e, o, ev);
        },
        |i, why| record(&entries[i], Outcome::not_exercised(why), json!({})),
    );
}

/// A planned keys row the run never reached.
pub const KEYS_NOT_REACHED: &str = "not reached: the run stopped before this fixture";

/// Sets one keys fixture's row (replacing an earlier one): a check for a
/// verdict entry, an `f1_recorded` item for a recorded one.
pub fn record_keys_row(
    r: &mut crate::report::Report,
    case: &str,
    e: &paste::FEntry,
    o: Outcome,
    ev: Value,
) {
    if e.recorded {
        let row = json!({"id": e.id, "result": o.tag(), "cause": o.cause(), "evidence": ev});
        let mut rows = r.details["f1_recorded"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        match rows.iter_mut().find(|x| x["id"] == json!(e.id)) {
            Some(x) => *x = row,
            None => rows.push(row),
        }
        r.detail("f1_recorded", json!(rows));
    } else {
        r.checks.retain(|c| !(c.id == "A-6" && c.case == case));
        r.check("A-6", case, o, ev);
    }
}

/// The shortest input phase worth starting after SessionStart (bootstrap
/// turn, write, and some of the fixture wait).
const KEYS_MIN_INPUT: Duration = Duration::from_secs(20);

/// Per-fixture slots (M2): the time from the start to `cutoff` is split
/// into `n` equal slots with fixed ends; fixture `i` runs from whenever the
/// previous one ended until its own slot end, less the launch-and-teardown
/// reserve, so time an early fixture leaves carries forward and a slow
/// fixture never eats a later one's slot. A fixture with less than
/// `min_start` before its wait end is not started and becomes an explicit
/// `skip` row; every fixture yields exactly one result.
pub fn sweep_slots<T>(
    n: usize,
    now: impl Fn() -> Instant,
    cutoff: Instant,
    min_start: Duration,
    mut run: impl FnMut(usize, Instant) -> T,
    mut skip: impl FnMut(usize, String) -> T,
) -> Vec<T> {
    let start = now();
    let total = cutoff.saturating_duration_since(start);
    let slot = total / n.max(1) as u32;
    (0..n)
        .map(|i| {
            let slot_end = (start + slot * (i as u32 + 1)).min(cutoff);
            let end = slot_end.checked_sub(KEYS_OVERHEAD).unwrap_or(start);
            let left = end.saturating_duration_since(now());
            if left < min_start {
                skip(
                    i,
                    format!(
                        "not started: {} s left in its slot, {} s needed",
                        left.as_secs(),
                        min_start.as_secs()
                    ),
                )
            } else {
                run(i, end)
            }
        })
        .collect()
}

/// `--f1-entries`: only the named entries (all when empty), in corpus order.
pub fn select_entries(entries: Vec<paste::FEntry>, only: &[String]) -> Vec<paste::FEntry> {
    if only.is_empty() {
        return entries;
    }
    entries
        .into_iter()
        .filter(|e| only.contains(&e.id))
        .collect()
}

/// A case that ended because the CLI never started (no bound
/// SessionStart) is retried once in a fresh session.
pub fn startup_retry(o: &Outcome) -> bool {
    matches!(o, Outcome::NotExercised { reason } if reason == NO_BOUND_START)
}

const NO_BOUND_START: &str = "no bound SessionStart";

/// One case in its own session; `attempt` suffixes the launch label.
fn keys_case_run(
    ctx: &Ctx,
    a: &Agent,
    newline: NewlineKey,
    e: &paste::FEntry,
    allow: &[Lossy],
    attempt: &str,
    end: Instant,
) -> (Outcome, Value) {
    // Every wait of this case ends by its slot's end.
    let until = |d: Duration| ctx.until(d).min(end);
    let label = format!("{}-keys-{}-{}{attempt}", a.name, newline.name(), e.id);
    let argv = keys_launch_argv(&ctx.args, &a.bin, &hook_command(&ctx.args.root, a.name));
    let l = match start_argv(ctx, a, &label, &default_cwd(ctx, a), argv) {
        Ok(l) => l,
        Err(err) => return (Outcome::harness(err.clone()), json!({"not_run": err})),
    };
    let (bound, ms, snap) = wait_start_by(ctx, &l, end);
    let mut fence = None;
    let mut send = None;
    if bound.is_some() {
        // The bootstrap turn (positional first prompt) ends before any input.
        fence = nth_bound(&l, "Stop", 1, until(KEYS_BOOTSTRAP)).map(|s| s.seq);
    }
    if let Some(f) = fence {
        // One deadline for the submit and the Stop of its turn.
        send = Some(send_keys_case(
            || {
                l.type_keys(ctx, &e.text, newline, allow, end, || {
                    l.ready() && l.receiver.nth_bound("Stop", 1).is_some_and(|s| s.seq == f)
                })
            },
            || l.receiver.now_ms(),
            || l.receiver.events().iter().map(KeyEvent::of).collect(),
            || l.session.child_running(),
            f,
            KEYS_FIXTURE,
            end,
            ctx.cutoff,
            KEYS_SETTLE,
        ));
    }
    let completed = send.as_ref().is_some_and(|s| s.completed);
    // The screen as it was when the case ended, for a case that did not
    // complete (taken before the session stops).
    let snap = match snap {
        None if !completed => Some(write_snapshot(ctx, &format!("{label}-end"), &l.snapshot())),
        s => s,
    };
    let q = l.query_log();
    let (stop, summary, events) = l.finish_events();
    let fin = json!({"receiver": summary});
    ctx.detail(
        &format!("launch-{label}"),
        json!({"stop": stop, "receiver": fin["receiver"], "query_log": q}),
    );
    let mut base = json!({"latency_ms": ms, "snapshot": snap, "newline_key": newline.name(),
                      "allow_lossy": allow, "paste": send.as_ref().map(|s| &s.sent),
                      "bootstrap_stop_seq": fence,
                      "unicode_records": ctx.args.unicode_records.name(),
                      "codex_paste_burst": ctx.args.codex_paste_burst.name(),
                      "launch_branch": stop.launch.get("branch"),
                      "observed_on": format!("codex {}, source session-flags, mode no-daemon", a.version)});
    if let (Value::Object(b), Value::Object(f)) = (&mut base, send_flags(send.as_ref())) {
        b.extend(f);
    }
    let (o, mut ev) = if final_ambiguous(&fin) {
        (
            Outcome::not_exercised("binding became ambiguous"),
            json!({}),
        )
    } else if bound.is_none() {
        (Outcome::not_exercised(NO_BOUND_START), json!({}))
    } else if fence.is_none() {
        (
            Outcome::not_exercised("the bootstrap turn never ended"),
            json!({}),
        )
    } else {
        let evs: Vec<KeyEvent> = events.iter().map(KeyEvent::of).collect();
        let sent = send.as_ref().map(|s| &s.sent);
        match send
            .as_ref()
            .and_then(|s| keys_sent_outcome(s, &evs, fence.unwrap_or(0), &e.text, allow))
        {
            Some(r) => r,
            None => match sent {
                Some(o)
                    if o.state == "rejected"
                        && o.detail
                            .as_deref()
                            .is_some_and(|d| d.starts_with("lossy_input")) =>
                {
                    (
                        Outcome::not_exercised("lossy_input rejected (F-1d)"),
                        json!({}),
                    )
                }
                Some(o) => (unsent(o), json!({})),
                None => (Outcome::harness("nothing sent"), json!({})),
            },
        }
    };
    ev["base"] = base;
    (o, ev)
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
        Outcome::not_exercised("manual judgment required"),
        json!({"timed_out": bound.is_none(), "latency_ms": ms, "snapshot": snap}),
    );
}

/// Shows the snapshot and relays a person's keys as ungated `write_key` (C-2).
/// Returns the receiver-relative time at which the writer finished the first
/// key written while unbound.
fn attend(ctx: &Ctx, l: &Launch) -> (Option<u64>, Value) {
    let mut used = Vec::new();
    let mut first: Option<u64> = None;
    loop {
        for line in l.snapshot().lines.iter().filter(|s| !s.is_empty()) {
            println!("| {line}");
        }
        let Some(cmd) = ask_person(
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

/// C3 (second cwd or a fresh, empty cwd): the hook delivered (SessionStart
/// bound, so no review dialog held the first turn), and the session-flag key
/// found after the launch is the one trusted from the first cwd (a new or
/// different key falsifies). A key that cannot be found uniquely leaves C3
/// not exercised.
fn c3_outcome(config_ok: bool, bound: Outcome, first: Option<&str>, after: &KeyResult) -> Outcome {
    if !config_ok {
        return Outcome::harness("config.toml unreadable");
    }
    let Some(first) = first else {
        return Outcome::not_exercised("no session-flag trust key recorded in the first cwd");
    };
    match bound {
        Outcome::Pass => match after.key() {
            Some(k) => Outcome::observed(
                k == first,
                "this cwd added or changed the session-flag trust key",
            ),
            None => Outcome::not_exercised(format!("session-flag trust key {}", after.tag())),
        },
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
    let mut first_keys: std::collections::BTreeMap<&str, String> = Default::default();
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
        let mut manual = Value::Null;
        if is_mutated && bound.is_none() {
            for line in l.snapshot().lines.iter().filter(|s| !s.is_empty()) {
                println!("| {line}");
            }
            let ans = ask_person(
                "A-4: does the hooks review entry above name the mutated command (ending in `codez`)? [y/n]",
                ctx.until(Duration::from_secs(300)),
            );
            manual = json!({"prompt": "review entry names the mutated command", "answer": ans});
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
                    match manual["answer"].as_str().map(|s| s.to_ascii_lowercase()) {
                        Some(s) if s == "y" || s == "yes" => Outcome::Pass,
                        Some(s) if s == "n" || s == "no" => Outcome::platform(
                            "manual check: the review entry does not name the mutated command",
                        ),
                        _ => Outcome::not_exercised("no manual confirmation"),
                    }
                };
                ctx.check(
                    id,
                    &case,
                    o,
                    json!({"base": base, "delivered_from_mutated": delivered, "manual": manual}),
                );
            }
            "A-4" => ctx.check(id, &case, bound_outcome(&fin), base),
            "C3" => {
                let first = first_keys.get(mode).cloned();
                let o = c3_outcome(config_ok, bound_outcome(&fin), first.as_deref(), &after.key);
                ctx.check(
                    id,
                    &case,
                    o,
                    json!({"base": base, "first_cwd_key": first, "key_before": before.key.json(),
                           "key_after": after.key.json()}),
                );
            }
            _ => {
                let o = bound_outcome(&fin);
                if o == Outcome::Pass
                    && config_ok
                    && let Some(k) = after.key.key()
                {
                    first_keys.entry(mode).or_insert_with(|| k.to_string());
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
        argv: [a.bin.clone().into_os_string()]
            .into_iter()
            .chain(CODEX_EFFORT_ARGS.map(OsString::from))
            .chain([CODEX_FIRST_PROMPT.into()])
            .collect(),
        cwd: cwd.clone(),
        env: Vec::new(),
        cols: ctx.args.cols,
        rows: ctx.args.rows,
        responder: true,
        out_file: ctx.out.join("pty-output-codex-unrelated.bin"),
        hold_assign: false,
        inject: None,
        // A fixture outside the measured session: today's path.
        backend: crate::os::Backend::Portable,
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
    let _ = unrelated.owned.record_scan();
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

/// Claude's `--mcp-config` entry for the FakeCli sleeper: an explicit stdio
/// server with the launch's hook env (Claude does not pass its own env on).
fn claude_mcp_config(fake: &Path, env: &[(String, String)]) -> Value {
    let pass: serde_json::Map<String, Value> = env
        .iter()
        .filter(|(k, _)| {
            [
                "CHALDEA_HOOK_URL",
                "CHALDEA_HOOK_TOKEN",
                "CHALDEA_THREAD_ID",
            ]
            .contains(&k.as_str())
        })
        .map(|(k, v)| (k.clone(), json!(v)))
        .collect();
    json!({"mcpServers": {"chs": {"type": "stdio", "command": fake,
                                   "args": ["--mode", "sleeper", "--role", "mcp"], "env": pass}}})
}

/// A-8: stop after the MCP sleeper attested; nothing owned survives 5 s later.
fn tree_kill(ctx: &Ctx, a: &Agent) {
    let case = format!("{}_mcp_witness_tree", a.name);
    ctx.require("A-8", &case);
    // §4.8 `backend` kill proof: job trees only (Windows).
    let proof_case = format!("{}_kill_proof", a.name);
    if os::os_name() == "windows" {
        ctx.require("A-8", &proof_case);
    }
    let fake = util::sibling_bin("chs-fake-cli");
    let cwd = default_cwd(ctx, a);
    let cfg = ctx.run_dir.join("mcp.json");
    let mut before_spawn: Option<super::BeforeSpawn> = None;
    let extra: Vec<String> = if a.name == "claude" {
        // Claude does not hand its env to MCP servers: the per-launch values
        // go into the stdio entry itself, in a new 0600 file inside the run
        // dir (removed with it).
        let (path, fake) = (cfg.clone(), fake.clone());
        before_spawn = Some(Box::new(move |env: &[(String, String)]| {
            let v = claude_mcp_config(&fake, env);
            let mut f =
                os::create_private_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            std::io::Write::write_all(&mut f, v.to_string().as_bytes())
                .map_err(|e| format!("{}: {e}", path.display()))
        }));
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
    let l = match start_with(
        ctx,
        a,
        &format!("{}-tree-kill", a.name),
        &cwd,
        Some("no-daemon"),
        a.name,
        &extra,
        before_spawn,
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
    // Real-grandchild containment (e.g. the user's own MCP servers), kept as
    // evidence even when the chs witness is absent.
    let recorded = fin["stop"]["recorded"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let containment = json!({
        "recorded_descendants": recorded.iter().map(|p| json!({"pid": p["pid"], "name": p["name"]})).collect::<Vec<_>>(),
        "survivors": owned_survivors,
        "recorded_count": recorded.len(),
    });
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
               "owned_survivors": owned_survivors, "containment": containment}),
    );
    if os::os_name() == "windows" {
        let proof = &fin["stop"]["kill_proof"];
        let o = if proof.is_null() {
            Outcome::harness("the stop recorded no kill proof (no job tree)")
        } else {
            crate::session::kill_proof_outcome(proof, w.as_ref().map(|w| (w.pid, w.start_time)))
        };
        ctx.check(
            "A-8",
            &proof_case,
            o,
            json!({"kill_proof": proof, "witness": w, "launch": fin["stop"]["launch"],
                   "outside_job_identity":
                       outside_job_correlation(&proof["outside_job"], &fin["receiver"]["events"])}),
        );
    }
}

/// A4 (evidence only): for each outside-job process, three independent
/// fields: its image path, its package identity, and the hook invocations
/// whose emitter ancestry names it (pid and image name). The report never
/// states a cause: none of these shows how the process was created outside
/// the job.
pub fn outside_job_correlation(outside: &Value, events: &Value) -> Value {
    let empty = Vec::new();
    let evs = events.as_array().unwrap_or(&empty);
    let rows: Vec<Value> = outside
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .map(|o| {
            let pid = o["pid"].as_u64();
            let name = o["name"].as_str().unwrap_or("").to_ascii_lowercase();
            let hooks: Vec<Value> = evs
                .iter()
                .filter_map(|e| {
                    let chain = e["ancestry"].as_array()?;
                    let at = chain.iter().position(|a| {
                        a[0].as_u64() == pid
                            && a[1]
                                .as_str()
                                .is_some_and(|n| n.to_ascii_lowercase() == name)
                    })?;
                    Some(
                        json!({"seq": e["seq"], "event": e["event"], "class": e["class"],
                                "ancestry_index": at}),
                    )
                })
                .collect();
            json!({"pid": o["pid"], "name": o["name"],
                   "image": o["identity"]["image"],
                   "package_full_name": o["identity"]["package_full_name"],
                   "hook_ancestry_match": !hooks.is_empty(), "hooks": hooks,
                   "identity": o["identity"]})
        })
        .collect();
    json!(rows)
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

/// How long one pre-trust launch may run before the harness ends it.
const PRETRUST_LAUNCH: Duration = Duration::from_secs(1800);

/// `chs pretrust <claude|codex>` launches the harness's launch argv in each
/// folder the user must trust, one at a time and without a shell, and
/// says what to approve there; `chs print-launch` only prints that argv
/// (diagnostics).
pub fn print_launch(args: &Args) -> i32 {
    let run = args.positional.first().map(String::as_str) == Some("pretrust");
    let which = args.positional.get(1).map(String::as_str).unwrap_or("");
    let (name, explicit) = match which {
        "claude" => ("claude", args.claude_bin.as_ref()),
        "codex" => ("codex", args.codex_bin.as_ref()),
        _ => {
            eprintln!("usage: chs pretrust|print-launch <claude|codex>");
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
    let work = args.root.join("work");
    let mut cwds = vec![work.join(name)];
    if name == "codex" {
        cwds.push(work.join("codex2"));
    }
    cwds.push(c2_cwd(&args.root));
    if run {
        let approve = if name == "claude" {
            "accept folder trust, then quit with /exit".to_string()
        } else {
            format!(
                "accept folder trust; open \"Review hooks\" and trust only if every pending \
                 entry runs `{}` (otherwise quit without trusting and report it); let the \
                 first turn answer, then quit with /exit",
                hook_command_for(&args.root, "codex", "chs-hook-emit")
            )
        };
        for (i, cwd) in cwds.iter().enumerate() {
            println!(
                "pre-trust {}/{}: {name} in {}",
                i + 1,
                cwds.len(),
                cwd.display()
            );
            println!("  {approve}");
            let res = std::fs::create_dir_all(cwd)
                .map_err(|e| format!("{}: {e}", cwd.display()))
                .and_then(|_| pretrust_argv(args, name, &bin))
                .and_then(|argv| run_pretrust(&argv, cwd, PRETRUST_LAUNCH));
            match res {
                Ok(status) => println!("  {name} exited ({status})"),
                Err(e) => {
                    eprintln!("pre-trust stopped: {e}");
                    return 3;
                }
            }
        }
    } else {
        let argv = match pretrust_argv(args, name, &bin) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("{e}");
                return 3;
            }
        };
        println!("pre-trust (`chs pretrust {name}` runs this) in each of these folders:");
        for c in &cwds {
            println!("  {}", c.display());
        }
        println!("env (names only; values are per launch):");
        println!("  CHALDEA_HOOK_URL=http://127.0.0.1:<port>/hook");
        println!("  CHALDEA_HOOK_TOKEN=<64-hex, fresh per launch>");
        println!("  CHALDEA_THREAD_ID=<uuid, fresh per launch>");
        println!("argv (one JSON string per line):");
        for a in &argv {
            println!("  {}", serde_json::to_string(a).unwrap_or_default());
        }
    }
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

/// One pre-trust launch's argv: Claude with the harness settings and a
/// fresh session id; Codex as [`codex_pretrust_argv`].
fn pretrust_argv(args: &Args, name: &str, bin: &Path) -> Result<Vec<String>, String> {
    let bin = bin.display().to_string();
    if name == "codex" {
        return Ok(codex_pretrust_argv(
            &args.root,
            &bin,
            args.codex_mode == "no-daemon",
        ));
    }
    let dir = args.root.join("print-launch");
    os::ensure_private_dir(&args.root)
        .and_then(|_| std::fs::create_dir_all(&dir))
        .map_err(|e| e.to_string())?;
    let f = dir.join("claude-settings.json");
    std::fs::write(
        &f,
        serde_json::to_vec_pretty(&claude_settings(&args.root, "claude")).unwrap_or_default(),
    )
    .map_err(|e| format!("{}: {e}", f.display()))?;
    Ok(vec![
        bin,
        "--settings".into(),
        f.display().to_string(),
        "--session-id".into(),
        uuid::Uuid::new_v4().to_string(),
    ])
}

/// The Codex pre-trust command: the console emitter only. Codex keys hook
/// trust by source, event, and position and keeps one trusted hash per key,
/// so trusting another command string at the same position (the
/// GUI-subsystem emitter) would turn the console command's trust into
/// `Modified` (codex-rs `hooks/src/engine/discovery.rs`).
pub fn codex_pretrust_argv(root: &Path, bin: &str, no_daemon: bool) -> Vec<String> {
    let mut argv = vec![bin.to_string()];
    argv.extend(codex_launch_args(
        &hook_command_for(root, "codex", "chs-hook-emit"),
        no_daemon,
        &[],
    ));
    argv
}

/// The pre-trust process: `argv` spawned directly (no shell, so no quoting
/// layer) in `cwd`, with the launching console inherited.
pub fn pretrust_command(argv: &[String], cwd: &Path) -> std::process::Command {
    let mut c = std::process::Command::new(&argv[0]);
    c.args(&argv[1..]).current_dir(cwd);
    c
}

/// Runs one pre-trust launch until it exits; ends it after `limit`.
fn run_pretrust(argv: &[String], cwd: &Path, limit: Duration) -> Result<String, String> {
    let mut child = pretrust_command(argv, cwd)
        .spawn()
        .map_err(|e| format!("{}: {e}", argv[0]))?;
    let end = Instant::now() + limit;
    loop {
        let why = match child.try_wait() {
            Ok(Some(s)) => return Ok(s.to_string()),
            Ok(None) if Instant::now() < end => {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            Ok(None) => format!("still running after {} min; ended it", limit.as_secs() / 60),
            Err(e) => e.to_string(),
        };
        let _ = child.kill();
        let _ = child.wait();
        return Err(why);
    }
}

/// C-2's spaced, non-ASCII cwd (`C:\chs\work\日本 dir` on Windows).
pub fn c2_cwd(root: &Path) -> PathBuf {
    root.join("work").join("日本 dir")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

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
    fn claude_mcp_entry_is_stdio_with_launch_env() {
        let env = [
            (
                "CHALDEA_HOOK_URL".to_string(),
                "http://127.0.0.1:1/hook".to_string(),
            ),
            ("CHALDEA_HOOK_TOKEN".to_string(), "t".to_string()),
            ("CHALDEA_THREAD_ID".to_string(), "th".to_string()),
            ("CHALDEA_HOOK_LOG".to_string(), "/x".to_string()),
        ];
        let v = claude_mcp_config(Path::new("/b/chs-fake-cli"), &env);
        let e = &v["mcpServers"]["chs"];
        assert_eq!(e["type"], "stdio");
        assert_eq!(e["env"]["CHALDEA_HOOK_TOKEN"], "t");
        assert_eq!(e["env"]["CHALDEA_THREAD_ID"], "th");
        assert!(e["env"].get("CHALDEA_HOOK_LOG").is_none());
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

    fn found(k: &str) -> KeyResult {
        KeyResult::Found {
            key: k.into(),
            raw: vec![k.into()],
        }
    }

    /// F-3b: the session-flag key is found structurally on every OS.
    #[test]
    fn session_flag_key_fixtures() {
        let mac = "/<session-flags>/config.toml:session_start:0:0";
        // Table form, with other keys and the hash beside it.
        let cfg = format!(
            "[x]\ny = 1\n[hooks.state.\"{mac}\"]\ntrusted_hash = \"sha256:1\"\n\
             [hooks.state.\"/<session-flags>/config.toml:stop:0:0\"]\ntrusted_hash = \"sha256:2\"\n\
             [hooks.state.\"/x/hooks.json:session_start:0:0\"]\ntrusted_hash = \"sha256:3\"\n"
        );
        let r = session_flag_key(cfg.as_bytes());
        assert_eq!(r.key(), Some(mac));
        assert!(
            !format!("{r:?}").contains("sha256"),
            "hashes never returned"
        );
        // Windows form: drive colon, TOML-escaped backslashes, dotted keys.
        let win = r"C:\<session-flags>\config.toml:session_start:0:0";
        let cfg = "hooks.state.\"C:\\\\<session-flags>\\\\config.toml:session_start:0:0\".trusted_hash = \"sha256:9\"\n";
        assert_eq!(session_flag_key(cfg.as_bytes()).key(), Some(win));
        // Near-matches are ignored.
        let near = "[hooks.state.\"/<session-flags2>/config.toml:session_start:0:0\"]\n\
                    [hooks.state.\"/<session-flags>/config.toml:session_start_x:0:0\"]\n\
                    [hooks.state.\"/<session-flags>/other.toml:session_start:0:0\"]\n";
        assert_eq!(session_flag_key(near.as_bytes()), KeyResult::None);
        let with_real = format!("{near}[hooks.state.\"{mac}\"]\n");
        assert_eq!(session_flag_key(with_real.as_bytes()).key(), Some(mac));
        // Non-numeric indices: unrecognized.
        let bad = "[hooks.state.\"/<session-flags>/config.toml:session_start:a:0\"]\n";
        assert!(matches!(
            session_flag_key(bad.as_bytes()),
            KeyResult::Unrecognized(_)
        ));
        let bad = "[hooks.state.\"<session-flags>\"]\n";
        assert!(matches!(
            session_flag_key(bad.as_bytes()),
            KeyResult::Unrecognized(_)
        ));
        // Empty hooks.state and no config: no persisted entry.
        assert_eq!(session_flag_key(b"[hooks.state]\n"), KeyResult::None);
        assert_eq!(session_flag_key(b""), KeyResult::None);
        // Two matches: ambiguous.
        let two = format!(
            "[hooks.state.\"{mac}\"]\n[hooks.state.\"{win_esc}\"]\n",
            win_esc = r"C:\\<session-flags>\\config.toml:session_start:0:0"
        );
        assert!(matches!(
            session_flag_key(two.as_bytes()),
            KeyResult::Ambiguous(_)
        ));
        // Invalid TOML: unreadable.
        assert!(matches!(
            session_flag_key(b"[hooks.state"),
            KeyResult::Unreadable(_)
        ));
        // The old line scanner found nothing in the Windows form; this does.
        assert_eq!(session_flag_key(cfg.as_bytes()).tag(), "found");
    }

    /// C3: delivered, no review, and the same key as the first cwd.
    /// L1: an unrecognized `<session-flags>` candidate takes precedence over
    /// a valid key (the key under test is not established), and its raw
    /// key is kept.
    #[test]
    fn unrecognized_candidate_takes_precedence() {
        let cfg = br#"
[hooks.state."/<session-flags>/config.toml:session_start:0:0"]
trusted_hash = "sha256:aa"
[hooks.state."/<session-flags>/config.toml:session_start:x:0"]
trusted_hash = "sha256:bb"
"#;
        match session_flag_key(cfg) {
            KeyResult::Unrecognized(raw) => assert_eq!(raw.len(), 2, "{raw:?}"),
            other => panic!("{other:?}"),
        }
    }

    /// H1: Codex keys hook trust by source and position, not by command, so
    /// pre-trusting a second emitter variant at the same position would mark
    /// the console command `Modified`. Pre-trust is the console emitter only.
    #[test]
    fn pretrust_is_one_console_command() {
        let argv = codex_pretrust_argv(Path::new("/r"), "codex", true);
        assert!(argv.windows(2).any(|w| w == CODEX_EFFORT_ARGS), "{argv:?}");
        let hooks: Vec<&String> = argv.iter().filter(|a| a.starts_with("hooks.")).collect();
        assert_eq!(hooks.len(), 3, "{argv:?}");
        let emitter = hook_command_for(Path::new("/r"), "codex", "chs-hook-emit");
        assert!(
            hooks.iter().all(|h| h.contains(emitter.as_str())),
            "{hooks:?}"
        );
        assert_eq!(
            argv.iter()
                .filter(|a| a.as_str() == CODEX_FIRST_PROMPT)
                .count(),
            1,
            "{argv:?}"
        );
    }

    /// H1: pre-trust spawns the harness's own Codex launch argv directly
    /// (no shell), in the listed folder: a path with spaces and `-c`
    /// values holding double quotes reach Codex unchanged.
    #[test]
    fn pretrust_spawns_the_harness_argv_without_a_shell() {
        let root = Path::new(r"C:\chs root");
        let exe = r"C:\chs\npm\codex 1\codex.exe";
        let cwd = c2_cwd(root);
        let argv = codex_pretrust_argv(root, exe, true);
        let mut harness = vec![exe.to_string()];
        harness.extend(codex_launch_args(
            &hook_command_for(root, "codex", "chs-hook-emit"),
            true,
            &[],
        ));
        assert_eq!(argv, harness);
        assert!(argv.iter().any(|a| a.contains('"')), "{argv:?}");
        let cmd = pretrust_command(&argv, &cwd);
        assert_eq!(cmd.get_program(), exe);
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, argv[1..].iter().map(OsStr::new).collect::<Vec<_>>());
        assert_eq!(cmd.get_current_dir(), Some(cwd.as_path()));
    }

    #[test]
    fn c3_needs_delivery_and_the_same_key() {
        let k = "/<session-flags>/config.toml:session_start:0:0";
        assert_eq!(
            c3_outcome(true, Outcome::Pass, Some(k), &found(k)),
            Outcome::Pass
        );
        // A changed key is detected.
        assert!(matches!(
            c3_outcome(
                true,
                Outcome::Pass,
                Some(k),
                &found("/<session-flags>/config.toml:session_start:0:1")
            ),
            Outcome::PlatformFail { .. }
        ));
        assert_eq!(
            c3_outcome(true, Outcome::Missing, Some(k), &found(k)),
            Outcome::Missing
        );
        assert!(matches!(
            c3_outcome(true, Outcome::Pass, None, &found(k)),
            Outcome::NotExercised { .. }
        ));
        for r in [
            KeyResult::None,
            KeyResult::Unrecognized(vec![]),
            KeyResult::Ambiguous(vec![]),
        ] {
            assert!(matches!(
                c3_outcome(true, Outcome::Pass, Some(k), &r),
                Outcome::NotExercised { .. }
            ));
        }
        assert!(matches!(
            c3_outcome(false, Outcome::Pass, Some(k), &found(k)),
            Outcome::HarnessFail { .. }
        ));
    }

    /// Only a case whose CLI never bound a SessionStart is retried.
    #[test]
    fn only_a_failed_start_is_retried() {
        assert!(startup_retry(&Outcome::not_exercised(
            "no bound SessionStart"
        )));
        assert!(!startup_retry(&Outcome::not_exercised(
            "the bootstrap turn never ended"
        )));
        assert!(!startup_retry(&Outcome::platform("no bound SessionStart")));
        assert!(!startup_retry(&Outcome::Pass));
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
    fn wrappers_rejected() {
        let d = std::env::temp_dir().join(format!("chs-bin-test-{}", util::run_id()));
        std::fs::create_dir_all(&d).unwrap();
        let cmd = d.join("codex.cmd");
        std::fs::write(&cmd, b"@echo off").unwrap();
        let e = resolve_bin("codex", Some(&cmd)).unwrap_err();
        assert!(e.contains(".cmd"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A Mac where PATH has a mise shim `codex` that links to the
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

    fn ev(seq: u64, event: &str, prompt: Option<&str>, ms: u64) -> KeyEvent {
        KeyEvent {
            seq,
            event: event.into(),
            bound: true,
            prompt: prompt.map(str::to_string),
            received_ms: ms,
            turn_id: Some("turn-1".into()),
        }
    }

    /// M4: the fixture's submit is the first bound one after the bootstrap
    /// fence, whatever the bootstrap turn itself delivered.
    #[test]
    fn fixture_submit_is_the_first_after_the_fence() {
        // The bootstrap turn's own submit never arrived bound.
        let evs = vec![
            ev(1, "SessionStart", None, 0),
            ev(2, "Stop", None, 5),
            ev(3, "UserPromptSubmit", Some("x"), 9),
        ];
        assert_eq!(
            first_bound_after(&evs, "UserPromptSubmit", 2).map(|e| e.seq),
            Some(3)
        );
        // An unbound (foreign) submit after the fence does not count.
        let mut foreign = ev(4, "UserPromptSubmit", Some("y"), 10);
        foreign.bound = false;
        let evs = vec![
            ev(1, "UserPromptSubmit", Some("boot"), 0),
            ev(2, "Stop", None, 5),
            foreign,
            ev(5, "UserPromptSubmit", Some("x"), 11),
        ];
        assert_eq!(
            first_bound_after(&evs, "UserPromptSubmit", 2).map(|e| e.seq),
            Some(5)
        );
    }

    /// A1: the paste-burst arms differ only in the explicit
    /// `tui.disable_paste_burst` value, placed after the hook flags; every
    /// hook-related argument (and its position) is identical across arms.
    #[test]
    fn paste_burst_arms_keep_the_hook_argv() {
        use crate::cli::PasteBurst;
        let cmd = "/x/chs-hook-emit codex";
        let off = codex_launch_args(cmd, true, &PasteBurst::Off.codex_args());
        let on = codex_launch_args(cmd, true, &PasteBurst::On.codex_args());
        let hooks = |v: &[String]| -> Vec<(usize, String)> {
            v.iter()
                .enumerate()
                .filter(|(_, a)| a.starts_with("hooks."))
                .map(|(i, a)| (i, a.clone()))
                .collect()
        };
        assert_eq!(hooks(&off).len(), 3);
        assert_eq!(hooks(&off), hooks(&on));
        let diff: Vec<(&String, &String)> = off.iter().zip(&on).filter(|(a, b)| a != b).collect();
        assert_eq!(
            diff,
            [(
                &"tui.disable_paste_burst=true".to_string(),
                &"tui.disable_paste_burst=false".to_string()
            )]
        );
        let at = off.iter().position(|a| a.starts_with("tui.")).unwrap();
        assert!(at > hooks(&off).last().unwrap().0);
        assert_eq!(off.last().map(String::as_str), Some(CODEX_FIRST_PROMPT));
    }

    /// LOW: the argv a keys case actually launches carries the arm's
    /// explicit burst value (built from parsed arguments, the same function
    /// the launch calls).
    #[test]
    fn keys_launch_argv_carries_the_arm() {
        let argv = |extra: &[&str]| -> Vec<String> {
            let mut v = vec!["real", "codex-paste"];
            v.extend(extra);
            let a =
                crate::cli::parse(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
            keys_launch_argv(&a, Path::new("/x/codex"), "/x/chs-hook-emit codex")
                .into_iter()
                .map(|s| s.into_string().unwrap())
                .collect()
        };
        let on = argv(&["--codex-paste-burst", "on"]);
        let off = argv(&[]);
        assert!(
            on.windows(2)
                .any(|w| w == ["-c", "tui.disable_paste_burst=false"])
        );
        assert!(
            off.windows(2)
                .any(|w| w == ["-c", "tui.disable_paste_burst=true"])
        );
        assert_eq!(on[0], "/x/codex");
        assert_eq!(on[1], "--no-daemon");
        assert_eq!(on.last().map(String::as_str), Some(CODEX_FIRST_PROMPT));
        let hooks = |v: &[String]| {
            v.iter()
                .filter(|a| a.starts_with("hooks."))
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(hooks(&on), hooks(&off));
        assert_eq!(hooks(&on).len(), 3);
    }

    /// A rig that records every write the writer thread makes.
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().push(b.to_vec());
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn keys_rig(events: Vec<KeyEvent>, wait: Duration) -> (KeysSend, Vec<Vec<u8>>) {
        let writes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (tx, j) = crate::pty::spawn_writer(Box::new(Capture(writes.clone())));
        let opts = paste::KeysOpts {
            newline: NewlineKey::W32,
            records: paste::UnicodeRecords::Marks,
            allow: Vec::new(),
            enter_delay: Duration::from_millis(1),
            write_deadline: Instant::now() + Duration::from_secs(5),
        };
        let r = send_keys_case(
            || paste::type_keys(&tx, "一\n二\u{200d}".as_bytes(), &opts, || true),
            || 7,
            || events.clone(),
            || true,
            2,
            wait,
            Instant::now() + Duration::from_secs(60),
            Instant::now() + Duration::from_secs(5),
            Duration::from_millis(1),
        );
        tx.send(crate::pty::WriteMsg::Close);
        j.join().unwrap();
        let w = writes.lock().unwrap().clone();
        (r, w)
    }

    /// A7: a send without an acknowledgment by the deadline is
    /// `unconfirmed`, and exactly one CR was written (never resent).
    #[test]
    fn unacked_send_is_unconfirmed_with_one_cr() {
        let (r, w) = keys_rig(vec![ev(2, "Stop", None, 5)], Duration::from_millis(200));
        assert_eq!(r.state, "unconfirmed");
        assert!(!r.completed);
        let crs: usize = w
            .iter()
            .map(|b| b.iter().filter(|c| **c == b'\r').count())
            .sum();
        assert_eq!(crs, 1);
        assert_eq!(w.last().map(Vec::as_slice), Some(&b"\r"[..]));
        // The text, its newline record pair, and the ZWJ record went out
        // before the CR (no CR inside them).
        assert!(w.len() >= 4);
    }

    /// A7: an acknowledgment without the turn's Stop is `acked`, not
    /// completed; the oracle keeps fidelity apart from completion.
    #[test]
    fn ack_without_stop_keeps_fidelity() {
        let evs = vec![
            ev(2, "Stop", None, 5),
            ev(3, "UserPromptSubmit", Some("一\n二\u{200d}"), 9),
        ];
        let (r, w) = keys_rig(evs.clone(), Duration::from_millis(200));
        assert_eq!(r.state, "acked");
        assert!(!r.completed && r.settled);
        assert_eq!(w.iter().filter(|b| b.as_slice() == b"\r").count(), 1);
        let (o, e) = win_keys_case(&evs, 2, "一\n二\u{200d}", Some(r.write_ms));
        assert_eq!(o, Outcome::Missing);
        assert_eq!(
            (&e["ack"], &e["fidelity"], &e["completion"]),
            (&json!("ok"), &json!("ok"), &json!("missing"))
        );
        // With the Stop of its turn the case passes and completion is ok.
        let mut done = evs;
        done.push(ev(4, "Stop", None, 12));
        let (r, _) = keys_rig(done.clone(), Duration::from_millis(200));
        assert!(r.completed && r.settled && r.state == "acked");
        let (o, e) = win_keys_case(&done, 2, "一\n二\u{200d}", None);
        assert_eq!((o, &e["completion"]), (Outcome::Pass, &json!("ok")));
        // No submit at all: ack missing.
        let (_, e) = win_keys_case(&done[..1], 2, "x", None);
        assert_eq!(e["ack"], json!("missing"));
        // A wrong prompt: fidelity differs.
        let (_, e) = win_keys_case(&done, 2, "x", None);
        assert_eq!(e["fidelity"], json!("differs"));
    }

    /// A7: a settle interval the run cutoff cuts short is not settled, so
    /// the case is never a pass.
    #[test]
    fn cutoff_cuts_the_settle() {
        let evs = vec![
            ev(2, "Stop", None, 5),
            ev(3, "UserPromptSubmit", Some("x"), 9),
            ev(4, "Stop", None, 12),
        ];
        let sent = || paste::Outcome {
            state: "sent".into(),
            rejected: None,
            bytes_before: 0,
            bytes_after: 1,
            detail: None,
            lossy: Vec::new(),
            changes: Vec::new(),
        };
        let now = Instant::now();
        let r = send_keys_case(
            sent,
            || 0,
            || evs.clone(),
            || true,
            2,
            Duration::from_millis(50),
            now + Duration::from_secs(60),
            now,
            Duration::from_secs(5),
        );
        assert!(r.completed && !r.settled);
        let (o, _) = win_keys_case(&evs, 2, "x", None);
        assert!(matches!(
            keys_settled(o, r.settled),
            Outcome::HarnessFail { .. }
        ));
    }

    /// A writer that sleeps before completing each write: `cr` for the CR,
    /// `text` for everything else.
    struct Late {
        out: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
        text: Duration,
        cr: Duration,
    }

    impl std::io::Write for Late {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            std::thread::sleep(if b == b"\r" { self.cr } else { self.text });
            self.out.lock().unwrap().push(b.to_vec());
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn late_rig(
        text: Duration,
        cr: Duration,
        deadline: Duration,
        events: impl Fn() -> Vec<KeyEvent>,
        window: Duration,
    ) -> (KeysSend, Vec<Vec<u8>>) {
        let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (tx, j) = crate::pty::spawn_writer(Box::new(Late {
            out: out.clone(),
            text,
            cr,
        }));
        let opts = paste::KeysOpts {
            newline: NewlineKey::W32,
            records: paste::UnicodeRecords::None,
            allow: Vec::new(),
            enter_delay: Duration::from_millis(1),
            write_deadline: Instant::now() + deadline,
        };
        let r = send_keys_case(
            || paste::type_keys(&tx, b"x", &opts, || true),
            || 0,
            events,
            || true,
            2,
            window,
            Instant::now() + Duration::from_secs(60),
            Instant::now() + Duration::from_secs(10),
            Duration::from_millis(1),
        );
        tx.send(crate::pty::WriteMsg::Close);
        j.join().unwrap();
        let w = out.lock().unwrap().clone();
        (r, w)
    }

    /// M3: a CR write that completes after its deadline is `unconfirmed`
    /// (not `enter_not_sent`), the hooks still decide, and it is written
    /// once.
    #[test]
    fn late_cr_completion_is_unconfirmed_and_reconciled() {
        let evs = vec![
            ev(2, "Stop", None, 5),
            ev(3, "UserPromptSubmit", Some("x"), 9),
            ev(4, "Stop", None, 12),
        ];
        let (r, w) = late_rig(
            Duration::ZERO,
            Duration::from_millis(400),
            Duration::from_millis(150),
            || evs.clone(),
            Duration::from_millis(500),
        );
        assert_eq!(r.sent.state, "unconfirmed");
        assert!(submit_may_have_reached(&r.sent));
        assert_eq!(r.state, "acked");
        assert!(r.completed);
        assert_eq!(w.iter().filter(|b| b.as_slice() == b"\r").count(), 1);
        let (o, _) = win_keys_case(&evs, 2, "x", None);
        assert_eq!(o, Outcome::Pass);
        // With no hook at all it stays unconfirmed.
        let (r, _) = late_rig(
            Duration::ZERO,
            Duration::from_millis(400),
            Duration::from_millis(150),
            || vec![ev(2, "Stop", None, 5)],
            Duration::from_millis(100),
        );
        assert_eq!(r.state, "unconfirmed");
    }

    /// M3: the fixture's wait starts after the write returns, so a slow
    /// write does not eat the observation window.
    #[test]
    fn fixture_window_starts_after_the_write() {
        let t0 = Instant::now();
        let late_ack = move || {
            let mut v = vec![ev(2, "Stop", None, 5)];
            if t0.elapsed() >= Duration::from_millis(400) {
                v.push(ev(3, "UserPromptSubmit", Some("x"), 9));
            }
            v
        };
        let (r, _) = late_rig(
            Duration::from_millis(300),
            Duration::ZERO,
            Duration::from_secs(5),
            late_ack,
            Duration::from_millis(300),
        );
        assert_eq!(r.sent.state, "sent");
        assert_eq!(r.state, "acked");
    }

    /// M2: a slow first fixture ends at its own slot; the second still gets
    /// its slot, and one that cannot start in time is an explicit row.
    #[test]
    fn slow_fixture_leaves_the_next_its_slot() {
        use std::cell::Cell;
        let base = Instant::now();
        let t = Cell::new(Duration::ZERO);
        let now = || base + t.get();
        let cutoff = base + Duration::from_secs(220);
        let min_start = Duration::from_secs(40);
        // Each run takes every second it is given, then its teardown.
        let mut ends = Vec::new();
        let rows = sweep_slots(
            2,
            now,
            cutoff,
            min_start,
            |i, end| {
                ends.push(end - base);
                t.set(end - base + KEYS_OVERHEAD);
                format!("ran {i}")
            },
            |i, why| format!("skipped {i}: {why}"),
        );
        assert_eq!(rows, ["ran 0", "ran 1"]);
        assert_eq!(ends, [Duration::from_secs(90), Duration::from_secs(200)]);
        // A first fixture that overruns its slot (e.g. a hung teardown)
        // leaves the second an explicit not-started row, never a dropped one.
        let t = Cell::new(Duration::ZERO);
        let now = || base + t.get();
        let rows = sweep_slots(
            2,
            now,
            cutoff,
            min_start,
            |i, _| {
                t.set(Duration::from_secs(190));
                format!("ran {i}")
            },
            |i, why| format!("skipped {i}: {why}"),
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], "ran 0");
        assert!(rows[1].starts_with("skipped 1: not started"), "{}", rows[1]);
        // An early finisher's leftover carries forward.
        let t = Cell::new(Duration::ZERO);
        let now = || base + t.get();
        let mut ends = Vec::new();
        sweep_slots(
            2,
            now,
            cutoff,
            min_start,
            |_, end| {
                ends.push((t.get(), end - base));
                t.set(t.get() + Duration::from_secs(10));
            },
            |_, _| (),
        );
        assert_eq!(ends[1], (Duration::from_secs(10), Duration::from_secs(200)));
    }

    /// A1: `lossy_tab`'s disposition under the burst override.
    #[test]
    fn tab_outcome_is_classified() {
        let with = |p: &str| {
            vec![
                ev(2, "Stop", None, 5),
                ev(3, "UserPromptSubmit", Some(p), 9),
            ]
        };
        let want = "列1\t列2";
        assert_eq!(tab_outcome(&with(want), 2, want), "literal_tab");
        assert_eq!(tab_outcome(&with("列1"), 2, want), "submitted_early");
        assert_eq!(tab_outcome(&with("列1    列2"), 2, want), "other");
        assert_eq!(tab_outcome(&with(want)[..1], 2, want), "other");
    }

    /// A4: an outside-job shell is correlated with the hook whose emitter
    /// ancestry names it (pid and name, case-insensitive); package identity,
    /// the hook match, and the image are independent fields, never a cause.
    #[test]
    fn outside_job_is_correlated_with_its_hook() {
        let outside = json!([
            {"pid": 19224, "name": "pwsh.exe",
             "identity": {"image": "C:\\Program Files\\PowerShell\\7\\pwsh.exe",
                          "package_full_name": "none"}},
            {"pid": 77, "name": "pwsh.exe",
             "identity": {"package_full_name": "Microsoft.PowerShell_7.5.4.0_x64__8wekyb3d8bbwe"}},
        ]);
        let events = json!([
            {"seq": 1, "event": "SessionStart", "class": "bound",
             "ancestry": [[63820, "chs-hook-emit.exe"], [19224, "PWSH.EXE"], [89300, "codex.exe"]]},
            {"seq": 2, "event": "Stop", "class": "bound",
             "ancestry": [[1, "chs-hook-emit.exe"], [19224, "cmd.exe"]]},
        ]);
        let r = outside_job_correlation(&outside, &events);
        assert_eq!(
            r[0]["hooks"],
            json!([{"seq": 1, "event": "SessionStart", "class": "bound", "ancestry_index": 1}])
        );
        assert_eq!(r[0]["hook_ancestry_match"], json!(true));
        assert_eq!(r[0]["package_full_name"], json!("none"));
        assert_eq!(
            r[0]["image"],
            json!("C:\\Program Files\\PowerShell\\7\\pwsh.exe")
        );
        // A packaged process with no hook naming it: the package is
        // reported, the match is false, and no cause is ever stated.
        assert_eq!(r[1]["hooks"], json!([]));
        assert_eq!(r[1]["hook_ancestry_match"], json!(false));
        assert!(
            r[1]["package_full_name"]
                .as_str()
                .unwrap()
                .starts_with("Microsoft.")
        );
        for row in r.as_array().unwrap() {
            assert!(row.get("cause").is_none());
        }
        assert_eq!(outside_job_correlation(&Value::Null, &events), json!([]));
    }

    /// M2: the Enter delay never runs past the fixture's write deadline; a
    /// CR due after it is never written.
    #[test]
    fn enter_delay_stops_at_the_deadline() {
        let writes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (tx, j) = crate::pty::spawn_writer(Box::new(Capture(writes.clone())));
        let opts = paste::KeysOpts {
            newline: NewlineKey::W32,
            records: paste::UnicodeRecords::None,
            allow: Vec::new(),
            enter_delay: Duration::from_secs(30),
            write_deadline: Instant::now() + Duration::from_millis(200),
        };
        let t = Instant::now();
        let o = paste::type_keys(&tx, b"x", &opts, || true);
        assert!(t.elapsed() < Duration::from_secs(5));
        assert_eq!(o.state, "enter_not_sent");
        tx.send(crate::pty::WriteMsg::Close);
        j.join().unwrap();
        assert!(!writes.lock().unwrap().iter().any(|w| w == b"\r"));
    }

    /// M2: every planned row starts as not exercised and a fixture's row
    /// replaces only its own, so a later hard stop keeps completed rows.
    #[test]
    fn keys_rows_are_replaced_in_place() {
        let corpus = paste::corpus_f();
        let pick = |id: &str| corpus.iter().find(|e| e.id == id).unwrap();
        let (plain, rec) = (pick("long_multiline"), pick("small_kana_mark"));
        assert!(!plain.recorded && rec.recorded);
        let case = |e: &paste::FEntry| keys_case("codex", NewlineKey::W32, &e.id);
        let mut r = crate::report::Report::new("x", "00000000");
        for e in [plain, rec] {
            record_keys_row(
                &mut r,
                &case(e),
                e,
                Outcome::not_exercised(KEYS_NOT_REACHED),
                json!({}),
            );
        }
        record_keys_row(&mut r, &case(plain), plain, Outcome::Pass, json!({"k": 1}));
        assert_eq!(r.checks.len(), 1);
        assert_eq!(r.checks[0].result, "pass");
        let rows = r.details["f1_recorded"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["result"], "not_exercised");
        assert_eq!(rows[0]["cause"], KEYS_NOT_REACHED);
        record_keys_row(&mut r, &case(rec), rec, Outcome::Missing, json!({}));
        assert_eq!(r.details["f1_recorded"].as_array().unwrap().len(), 1);
        assert_eq!(r.details["f1_recorded"][0]["result"], "missing");
    }

    /// `--f1-entries` keeps corpus order and recorded entries.
    #[test]
    fn f1_entries_select_by_id() {
        let ids = |v: Vec<paste::FEntry>| v.into_iter().map(|e| e.id).collect::<Vec<_>>();
        let only = ["small_kana_mark".to_string(), "emoji_zwj".to_string()];
        assert_eq!(
            ids(select_entries(paste::corpus_f(), &only)),
            ["emoji_zwj", "small_kana_mark"]
        );
        assert_eq!(
            select_entries(paste::corpus_f(), &[]).len(),
            paste::corpus_f().len()
        );
    }

    /// M2: the fixture's Stop must belong to its UserPromptSubmit's turn.
    #[test]
    fn win_keys_stop_matches_the_turn() {
        let turn = |mut e: KeyEvent, t: Option<&str>| {
            e.turn_id = t.map(String::from);
            e
        };
        let ups = |t| turn(ev(4, "UserPromptSubmit", Some("x"), 9), t);
        let stop = |t| turn(ev(5, "Stop", None, 9), t);
        let o = win_keys_case(&[ups(Some("A")), stop(Some("B"))], 3, "x", None).0;
        assert_eq!(o, Outcome::Missing);
        let o = win_keys_case(&[ups(None), stop(None)], 3, "x", None).0;
        assert_eq!(o, Outcome::Missing);
        let o = win_keys_case(&[ups(Some("A")), stop(Some("A"))], 3, "x", None).0;
        assert_eq!(o, Outcome::Pass);
    }

    /// L3: a literal `[13;2u` the payload itself holds is not a reassembly
    /// failure; one that adds a marker (at an encoded newline) still is.
    #[test]
    fn literal_marker_is_diagnostic_only_on_a_mismatch() {
        let case = |want: &str, got: &str| {
            win_keys_case(
                &[
                    ev(4, "UserPromptSubmit", Some(got), 9),
                    ev(5, "Stop", None, 9),
                ],
                3,
                want,
                None,
            )
        };
        assert_eq!(case("a[13;2u", "a[13;2u").0, Outcome::Pass);
        let (o, e) = case("a[13;2u\nb", "a[13;2u[13;2ub");
        assert!(matches!(o, Outcome::PlatformFail { .. }));
        assert_eq!(e["classification"], "csi_u_not_reassembled");
    }

    /// M1: a case decided after a settle interval the cutoff cut short is
    /// never a pass; other outcomes keep their evidence.
    #[test]
    fn cut_settle_is_not_a_pass() {
        assert!(matches!(
            keys_settled(Outcome::Pass, false),
            Outcome::HarnessFail { .. }
        ));
        assert_eq!(keys_settled(Outcome::Pass, true), Outcome::Pass);
        let p = Outcome::platform("x");
        assert_eq!(keys_settled(p.clone(), false), p);
    }

    /// F-1a oracle over synthetic drained events (fence = bootstrap Stop seq 3).
    #[test]
    fn win_keys_case_oracle() {
        let want = "一行目\n二行目";
        let boot = vec![
            ev(1, "SessionStart", None, 0),
            ev(
                2,
                "UserPromptSubmit",
                Some("Reply with the single word OK."),
                1,
            ),
            ev(3, "Stop", None, 2),
        ];
        let with = |more: Vec<KeyEvent>| [boot.clone(), more].concat();
        // One matching prompt + Stop: pass, with its latency.
        let (o, e) = win_keys_case(
            &with(vec![
                ev(4, "UserPromptSubmit", Some(want), 900),
                ev(5, "Stop", None, 950),
            ]),
            3,
            want,
            Some(400),
        );
        assert_eq!(o, Outcome::Pass);
        assert_eq!(e["write_to_ups_ms"], 500);
        // Failed reassembly: a literal [13;2u (Esc replayed).
        let (o, e) = win_keys_case(
            &with(vec![
                ev(4, "UserPromptSubmit", Some("一行目[13;2u二行目"), 9),
                ev(5, "Stop", None, 9),
            ]),
            3,
            want,
            None,
        );
        assert!(matches!(o, Outcome::PlatformFail { .. }));
        assert_eq!(e["classification"], "csi_u_not_reassembled");
        // Two submits (split prompt).
        let (o, _) = win_keys_case(
            &with(vec![
                ev(4, "UserPromptSubmit", Some("一行目"), 9),
                ev(5, "UserPromptSubmit", Some("二行目"), 9),
                ev(6, "Stop", None, 9),
            ]),
            3,
            want,
            None,
        );
        assert!(matches!(o, Outcome::PlatformFail { .. }));
        // A second submit after the fixture's Stop, within the settle interval.
        let (o, _) = win_keys_case(
            &with(vec![
                ev(4, "UserPromptSubmit", Some(want), 9),
                ev(5, "Stop", None, 9),
                ev(6, "UserPromptSubmit", Some("x"), 9),
            ]),
            3,
            want,
            None,
        );
        assert!(matches!(o, Outcome::PlatformFail { .. }));
        // A different prompt.
        let (o, _) = win_keys_case(
            &with(vec![
                ev(4, "UserPromptSubmit", Some("一行目二行目"), 9),
                ev(5, "Stop", None, 9),
            ]),
            3,
            want,
            None,
        );
        assert!(matches!(o, Outcome::PlatformFail { .. }));
        // A matching prompt without Stop: missing.
        let (o, _) = win_keys_case(
            &with(vec![ev(4, "UserPromptSubmit", Some(want), 9)]),
            3,
            want,
            None,
        );
        assert_eq!(o, Outcome::Missing);
        // Nothing after the fence (the bootstrap submit is ignored): missing.
        let (o, _) = win_keys_case(&boot, 3, "Reply with the single word OK.", None);
        assert_eq!(o, Outcome::Missing);
        // A delayed delivery still before the timeout passes with its latency.
        let (o, e) = win_keys_case(
            &with(vec![
                ev(4, "UserPromptSubmit", Some(want), 20_400),
                ev(5, "Stop", None, 25_000),
            ]),
            3,
            want,
            Some(400),
        );
        assert_eq!(o, Outcome::Pass);
        assert_eq!(e["write_to_ups_ms"], 20_000);
        // Foreign (unbound) events never count.
        let mut foreign = ev(4, "UserPromptSubmit", Some("x"), 9);
        foreign.bound = false;
        let (o, _) = win_keys_case(
            &with(vec![
                foreign,
                ev(5, "UserPromptSubmit", Some(want), 9),
                ev(6, "Stop", None, 9),
            ]),
            3,
            want,
            None,
        );
        assert_eq!(o, Outcome::Pass);
    }
}
