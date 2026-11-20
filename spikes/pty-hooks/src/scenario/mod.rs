//! Scenario plumbing shared by `fake`, `real`, and `win-probe`.

pub mod fake;
pub mod real;
pub mod win_probe;

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::cli::Args;
use crate::os;
use crate::receiver::{BindRule, Config, Receiver};
use crate::report::{Report, Sink};
use crate::session::{self, Inject, Session, SpawnOpts};
use crate::util;
use crate::verdict::Outcome;
use crate::vt::Snapshot;

pub struct Ctx {
    pub args: Args,
    pub run_id: String,
    pub out: PathBuf,
    pub run_dir: PathBuf,
    pub sink: Sink,
    pub t0: Instant,
    pub cutoff: Instant,
    pub deadline: Instant,
}

impl Ctx {
    /// Creates `R/out/<run-id>` (or `--out`) and `R/runs/<run-id>`, starts the watchdog.
    pub fn new(args: Args, scenario: &str) -> Result<Ctx, String> {
        let t0 = Instant::now();
        let run_id = args.run_id.clone().unwrap_or_else(util::run_id);
        os::ensure_private_dir(&args.root).map_err(|e| format!("{}: {e}", args.root.display()))?;
        let out = args
            .out
            .clone()
            .unwrap_or_else(|| args.root.join("out").join(&run_id));
        std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
        let run_dir = args.root.join("runs").join(&run_id);
        std::fs::create_dir_all(&run_dir).map_err(|e| format!("{}: {e}", run_dir.display()))?;
        let mut report = Report::new(scenario, &run_id);
        // Every report records the console code page; the ConPTY module is
        // sampled after the first PTY launch (`conpty_module_after_launch`).
        report.host["console_cp"] = json!(os::console_code_page());
        report.config = json!({
            "deadline_secs": args.deadline_secs,
            "session_start_timeout": args.session_start_timeout,
            "cols": args.cols, "rows": args.rows,
            "enter_delay_ms": args.enter_delay_ms,
            "chunk": args.chunk,
            "paste_newline": args.paste_newline.map(|n| n.name()),
        });
        let sink = Sink::new(&out, report);
        let deadline = t0 + Duration::from_secs(args.deadline_secs);
        let cutoff = deadline - Duration::from_secs(10);
        session::watchdog_start(sink.clone(), cutoff, deadline);
        if !args.keep_temp {
            let (root, dir) = (args.root.clone(), run_dir.clone());
            session::watchdog_run_dir_cleanup(Box::new(move || {
                remove_run_dir(&root, &dir);
                "removed".into()
            }));
        }
        Ok(Ctx {
            args,
            run_id,
            out,
            run_dir,
            sink,
            t0,
            cutoff,
            deadline,
        })
    }

    /// `now + d`, capped at the scenario cutoff.
    pub fn until(&self, d: Duration) -> Instant {
        (Instant::now() + d).min(self.cutoff)
    }

    pub fn past_cutoff(&self) -> bool {
        Instant::now() >= self.cutoff
    }

    pub fn check(&self, id: &str, case: &str, outcome: Outcome, evidence: Value) {
        self.sink.with(|r| r.check(id, case, outcome, evidence));
    }

    /// Declares a case this run must produce; a missing one makes the run inconclusive.
    pub fn require(&self, id: &str, case: &str) {
        self.sink.with(|r| r.require(id, case));
    }

    pub fn detail(&self, key: &str, v: Value) {
        self.sink.with(|r| r.detail(key, v));
    }

    pub fn error(&self, e: impl Into<String>) {
        self.sink.with(|r| r.errors.push(e.into()));
    }

    /// Writes the final report, removes the run dir, returns the exit code.
    pub fn finish(self) -> i32 {
        session::watchdog_finish();
        let status = self.sink.with(|r| {
            if r.status == "running" || r.status.is_empty() {
                r.status = r.derive_status();
            }
            r.detail("elapsed_ms", json!(self.t0.elapsed().as_millis() as u64));
            r.status.clone()
        });
        if !self.args.keep_temp {
            remove_run_dir(&self.args.root, &self.run_dir);
        }
        if let Err(e) = self.sink.write() {
            eprintln!("chs: writing report failed: {e}");
        }
        println!(
            "{} {} -> {}",
            self.sink.with(|r| r.scenario.clone()),
            status,
            self.sink.path.display()
        );
        exit_code(&status)
    }
}

