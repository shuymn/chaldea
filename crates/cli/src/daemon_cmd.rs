//! `chaldea daemon run | start | stop` (docs/design/daemon-lifecycle.md §2.10, §2.12).
//!
//! `start` launches a detached child and waits for its one readiness line within one 15 s
//! deadline: on a macOS GUI host through a one-shot launchd job whose process forks the
//! daemon, on macOS without a GUI domain as a direct child that calls `setsid`, on Windows with
//! `DETACHED_PROCESS` and the parent job's breakaway. It never kills the child and never falls
//! back from launchd to a direct child. `stop` signals the published daemon through the OS and
//! waits, within one 15 s deadline, until that identity is gone.

use std::cell::Cell;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chaldea_daemon::lifecycle::{
    self, Budgets, DaemonConfig, DetachVia, Discovery, DiscoveryRecord, ExitCode, LockProbe,
    RunMode, diag, names,
};
use chaldea_pty_runtime::host::{
    self, Breakaway, DetachedChild, GuiDomain, LaunchctlOutput, ReadyArg, ReadyRead, ReadyReader,
    StopSent, StopTarget,
};

use crate::Seams;
use crate::client;

/// `daemon start`'s one deadline (herdr's bound, design L9).
pub(crate) const READY_TIMEOUT: Duration = Duration::from_secs(15);
/// `daemon stop`'s one deadline: the 10 s shutdown budget and 5 s for the process to exit.
pub(crate) const STOP_WAIT: Duration = Duration::from_secs(15);
/// The GUI-domain probe's share of the start deadline.
const GUI_PROBE_SHARE: Duration = Duration::from_secs(3);
/// The end of the start deadline kept for resolving a submission by its label.
const LABEL_RESERVE: Duration = Duration::from_secs(3);
/// How often the label is looked up after a `bootstrap` that timed out.
const LABEL_POLL: Duration = Duration::from_millis(250);
/// A `dev.chaldea.daemon.*` label older than this that is not running is stale (beyond any
/// start's 15 s deadline).
const STALE_LABEL_AGE: Duration = Duration::from_secs(60);
/// The share of the start deadline the stale-label cleanup may use.
const SCAVENGE_SHARE: Duration = Duration::from_secs(2);
/// The follow-up after the channel closed without a verdict.
const EOF_FOLLOW_UP: Duration = Duration::from_secs(2);
/// How often discovery, the lock, or a target's identity is polled.
const POLL: Duration = Duration::from_millis(50);
/// The owner-token request whose status shows a recorded daemon serves (design §2.10); only
/// the status line is read.
const SERVE_CHECK_PATH: &str = "/api/threads";
/// The prefix of every launchd label `daemon start` submits.
pub(crate) const LABEL_PREFIX: &str = "dev.chaldea.daemon.";
/// `launchctl print <service>` for a service that does not exist.
const LAUNCHCTL_NO_SERVICE: i32 = 113;

/// The starter's refusal when its own job allows no breakaway (design §2.12).
const JOB_REFUSAL: &str = "this terminal runs inside a job that Chaldea cannot leave, so a detached daemon could be killed when the terminal closes; run `chaldea daemon run` in a window you keep open, or start Chaldea from a terminal outside such a job";
/// Where a start that cannot detach safely points the user.
const RUN_INSTEAD: &str = "run `chaldea daemon run` in a terminal you keep open";

/// A diagnostic on stderr, through `diag`.
fn say(msg: &str) {
    let _ = writeln!(io::stderr(), "chaldea: {}", diag(msg));
}

/// A diagnostic and `code`.
fn fail_with(code: ExitCode, msg: &str) -> ExitCode {
    say(msg);
    code
}

fn fail(msg: &str) -> ExitCode {
    fail_with(ExitCode::Failed, msg)
}

/// A line on stdout, through `diag`.
fn print_out(msg: &str) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{}", diag(msg));
    let _ = out.flush();
}

// ---- run ----

/// What `daemon run` was asked to be.
pub(crate) struct RunArgs {
    pub(crate) port: Option<u16>,
    pub(crate) from_launchd: bool,
    pub(crate) detached_child: bool,
    pub(crate) ready: Option<OsString>,
}

/// The detached child's readiness channel: `None` for a foreground run.
///
/// # Errors
///
/// The usage error: `--detached-child` without `--ready`, or a `--ready` that does not parse.
pub(crate) fn ready_channel_arg(
    detached_child: bool,
    ready: Option<&OsStr>,
) -> Result<Option<ReadyArg>, String> {
    if !detached_child {
        return Ok(None);
    }
    let Some(ready) = ready else {
        return Err("--detached-child needs --ready".to_owned());
    };
    ReadyArg::parse(ready).map(Some).map_err(|e| e.to_string())
}

/// `daemon run`: a foreground daemon, or the detached child `start` launched.
pub(crate) fn run(state_dir: PathBuf, a: &RunArgs, seams: &mut Seams) -> ExitCode {
    let ready = match ready_channel_arg(a.detached_child, a.ready.as_deref()) {
        Ok(r) => r,
        Err(e) => return fail_with(ExitCode::Usage, &e),
    };
    let mode = if let Some(arg) = ready {
        let via = if a.from_launchd {
            DetachVia::Launchd
        } else if cfg!(windows) {
            DetachVia::Windows
        } else {
            DetachVia::Setsid
        };
        let open = lifecycle::host_ready_opener(arg);
        let ready: lifecycle::ReadyOpener = match seams.before_ready.take() {
            Some(pause) => Box::new(move || {
                pause();
                open()
            }),
            None => open,
        };
        RunMode::DetachedChild { ready, via }
    } else {
        RunMode::Foreground
    };
    let components = seams
        .components
        .take()
        .map_or_else(|| Box::new(lifecycle::MvpComponents) as _, |f| f());
    lifecycle::run(
        DaemonConfig {
            state_dir,
            port: a.port,
            mode,
            budgets: Budgets::DEFAULT,
        },
        components,
    )
    .code
}

// ---- start ----

/// The `launchctl` the starter calls (the real one, or a test's).
type Launchctl<'a> = &'a dyn Fn(&[&OsStr], Instant) -> io::Result<LaunchctlOutput>;

/// How the child was launched, for the follow-ups and the timeout message.
enum Launched {
    /// A direct child: `setsid` (macOS without a GUI domain) or Windows.
    Child(DetachedChild),
    /// A launchd job in `target` (`gui/<uid>`).
    Launchd { label: String, target: String },
}

impl Launched {
    fn pending(&self) -> String {
        match self {
            Launched::Child(c) => format!("the pending launch is the child pid {}", c.id()),
            Launched::Launchd { label, target } => format!(
                "the pending launch is the launchd label `{label}` (`launchctl bootout {target}/{label}` cancels a launch that has not forked yet)"
            ),
        }
    }
}

