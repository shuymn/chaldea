//! `chaldea spawn`, `resume`, `stop`, `close`, and `wait` (docs/design/supervisor.md §2.2,
//! SD16), and `send`, `ready`, `key`, `input resolve`, and `input cancel`
//! (docs/design/supervisor-send.md §2.2): each command is one `POST /api/commands/{name}` with one
//! `command_id`, sent again unchanged on a transport failure, `408`, `503`, or `409 in_progress`
//! until one cutoff; the reads are `GET /api/threads/{thread_id}` under the same rules.
//!
//! `inspect` (C1 D2) sends no command: it reads the thread's snapshot with one
//! `GET /api/events`, its summary only when the snapshot cannot be read, then
//! `GET /api/threads/{thread_id}/screen`.

use std::cell::Cell;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chaldea_core::command::{
    CloseArgs, CloseResult, InputCancelArgs, InputCommandResult, InputResolveArgs, KeyArgs,
    KeyOutcome, KeyResult, LaunchOutcome, ReadyArgs, ReadyResult, ResumeArgs, ResumeResult,
    SendArgs, SendResult, SpawnArgs, SpawnResult, StopArgs, StopOutcome, StopResult,
};
use chaldea_core::http::{ErrorBody, ErrorCode};
use chaldea_core::ids::{Agent, ApprovalId, CommandId, InputId, LaunchId, ThreadId};
use chaldea_core::input::encode::{Purpose, check};
use chaldea_core::input::key::KeyToken;
use chaldea_core::input::{MAX_PROMPT_BYTES, RejectReason};
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
/// The tail of the `death_unconfirmed` lines; `<thread>` is the thread's id.
const UNCONFIRMED_TAIL: &str = "To review what was checked, run `chaldea stop <thread> --assume-dead`; once you have made sure its processes ended, confirm with `--launch <id>`. Its history is kept, and new threads can be started.";
/// The line of a stop whose death was confirmed after its 5 s, counted from the attempt the
/// daemon accepted.
const CONFIRMED_LATE: &str = "The process ended, but its end was not confirmed within 5 s of the stop request the daemon accepted.";
/// The line of an `already_exited` stop not confirmed within its 5 s: the result does not say
/// whether the owner's assertion confirmed it.
const ALREADY_EXITED_LATE: &str = "The process had already ended, but this stop did not confirm its end within 5 s (its end may have been confirmed by the owner's assertion).";

/// Why a read bound to one run ended when discovery could not re-check that run.
pub(crate) const UNVERIFIED: &str = "the daemon could not be found serving again after the read";

/// Where the daemon is, as one attempt sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Found {
    /// Serving: its port and its run's `instance`.
    Port(u16, String),
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
            .map(|p| Found::Port(p, r.instance))
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
    /// An error a test injects in place of the request to this path, at the request's
    /// boundary (unit tests only).
    #[cfg(test)]
    pub fault: &'a dyn Fn(&str) -> Option<io::ErrorKind>,
}

/// Why an exchange ended without a response to adopt.
#[derive(Debug)]
pub(crate) enum End {
    /// The cutoff passed, or a request ended `TimedOut` at it.
    Timeout,
    /// The message to print.
    Fail(String),
    /// A response body over the client's limit (`client::is_body_too_large`).
    TooLarge,
    /// A request bound to one daemon run found another run serving.
    Restarted,
    /// A response read from the bound run, after which discovery found no daemon running.
    Stopped,
    /// A response read from the bound run, after which discovery could not tell whether it
    /// still serves (starting, stopping, or a record not readable yet).
    Unverified,
}

/// Why a command ended without its result.
#[derive(Debug)]
enum CommandEnd {
    Ended(End),
    /// The refusal's status and code.
    Refused(u16, Option<ErrorCode>),
}

impl From<End> for CommandEnd {
    fn from(e: End) -> Self {
        CommandEnd::Ended(e)
    }
}