pub fn exit_code(status: &str) -> i32 {
    match status {
        "pass" => 0,
        "fail" => 1,
        "inconclusive" => 2,
        "timeout" => 124,
        _ => 3,
    }
}

/// Removes `R/runs/<id>` only if it is a real directory under `root/runs` (no symlinks).
fn remove_run_dir(root: &Path, dir: &Path) {
    let runs = root.join("runs");
    if dir.parent() != Some(runs.as_path()) {
        return;
    }
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => {
            let _ = std::fs::remove_dir_all(dir);
        }
        _ => {}
    }
}

/// Emitter binary name; `CHS_EMITTER_VARIANT=gui` selects the GUI-subsystem
/// emitter for C-5's re-run (both the hook command and direct invocations).
pub fn emitter_name() -> &'static str {
    match std::env::var("CHS_EMITTER_VARIANT").as_deref() {
        Ok("gui") => "chs-hook-emit-gui",
        _ => "chs-hook-emit",
    }
}

pub fn emitter_path(root: &Path) -> PathBuf {
    let name = emitter_name();
    let installed = root.join("bin").join(util::bin_name(name));
    if installed.is_file() {
        installed
    } else {
        util::sibling_bin(name)
    }
}

/// Fixed hook command string: `<abs emitter> <agent>` (forward slashes, no spaces).
pub fn hook_command(root: &Path, agent: &str) -> String {
    let p = root.join("bin").join(util::bin_name(emitter_name()));
    format!("{} {agent}", p.display().to_string().replace('\\', "/"))
}

pub fn install_emitter(args: &Args) -> i32 {
    let root = &args.root;
    let bin = root.join("bin");
    let res = (|| -> Result<Vec<PathBuf>, String> {
        os::ensure_private_dir(root).map_err(|e| e.to_string())?;
        os::check_secure_path(root)?;
        std::fs::create_dir_all(&bin).map_err(|e| e.to_string())?;
        os::check_secure_path(&bin)?;
        let mut installed = Vec::new();
        for name in ["chs-hook-emit", "chs-hook-emit-gui"] {
            let src = util::sibling_bin(name);
            if !src.is_file() {
                if name == "chs-hook-emit" {
                    return Err(format!("{} not built", src.display()));
                }
                continue;
            }
            let dst = bin.join(util::bin_name(name));
            if let Ok(m) = std::fs::symlink_metadata(&dst)
                && m.file_type().is_symlink()
            {
                return Err(format!("{} is a symlink; refusing", dst.display()));
            }
            let tmp = bin.join(format!(".{}.tmp-{}", name, std::process::id()));
            std::fs::copy(&src, &tmp).map_err(|e| format!("copy: {e}"))?;
            os::make_executable(&tmp).map_err(|e| e.to_string())?;
            std::fs::rename(&tmp, &dst).map_err(|e| format!("rename: {e}"))?;
            os::check_secure_path(&dst)?;
            installed.push(dst);
        }
        Ok(installed)
    })();
    match res {
        Ok(paths) => {
            for p in paths {
                println!("installed {}", p.display());
            }
            println!("hook command (claude): {}", hook_command(root, "claude"));
            println!("hook command (codex):  {}", hook_command(root, "codex"));
            0
        }
        Err(e) => {
            eprintln!("chs install-emitter: {e}");
            3
        }
    }
}

pub struct LaunchSpec {
    pub label: String,
    pub argv: Vec<OsString>,
    pub cwd: PathBuf,
    pub agent: String,
    pub cli_version: String,
    pub session_id: Option<String>,
    pub cli_names: Vec<String>,
    pub extra_env: Vec<(String, String)>,
    pub responder: bool,
    pub hold_assign: bool,
    pub inject: Option<Inject>,
    /// Runs with the launch's env (URL, token, thread id) after the receiver
    /// started and before the spawn, e.g. to write a per-launch config.
    pub before_spawn: Option<BeforeSpawn>,
}

pub type BeforeSpawn = Box<dyn FnOnce(&[(String, String)]) -> Result<(), String>>;

pub struct Launch {
    pub label: String,
    pub receiver: Receiver,
    pub session: Session,
    pub thread_id: String,
    token: String,
}