/// `daemon start` (design §2.10).
pub(crate) fn start(state_dir: &Path, port: Option<u16>, seams: &Seams) -> ExitCode {
    let deadline = Instant::now() + seams.deadline.unwrap_or(READY_TIMEOUT);
    let real = |args: &[&OsStr], d: Instant| host::launchctl(args, d);
    let launchctl: Launchctl<'_> = seams.launchctl.as_deref().unwrap_or(&real);
    let prefix = seams.label_prefix.as_deref().unwrap_or(LABEL_PREFIX);

    // 1. Windows: the breakaway from the starter's own job, or a refusal before any spawn.
    let breakaway = if cfg!(windows) {
        match host::job_state() {
            Ok(j) => match host::breakaway_for(&j) {
                Some(b) => b,
                None => return fail(JOB_REFUSAL),
            },
            Err(e) => return fail(&format!("this terminal's job state cannot be read: {e}")),
        }
    } else {
        Breakaway::NotRequested
    };
    // 2. The readiness channel, under the temp directory resolved once: also the child's
    // working directory and the launchd job's.
    let temp = match std::path::absolute(std::env::temp_dir()) {
        Ok(t) => t,
        Err(e) => return fail(&format!("the temp directory cannot be resolved: {e}")),
    };
    let (mut reader, ready) = match host::ready_channel_in(&temp) {
        Ok(c) => c,
        Err(e) => return fail(&format!("the readiness channel cannot be created: {e}")),
    };
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return fail(&format!("this program's path cannot be read: {e}")),
    };
    let mut child_args: Vec<OsString> = vec![
        "daemon".into(),
        "run".into(),
        "--detached-child".into(),
        "--ready".into(),
        ready.to_os_string(),
        "--state-dir".into(),
        state_dir.as_os_str().to_owned(),
    ];
    if let Some(p) = port {
        child_args.extend(["--port".into(), p.to_string().into()]);
    }
    // 3. The launch.
    let launched = if cfg!(target_os = "macos") {
        let probe_end = deadline.min(Instant::now() + GUI_PROBE_SHARE);
        match host::gui_domain_via(probe_end, launchctl) {
            GuiDomain::ProbeFailed { why } => {
                return fail(&format!(
                    "could not determine whether a GUI session exists ({why}); {RUN_INSTEAD}"
                ));
            }
            GuiDomain::Absent { why } => {
                say(&format!("detaching through setsid: no GUI domain ({why})"));
                launch_direct(Breakaway::NotRequested, &mut |b| {
                    host::spawn_detached(&exe, &child_args, &temp, b, &mut reader, deadline)
                })
                .map_err(|(code, msg)| fail_with(code, &msg))
            }
            GuiDomain::Available => {
                let mut args = child_args.clone();
                args.insert(2, "--from-launchd".into());
                launch_launchd(
                    &Job {
                        exe: &exe,
                        args: &args,
                        cwd: &temp,
                        prefix,
                    },
                    &mut reader,
                    launchctl,
                    deadline,
                )
            }
        }
    } else {
        say(&format!(
            "detaching as a process without a console (breakaway: {})",
            match breakaway {
                Breakaway::Explicit => "explicit",
                Breakaway::NotRequested => "not requested",
            }
        ));
        launch_direct(breakaway, &mut |b| {
            host::spawn_detached(&exe, &child_args, &temp, b, &mut reader, deadline)
        })
        .map_err(|(code, msg)| fail_with(code, &msg))
    };
    let mut launched = match launched {
        Ok(l) => l,
        Err(code) => return code,
    };
    // 4. The verdict.
    let code = await_verdict(
        &mut reader,
        &mut launched,
        state_dir,
        port,
        launchctl,
        StartPauses {
            after_verdict: seams.after_verdict.as_deref(),
            after_answer: seams.after_answer.as_deref(),
            after_recheck: seams.after_recheck.as_deref(),
            on_discovery: seams.on_discovery.as_deref(),
            after_eof: seams.after_eof.as_deref(),
        },
        deadline,
    );
    drop(reader);
    code
}

/// The direct child (`setsid` path on macOS, every Windows launch) through `spawn`, called
/// once: a refused explicit breakaway (`ERROR_ACCESS_DENIED`) is the job refusal, never retried
/// without the flag (design §2.12); a launch the deadline ended is a timeout. The error is the
/// exit code and its message.
fn launch_direct(
    breakaway: Breakaway,
    spawn: &mut dyn FnMut(Breakaway) -> io::Result<DetachedChild>,
) -> Result<Launched, (ExitCode, String)> {
    match spawn(breakaway) {
        Ok(c) => Ok(Launched::Child(c)),
        Err(e) if breakaway == Breakaway::Explicit && e.raw_os_error() == Some(5) => {
            Err((ExitCode::Failed, JOB_REFUSAL.to_owned()))
        }
        Err(e) if e.kind() == io::ErrorKind::TimedOut => Err((
            ExitCode::Timeout,
            "the daemon was not launched within 15 s".to_owned(),
        )),
        Err(e) => Err((
            ExitCode::Failed,
            format!("the daemon cannot be started: {e}"),
        )),
    }
}

/// Polls `observe` every `period` until it decides, each observation through
/// `host::observed_before`: `None` at the deadline, also for a result first observed at or
/// after it.
fn poll_until<T>(
    deadline: Instant,
    period: Duration,
    observe: impl FnMut() -> Option<T>,
) -> Option<T> {
    poll_until_cutoff(&Cell::new(deadline), period, observe)
}

/// [`poll_until`] against a cutoff a test seam can bring forward while an observation runs: a
/// result then counts only if it came before the cutoff as it stands after the observation.
/// Nothing moves the cutoff outside tests.
fn poll_until_cutoff<T>(
    cutoff: &Cell<Instant>,
    period: Duration,
    mut observe: impl FnMut() -> Option<T>,
) -> Option<T> {
    loop {
        let before = cutoff.get();
        match host::observed_before(before, &mut observe) {
            None => return None,
            Some(_) if cutoff.get() != before && Instant::now() >= cutoff.get() => return None,
            Some(Some(t)) => return Some(t),
            Some(None) => {}
        }
        std::thread::sleep(period.min(cutoff.get().saturating_duration_since(Instant::now())));
    }
}

/// What a label lookup found.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Lookup {
    Present,
    Absent,
    Failed(String),
}

fn lookup(launchctl: Launchctl<'_>, service: &str, until: Instant) -> Lookup {
    match launchctl(&["print".as_ref(), service.as_ref()], until) {
        Ok(o) if o.code == Some(0) => Lookup::Present,
        Ok(o) if o.code == Some(LAUNCHCTL_NO_SERVICE) => Lookup::Absent,
        Ok(o) => Lookup::Failed(format!(
            "launchctl print exited {:?}: {}",
            o.code,
            o.stderr.trim()
        )),
        Err(e) => Lookup::Failed(e.to_string()),
    }
}

/// After a `bootstrap` that timed out: the label looked up every 250 ms until `until`, a
/// presence first observed at or after `until` not counting. `true` once it is present.
fn poll_label(launchctl: Launchctl<'_>, service: &str, until: Instant) -> bool {
    poll_until(until, LABEL_POLL, || {
        (lookup(launchctl, service, until) == Lookup::Present).then_some(())
    })
    .is_some()
}

/// The launchd job a GUI-host start submits.
struct Job<'a> {
    exe: &'a Path,
    args: &'a [OsString],
    /// The job's working directory (the resolved temp directory).
    cwd: &'a Path,
    prefix: &'a str,
}