/// A request to send, the same bytes on every attempt.
enum Req<'a> {
    Get(&'a str),
    /// A `GET` whose response body may be up to this many bytes.
    GetUpTo(&'a str, usize),
    Post(&'a str, &'a [u8]),
}

impl Client<'_> {
    /// Sends `req` until `decide` takes a response, the cutoff, or a failure; `decide`
    /// returns `None` to send it again. With `daemon_required`, no daemon is a failure, else
    /// polled. A response read at or after the cutoff is not adopted.
    fn exchange<T>(
        &self,
        cutoff: Instant,
        req: &Req<'_>,
        daemon_required: bool,
        decide: &mut dyn FnMut(Response) -> Option<Result<T, End>>,
    ) -> Result<T, End> {
        self.exchange_in(cutoff, req, daemon_required, None, decide)
    }

    /// [`Client::exchange`] bound to the daemon run `run` when given: every attempt, a retry
    /// included, is sent only to that run, and a response is adopted only when discovery still
    /// names that run after it was read; another run serving ends the exchange `Restarted`,
    /// and none serving after a response ends it `Stopped`.
    fn exchange_in<T>(
        &self,
        cutoff: Instant,
        req: &Req<'_>,
        daemon_required: bool,
        run: Option<&str>,
        decide: &mut dyn FnMut(Response) -> Option<Result<T, End>>,
    ) -> Result<T, End> {
        self.exchange_noting(cutoff, req, daemon_required, run, &|_| {}, decide)
    }

    /// [`Client::exchange_in`] that shows `note` each transport failure it sends the request
    /// again after.
    fn exchange_noting<T>(
        &self,
        cutoff: Instant,
        req: &Req<'_>,
        daemon_required: bool,
        run: Option<&str>,
        note: &dyn Fn(&io::Error),
        decide: &mut dyn FnMut(Response) -> Option<Result<T, End>>,
    ) -> Result<T, End> {
        let same_run = |found: &Result<Found, String>| match (run, found) {
            (None, _) => true,
            (Some(r), Ok(Found::Port(_, i))) => i == r,
            (Some(_), _) => false,
        };
        let seen = poll_until_with(&Cell::new(cutoff), self.period, self.now, || {
            let found = (self.discover)();
            if run.is_some() && matches!(found, Ok(Found::Port(..))) && !same_run(&found) {
                return Some(Err(End::Restarted));
            }
            let port = match found {
                Ok(Found::Port(p, _)) => p,
                Ok(Found::NotRunning) if daemon_required => {
                    return Some(Err(End::Fail(
                        "daemon not running; run `chaldea daemon start`".to_owned(),
                    )));
                }
                Ok(Found::NotRunning | Found::NotYet) => return None,
                Err(m) => return Some(Err(End::Fail(m))),
            };
            let token = Some(self.token.as_str());
            let send = || match req {
                Req::Get(path) => client::request(port, "GET", path, token, cutoff),
                Req::GetUpTo(path, max) => {
                    client::request_up_to(port, "GET", path, token, cutoff, *max)
                }
                Req::Post(path, body) => client::post_json(port, path, token, body, cutoff),
            };
            #[cfg(test)]
            let send = || {
                let (Req::Get(path) | Req::GetUpTo(path, _) | Req::Post(path, _)) = req;
                (self.fault)(path).map_or_else(send, |kind| Err(io::Error::from(kind)))
            };
            let r = send();
            match r {
                Err(e) if e.kind() == io::ErrorKind::TimedOut => Some(Err(End::Timeout)),
                Err(e) if client::is_body_too_large(&e) => Some(Err(End::TooLarge)),
                Err(e) if transient(e.kind()) => {
                    note(&e);
                    None
                }
                Err(e) => Some(Err(End::Fail(format!("the request failed: {e}")))),
                Ok(r) => {
                    let decided = decide(r);
                    if run.is_some() && matches!(decided, Some(Ok(_))) {
                        match (self.discover)() {
                            Ok(Found::NotRunning) => return Some(Err(End::Stopped)),
                            Ok(Found::NotYet) => return Some(Err(End::Unverified)),
                            Err(m) => return Some(Err(End::Fail(m))),
                            found if !same_run(&found) => return Some(Err(End::Restarted)),
                            Ok(Found::Port(..)) => {}
                        }
                    }
                    decided
                }
            }
        });
        seen.unwrap_or(Err(End::Timeout))
    }

    /// `GET /api/threads/{t}` until `cond` holds of a summary or `decide` ends the read,
    /// under SD16's rules.
    fn summary_until(
        &self,
        cutoff: Instant,
        t: ThreadId,
        daemon_required: bool,
        run: Option<&str>,
        cond: &dyn Fn(&ThreadSummary) -> Option<Result<(), End>>,
    ) -> Result<ThreadSummary, End> {
        let path = format!("/api/threads/{t}");
        self.exchange_in(
            cutoff,
            &Req::Get(&path),
            daemon_required,
            run,
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
    fn summary(&self, cutoff: Instant, t: ThreadId) -> Result<ThreadSummary, End> {
        self.summary_until(cutoff, t, true, None, &|_| Some(Ok(())))
    }

    /// One command: its decoded result, or why it ended without one.
    fn command<A: Serialize, T: serde::de::DeserializeOwned>(
        &self,
        cutoff: Instant,
        name: &str,
        args: &A,
    ) -> Result<T, CommandEnd> {
        let body = serde_json::to_vec(&serde_json::json!({
            "command_id": CommandId(uuid::Uuid::new_v4()),
            "args": args,
        }))
        .map_err(|e| End::Fail(format!("the command could not be encoded: {e}")))?;
        let path = format!("/api/commands/{name}");
        let r = self.exchange(cutoff, &Req::Post(&path, &body), true, &mut |r| {
            let again = matches!(r.status, 408 | 503)
                || (r.status == 409 && code(&r) == Some(ErrorCode::InProgress));
            match r.status {
                _ if again => None,
                200 => Some(Ok(Ok(r))),
                s => Some(Ok(Err((s, code(&r))))),
            }
        })?;
        match r {
            Ok(r) => Ok(decode(&r)?),
            Err((s, c)) => Err(CommandEnd::Refused(s, c)),
        }
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

/// A wire value's text: a string as itself, any other value as JSON.
fn wire<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
        .unwrap_or_default()
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
        End::TooLarge => {
            say(err, "the daemon's response is larger than the client reads");
            ExitCode::Failed
        }
        End::Restarted => {
            say(err, "the daemon restarted during the read");
            ExitCode::Failed
        }
        End::Stopped => {
            say(err, "the daemon stopped during the read");
            ExitCode::Failed
        }
        End::Unverified => {
            say(err, UNVERIFIED);
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

/// A command's end without its result; `death_unconfirmed` is the caller's.
fn not_done(e: CommandEnd, err: &mut dyn Write) -> ExitCode {
    match e {
        CommandEnd::Ended(e) => ended(e, err),
        CommandEnd::Refused(s, c) => refused(s, c, err),
    }
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
    t: ThreadId,
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
    say(err, &UNCONFIRMED_TAIL.replace("<thread>", &t.to_string()));
    ExitCode::Failed
}

/// How a stop's result ends the command (§2.2, SD20; `docs/design/supervisor-recovery.md`
/// §2.5 for `asserted` and an `already_exited` not confirmed in time).
fn stop_exit(t: ThreadId, r: &StopResult, err: &mut dyn Write) -> ExitCode {
    if r.outcome == StopOutcome::Asserted {
        say(
            err,
            &format!("The owner confirmed that launch {} has ended.", r.launch_id),
        );
        ExitCode::Ok
    } else if r.confirmed_within_deadline {
        ExitCode::Ok
    } else if r.death_confirmed {
        let line = if r.outcome == StopOutcome::AlreadyExited {
            ALREADY_EXITED_LATE
        } else {
            CONFIRMED_LATE
        };
        say(err, line);
        ExitCode::Timeout
    } else {
        unconfirmed(t, None, None, err)
    }
}

/// Why Claude's `lossy_text` refuses a prompt (C1 D16).
const CLAUDE_COMMAND: &str = "Claude runs a prompt that starts with `!` as a shell command and \
one that starts with `/` (after leading white space) as a slash command, so it is not sent; \
type it through attach";

/// The D16 explanation where the thread's agent is not known (`send`, `resume`): named for a
/// Claude thread, printed when the shared check refuses `text` for Claude as `lossy_text`, so
/// an earlier rule (a control character) takes precedence as it does at `spawn --prompt`.
fn claude_command_hint(text: &str, err: &mut dyn Write) {
    if check(
        Agent::Claude,
        InputEncoding::BracketedPaste,
        Purpose::Send,
        text,
    )
    .is_err_and(|r| r.reason == RejectReason::LossyText)
    {
        say(err, &format!("for a Claude thread: {CLAUDE_COMMAND}"));
    }
}

/// The prompt checks the daemon makes first, so their reason can be printed: the size, and,
/// when the agent is known, `check` in its first prompt's encoding (Codex's positional
/// argument, Claude's queued input). A resumed thread's agent is the daemon's to check.
fn admit_prompt(agent: Option<Agent>, prompt: Option<&str>, err: &mut dyn Write) -> bool {
    let Some(text) = prompt else {
        return true;
    };
    if text.len() > MAX_PROMPT_BYTES {
        say(
            err,
            &format!("the prompt is longer than {MAX_PROMPT_BYTES} bytes"),
        );
        return false;
    }
    let encoding = match agent {
        Some(Agent::Codex) => InputEncoding::Positional,
        Some(Agent::Claude) => InputEncoding::BracketedPaste,
        None => return true,
    };
    if let Some(a) = agent
        && let Err(r) = check(a, encoding, Purpose::Send, text)
    {
        let reason = serde_json::to_value(r.reason)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let at = r.at.map(|a| format!(" at byte {a}")).unwrap_or_default();
        say(err, &format!("the prompt is refused: {reason}{at}"));
        if a == Agent::Claude && r.reason == RejectReason::LossyText {
            say(err, CLAUDE_COMMAND);
        }
        return false;
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
    fn cutoff(&self, d: Duration) -> Instant {
        (self.now)() + d
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
        match self.command::<_, SpawnResult>(cutoff, "spawn", args) {
            Ok(v) => {
                print(out, &v);
                launch_outcome(v.outcome, err)
            }
            Err(e) => not_done(e, err),
        }
    }

    /// The launch a command pins as `expected_launch_id` (or `key`'s `launch_id`): `given`
    /// (`--launch`), else the current launch, read once before the command.
    fn pinned(
        &self,
        cutoff: Instant,
        t: ThreadId,
        given: Option<LaunchId>,
    ) -> Result<LaunchId, End> {
        if let Some(l) = given {
            return Ok(l);
        }
        let s = self.summary(cutoff, t)?;
        s.launch
            .map(|l| l.launch_id)
            .ok_or_else(|| End::Fail("the thread has no launch".to_owned()))
    }

    /// A `death_unconfirmed` refusal of `resume` or `close`: the summary read once more
    /// within the same cutoff decides the line.
    fn pinned_unconfirmed(
        &self,
        cutoff: Instant,
        t: ThreadId,
        pinned: LaunchId,
        err: &mut dyn Write,
    ) -> ExitCode {
        let s = self.summary(cutoff, t).ok();
        unconfirmed(t, Some(pinned), s.as_ref(), err)
    }

    pub(crate) fn resume(
        &self,
        t: ThreadId,
        prompt: Option<String>,
        launch: Option<LaunchId>,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> ExitCode {
        if !admit_prompt(None, prompt.as_deref(), err) {
            return ExitCode::Usage;
        }
        let cutoff = self.cutoff(COMMAND_CUTOFF);
        let l = match self.pinned(cutoff, t, launch) {
            Ok(l) => l,
            Err(e) => return ended(e, err),
        };
        let args = ResumeArgs {
            thread_id: t,
            expected_launch_id: l,
            prompt,
        };
        match self.command::<_, ResumeResult>(cutoff, "resume", &args) {
            Ok(v) => {
                print(out, &v);
                launch_outcome(v.outcome, err)
            }
            Err(CommandEnd::Refused(_, Some(ErrorCode::DeathUnconfirmed))) => {
                self.pinned_unconfirmed(cutoff, t, l, err)
            }
            Err(e @ CommandEnd::Refused(_, Some(ErrorCode::PromptRejected))) => {
                let code = not_done(e, err);
                if let Some(p) = args.prompt.as_deref() {
                    claude_command_hint(p, err);
                }
                code
            }
            Err(e) => not_done(e, err),
        }
    }

    /// `chaldea stop`, of launch `launch` only when given; with `assume_dead`, the owner's
    /// assertion for launch `launch` (`docs/design/supervisor-recovery.md` §2.5).
    pub(crate) fn stop(
        &self,
        t: ThreadId,
        launch: Option<LaunchId>,
        assume_dead: bool,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> ExitCode {
        let cutoff = self.cutoff(COMMAND_CUTOFF);
        let args = StopArgs {
            thread_id: t,
            expected_launch_id: launch,
            assume_dead,
        };
        match self.command::<_, StopResult>(cutoff, "stop", &args) {
            Ok(v) => {
                print(out, &v);
                stop_exit(t, &v, err)
            }
            Err(CommandEnd::Refused(_, Some(ErrorCode::DeathUnconfirmed))) => {
                unconfirmed(t, None, None, err)
            }
            Err(e) => not_done(e, err),
        }
    }

    pub(crate) fn close(
        &self,
        t: ThreadId,
        stop: bool,
        launch: Option<LaunchId>,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> ExitCode {
        let cutoff = self.cutoff(COMMAND_CUTOFF);
        let l = match self.pinned(cutoff, t, launch) {
            Ok(l) => l,
            Err(e) => return ended(e, err),
        };
        let args = CloseArgs {
            thread_id: t,
            expected_launch_id: l,
            stop,
        };
        match self.command::<_, CloseResult>(cutoff, "close", &args) {
            Ok(v) => {
                print(out, &v);
                v.stop
                    .as_ref()
                    .map_or(ExitCode::Ok, |s| stop_exit(t, s, err))
            }
            Err(CommandEnd::Refused(_, Some(ErrorCode::DeathUnconfirmed))) => {
                self.pinned_unconfirmed(cutoff, t, l, err)
            }
            Err(e) => not_done(e, err),
        }
    }

    /// `chaldea send`: the input and its state; a rejected text exits 1 with its reason.
    pub(crate) fn send(
        &self,
        t: ThreadId,
        text: String,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> ExitCode {
        if text.len() > MAX_PROMPT_BYTES {
            say(
                err,
                &format!("the prompt is longer than {MAX_PROMPT_BYTES} bytes"),
            );
            return ExitCode::Failed;
        }
        let cutoff = self.cutoff(COMMAND_CUTOFF);
        let args = SendArgs {
            thread_id: t,
            text,
            origin: None,
        };
        match self.command::<_, SendResult>(cutoff, "send", &args) {
            Ok(v) => {
                print(out, &v);
                if let Some(r) = v.reject_reason {
                    let reason = serde_json::to_value(r)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default();
                    say(err, &format!("the prompt is refused: {reason}"));
                    if r == RejectReason::LossyText {
                        claude_command_hint(&args.text, err);
                    }
                    return ExitCode::Failed;
                }
                ExitCode::Ok
            }
            Err(e) => not_done(e, err),
        }
    }

    /// `chaldea ready`, pinned to `launch` when given, else to the current launch.
    pub(crate) fn ready(
        &self,
        t: ThreadId,
        launch: Option<LaunchId>,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> ExitCode {
        let cutoff = self.cutoff(COMMAND_CUTOFF);
        let l = match self.pinned(cutoff, t, launch) {
            Ok(l) => l,
            Err(e) => return ended(e, err),
        };
        let args = ReadyArgs {
            thread_id: t,
            expected_launch_id: Some(l),
        };
        match self.command::<_, ReadyResult>(cutoff, "ready", &args) {
            Ok(v) => {
                print(out, &v);
                ExitCode::Ok
            }
            Err(e) => not_done(e, err),
        }
    }

    /// `chaldea key`, to `launch` when given, else to the current launch: 0 once every byte
    /// was written; 1 for `incomplete` or `pending`, whose bytes may or may not have reached
    /// the CLI.
    pub(crate) fn key(
        &self,
        t: ThreadId,
        keys: Vec<KeyToken>,
        approval: Option<ApprovalId>,
        launch: Option<LaunchId>,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> ExitCode {
        let cutoff = self.cutoff(COMMAND_CUTOFF);
        let l = match self.pinned(cutoff, t, launch) {
            Ok(l) => l,
            Err(e) => return ended(e, err),
        };
        let args = KeyArgs {
            thread_id: t,
            launch_id: l,
            keys,
            approval_id: approval,
        };
        match self.command::<_, KeyResult>(cutoff, "key", &args) {
            Ok(v) => {
                print(out, &v);
                match v.outcome {
                    KeyOutcome::Written => ExitCode::Ok,
                    KeyOutcome::Incomplete => {
                        say(err, "only part of the keys was written");
                        ExitCode::Failed
                    }
                    KeyOutcome::Pending => {
                        say(err, "the keys were not confirmed written in time");
                        ExitCode::Failed
                    }
                }
            }
            Err(e) => not_done(e, err),
        }
    }

    /// `chaldea input resolve` and `chaldea input cancel`.
    pub(crate) fn input(
        &self,
        name: &str,
        args: &impl Serialize,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> ExitCode {
        let cutoff = self.cutoff(COMMAND_CUTOFF);
        match self.command::<_, InputCommandResult>(cutoff, name, args) {
            Ok(v) => {
                print(out, &v);
                ExitCode::Ok
            }
            Err(e) => not_done(e, err),
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
        // Checked again here, where the cutoff is built: the parse checked an earlier now.
        let Some(at) = (self.now)().checked_add(timeout) else {
            say(err, "the timeout is too long");
            return ExitCode::Usage;
        };
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
        match self.summary_until(at, t, false, None, &cond) {
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
/// A key name (`enter`, `ctrl_c`, ...) or one printable character.
pub(crate) fn key_token(s: &str) -> Result<KeyToken, String> {
    if let Ok(k) = serde_json::from_value::<KeyToken>(serde_json::Value::String(s.to_owned())) {
        return Ok(k);
    }
    let mut cs = s.chars();
    match (cs.next(), cs.next()) {
        (Some(c), None) if (' '..='~').contains(&c) => Ok(KeyToken::Char(c)),
        _ => Err(format!("not a key: {s}")),
    }
}

pub(crate) fn approval_id(s: &str) -> Result<ApprovalId, String> {
    uuid::Uuid::parse_str(s)
        .map(ApprovalId)
        .map_err(|e| format!("not an approval id: {e}"))
}

pub(crate) fn input_id(s: &str) -> Result<InputId, String> {
    uuid::Uuid::parse_str(s)
        .map(InputId)
        .map_err(|e| format!("not an input id: {e}"))
}

pub(crate) fn launch_id(s: &str) -> Result<LaunchId, String> {
    uuid::Uuid::parse_str(s)
        .map(LaunchId)
        .map_err(|_| format!("`{s}` is not a launch id"))
}

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
            #[cfg(test)]
            fault: &|_| None,
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
        launch: Option<LaunchId>,
    },
    Send {
        thread: ThreadId,
        text: String,
    },
    Ready {
        thread: ThreadId,
        launch: Option<LaunchId>,
    },
    Key {
        thread: ThreadId,
        keys: Vec<KeyToken>,
        approval: Option<ApprovalId>,
        launch: Option<LaunchId>,
    },
    InputResolve {
        input: InputId,
        delivered: bool,
    },
    InputCancel {
        input: InputId,
    },
    /// A plain stop, of launch `launch` only when given.
    Stop {
        thread: ThreadId,
        launch: Option<LaunchId>,
    },
    /// `stop --assume-dead`: the preview without a launch, the assertion with one.
    AssumeDead {
        thread: ThreadId,
        launch: Option<LaunchId>,
    },
    Close {
        thread: ThreadId,
        stop: bool,
        launch: Option<LaunchId>,
    },
    Wait {
        thread: ThreadId,
        until: Until,
        timeout: Duration,
    },
    Inspect {
        thread: ThreadId,
        json: bool,
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
        Verb::Resume {
            thread,
            prompt,
            launch,
        } => c.resume(thread, prompt, launch, out, err),
        Verb::Send { thread, text } => c.send(thread, text, out, err),
        Verb::Ready { thread, launch } => c.ready(thread, launch, out, err),
        Verb::Key {
            thread,
            keys,
            approval,
            launch,
        } => c.key(thread, keys, approval, launch, out, err),
        Verb::InputResolve { input, delivered } => c.input(
            "input_resolve",
            &InputResolveArgs {
                input_id: input,
                delivered,
            },
            out,
            err,
        ),
        Verb::InputCancel { input } => c.input(
            "input_cancel",
            &InputCancelArgs { input_id: input },
            out,
            err,
        ),
        Verb::Stop { thread, launch } => c.stop(thread, launch, false, out, err),
        Verb::AssumeDead {
            thread,
            launch: Some(l),
        } => c.stop(thread, Some(l), true, out, err),
        Verb::AssumeDead {
            thread,
            launch: None,
        } => c.preview(thread, out, err),
        Verb::Close {
            thread,
            stop,
            launch,
        } => c.close(thread, stop, launch, out, err),
        Verb::Wait {
            thread,
            until,
            timeout,
        } => c.wait(thread, until, timeout, out, err),
        Verb::Inspect { thread, json } => c.inspect(thread, json, out, err),
    }
}

mod inspect;
mod preview;

#[cfg(test)]
mod tests;