impl Launch {
    /// Env for helper processes the harness starts outside the session (e.g. `unrelated`).
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            ("CHALDEA_HOOK_URL".into(), self.receiver.url()),
            ("CHALDEA_HOOK_TOKEN".into(), self.token.clone()),
            ("CHALDEA_THREAD_ID".into(), self.thread_id.clone()),
        ]
    }

    /// Readiness: a bound, unambiguous SessionStart (`bound()` is None once
    /// anything is ambiguous). The DECSET 2004 half lives in the writer gate.
    pub fn ready(&self) -> bool {
        self.receiver.bound().is_some()
    }

    pub fn gate_open(&self) -> bool {
        self.ready() && self.session.io().tx.gate.admit().is_some()
    }

    /// Waits until input may be sent (bound SessionStart + DECSET 2004).
    pub fn wait_gate(&self, deadline: Instant) -> bool {
        loop {
            if self.gate_open() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.session
            .io()
            .vt
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot()
    }

    pub fn query_log(&self) -> Value {
        json!(
            *self
                .session
                .io()
                .log
                .lock()
                .unwrap_or_else(|e| e.into_inner())
        )
    }

    /// Stops the tree, then the receiver (§4.7 order). Returns evidence.
    pub fn finish(mut self) -> (session::StopReport, Value) {
        let stop = self.session.stop();
        self.receiver.stop();
        let summary = self.receiver.summary();
        (stop, summary)
    }
}

pub fn launch(ctx: &Ctx, spec: LaunchSpec) -> Result<Launch, String> {
    let thread_id = uuid::Uuid::new_v4().to_string();
    let token = util::token();
    let receiver = Receiver::start(Config {
        token: token.clone(),
        thread_id: thread_id.clone(),
        agent: spec.agent.clone(),
        cli_version: spec.cli_version.clone(),
        rule: match &spec.session_id {
            Some(s) => BindRule::SessionId(s.clone()),
            None => BindRule::Ancestor,
        },
        launch_id: format!(
            "{}-{}",
            util::sanitize_component(&spec.label),
            &thread_id[..8]
        ),
        fixtures_dir: ctx.out.join("fixtures"),
        run_id: ctx.run_id.clone(),
        cli_names: spec.cli_names.clone(),
    })
    .map_err(|e| format!("receiver: {e}"))?;
    let mut env = vec![
        ("CHALDEA_HOOK_URL".to_string(), receiver.url()),
        ("CHALDEA_HOOK_TOKEN".to_string(), token.clone()),
        ("CHALDEA_THREAD_ID".to_string(), thread_id.clone()),
        (
            "CHALDEA_HOOK_LOG".to_string(),
            ctx.out.join("hook.log").display().to_string(),
        ),
    ];
    env.extend(spec.extra_env.iter().cloned());
    if let Some(f) = spec.before_spawn {
        f(&env)?;
    }
    let session = Session::spawn(SpawnOpts {
        argv: spec.argv,
        cwd: spec.cwd,
        env,
        cols: ctx.args.cols,
        rows: ctx.args.rows,
        responder: spec.responder,
        out_file: ctx.out.join(format!("pty-output-{}.bin", spec.label)),
        hold_assign: spec.hold_assign,
        inject: spec.inject,
    })?;
    receiver.set_cli_pid(session.owned.child_pid);
    // Ambiguous binding revokes the writer gate (queued pastes are dropped).
    let gate = session.io().tx.gate.clone();
    receiver.on_ambiguous(Box::new(move || gate.revoke()));
    // The ConPTY provider is only known once a PTY launch has loaded it.
    ctx.sink.with(|r| {
        if r.host.get("conpty_module_after_launch").is_none() {
            r.host["conpty_module_after_launch"] = json!(os::conpty_module());
        }
    });
    Ok(Launch {
        label: spec.label,
        receiver,
        session,
        thread_id,
        token,
    })
}

pub fn write_snapshot(ctx: &Ctx, label: &str, snap: &Snapshot) -> PathBuf {
    let dir = ctx.out.join("snapshots");
    let _ = std::fs::create_dir_all(&dir);
    let p = dir.join(format!("{}.json", util::sanitize_component(label)));
    let _ = std::fs::write(&p, serde_json::to_vec_pretty(snap).unwrap_or_default());
    p
}

/// A fresh output dir for one child run, created exclusively here (an
/// existing dir is never reused), and the run id the child must report.
pub fn child_out(parent: &Path, name: &str) -> Result<(PathBuf, String), String> {
    let id = util::run_id();
    let dir = parent.join(format!("{}-{id}", util::sanitize_component(name)));
    std::fs::create_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok((dir, id))
}