/// The macOS GUI-host launch (design §2.10): stale labels removed, the plist written into the
/// readiness directory, `bootstrap` with all but the last 3 s, and the label resolving a
/// failed or timed-out submission within those 3 s.
fn launch_launchd(
    job: &Job<'_>,
    reader: &mut ReadyReader,
    launchctl: Launchctl<'_>,
    deadline: Instant,
) -> Result<Launched, ExitCode> {
    let target = host::gui_domain_target();
    let now_ms = unix_ms();
    // Ownership is checked against the resolved executable; without one, nothing is removed.
    if let Ok(exe) = std::fs::canonicalize(job.exe) {
        scavenge(
            launchctl,
            &target,
            job.prefix,
            &exe,
            now_ms,
            deadline.min(Instant::now() + SCAVENGE_SHARE),
        );
    }
    let label = format!("{}{now_ms}-{}", job.prefix, uuid::Uuid::new_v4());
    let mut program = vec![job.exe.as_os_str().to_owned()];
    program.extend(job.args.iter().cloned());
    let plist = match render_plist(
        &label,
        &program,
        std::env::vars_os(),
        job.cwd,
        host::current_umask(),
    ) {
        Ok(p) => p,
        Err(name) => {
            return Err(fail(&format!(
                "this environment cannot be passed to launchd ({name}); {RUN_INSTEAD}"
            )));
        }
    };
    let path = match reader.write_plist(plist.as_bytes()) {
        Ok(p) => p,
        Err(e) => return Err(fail(&format!("the launchd job cannot be written: {e}"))),
    };
    say(&format!("detaching through launchd as {label}"));
    let service = format!("{target}/{label}");
    let launched = || Launched::Launchd {
        label: label.clone(),
        target: target.clone(),
    };
    let reserve_from = deadline.checked_sub(LABEL_RESERVE).unwrap_or(deadline);
    let boot = launchctl(
        &["bootstrap".as_ref(), target.as_ref(), path.as_os_str()],
        reserve_from,
    );
    let indeterminate = |why: &str| {
        fail_with(
            ExitCode::Timeout,
            &format!(
                "the launch may still be pending as `{label}` ({why}): `launchctl print {service}` shows it; until the daemon forks, `launchctl bootout {service}` cancels it; once it has published its record, use `chaldea daemon stop`"
            ),
        )
    };
    match boot {
        Ok(o) if o.code == Some(0) => Ok(launched()),
        Err(e) if e.kind() == io::ErrorKind::TimedOut => {
            // The submission may still complete after any lookup: poll the whole reserve.
            if poll_label(launchctl, &service, deadline) {
                Ok(launched())
            } else {
                Err(indeterminate("launchctl bootstrap did not finish in time"))
            }
        }
        other => {
            // `bootstrap` finished: nothing of this attempt is in flight, so one lookup
            // decides.
            let why = match &other {
                Ok(o) => format!(
                    "launchctl bootstrap exited {:?}: {}",
                    o.code,
                    o.stderr.trim()
                ),
                Err(e) => format!("launchctl bootstrap failed: {e}"),
            };
            match lookup(
                launchctl,
                &service,
                deadline.min(Instant::now() + LABEL_RESERVE),
            ) {
                Lookup::Present => Ok(launched()),
                Lookup::Absent => Err(fail(&format!(
                    "launchd did not accept the daemon's job ({why}); {RUN_INSTEAD}"
                ))),
                Lookup::Failed(e) => Err(indeterminate(&format!("{why}; lookup: {e}"))),
            }
        }
    }
}

fn unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// The labels with `prefix` that `launchctl print <domain>` lists and that are older than
/// [`STALE_LABEL_AGE`] at `now_ms`.
fn stale_candidates(domain_listing: &str, prefix: &str, now_ms: u128) -> Vec<String> {
    let mut out: Vec<String> = domain_listing
        .split_whitespace()
        .filter(|t| t.starts_with(prefix))
        .filter(|t| {
            t[prefix.len()..]
                .split_once('-')
                .and_then(|(ms, _)| ms.parse::<u128>().ok())
                .is_some_and(|ms| now_ms.saturating_sub(ms) > STALE_LABEL_AGE.as_millis())
        })
        .map(str::to_owned)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Whether `launchctl print <service>` affirmatively shows a finished job of this binary: its
/// top-level `state = not running` with a numeric `last exit code`, and program arguments
/// `<program> daemon run --from-launchd …` whose program resolves to `exe` (this starter's
/// resolved executable). An empty or unrecognised listing, a job that never exited, or another
/// executable is no proof.
fn finished_own_job(print: &str, exe: &Path) -> bool {
    let mut finished = false;
    let mut exited = false;
    let mut program = None;
    let mut argv: Vec<&str> = Vec::new();
    let mut in_arguments = false;
    for line in print.lines() {
        if in_arguments {
            if line == "\t}" {
                in_arguments = false;
            } else if let Some(a) = line.strip_prefix("\t\t") {
                argv.push(a.trim());
            }
            continue;
        }
        // Top-level keys only: nested blocks (the resource coalition) have their own state.
        let Some(top) = line.strip_prefix('\t').filter(|l| !l.starts_with('\t')) else {
            continue;
        };
        if top == "state = not running" {
            finished = true;
        } else if let Some(c) = top.strip_prefix("last exit code = ") {
            exited = c.trim().parse::<i64>().is_ok();
        } else if let Some(p) = top.strip_prefix("program = ") {
            program = Some(p.trim());
        } else if top == "arguments = {" {
            in_arguments = true;
        }
    }
    let this_program = |p: &str| std::fs::canonicalize(p).is_ok_and(|p| p == exe);
    finished
        && exited
        && program.is_some_and(this_program)
        && argv.first().copied() == program
        && argv.get(1..4) == Some(&["daemon", "run", "--from-launchd"][..])
}

/// Removes this prefix's labels proven stale: older than 60 s, and shown finished and owned
/// ([`finished_own_job`]). Nothing else; `launchctl` runs nothing at or after `until`, and the
/// first call it ends that way ends the cleanup.
fn scavenge(
    launchctl: Launchctl<'_>,
    target: &str,
    prefix: &str,
    exe: &Path,
    now_ms: u128,
    until: Instant,
) {
    let Ok(listing) = launchctl(&["print".as_ref(), target.as_ref()], until) else {
        return;
    };
    if listing.code != Some(0) {
        return;
    }
    let timed_out = |e: &io::Error| e.kind() == io::ErrorKind::TimedOut;
    for label in stale_candidates(&listing.stdout, prefix, now_ms) {
        let service = format!("{target}/{label}");
        let p = match launchctl(&["print".as_ref(), service.as_ref()], until) {
            Ok(p) => p,
            Err(e) if timed_out(&e) => return,
            Err(_) => continue,
        };
        if p.code != Some(0) || !finished_own_job(&p.stdout, exe) {
            continue;
        }
        match launchctl(&["bootout".as_ref(), service.as_ref()], until) {
            Ok(o) if o.code == Some(0) => say(&format!("removed the stale launchd job {label}")),
            Err(e) if timed_out(&e) => return,
            _ => {}
        }
    }
}

/// `s` as plist string content, or `None` when launchd's parser would not give it back
/// unchanged. Markup characters are escaped; every control character (C0 including tab,
/// newline, and carriage return; DEL; C1) is written as a decimal character reference, which
/// launchd maps back exactly (`plist_round_trips_control_characters`); U+FFFE and U+FFFF come
/// back altered and are refused.
fn plist_text(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\u{FFFE}' | '\u{FFFF}' => return None,
            c if c.is_control() => {
                let _ = write!(out, "&#{};", u32::from(c));
            }
            c => out.push(c),
        }
    }
    Some(out)
}

