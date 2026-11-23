//! `chaldea daemon run | start | stop` (docs/design/daemon-lifecycle.md §2.10, §2.12).
//!
//! `start` launches a detached child and waits for its one readiness line within one 15 s
//! deadline: on a macOS GUI host through a one-shot launchd job whose process forks the
//! daemon, on macOS without a GUI domain as a direct child that calls `setsid`, on Windows with
//! `DETACHED_PROCESS` and the parent job's breakaway. It never kills the child and never falls
//! back from launchd to a direct child. `stop` signals the published daemon through the OS and
//! waits, within one 15 s deadline, until that identity is gone.

use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use chaldea_daemon::lifecycle::{
    self, Budgets, DaemonConfig, DetachVia, Discovery, ExitCode, LockProbe, RunMode, diag, names,
};
use chaldea_pty_runtime::host::{
    self, Breakaway, GuiDomain, LaunchctlOutput, ReadyArg, ReadyRead, ReadyReader, StopSent,
    StopTarget,
};

use crate::Seams;
use crate::client;
use crate::spawn::spawn_locked;

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

/// `daemon run`: a foreground daemon, or the detached child `start` launched.
pub(crate) fn run(state_dir: PathBuf, a: &RunArgs, seams: &mut Seams) -> ExitCode {
    let mode = if a.detached_child {
        let Some(ready) = a.ready.as_deref() else {
            return fail_with(ExitCode::Usage, "--detached-child needs --ready");
        };
        let arg = match ReadyArg::parse(ready) {
            Ok(r) => r,
            Err(e) => return fail_with(ExitCode::Usage, &e.to_string()),
        };
        let via = if a.from_launchd {
            DetachVia::Launchd
        } else if cfg!(windows) {
            DetachVia::Windows
        } else {
            DetachVia::Setsid
        };
        RunMode::DetachedChild {
            ready: lifecycle::host_ready_opener(arg),
            via,
        }
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
    Child(Child),
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
    let deadline = Instant::now() + READY_TIMEOUT;
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
    // 2. The readiness channel.
    let (mut reader, ready) = match host::ready_channel() {
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
                spawn_direct(&exe, &child_args, &mut reader, Breakaway::NotRequested)
            }
            GuiDomain::Available => {
                let mut args = child_args.clone();
                args.insert(2, "--from-launchd".into());
                launch_launchd(&exe, &args, &reader, prefix, launchctl, deadline)
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
        spawn_direct(&exe, &child_args, &mut reader, breakaway)
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
        deadline,
    );
    drop(reader);
    code
}

/// The direct child: `setsid` path on macOS, Windows with its creation flags.
fn spawn_direct(
    exe: &Path,
    args: &[OsString],
    reader: &mut ReadyReader,
    breakaway: Breakaway,
) -> Result<Launched, ExitCode> {
    if let Err(e) = host::uninherit_std_handles() {
        say(&format!(
            "this terminal's handles may stay open in the daemon: {e}"
        ));
    }
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    if cfg!(windows) {
        cmd.stdout(Stdio::piped());
    } else {
        cmd.stdout(Stdio::null());
    }
    host::configure_detached(&mut cmd, breakaway);
    match spawn_locked(&mut cmd) {
        Ok(mut c) => {
            if let Some(out) = c.stdout.take() {
                reader.attach_stdout(out);
            }
            Ok(Launched::Child(c))
        }
        // `ERROR_ACCESS_DENIED`: the job refused the breakaway; no retry without it.
        Err(e) if breakaway == Breakaway::Explicit && e.raw_os_error() == Some(5) => {
            Err(fail(JOB_REFUSAL))
        }
        Err(e) => Err(fail(&format!("the daemon cannot be started: {e}"))),
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

/// After a `bootstrap` that timed out: the label looked up every 250 ms until `until`,
/// deadline first. `true` once it is present.
fn poll_label(launchctl: Launchctl<'_>, service: &str, until: Instant) -> bool {
    loop {
        if Instant::now() >= until {
            return false;
        }
        if lookup(launchctl, service, until) == Lookup::Present {
            return true;
        }
        let left = until.saturating_duration_since(Instant::now());
        std::thread::sleep(LABEL_POLL.min(left));
    }
}

/// The macOS GUI-host launch (design §2.10): stale labels removed, the plist written into the
/// readiness directory, `bootstrap` with all but the last 3 s, and the label resolving a
/// failed or timed-out submission within those 3 s.
fn launch_launchd(
    exe: &Path,
    args: &[OsString],
    reader: &ReadyReader,
    prefix: &str,
    launchctl: Launchctl<'_>,
    deadline: Instant,
) -> Result<Launched, ExitCode> {
    let target = host::gui_domain_target();
    let now_ms = unix_ms();
    scavenge(
        launchctl,
        &target,
        prefix,
        now_ms,
        deadline.min(Instant::now() + SCAVENGE_SHARE),
    );
    let label = format!("{prefix}{now_ms}-{}", uuid::Uuid::new_v4());
    let Some(dir) = reader.dir() else {
        return Err(fail("the readiness channel has no directory for the job"));
    };
    let mut program = vec![exe.as_os_str().to_owned()];
    program.extend(args.iter().cloned());
    let plist = match render_plist(
        &label,
        &program,
        std::env::vars_os(),
        &std::env::temp_dir(),
        host::current_umask(),
    ) {
        Ok(p) => p,
        Err(name) => {
            return Err(fail(&format!(
                "this environment cannot be passed to launchd ({name}); {RUN_INSTEAD}"
            )));
        }
    };
    let path = dir.join("job.plist");
    let written = host::create_private_file(&path).and_then(|mut f| {
        f.write_all(plist.as_bytes())?;
        f.sync_all()
    });
    if let Err(e) = written {
        return Err(fail(&format!("the launchd job cannot be written: {e}")));
    }
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

/// Removes this prefix's labels proven stale: older than 60 s and not running. Nothing else.
fn scavenge(launchctl: Launchctl<'_>, target: &str, prefix: &str, now_ms: u128, until: Instant) {
    let Ok(listing) = launchctl(&["print".as_ref(), target.as_ref()], until) else {
        return;
    };
    if listing.code != Some(0) {
        return;
    }
    for label in stale_candidates(&listing.stdout, prefix, now_ms) {
        if Instant::now() >= until {
            return;
        }
        let service = format!("{target}/{label}");
        let Ok(p) = launchctl(&["print".as_ref(), service.as_ref()], until) else {
            continue;
        };
        if p.code == Some(0) && !p.stdout.contains("state = running") {
            let removed = launchctl(&["bootout".as_ref(), service.as_ref()], until);
            if removed.is_ok_and(|o| o.code == Some(0)) {
                say(&format!("removed the stale launchd job {label}"));
            }
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

fn await_verdict(
    reader: &mut ReadyReader,
    launched: &mut Launched,
    dir: &Path,
    port: Option<u16>,
    launchctl: Launchctl<'_>,
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
    let log = dir.join(names::LOG);
    match read {
        Ok(ReadyRead::Line(line)) => match parse_verdict(&line) {
            Some(Verdict::Ready { port: p, pid }) => {
                if let Some(code) = port_conflict(dir, p, port) {
                    return code;
                }
                print_out(&format!("daemon started: http://127.0.0.1:{p} (pid {pid})"));
                ExitCode::Ok
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
                        await_running(dir, port, deadline)
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
        Ok(ReadyRead::Eof) => eof_follow_up(launched, dir, deadline),
        Ok(ReadyRead::TimedOut) => fail_with(
            ExitCode::Timeout,
            &format!(
                "the daemon did not report readiness within 15 s; it may still be starting: once it has published its record, `chaldea daemon stop` reaches it; before that, {}",
                launched.pending()
            ),
        ),
        Err(e) => fail(&format!(
            "the readiness channel failed: {e}; see {}",
            log.display()
        )),
    }
}

/// After `fail already_running`: the winner's record, polled every 50 ms, deadline first.
fn await_running(dir: &Path, port: Option<u16>, deadline: Instant) -> ExitCode {
    loop {
        if Instant::now() >= deadline {
            return fail_with(
                ExitCode::Timeout,
                &format!(
                    "a daemon holds {} but did not report serving within 15 s; it may still be starting",
                    dir.display()
                ),
            );
        }
        match client::discover(dir) {
            Discovery::Serving(r) => {
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
                return ExitCode::Ok;
            }
            Discovery::NotRunning => {
                return fail(&format!(
                    "the daemon starting for {} exited; see {}",
                    dir.display(),
                    dir.join(names::LOG).display()
                ));
            }
            Discovery::Starting(_) | Discovery::Unknown => {}
        }
        std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// The channel closed without a verdict: what can be observed for at most 2 s, never past
/// the deadline (Windows: the child's exit; macOS: the lock).
fn eof_follow_up(launched: &mut Launched, dir: &Path, deadline: Instant) -> ExitCode {
    let until = deadline.min(Instant::now() + EOF_FOLLOW_UP);
    let log = dir.join(names::LOG);
    let observed = loop {
        let done = Instant::now() >= until;
        let seen = match launched {
            Launched::Child(c) if cfg!(windows) => match c.try_wait() {
                Ok(Some(s)) => Some(format!("it exited ({s})")),
                Ok(None) if done => Some(format!("it is still running (pid {})", c.id())),
                Ok(None) => None,
                Err(e) => Some(format!("its status cannot be read: {e}")),
            },
            _ => match lifecycle::probe(dir) {
                Ok(LockProbe::Free | LockProbe::Missing) => {
                    Some("no daemon holds the state directory".to_owned())
                }
                Ok(LockProbe::Held) if done => {
                    Some("a daemon still holds the state directory".to_owned())
                }
                Ok(LockProbe::Held) => None,
                Err(e) => Some(format!("the lock cannot be probed: {e}")),
            },
        };
        if let Some(s) = seen {
            break s;
        }
        std::thread::sleep(POLL.min(until.saturating_duration_since(Instant::now())));
    };
    fail(&format!(
        "the daemon exited before reporting readiness: {observed}; see {}",
        log.display()
    ))
}

// ---- stop ----

/// `daemon stop` (design §2.10): one request to the published daemon, then a wait until its
/// identity is gone.
pub(crate) fn stop(dir: &Path, seams: &mut Seams) -> ExitCode {
    let deadline = Instant::now() + STOP_WAIT;
    let record = loop {
        match client::discover(dir) {
            Discovery::NotRunning => {
                print_out("not running");
                return ExitCode::Ok;
            }
            Discovery::Starting(r) | Discovery::Serving(r) => break r,
            Discovery::Unknown => {}
        }
        if Instant::now() >= deadline {
            return fail_with(
                ExitCode::Timeout,
                &format!(
                    "a daemon holds {} but has not published its identity",
                    dir.display()
                ),
            );
        }
        std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
    };
    let target = StopTarget {
        pid: record.pid,
        start_time: record.start_time.clone(),
        instance: record.instance.clone(),
    };
    match host::send_stop(&target) {
        Ok(StopSent::Sent) => {}
        Ok(StopSent::Exited | StopSent::IdentityMismatch) => {
            print_out("the daemon had already exited");
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
            if let Discovery::Starting(r) | Discovery::Serving(r) = client::discover(dir)
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
        Gone::TimedOut => fail_with(
            ExitCode::Timeout,
            &format!("still running after 15 s (pid {})", target.pid),
        ),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Gone {
    Yes,
    TimedOut,
}

/// Polls every 50 ms, deadline first, until the target's `(pid, start_time)` names no live
/// process: generation-aware, so a new daemon on the same directory does not count.
fn wait_gone(target: &StopTarget, deadline: Instant) -> Gone {
    wait_until_gone(deadline, || match host::process_identity(target.pid) {
        Ok(None) => true,
        Ok(Some(id)) => id.start_time != target.start_time,
        Err(_) => false,
    })
}

fn wait_until_gone(deadline: Instant, mut gone: impl FnMut() -> bool) -> Gone {
    loop {
        if Instant::now() >= deadline {
            return Gone::TimedOut;
        }
        if gone() {
            return Gone::Yes;
        }
        std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
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
    /// job writes the variable's bytes to a file).
    #[cfg(target_os = "macos")]
    #[test]
    fn plist_round_trips_control_characters() {
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
