//! `chaldea spawn`, `resume`, `stop`, `close`, and `wait` (docs/design/supervisor.md §2.2,
//! SD16): each command is one `POST /api/commands/{name}` with one `command_id`, sent again
//! unchanged on a transport failure, `408`, `503`, or `409 in_progress` until one cutoff; the
//! reads are `GET /api/threads/{thread_id}` under the same rules.

use std::cell::Cell;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chaldea_core::command::{
    CloseArgs, CloseResult, LaunchOutcome, ResumeArgs, ResumeResult, SpawnArgs, SpawnResult,
    StopArgs, StopResult,
};
use chaldea_core::http::{ErrorBody, ErrorCode};
use chaldea_core::ids::{Agent, CommandId, LaunchId, ThreadId};
use chaldea_core::input::MAX_PROMPT_BYTES;
use chaldea_core::input::encode::{Purpose, check};
use chaldea_core::payload::{ExitReason, InputEncoding};
use chaldea_core::state::{ProcessState, TurnState};
use chaldea_core::sync::ThreadSummary;
use chaldea_daemon::lifecycle::{ExitCode, diag};
use serde::Serialize;

use crate::client::{self, Discovery, Response};
use crate::poll::poll_until_with;

/// How long a command is sent again before the CLI gives up: the daemon's own deadline (30 s)
/// plus its reply grace, with room for a retry.
const COMMAND_CUTOFF: Duration = Duration::from_secs(60);
/// The pause between attempts and between a wait's polls.
const PERIOD: Duration = Duration::from_millis(250);

/// `chaldea wait`'s condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Until {
    /// The process exited.
    Exited,
    /// The process runs and its turn is idle.
    Idle,
    /// The thread has unresolved attention.
    Attention,
}

/// The general `death_unconfirmed` line.
const UNCONFIRMED: &str = "The thread's last process could not be confirmed to have ended.";
const UNCONFIRMED_RESTART: &str = "The previous process of this thread could not be confirmed to have ended after the daemon restarted.";
const UNCONFIRMED_STOPPED: &str =
    "The previous process of this thread could not be confirmed to have ended when it stopped.";
const UNCONFIRMED_TAIL: &str = "This version cannot resume or close this thread. Its history is kept, and new threads can be started.";
/// The line of a stop whose death was confirmed after its 5 s.
const CONFIRMED_LATE: &str =
    "The process ended, but its end was not confirmed within 5 s of the stop request.";

/// Where the daemon is, as one attempt sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Found {
    Port(u16),
    /// Starting, stopping, or not known yet: try again.
    NotYet,
    /// No daemon holds the lock.
    NotRunning,
}

/// Discovery under the liveness rule, as an attempt uses it.
fn found(d: Discovery) -> Result<Found, String> {
    match d {
        Discovery::Serving(r) => r
            .port
            .map(Found::Port)
            .ok_or_else(|| "the daemon's record names no port".to_owned()),
        Discovery::NotRunning => Ok(Found::NotRunning),
        Discovery::Unknown | Discovery::Starting(_) | Discovery::Stopping(_) => Ok(Found::NotYet),
    }
}

/// The daemon as the commands reach it: discovery, the owner token, and the clock.
pub(crate) struct Client<'a> {
    pub discover: &'a dyn Fn() -> Result<Found, String>,
    pub token: String,
    pub now: &'a dyn Fn() -> Instant,
    pub period: Duration,
}

/// Why an exchange ended without a response to adopt.
#[derive(Debug)]
pub(crate) enum End {
    /// The cutoff passed, or a request ended `TimedOut` at it.
    Timeout,
    /// The message to print.
    Fail(String),
}

/// A request to send, the same bytes on every attempt.
enum Req<'a> {
    Get(&'a str),
    Post(&'a str, &'a [u8]),
}