/// The one-shot job's plist (design §2.10): its program arguments, the starter's environment,
/// working directory, and umask, stdio `/dev/null`, `RunAtLoad`, no `KeepAlive`, and an
/// abandoned process group.
///
/// # Errors
///
/// The name of a value a plist cannot represent unchanged (a non-UTF-8 argument or variable).
fn render_plist(
    label: &str,
    program: &[OsString],
    env: impl Iterator<Item = (OsString, OsString)>,
    cwd: &Path,
    umask: u32,
) -> Result<String, String> {
    let text = |v: &OsStr, name: &str| -> Result<String, String> {
        v.to_str()
            .and_then(plist_text)
            .ok_or_else(|| name.to_owned())
    };
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n",
    );
    let _ = writeln!(
        s,
        "<key>Label</key><string>{}</string>",
        text(label.as_ref(), "the label")?
    );
    s.push_str("<key>ProgramArguments</key><array>\n");
    for (i, a) in program.iter().enumerate() {
        let _ = writeln!(s, "<string>{}</string>", text(a, &format!("argument {i}"))?);
    }
    s.push_str("</array>\n<key>EnvironmentVariables</key><dict>\n");
    let mut vars: Vec<(OsString, OsString)> = env.collect();
    vars.sort();
    for (k, v) in &vars {
        let name = k.to_string_lossy().into_owned();
        let key = text(k, &format!("the variable name {name}"))?;
        let val = text(v, &format!("the variable {name}"))?;
        let _ = writeln!(s, "<key>{key}</key><string>{val}</string>");
    }
    s.push_str("</dict>\n");
    let _ = writeln!(
        s,
        "<key>WorkingDirectory</key><string>{}</string>",
        text(cwd.as_os_str(), "the working directory")?
    );
    let _ = writeln!(s, "<key>Umask</key><integer>{umask}</integer>");
    for k in ["StandardInPath", "StandardOutPath", "StandardErrorPath"] {
        let _ = writeln!(s, "<key>{k}</key><string>/dev/null</string>");
    }
    s.push_str(
        "<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><false/>\n<key>AbandonProcessGroup</key><true/>\n</dict>\n</plist>\n",
    );
    Ok(s)
}

/// A readiness line as the starter reads it.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Ready {
        port: u16,
        pid: u32,
    },
    Fail {
        reason: String,
        port_saved: Option<u16>,
        message: String,
    },
}

fn parse_verdict(line: &str) -> Option<Verdict> {
    let mut it = line.splitn(2, ' ');
    match (it.next()?, it.next()) {
        ("ready", Some(rest)) => {
            let (port, pid) = rest.split_once(' ')?;
            Some(Verdict::Ready {
                port: port.parse().ok()?,
                pid: pid.parse().ok()?,
            })
        }
        ("fail", Some(rest)) => {
            let (reason, rest) = rest.split_once(' ').unwrap_or((rest, ""));
            let (port_saved, message) = match rest.strip_prefix("port_saved=") {
                Some(r) => {
                    let (n, m) = r.split_once(' ').unwrap_or((r, ""));
                    (Some(n.parse().ok()?), m)
                }
                None => (None, rest),
            };
            Some(Verdict::Fail {
                reason: reason.to_owned(),
                port_saved,
                message: message.to_owned(),
            })
        }
        _ => None,
    }
}

/// `--port <n>` against the port a daemon serves on.
fn port_conflict(dir: &Path, serving: u16, requested: Option<u16>) -> Option<ExitCode> {
    match requested {
        Some(n) if n != serving => Some(fail(&format!(
            "a daemon serves {} on port {serving}, not {n}",
            dir.display()
        ))),
        _ => None,
    }
}

/// The starter's pauses (test seams).
#[derive(Clone, Copy, Default)]
struct StartPauses<'a> {
    after_verdict: Option<&'a dyn Fn()>,
    after_answer: Option<&'a dyn Fn(Instant) -> Instant>,
    after_recheck: Option<&'a dyn Fn(&Discovery)>,
    on_discovery: Option<&'a dyn Fn(&Discovery)>,
    after_eof: Option<&'a dyn Fn(Instant) -> Instant>,
}

fn await_verdict(
    reader: &mut ReadyReader,
    launched: &mut Launched,
    dir: &Path,
    port: Option<u16>,
    launchctl: Launchctl<'_>,
    seams: StartPauses<'_>,
    deadline: Instant,
) -> ExitCode {
    let child = match launched {
        Launched::Child(c) => Some(c),
        Launched::Launchd { .. } => None,
    };
    let read = reader.read_line(deadline, child);
    if let (Ok(ReadyRead::Line(_)), Launched::Launchd { label, target }) = (&read, &*launched) {
        // Only the forked daemon sends a verdict, so the handoff happened.
        let service = format!("{target}/{label}");
        match launchctl(&["bootout".as_ref(), service.as_ref()], deadline) {
            Ok(o) if o.code == Some(0) => say(&format!("removed the launchd job {label}")),
            Ok(o) => say(&format!(
                "left the launchd job {label} for a later start to remove (bootout exited {:?})",
                o.code
            )),
            Err(e) => say(&format!(
                "left the launchd job {label} for a later start to remove ({e})"
            )),
        }
    }
    if let (Ok(ReadyRead::Line(_)), Some(pause)) = (&read, seams.after_verdict) {
        pause();
    }
    let log = dir.join(names::LOG);
    match read {
        Ok(ReadyRead::Line(line)) => match parse_verdict(&line) {
            Some(Verdict::Ready { port: p, pid }) => {
                if let Some(code) = port_conflict(dir, p, port) {
                    return code;
                }
                confirm_started(dir, p, pid, seams, &Cell::new(deadline))
            }
            Some(Verdict::Fail {
                reason,
                port_saved,
                message,
            }) => {
                if let Some(n) = port_saved {
                    say(&format!(
                        "port {n} was saved for {} before the start ended; the next start uses it, so re-pair with `chaldea open`",
                        dir.display()
                    ));
                }
                match reason.as_str() {
                    "already_running" => {
                        say(&format!(
                            "another daemon holds {}; waiting until it serves",
                            dir.display()
                        ));
                        await_running(dir, port, seams, &Cell::new(deadline))
                    }
                    "stopped" => fail("the daemon was stopped before it became ready"),
                    _ => fail(&format!("the daemon did not start ({reason}): {message}")),
                }
            }
            None => fail(&format!(
                "the daemon sent an unreadable readiness line; see {}",
                log.display()
            )),
        },
        Ok(ReadyRead::Eof) => {
            let cutoff = seams.after_eof.map_or(deadline, |pause| pause(deadline));
            eof_follow_up(launched, dir, cutoff)
        }
        Ok(ReadyRead::TimedOut) => fail_with(
            ExitCode::Timeout,
            &format!(
                "the daemon did not report readiness within 15 s; it may still be starting: once it has published its record, `chaldea daemon stop` stops it (and says `not running` when no daemon is left); before that, {}",
                launched.pending()
            ),
        ),
        Err(e) => fail(&format!(
            "the readiness channel failed: {e}; see {}",
            log.display()
        )),
    }
}