/// The child's report from its own fresh dir, accepted only when it carries
/// the run id the parent passed in; anything else is `not_run`.
pub fn child_report(out: &Path, run_id: &str) -> Result<Report, String> {
    match crate::report::read_report(&out.join("report.json")) {
        None => Err("not_run: the child wrote no report".into()),
        Some(r) if r.run_id != run_id => Err(format!(
            "not_run: report run_id {:?} is not the child's {run_id:?}",
            r.run_id
        )),
        Some(r) => Ok(r),
    }
}

/// Runs `cmd` with a deadline; kills only the child we started on timeout.
/// Non-interactive: no console window on Windows (C-5).
pub fn run_bounded(cmd: &mut Command, timeout: Duration) -> Result<(i32, String), String> {
    crate::os::no_window(cmd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(o) = stdout.as_mut() {
            let _ = o.take(1 << 20).read_to_string(&mut s);
        }
        s
    });
    let end = Instant::now() + timeout;
    loop {
        if let Some(st) = child.try_wait().map_err(|e| e.to_string())? {
            let out = reader.join().unwrap_or_default();
            return Ok((st.code().unwrap_or(-1), out));
        }
        if Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            return Err("timeout".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `<bin> --version`, first `x.y.z` token.
pub fn cli_version(bin: &Path) -> String {
    match run_bounded(Command::new(bin).arg("--version"), Duration::from_secs(15)) {
        Ok((_, out)) => out
            .split_whitespace()
            .find(|t| {
                t.split('.').count() >= 3 && t.chars().next().is_some_and(|c| c.is_ascii_digit())
            })
            .map(str::to_string)
            .unwrap_or_else(|| "unknown".into()),
        Err(_) => "unknown".into(),
    }
}

/// Evidence bundle (§5): allowlisted files under `R/out`, zipped by the OS `tar` (bsdtar).
pub fn bundle(args: &Args) -> i32 {
    let out_root = args.root.join("out");
    let mut files = Vec::new();
    fn walk(dir: &Path, base: &Path, include_pty: bool, files: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                walk(&p, base, include_pty, files);
                continue;
            }
            let rel = p
                .strip_prefix(base)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let allowed = name == "report.json"
                || name == "corpus.json"
                || rel
                    .split('/')
                    .any(|c| c == "fixtures" || c.starts_with("fixtures-"))
                || rel.contains("/snapshots/")
                || (include_pty && name.starts_with("pty-output") && name.ends_with(".bin"));
            if allowed {
                files.push(rel);
            }
        }
    }
    walk(&out_root, &args.root, args.include_pty_output, &mut files);
    if files.is_empty() {
        eprintln!("chs bundle: nothing under {}", out_root.display());
        return 3;
    }
    let zip = args.root.join(format!("chs-out-{}.zip", os::os_name()));
    let list = args
        .root
        .join(format!(".bundle-list-{}", std::process::id()));
    if std::fs::write(&list, files.join("\n")).is_err() {
        return 3;
    }
    let r = run_bounded(
        Command::new("tar")
            .arg("-a")
            .arg("-c")
            .arg("-f")
            .arg(&zip)
            .arg("-C")
            .arg(&args.root)
            .arg("-T")
            .arg(&list),
        Duration::from_secs(120),
    );
    let _ = std::fs::remove_file(&list);
    match r {
        Ok((0, _)) => {
            println!(
                "wrote {} ({} files); review it before sending",
                zip.display(),
                files.len()
            );
            0
        }
        other => {
            eprintln!("chs bundle: tar failed: {other:?}");
            3
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// r3: a child never reads a report left by an earlier run. Its dir is
    /// fresh and exclusive, and a report with another run id is `not_run`.
    #[test]
    fn stale_child_report_is_not_run() {
        let parent = std::env::temp_dir().join(format!("chs-child-test-{}", util::run_id()));
        std::fs::create_dir_all(&parent).unwrap();
        let (out, id) = child_out(&parent, "probe-kill").unwrap();
        assert!(std::fs::read_dir(&out).unwrap().next().is_none());
        assert!(child_report(&out, &id).is_err(), "no report is not_run");
        let stale = Report::new("win-probe kill", "00000000");
        std::fs::write(out.join("report.json"), serde_json::to_vec(&stale).unwrap()).unwrap();
        let e = child_report(&out, &id).unwrap_err();
        assert!(e.starts_with("not_run"), "{e}");
        let own = Report::new("win-probe kill", &id);
        std::fs::write(out.join("report.json"), serde_json::to_vec(&own).unwrap()).unwrap();
        assert_eq!(child_report(&out, &id).unwrap().run_id, id);
        let _ = std::fs::remove_dir_all(&parent);
    }
}