impl Client<'_> {
    /// Sends `req` until `decide` takes a response, the cutoff, or a failure; `decide`
    /// returns `None` to send it again. With `daemon_required`, no daemon is a failure, else
    /// polled. A response read at or after the cutoff is not adopted.
    fn exchange<T>(
        &self,
        cutoff: &Cell<Instant>,
        req: &Req<'_>,
        daemon_required: bool,
        decide: &mut dyn FnMut(Response) -> Option<Result<T, End>>,
    ) -> Result<T, End> {
        let seen = poll_until_with(cutoff, self.period, self.now, || {
            let port = match (self.discover)() {
                Ok(Found::Port(p)) => p,
                Ok(Found::NotRunning) if daemon_required => {
                    return Some(Err(End::Fail(
                        "daemon not running; run `chaldea daemon start`".to_owned(),
                    )));
                }
                Ok(Found::NotRunning | Found::NotYet) => return None,
                Err(m) => return Some(Err(End::Fail(m))),
            };
            let token = Some(self.token.as_str());
            let r = match req {
                Req::Get(path) => client::request(port, "GET", path, token, cutoff.get()),
                Req::Post(path, body) => client::post_json(port, path, token, body, cutoff.get()),
            };
            match r {
                Err(e) if e.kind() == io::ErrorKind::TimedOut => Some(Err(End::Timeout)),
                Err(e) if transient(e.kind()) => None,
                Err(e) => Some(Err(End::Fail(format!("the request failed: {e}")))),
                Ok(r) => decide(r),
            }
        });
        seen.unwrap_or(Err(End::Timeout))
    }

    /// `GET /api/threads/{t}` until `cond` holds of a summary or `decide` ends the read,
    /// under SD16's rules.
    fn summary_until(
        &self,
        cutoff: &Cell<Instant>,
        t: ThreadId,
        daemon_required: bool,
        cond: &dyn Fn(&ThreadSummary) -> Option<Result<(), End>>,
    ) -> Result<ThreadSummary, End> {
        let path = format!("/api/threads/{t}");
        self.exchange(
            cutoff,
            &Req::Get(&path),
            daemon_required,
            &mut |r| match r.status {
                408 | 503 => None,
                200 => match serde_json::from_slice::<ThreadSummary>(&r.body) {
                    Ok(s) => cond(&s).map(|c| c.map(|()| s)),
                    Err(e) => Some(Err(End::Fail(format!(
                        "the thread summary is not readable: {e}"
                    )))),
                },
                404 => Some(Err(End::Fail("no such thread or not readable".to_owned()))),
                s => Some(Err(End::Fail(format!(
                    "the daemon answered the read with {s} {}",
                    code_name(&r)
                )))),
            },
        )
    }

    /// One summary.
    fn summary(&self, cutoff: &Cell<Instant>, t: ThreadId) -> Result<ThreadSummary, End> {
        self.summary_until(cutoff, t, true, &|_| Some(Ok(())))
    }

    /// One command: its result, or the refusal's status and code.
    fn command<A: Serialize>(
        &self,
        cutoff: &Cell<Instant>,
        name: &str,
        args: &A,
    ) -> Result<Result<Response, (u16, Option<ErrorCode>)>, End> {
        let body = serde_json::to_vec(&serde_json::json!({
            "command_id": CommandId(uuid::Uuid::new_v4()),
            "args": args,
        }))
        .map_err(|e| End::Fail(format!("the command could not be encoded: {e}")))?;
        let path = format!("/api/commands/{name}");
        self.exchange(cutoff, &Req::Post(&path, &body), true, &mut |r| {
            let again = matches!(r.status, 408 | 503)
                || (r.status == 409 && code(&r) == Some(ErrorCode::InProgress));
            match r.status {
                _ if again => None,
                200 => Some(Ok(Ok(r))),
                s => Some(Ok(Err((s, code(&r))))),
            }
        })
    }
}

/// A connection refused, reset, or closed before the response was complete.
fn transient(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::UnexpectedEof
    )
}

fn code(r: &Response) -> Option<ErrorCode> {
    serde_json::from_slice::<ErrorBody>(&r.body)
        .ok()
        .map(|b| b.code)
}

fn code_name(r: &Response) -> String {
    code(r)
        .and_then(|c| serde_json::to_value(c).ok())
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "(an unreadable error)".to_owned())
}

fn ended(e: End, err: &mut dyn Write) -> ExitCode {
    match e {
        End::Timeout => {
            say(err, "the daemon did not answer in time");
            ExitCode::Timeout
        }
        End::Fail(m) => {
            say(err, &m);
            ExitCode::Failed
        }
    }
}

fn say(err: &mut dyn Write, msg: &str) {
    let _ = writeln!(err, "chaldea: {}", diag(msg));
}

fn print<T: Serialize>(out: &mut dyn Write, v: &T) {
    if let Ok(s) = serde_json::to_string(v) {
        let _ = writeln!(out, "{s}");
    }
}