/// What one look at a recorded daemon found (design §2.10 "success only for a serving
/// instance").
#[derive(Debug, Clone, PartialEq, Eq)]
enum Serving {
    /// The record names it with the lock held, an owner-token request to it succeeded, and the
    /// record still names the same instance after the reply.
    Confirmed(DiscoveryRecord),
    /// No daemon holds the directory.
    Gone,
    /// The record names another process than the expected one.
    Replaced(DiscoveryRecord),
    /// Something a later look cannot change.
    Failed(String),
}

/// Whether an owner-token `GET` answers 200 before `deadline`: the status line only, so the
/// answer never depends on how much the daemon stores.
fn answers(port: u16, token: &str, deadline: Instant) -> bool {
    client::request_status(port, "GET", SERVE_CHECK_PATH, Some(token), deadline)
        .is_ok_and(|status| status == 200)
}

/// One look at the daemon serving `dir` (`expect`: the pid and port its verdict named); `None`
/// while it is starting, stopping, or recorded but not answering. `pauses.after_answer` runs
/// between the answer and the last discovery and may bring `cutoff` forward,
/// `pauses.after_recheck` sees that discovery, and `pauses.on_discovery` sees each
/// discovery (a test's pause and marks).
fn check_serving(
    dir: &Path,
    expect: Option<(u32, u16)>,
    pauses: StartPauses<'_>,
    cutoff: &Cell<Instant>,
) -> Option<Serving> {
    let discover = || {
        let d = client::discover(dir);
        if let Some(mark) = pauses.on_discovery {
            mark(&d);
        }
        d
    };
    let r = match discover() {
        Discovery::NotRunning => return Some(Serving::Gone),
        Discovery::Starting(_) | Discovery::Stopping(_) | Discovery::Unknown => return None,
        Discovery::Serving(r) => r,
    };
    if let Some((pid, port)) = expect
        && (r.pid != pid || r.port != Some(port))
    {
        return Some(Serving::Replaced(r));
    }
    let Some(port) = r.port else {
        return Some(Serving::Failed(
            "the serving daemon's record names no port".to_owned(),
        ));
    };
    let token = match client::read_owner_token(dir) {
        Ok(t) => t,
        Err(e) => return Some(Serving::Failed(format!("the owner token: {e}"))),
    };
    if !answers(port, &token, cutoff.get()) {
        return None;
    }
    if let Some(pause) = pauses.after_answer {
        cutoff.set(pause(cutoff.get()));
    }
    // The answer counts only if the record still says this instance serves, with the lock
    // held: a daemon that began its shutdown has marked its record `stopping`.
    let again = discover();
    if let Some(mark) = pauses.after_recheck {
        mark(&again);
    }
    match again {
        Discovery::Serving(again) if again.instance == r.instance => Some(Serving::Confirmed(r)),
        Discovery::NotRunning => Some(Serving::Gone),
        _ => None,
    }
}

/// After `ready <port> <pid>`: success only once that daemon is confirmed serving, polled every
/// 50 ms until the deadline.
fn confirm_started(
    dir: &Path,
    port: u16,
    pid: u32,
    pauses: StartPauses<'_>,
    cutoff: &Cell<Instant>,
) -> ExitCode {
    match poll_until_cutoff(cutoff, POLL, || {
        check_serving(dir, Some((pid, port)), pauses, cutoff)
    }) {
        Some(Serving::Confirmed(_)) => {
            print_out(&format!(
                "daemon started: http://127.0.0.1:{port} (pid {pid})"
            ));
            ExitCode::Ok
        }
        Some(Serving::Gone | Serving::Replaced(_)) => fail(&format!(
            "the daemon (pid {pid}) reported ready but stopped before the start finished; see {}",
            dir.join(names::LOG).display()
        )),
        Some(Serving::Failed(m)) => fail(&m),
        None => fail_with(
            ExitCode::Timeout,
            &format!(
                "the daemon (pid {pid}) reported ready, but its serving on port {port} was not confirmed within 15 s; it may still be running: `chaldea daemon stop` stops it (and says `not running` when no daemon is left)"
            ),
        ),
    }
}

/// After `fail already_running`: the winner, once confirmed serving, polled every 50 ms until
/// the deadline.
fn await_running(
    dir: &Path,
    port: Option<u16>,
    pauses: StartPauses<'_>,
    cutoff: &Cell<Instant>,
) -> ExitCode {
    match poll_until_cutoff(cutoff, POLL, || check_serving(dir, None, pauses, cutoff)) {
        Some(Serving::Confirmed(r)) => {
            let Some(p) = r.port else {
                return fail("the serving daemon's record names no port");
            };
            if let Some(code) = port_conflict(dir, p, port) {
                return code;
            }
            print_out(&format!(
                "daemon already running: http://127.0.0.1:{p} (pid {})",
                r.pid
            ));
            ExitCode::Ok
        }
        Some(Serving::Gone | Serving::Replaced(_)) => fail(&format!(
            "the daemon starting for {} exited; see {}",
            dir.display(),
            dir.join(names::LOG).display()
        )),
        Some(Serving::Failed(m)) => fail(&m),
        None => fail_with(
            ExitCode::Timeout,
            &format!(
                "no daemon was confirmed serving {} within 15 s; another start's daemon may still be starting",
                dir.display()
            ),
        ),
    }
}

/// One follow-up observation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    /// Decides the follow-up.
    Final(String),
    /// What holds for now (a child still running, a lock still held).
    Pending(String),
}

/// Polls `observe` every 50 ms until it sees something final, each observation through
/// `host::observed_before`: at `until`, the last observation made before it, or `None` when
/// none was.
fn follow_up(until: Instant, mut observe: impl FnMut() -> Seen) -> Option<String> {
    let mut last = None;
    loop {
        match host::observed_before(until, &mut observe) {
            None => return last,
            Some(Seen::Final(s)) => return Some(s),
            Some(Seen::Pending(s)) => last = Some(s),
        }
        std::thread::sleep(POLL.min(until.saturating_duration_since(Instant::now())));
    }
}

/// The channel closed without a verdict: what can be observed for at most 2 s, never at or
/// past the deadline (Windows: the child's exit, waited on; macOS: the lock, polled). The
/// closed channel alone says nothing of the daemon's state, so only an observation names one.
fn eof_follow_up(launched: &mut Launched, dir: &Path, deadline: Instant) -> ExitCode {
    let observed = eof_observed(launched, dir, deadline);
    fail(&format!(
        "the daemon closed its readiness channel without a verdict: {observed}; see {}",
        dir.join(names::LOG).display()
    ))
}