fn decode<T: serde::de::DeserializeOwned>(r: &Response) -> Result<T, End> {
    serde_json::from_slice(&r.body)
        .map_err(|e| End::Fail(format!("the daemon's result is not readable: {e}")))
}

/// A refusal's message and exit code; `death_unconfirmed` is the caller's.
fn refused(status: u16, c: Option<ErrorCode>, err: &mut dyn Write) -> ExitCode {
    let name = c
        .and_then(|c| serde_json::to_value(c).ok())
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "(an unreadable error)".to_owned());
    let msg = match c {
        Some(ErrorCode::LaunchChanged) => {
            "the thread's current launch changed; nothing was sent again".to_owned()
        }
        Some(ErrorCode::NotFound) => "no such thread or not readable".to_owned(),
        _ => format!("the daemon refused the command: {status} {name}"),
    };
    say(err, &msg);
    ExitCode::Failed
}

/// The `death_unconfirmed` lines (§2.2): the reason's line only when `summary` shows the
/// `pinned` launch exited with death unconfirmed, the general line otherwise.
fn unconfirmed(
    pinned: Option<LaunchId>,
    summary: Option<&ThreadSummary>,
    err: &mut dyn Write,
) -> ExitCode {
    let launch = summary.and_then(|s| s.launch.as_ref());
    let line = match (pinned, launch) {
        (Some(p), Some(l)) if l.launch_id == p && !l.death_confirmed => match l.exit_reason {
            Some(ExitReason::DaemonRestart | ExitReason::UnknownAfterCrash) => UNCONFIRMED_RESTART,
            Some(ExitReason::Stopped | ExitReason::Exit | ExitReason::SpawnFailed) => {
                UNCONFIRMED_STOPPED
            }
            None => UNCONFIRMED,
        },
        _ => UNCONFIRMED,
    };
    say(err, line);
    say(err, UNCONFIRMED_TAIL);
    ExitCode::Failed
}

/// How a stop's result ends the command (§2.2, SD20).
fn stop_exit(r: &StopResult, err: &mut dyn Write) -> ExitCode {
    if r.confirmed_within_deadline {
        ExitCode::Ok
    } else if r.death_confirmed {
        say(err, CONFIRMED_LATE);
        ExitCode::Timeout
    } else {
        unconfirmed(None, None, err)
    }
}

/// The prompt checks the daemon makes first, so their reason can be printed.
fn admit_prompt(agent: Option<Agent>, prompt: Option<&str>, err: &mut dyn Write) -> bool {
    let Some(text) = prompt else {
        return true;
    };
    let refuse = |err: &mut dyn Write, m: &str| {
        say(err, m);
        false
    };
    if agent == Some(Agent::Claude) {
        return refuse(
            err,
            "a Claude prompt cannot be given at spawn; start it without --prompt",
        );
    }
    if text.len() > MAX_PROMPT_BYTES {
        return refuse(
            err,
            &format!("the prompt is longer than {MAX_PROMPT_BYTES} bytes"),
        );
    }
    if let Err(r) = check(Agent::Codex, InputEncoding::Positional, Purpose::Send, text) {
        let reason = serde_json::to_value(r.reason)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let at = r.at.map(|a| format!(" at byte {a}")).unwrap_or_default();
        return refuse(err, &format!("the prompt is refused: {reason}{at}"));
    }
    true
}

fn launch_outcome(o: LaunchOutcome, err: &mut dyn Write) -> ExitCode {
    match o {
        LaunchOutcome::Started => ExitCode::Ok,
        LaunchOutcome::SpawnFailed => {
            say(err, "the agent's program could not be started");
            ExitCode::Failed
        }
    }
}