/// What [`eof_follow_up`] observed before its cutoff.
fn eof_observed(launched: &mut Launched, dir: &Path, deadline: Instant) -> String {
    eof_observed_at(launched, Instant::now(), deadline, || lifecycle::probe(dir))
}

/// [`eof_observed`] for a follow-up that starts at `now`, probing the lock through `probe`
/// (macOS).
fn eof_observed_at(
    launched: &mut Launched,
    now: Instant,
    deadline: Instant,
    mut probe: impl FnMut() -> io::Result<LockProbe>,
) -> String {
    let until = deadline.min(now + EOF_FOLLOW_UP);
    match launched {
        Launched::Child(c) if cfg!(windows) => Some(match c.exit_before(until) {
            Ok(Some(s)) => format!("it exited ({s})"),
            // The wait's end is not a state observed before it.
            Ok(None) if until >= deadline => {
                "its exit was not observed before the deadline".to_owned()
            }
            Ok(None) => format!(
                "its exit was not observed within {} s (pid {})",
                EOF_FOLLOW_UP.as_secs(),
                c.id()
            ),
            Err(e) => format!("its status cannot be read: {e}"),
        }),
        _ => follow_up(until, || match probe() {
            Ok(LockProbe::Free | LockProbe::Missing) => {
                Seen::Final("no daemon holds the state directory".to_owned())
            }
            Ok(LockProbe::Held) => {
                Seen::Pending("a daemon still holds the state directory".to_owned())
            }
            Err(e) => Seen::Final(format!("the lock cannot be probed: {e}")),
        }),
    }
    .unwrap_or_else(|| "nothing could be observed before the deadline".to_owned())
}

// ---- stop ----

/// `daemon stop` (design §2.10): one request to the published daemon, then a wait until its
/// identity is gone.
pub(crate) fn stop(dir: &Path, seams: &mut Seams) -> ExitCode {
    let deadline = Instant::now() + seams.deadline.unwrap_or(STOP_WAIT);
    let found = poll_until(deadline, POLL, || match client::discover(dir) {
        Discovery::NotRunning => Some(None),
        Discovery::Starting(r) | Discovery::Serving(r) | Discovery::Stopping(r) => Some(Some(r)),
        Discovery::Unknown => None,
    });
    let record = match found {
        Some(Some(r)) => r,
        Some(None) => {
            print_out("not running");
            return ExitCode::Ok;
        }
        None => {
            return fail_with(
                ExitCode::Timeout,
                &format!(
                    "no daemon of {} could be identified within 15 s",
                    dir.display()
                ),
            );
        }
    };
    let target = StopTarget {
        pid: record.pid,
        start_time: record.start_time.clone(),
        instance: record.instance.clone(),
    };
    // A daemon already stopping is not signalled again; its exit is awaited the same way.
    let sent = if record.state == lifecycle::RecordState::Stopping {
        Ok(StopSent::Sent)
    } else {
        host::send_stop(&target, deadline)
    };
    // Whether the stop already found the target exited; the completion check then only
    // confirms it.
    let mut exited = false;
    match sent {
        Ok(StopSent::Sent) => {}
        Ok(StopSent::DeadlinePassed) => {
            return fail_with(
                ExitCode::Timeout,
                &format!(
                    "the stop request was not sent within 15 s (pid {})",
                    target.pid
                ),
            );
        }
        Ok(StopSent::Exited | StopSent::IdentityMismatch) => {
            print_out("the daemon had already exited");
            exited = true;
        }
        Ok(StopSent::AccessDenied) => {
            return fail(if cfg!(windows) {
                "access denied; the daemon may run elevated: run `chaldea daemon stop` from an elevated terminal"
            } else {
                "access denied; run `chaldea daemon stop` as the daemon's user"
            });
        }
        Ok(StopSent::NoListener) => {
            return fail(&format!(
                "the daemon (pid {}) is running without a stop listener; see {}",
                target.pid,
                dir.join(names::LOG).display()
            ));
        }
        Err(e) => return fail(&format!("the stop request failed: {e}")),
    }
    if let Some(pause) = seams.after_stop_sent.take() {
        pause();
    }
    match wait_gone(&target, deadline) {
        Gone::Yes => {
            print_out("stopped");
            // A note about another holder, only when seen before the deadline.
            if let Some(Discovery::Starting(r) | Discovery::Serving(r) | Discovery::Stopping(r)) =
                host::observed_before(deadline, || client::discover(dir))
                && r.instance != target.instance
            {
                print_out(&format!(
                    "a new daemon (pid {}) now holds {}",
                    r.pid,
                    dir.display()
                ));
            }
            ExitCode::Ok
        }
        // Only what the expired completion check could not confirm.
        Gone::TimedOut if exited => fail_with(
            ExitCode::Timeout,
            &format!(
                "the completion check did not confirm within 15 s that pid {} is gone",
                target.pid
            ),
        ),
        Gone::TimedOut => fail_with(
            ExitCode::Timeout,
            &format!(
                "the daemon's exit (pid {}) was not observed within 15 s",
                target.pid
            ),
        ),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Gone {
    Yes,
    TimedOut,
}

/// Polls every 50 ms until the target's `(pid, start_time)` names no live process, a
/// disappearance first observed at or after the deadline not counting: generation-aware, so a
/// new daemon on the same directory does not count.
fn wait_gone(target: &StopTarget, deadline: Instant) -> Gone {
    wait_until_gone(deadline, || match host::process_identity(target.pid) {
        Ok(None) => true,
        Ok(Some(id)) => id.start_time != target.start_time,
        Err(_) => false,
    })
}

fn wait_until_gone(deadline: Instant, mut gone: impl FnMut() -> bool) -> Gone {
    poll_until(deadline, POLL, || gone().then_some(())).map_or(Gone::TimedOut, |()| Gone::Yes)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn verdict_lines_parse() {
        assert_eq!(
            parse_verdict("ready 7373 42"),
            Some(Verdict::Ready {
                port: 7373,
                pid: 42
            })
        );
        assert_eq!(
            parse_verdict("fail store_open port_saved=7374 the store is broken"),
            Some(Verdict::Fail {
                reason: "store_open".into(),
                port_saved: Some(7374),
                message: "the store is broken".into()
            })
        );
        assert_eq!(
            parse_verdict("fail stopped stopped before it became ready"),
            Some(Verdict::Fail {
                reason: "stopped".into(),
                port_saved: None,
                message: "stopped before it became ready".into()
            })
        );
        assert_eq!(parse_verdict("hello"), None);
        assert_eq!(parse_verdict("ready x 1"), None);
    }

    /// Escaping, and values a plist would alter refused by name.
    #[test]
    fn plist_escapes_and_refuses() {
        let p = render_plist(
            "dev.chaldea.daemon.1-x",
            &[OsString::from("/a b/<c>&\"d'")],
            [(OsString::from("K"), OsString::from("v\tw\nx"))].into_iter(),
            Path::new("/tmp"),
            0o22,
        )
        .unwrap();
        assert!(
            p.contains("<string>/a b/&lt;c&gt;&amp;&quot;d&apos;</string>"),
            "{p}"
        );
        assert!(
            p.contains("<key>K</key><string>v&#9;w&#10;x</string>"),
            "{p}"
        );
        assert!(p.contains("<key>Umask</key><integer>18</integer>"), "{p}");
        assert!(p.contains("<key>KeepAlive</key><false/>"), "{p}");
        assert!(p.contains("<key>AbandonProcessGroup</key><true/>"), "{p}");
        let nonchar = render_plist(
            "l",
            &[],
            [(OsString::from("BAD"), OsString::from("a\u{FFFE}b"))].into_iter(),
            Path::new("/tmp"),
            0,
        );
        assert_eq!(nonchar.unwrap_err(), "the variable BAD");
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let bytes = render_plist(
                "l",
                &[OsString::from_vec(vec![0xff])],
                std::iter::empty(),
                Path::new("/tmp"),
                0,
            );
            assert_eq!(bytes.unwrap_err(), "argument 0");
        }
    }

    /// Every control character a variable can hold reaches a real launchd job unchanged (the
    /// job writes the variable's bytes to a file). Skipped, with the reason printed, without the
    /// user's GUI domain (an SSH session), where no job can be submitted.
    #[cfg(target_os = "macos")]
    #[test]
    fn plist_round_trips_control_characters() {
        let domain = host::gui_domain(Instant::now() + GUI_PROBE_SHARE);
        if domain != GuiDomain::Available {
            eprintln!("skipped: submitting a launchd job needs the user's GUI domain ({domain:?})");
            return;
        }
        let value: String = (1u32..32)
            .chain(127..160)
            .filter_map(char::from_u32)
            .chain("&<>\"'é".chars())
            .collect();
        let dir = std::env::temp_dir().join(format!("chaldea-plist-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let out = dir.join("out");
        let label = format!(
            "dev.chaldea.test.plist.{}-{}",
            unix_ms(),
            uuid::Uuid::new_v4()
        );
        let program = [
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from(format!("printf %s \"$A\" > '{}'", out.display())),
        ];
        let plist = render_plist(
            &label,
            &program,
            [(OsString::from("A"), OsString::from(&value))].into_iter(),
            &dir,
            0o22,
        )
        .unwrap();
        let path = dir.join("job.plist");
        std::fs::write(&path, plist).unwrap();
        let target = host::gui_domain_target();
        let service = format!("{target}/{label}");
        let deadline = || Instant::now() + Duration::from_secs(10);
        let boot = host::launchctl(
            &["bootstrap".as_ref(), target.as_ref(), path.as_os_str()],
            deadline(),
        )
        .unwrap();
        assert_eq!(boot.code, Some(0), "{boot:?}");
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end && std::fs::metadata(&out).map_or(true, |m| m.len() == 0) {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(100));
        let got = std::fs::read(&out);
        let _ = host::launchctl(&["bootout".as_ref(), service.as_ref()], deadline());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got.unwrap(), value.as_bytes());
    }

    /// Only this prefix's labels older than 60 s are candidates.
    #[test]
    fn stale_label_candidates() {
        let now: u128 = 1_000_000_000;
        let p = "dev.chaldea.daemon.";
        let listing = format!(
            "services = {{\n  0 - {p}{}-a\n  0 - {p}{}-b\n  0 - other.{}-c\n  0 - {p}junk\n}}",
            now - 120_000,
            now - 5_000,
            now - 120_000
        );
        assert_eq!(
            stale_candidates(&listing, p, now),
            vec![format!("{p}{}-a", now - 120_000)]
        );
    }

    /// Deadline first: a target gone at the same poll as the deadline is a timeout.
    #[test]
    fn stop_wait_is_deadline_first() {
        let past = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        assert_eq!(wait_until_gone(past, || true), Gone::TimedOut);
        let soon = Instant::now() + Duration::from_secs(5);
        assert_eq!(wait_until_gone(soon, || true), Gone::Yes);
    }

    /// Deadline first: a label present at the end of the reserve does not count.
    #[test]
    fn label_poll_is_deadline_first() {
        let present = |_: &[&OsStr], _: Instant| -> io::Result<LaunchctlOutput> {
            Ok(LaunchctlOutput {
                code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            })
        };
        let past = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        assert!(!poll_label(&present, "gui/1/x", past));
        assert!(poll_label(
            &present,
            "gui/1/x",
            Instant::now() + Duration::from_secs(1)
        ));
    }

    /// A result first observed at or after the deadline loses: a target gone, a label present,
    /// a discovery answer, a follow-up observation.
    #[test]
    fn observations_that_cross_the_deadline_lose() {
        let end = Instant::now() + Duration::from_millis(100);
        assert_eq!(
            wait_until_gone(end, || {
                std::thread::sleep(end.saturating_duration_since(Instant::now()));
                true
            }),
            Gone::TimedOut
        );

        let end = Instant::now() + Duration::from_millis(100);
        let late = |_: &[&OsStr], deadline: Instant| -> io::Result<LaunchctlOutput> {
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
            Ok(LaunchctlOutput {
                code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            })
        };
        assert!(!poll_label(&late, "gui/1/test", end));

        let end = Instant::now() + Duration::from_millis(100);
        let found = poll_until(end, POLL, || {
            std::thread::sleep(end.saturating_duration_since(Instant::now()));
            Some(())
        });
        assert_eq!(found, None);

        let end = Instant::now() + Duration::from_millis(300);
        let mut n = 0;
        let seen = follow_up(end, || {
            n += 1;
            if n == 1 {
                Seen::Pending("held".to_owned())
            } else {
                std::thread::sleep(end.saturating_duration_since(Instant::now()));
                Seen::Final("free".to_owned())
            }
        });
        assert_eq!(seen.as_deref(), Some("held"));
    }

    /// The EOF follow-up ends at its cutoff with the last observation made before it, and a
    /// final observation before the cutoff decides at once.
    #[test]
    fn follow_up_keeps_its_cutoff() {
        let t = Instant::now();
        let end = t + Duration::from_millis(300);
        let seen = follow_up(end, || Seen::Pending("running".to_owned()));
        assert_eq!(seen.as_deref(), Some("running"));
        assert!(t.elapsed() >= Duration::from_millis(300));
        let past = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        assert_eq!(
            follow_up(past, || panic!("observed after the cutoff")),
            None
        );
        let seen = follow_up(Instant::now() + Duration::from_secs(5), || {
            Seen::Final("exited".to_owned())
        });
        assert_eq!(seen.as_deref(), Some("exited"));
    }

    /// A file standing for this starter's executable, and its resolved path.
    fn exe_file(dir: &Path, name: &str) -> std::path::PathBuf {
        let bin = dir.join(name);
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("chaldea");
        std::fs::write(&exe, b"").unwrap();
        std::fs::canonicalize(exe).unwrap()
    }

    /// A `launchctl print <service>` listing.
    fn print_listing(state: &str, exit: &str, program: &Path, args: &[&str]) -> String {
        let mut s = format!(
            "gui/501/dev.chaldea.daemon.1-x = {{\n\tactive count = 0\n\tstate = {state}\n\n\tprogram = {}\n\targuments = {{\n",
            program.display()
        );
        for a in std::iter::once(&program.to_string_lossy().as_ref()).chain(args) {
            s.push_str(&format!("\t\t{a}\n"));
        }
        s.push_str(&format!(
            "\t}}\n\n\tlast exit code = {exit}\n\n\tresource coalition = {{\n\t\tstate = active\n\t}}\n}}\n"
        ));
        s
    }

    const OWN_ARGS: [&str; 4] = ["daemon", "run", "--from-launchd", "--detached-child"];

    /// Scavenging ends at the first `launchctl` call the share's end refused: no later label
    /// is looked up or removed. With time left, every label proven stale is removed.
    #[test]
    fn scavenging_ends_at_the_first_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = exe_file(tmp.path(), "bin");
        let labels = ["dev.chaldea.daemon.1-a", "dev.chaldea.daemon.1-b"];
        let run = |refuse: bool| {
            let calls = std::cell::RefCell::new(Vec::new());
            let fake = |args: &[&OsStr], _: Instant| -> io::Result<LaunchctlOutput> {
                let args: Vec<String> = args
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect();
                calls.borrow_mut().push(args.join(" "));
                let stdout = match args.as_slice() {
                    [p, t] if p == "print" && t == "gui/1" => format!(
                        "services = {{\n\t0 - {}\n\t0 - {}\n}}",
                        labels[0], labels[1]
                    ),
                    [p, _] if p == "print" => {
                        if refuse {
                            return Err(io::Error::from(io::ErrorKind::TimedOut));
                        }
                        print_listing("not running", "2", &exe, &OWN_ARGS)
                    }
                    _ => String::new(),
                };
                Ok(LaunchctlOutput {
                    code: Some(0),
                    stdout,
                    stderr: String::new(),
                })
            };
            let end = Instant::now() + Duration::from_secs(5);
            scavenge(&fake, "gui/1", "dev.chaldea.daemon.", &exe, 1_000_000, end);
            calls.into_inner()
        };
        let refused = run(true);
        assert_eq!(
            refused,
            vec![
                "print gui/1".to_owned(),
                format!("print gui/1/{}", labels[0])
            ]
        );
        let on_time = run(false);
        let removed: Vec<_> = on_time
            .iter()
            .filter(|c| c.starts_with("bootout"))
            .collect();
        assert_eq!(removed.len(), 2, "{on_time:?}");
    }

    /// PR8-CL-12 (design §2.12): a spawn with an explicit breakaway that fails with
    /// `ERROR_ACCESS_DENIED` is the job refusal, with its message, one spawn, and no retry
    /// without the flag.
    #[test]
    fn refused_breakaway_is_not_retried() {
        let mut calls = Vec::new();
        let got = launch_direct(Breakaway::Explicit, &mut |b| {
            calls.push(b);
            Err(io::Error::from_raw_os_error(5))
        });
        let Err((code, msg)) = got else {
            panic!("the refused launch succeeded")
        };
        assert_eq!((code, msg.as_str()), (ExitCode::Failed, JOB_REFUSAL));
        assert_eq!(calls, vec![Breakaway::Explicit]);
    }

    /// Only a listing that shows a finished job (a numeric last exit code) of this executable,
    /// resolved, with the starter's argument shape, proves a stale label ours.
    #[test]
    fn only_finished_own_jobs_are_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = exe_file(tmp.path(), "bin");
        let other = exe_file(tmp.path(), "other");
        assert!(finished_own_job(
            &print_listing("not running", "2", &exe, &OWN_ARGS),
            &exe
        ));
        #[cfg(unix)]
        {
            // Another path to the same file resolves to it.
            let link = tmp.path().join("link");
            std::os::unix::fs::symlink(&exe, &link).unwrap();
            assert!(finished_own_job(
                &print_listing("not running", "2", &link, &OWN_ARGS),
                &exe
            ));
        }
        assert!(!finished_own_job(
            &print_listing("not running", "2", &other, &OWN_ARGS),
            &exe
        ));
        assert!(!finished_own_job(
            &print_listing("not running", "(never exited)", &exe, &OWN_ARGS),
            &exe
        ));
        assert!(!finished_own_job(
            &print_listing("running", "2", &exe, &OWN_ARGS),
            &exe
        ));
        assert!(!finished_own_job(
            &print_listing("not running", "2", Path::new("/usr/bin/true"), &OWN_ARGS),
            &exe
        ));
        assert!(!finished_own_job(
            &print_listing("not running", "2", &exe, &["daemon", "run"]),
            &exe
        ));
        assert!(!finished_own_job("", &exe));
        assert!(!finished_own_job("Could not find service", &exe));
    }

    /// The EOF follow-up ends at the shared deadline when that comes before its own 2 s. The
    /// follow-up's start is injected an hour ahead, so its own 2 s window is open whenever the
    /// test runs: with the shared deadline already passed the lock is never probed (a probe
    /// panics) and nothing is reported (a follow-up bounded by its 2 s alone would probe it),
    /// and with the shared deadline beyond that window the probe runs and its result is
    /// reported.
    #[test]
    fn eof_follow_up_uses_the_shorter_shared_deadline() {
        let mut launch = Launched::Launchd {
            label: "unused".into(),
            target: "gui/1".into(),
        };
        let ahead = Instant::now() + Duration::from_secs(3600);
        let passed = Instant::now();
        let held = || -> io::Result<LockProbe> { panic!("probed past the shared deadline") };
        assert_eq!(
            eof_observed_at(&mut launch, ahead, passed, held),
            "nothing could be observed before the deadline"
        );
        let probes = Cell::new(0);
        let free = || {
            probes.set(probes.get() + 1);
            Ok(LockProbe::Free)
        };
        let later = ahead + Duration::from_secs(3600);
        assert_eq!(
            eof_observed_at(&mut launch, ahead, later, free),
            "no daemon holds the state directory"
        );
        assert_eq!(probes.get(), 1);
    }

    /// The serving check reads the status line only: a 200 whose body is larger than any
    /// response the client reads in full still counts.
    #[test]
    fn serve_check_reads_only_the_status() {
        use std::io::{Read as _, Write as _};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut got = Vec::new();
            let mut b = [0u8; 1024];
            while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = c.read(&mut b).unwrap();
                assert!(n > 0, "the request ended early");
                got.extend_from_slice(&b[..n]);
            }
            let _ = c.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4194304\r\n\r\n[");
            // Held open until the client closes its end.
            let _ = c.read(&mut b);
        });
        assert!(answers(port, "t", Instant::now() + Duration::from_secs(5)));
        server.join().unwrap();
    }

    /// The starter's breakaway decision (design §2.12), every row.
    #[test]
    fn breakaway_rows() {
        use chaldea_pty_runtime::host::{JobLimits, JobState, breakaway_for};
        let limits = |b, s| JobState {
            in_job: true,
            immediate: Some(JobLimits {
                kill_on_close: true,
                breakaway_ok: b,
                silent_breakaway_ok: s,
            }),
        };
        assert_eq!(
            breakaway_for(&JobState::NONE),
            Some(Breakaway::NotRequested)
        );
        assert_eq!(
            breakaway_for(&limits(true, false)),
            Some(Breakaway::Explicit)
        );
        assert_eq!(
            breakaway_for(&limits(false, true)),
            Some(Breakaway::NotRequested)
        );
        assert_eq!(breakaway_for(&limits(false, false)), None);
    }
}