impl Client<'_> {
    fn cutoff(&self, d: Duration) -> Cell<Instant> {
        Cell::new((self.now)() + d)
    }

    pub(crate) fn spawn(
        &self,
        args: &SpawnArgs,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> ExitCode {
        if !admit_prompt(Some(args.agent), args.prompt.as_deref(), err) {
            return ExitCode::Usage;
        }
        let cutoff = self.cutoff(COMMAND_CUTOFF);
        let r = match self.command(&cutoff, "spawn", args) {
            Err(e) => return ended(e, err),
            Ok(Err((s, c))) => return refused(s, c, err),
            Ok(Ok(r)) => r,
        };
        match decode::<SpawnResult>(&r) {
            Ok(v) => {
                print(out, &v);
                launch_outcome(v.outcome, err)
            }
            Err(e) => ended(e, err),
        }
    }

    /// The current launch, read once before the command, pinned as `expected_launch_id`.
    fn pinned(&self, cutoff: &Cell<Instant>, t: ThreadId) -> Result<LaunchId, End> {
        let s = self.summary(cutoff, t)?;
        s.launch
            .map(|l| l.launch_id)
            .ok_or_else(|| End::Fail("the thread has no launch".to_owned()))
    }

    /// A `death_unconfirmed` refusal of `resume` or `close`: the summary read once more
    /// within the same cutoff decides the line.
    fn pinned_unconfirmed(
        &self,
        cutoff: &Cell<Instant>,
        t: ThreadId,
        pinned: LaunchId,
        err: &mut dyn Write,
    ) -> ExitCode {
        let s = self.summary(cutoff, t).ok();
        unconfirmed(Some(pinned), s.as_ref(), err)
    }

    pub(crate) fn resume(
        &self,
        t: ThreadId,
        prompt: Option<String>,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> ExitCode {
        if !admit_prompt(None, prompt.as_deref(), err) {
            return ExitCode::Usage;
        }
        let cutoff = self.cutoff(COMMAND_CUTOFF);
        let l = match self.pinned(&cutoff, t) {
            Ok(l) => l,
            Err(e) => return ended(e, err),
        };
        let args = ResumeArgs {
            thread_id: t,
            expected_launch_id: l,
            prompt,
        };
        let r = match self.command(&cutoff, "resume", &args) {
            Err(e) => return ended(e, err),
            Ok(Err((_, Some(ErrorCode::DeathUnconfirmed)))) => {
                return self.pinned_unconfirmed(&cutoff, t, l, err);
            }
            Ok(Err((s, c))) => return refused(s, c, err),
            Ok(Ok(r)) => r,
        };
        match decode::<ResumeResult>(&r) {
            Ok(v) => {
                print(out, &v);
                launch_outcome(v.outcome, err)
            }
            Err(e) => ended(e, err),
        }
    }

    pub(crate) fn stop(&self, t: ThreadId, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
        let cutoff = self.cutoff(COMMAND_CUTOFF);
        let args = StopArgs {
            thread_id: t,
            expected_launch_id: None,
        };
        let r = match self.command(&cutoff, "stop", &args) {
            Err(e) => return ended(e, err),
            Ok(Err((_, Some(ErrorCode::DeathUnconfirmed)))) => {
                return unconfirmed(None, None, err);
            }
            Ok(Err((s, c))) => return refused(s, c, err),
            Ok(Ok(r)) => r,
        };
        match decode::<StopResult>(&r) {
            Ok(v) => {
                print(out, &v);
                stop_exit(&v, err)
            }
            Err(e) => ended(e, err),
        }
    }

    pub(crate) fn close(
        &self,
        t: ThreadId,
        stop: bool,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> ExitCode {
        let cutoff = self.cutoff(COMMAND_CUTOFF);
        let l = match self.pinned(&cutoff, t) {
            Ok(l) => l,
            Err(e) => return ended(e, err),
        };
        let args = CloseArgs {
            thread_id: t,
            expected_launch_id: l,
            stop,
        };
        let r = match self.command(&cutoff, "close", &args) {
            Err(e) => return ended(e, err),
            Ok(Err((_, Some(ErrorCode::DeathUnconfirmed)))) => {
                return self.pinned_unconfirmed(&cutoff, t, l, err);
            }
            Ok(Err((s, c))) => return refused(s, c, err),
            Ok(Ok(r)) => r,
        };
        match decode::<CloseResult>(&r) {
            Ok(v) => {
                print(out, &v);
                v.stop.as_ref().map_or(ExitCode::Ok, |s| stop_exit(s, err))
            }
            Err(e) => ended(e, err),
        }
    }

    /// `chaldea wait` (SD16): polls the summary until `until` holds, against one cutoff.
    pub(crate) fn wait(
        &self,
        t: ThreadId,
        until: Until,
        timeout: Duration,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> ExitCode {
        let cutoff = self.cutoff(timeout);
        let cond = |s: &ThreadSummary| {
            let exited = s.process_state == ProcessState::Exited;
            let holds = match until {
                Until::Exited => exited,
                Until::Idle => {
                    s.process_state == ProcessState::Running && s.turn_state == TurnState::Idle
                }
                Until::Attention => s.attention_count > 0,
            };
            if holds {
                Some(Ok(()))
            } else if exited {
                Some(Err(End::Fail("the launch exited".to_owned())))
            } else {
                None
            }
        };
        match self.summary_until(&cutoff, t, false, &cond) {
            Ok(s) => {
                print(out, &s);
                ExitCode::Ok
            }
            Err(e) => ended(e, err),
        }
    }
}

/// Parses `--timeout`: a whole number with `ms`, `s`, `m`, or `h` (seconds when bare), no
/// longer than an `Instant` can reach from now.
///
/// # Errors
///
/// The message clap prints.
pub(crate) fn duration(s: &str) -> Result<Duration, String> {
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (n, unit) = s.split_at(split);
    let n: u64 = n
        .parse()
        .map_err(|_| format!("`{s}` is not a duration such as 30s, 10m, or 500ms"))?;
    let d = match unit {
        "ms" => Duration::from_millis(n),
        "" | "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n.saturating_mul(60)),
        "h" => Duration::from_secs(n.saturating_mul(3600)),
        _ => return Err(format!("`{s}` has an unknown unit (ms, s, m, or h)")),
    };
    if d.is_zero() {
        return Err("the timeout must be above zero".to_owned());
    }
    // The wait's cutoff is an `Instant` this far ahead, which must exist.
    if Instant::now().checked_add(d).is_none() {
        return Err(format!("`{s}` is too long a timeout"));
    }
    Ok(d)
}

/// Parses a thread id.
///
/// # Errors
///
/// The message clap prints.
pub(crate) fn thread_id(s: &str) -> Result<ThreadId, String> {
    uuid::Uuid::parse_str(s)
        .map(ThreadId)
        .map_err(|_| format!("`{s}` is not a thread id"))
}

/// The client of the daemon serving `dir`, or the exit after printing why there is none.
fn connect<'a>(
    dir: &Path,
    discover: &'a dyn Fn() -> Result<Found, String>,
    now: &'a dyn Fn() -> Instant,
) -> Result<Client<'a>, ExitCode> {
    match client::read_owner_token(dir) {
        Ok(token) => Ok(Client {
            discover,
            token,
            now,
            period: PERIOD,
        }),
        Err(m) => {
            say(&mut io::stderr(), &m);
            Err(ExitCode::Failed)
        }
    }
}

/// What the command line asked for.
pub(crate) enum Verb {
    Spawn {
        agent: Agent,
        cwd: Option<PathBuf>,
        prompt: Option<String>,
        task: Option<String>,
    },
    Resume {
        thread: ThreadId,
        prompt: Option<String>,
    },
    Stop {
        thread: ThreadId,
    },
    Close {
        thread: ThreadId,
        stop: bool,
    },
    Wait {
        thread: ThreadId,
        until: Until,
        timeout: Duration,
    },
}

/// Runs `verb` against the daemon serving `dir`.
pub(crate) fn run(dir: &Path, verb: Verb) -> ExitCode {
    let discover = || found(client::discover(dir));
    let now = Instant::now;
    let c = match connect(dir, &discover, &now) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let (out, err) = (&mut io::stdout(), &mut io::stderr());
    match verb {
        Verb::Spawn {
            agent,
            cwd,
            prompt,
            task,
        } => {
            let cwd = match cwd.map_or_else(std::env::current_dir, std::path::absolute) {
                Ok(p) => p,
                Err(e) => {
                    say(err, &format!("the working directory cannot be read: {e}"));
                    return ExitCode::Failed;
                }
            };
            let Some(cwd) = cwd.to_str().map(str::to_owned) else {
                say(err, "the working directory's path is not UTF-8");
                return ExitCode::Failed;
            };
            c.spawn(
                &SpawnArgs {
                    agent,
                    cwd,
                    prompt,
                    task,
                },
                out,
                err,
            )
        }
        Verb::Resume { thread, prompt } => c.resume(thread, prompt, out, err),
        Verb::Stop { thread } => c.stop(thread, out, err),
        Verb::Close { thread, stop } => c.close(thread, stop, out, err),
        Verb::Wait {
            thread,
            until,
            timeout,
        } => c.wait(thread, until, timeout, out, err),
    }
}

#[cfg(test)]
mod tests;
