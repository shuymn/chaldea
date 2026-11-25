//! Commands and receipts (design §2.8): the envelope, the duplicate-member scan, request
//! identity, the dispatch seam (`CommandHandler`, `CommandRegistry`), and the
//! `CommandRunner`, the one registered task that owns a command after the guard.
//!
//! The request reads and checks the body, prepares the handler's intent, and registers the
//! runner; from then on it only watches the runner's progress until
//! `deadline + write_reply_grace`. The runner submits the intent job (token check,
//! `receipt_begin`, the intent, and for a completed intent `receipt_complete`, in one
//! transaction), runs a pending intent's effect once, and completes its receipt.

use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chaldea_core::auth::{CommandSpec, MVP_COMMANDS};
use chaldea_core::canonical::CanonicalJson;
use chaldea_core::command::{CommandResult, ReceiptLookup, request_identity};
use chaldea_core::http::{CommandRequest, ErrorBody, ErrorCode};
use chaldea_core::ids::CommandId;
use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::value::RawValue;
use tokio::sync::watch;

use super::auth::{Authorized, verify_opt_in_job};
use super::ctx::{AfterOk, Job, RequestStart, WriteCtx, run_write};
use super::error::{ApiError, WriteFailure, refused, write_failure};
use super::guard::path_param;
use super::registry::TaskKind;
use super::{MonotonicClock, Shared, StoreFatal};
use crate::store::{JobError, StoreError, SubmitError, WriteError, WriteTx};

/// The command route's path template.
pub(crate) const COMMAND_PATH: &str = "/api/commands/{name}";

/// How long after its first attempt a receipt completion may still admit an attempt.
pub(crate) const COMPLETION_BUDGET: Duration = Duration::from_secs(60);
/// The longest `not_after` of one completion attempt.
pub(crate) const COMPLETION_ATTEMPT: Duration = Duration::from_secs(5);
/// The first and the largest pause between completion attempts.
pub(crate) const BACKOFF_FIRST: Duration = Duration::from_millis(10);
pub(crate) const BACKOFF_MAX: Duration = Duration::from_secs(1);

// ---- the dispatch seam ----------------------------------------------------------------------

/// One command's handler (PR 10, 16, 20, 21; PR 7's tests register test-only ones).
pub trait CommandHandler: Send + Sync + 'static {
    /// The wire name; must be in the registry's command table, which gives its action.
    fn name(&self) -> &'static str;

    /// The request budget, from the connection's first head byte; `None` is the server's
    /// short deadline (5 s). Long commands state theirs (mvp §2.5).
    fn deadline(&self) -> Option<Duration> {
        None
    }

    /// Parses the duplicate-free `args` into the handler's type (400 `bad_args` when they do
    /// not fit it) and returns its canonical form and an intent closure built from data only.
    ///
    /// # Errors
    ///
    /// The refusal to answer, normally [`ApiError::bad_args`].
    fn prepare(&self, args: &RawValue) -> Result<Prepared, ApiError>;

    /// Required for a handler whose intent can return [`Intent::Pending`]; runs on the
    /// runner after the intent committed.
    fn effect(&self) -> Option<&dyn CommandEffect> {
        None
    }

    /// Whether the intent can revoke a token: the runner then passes the revocation nudge as
    /// the intent job's `after_ok`, so sockets re-check their tokens once it committed
    /// (design §2.4). A nudge after a commit that revoked nothing (a refusal's replay) is
    /// harmless: the nudge carries no decision.
    fn revokes(&self) -> bool {
        false
    }
}

/// A command's effect, run once by the runner after its intent committed.
pub trait CommandEffect: Send + Sync {
    /// Resolves to how the effect ended. `Done` carries the command's terminal result, a
    /// handler reply (design §2.8 "Handler replies": a JSON success other than 204 and 205,
    /// or an `ErrorBody` whose code a handler may use, at that code's status; otherwise the
    /// runner stores and answers 500 `internal`). The owning PR states the bound of each
    /// stage of its effect, and guarantees that a result it returns for work that did not
    /// happen (a refusal, or work that ran out of its time) means the work can no longer
    /// happen later. Work that ran out of its time is 504 with a code the owning PR adds to
    /// `ErrorCode` and classifies in `handler_code`, never 408: 408 says the request's
    /// deadline kept its intent from committing. The runner adds no timeout of its own. An
    /// effect may hand its work to a task its component owns, which outlives the runner,
    /// and await that task's result.
    fn run(&self, run: EffectRun) -> BoxFuture<'static, EffectOutcome>;
}

/// How a command's effect ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectOutcome {
    /// The terminal result: the runner completes the receipt with it and replies.
    Done(CommandResult),
    /// A job of the effect could not be recorded while the daemon runs: the receipt stays
    /// `in_progress` (it becomes `unknown_after_crash` at the next start), a waiting request
    /// gets 503 `outcome_unknown`, and the runner reports `ReceiptUncompleted`.
    Unrecorded,
    /// The effect stopped because the daemon is shutting down (admission closed, or the
    /// store refused a write as `Closed`): answered as `Unrecorded` on the wire, with no
    /// fatal report.
    ShuttingDown,
}

/// How a handler may reply with a code: at `status`, with `retry_after` exactly when
/// `retry_after` is true (omitted otherwise, never `null`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HandlerCode {
    status: StatusCode,
    retry_after: bool,
}

/// How a handler may reply with `code` (design §2.8 "Handler replies"), or `None` for a
/// code only the runner, the guard, or the store gives. One arm per code and no wildcard:
/// a new `ErrorCode` does not compile until it is classified here.
fn handler_code(code: ErrorCode) -> Option<HandlerCode> {
    let at = |status| {
        Some(HandlerCode {
            status,
            retry_after: false,
        })
    };
    match code {
        // A command's own outcome, or a refusal that says nothing happened.
        ErrorCode::BadArgs => at(StatusCode::BAD_REQUEST),
        ErrorCode::Forbidden => at(StatusCode::FORBIDDEN),
        ErrorCode::NotFound => at(StatusCode::NOT_FOUND),
        // Transient: the client retries after `retry_after`.
        ErrorCode::Busy => Some(HandlerCode {
            status: StatusCode::SERVICE_UNAVAILABLE,
            retry_after: true,
        }),
        // The daemon is stopping: no retry time, as every other `shutting_down` reply.
        ErrorCode::ShuttingDown => at(StatusCode::SERVICE_UNAVAILABLE),
        ErrorCode::Internal => at(StatusCode::INTERNAL_SERVER_ERROR),
        // The launch commands' refusals (supervisor §2.2): nothing happened, and the work
        // cannot happen later.
        ErrorCode::ThreadClosed
        | ErrorCode::LaunchChanged
        | ErrorCode::LaunchLive
        | ErrorCode::DeathUnconfirmed
        | ErrorCode::WorktreeUnavailable
        | ErrorCode::NoSession => at(StatusCode::CONFLICT),
        ErrorCode::PromptRejected | ErrorCode::PromptTooLarge => at(StatusCode::BAD_REQUEST),
        // A stop that could not take its launch in time; nothing changed.
        ErrorCode::LaunchBusy => at(StatusCode::GATEWAY_TIMEOUT),
        // Routing, framing, method, and authentication: the outer layer and the guard.
        ErrorCode::MisdirectedRequest
        | ErrorCode::OriginRejected
        | ErrorCode::Unauthorized
        | ErrorCode::MethodNotAllowed
        | ErrorCode::LengthRequired
        | ErrorCode::PayloadTooLarge
        | ErrorCode::BadRequest
        | ErrorCode::BadQuery
        | ErrorCode::NotImplemented
        // The request's deadline, the receipt, and an outcome not known: the runner.
        | ErrorCode::RequestTimeout
        | ErrorCode::InProgress
        | ErrorCode::Conflict
        | ErrorCode::UnknownAfterCrash
        | ErrorCode::OutcomeUnknown
        // The store's own state.
        | ErrorCode::DiskFull
        | ErrorCode::StoreFailed => None,
    }
}

/// Whether a command result may be stored and replayed as it is. A success (2xx except 204
/// and 205, which hyper or HTTP send without the stored body) has a JSON body. Any other
/// status has an `ErrorBody` (only `code` and `retry_after`, one of each) whose code a
/// handler may use at exactly that status, with `retry_after` present exactly when the code
/// calls for it, and not 503 (a replayed result would tell a client to retry it forever).
/// Anything that does not parse is refused.
fn result_allowed(r: &CommandResult) -> bool {
    let Ok(status) = StatusCode::from_u16(r.http_status) else {
        return false;
    };
    if status.is_success() {
        return status != StatusCode::NO_CONTENT
            && status != StatusCode::RESET_CONTENT
            && serde_json::from_str::<&RawValue>(&r.body).is_ok();
    }
    let Ok(body) = serde_json::from_str::<ErrorBody>(&r.body) else {
        return false;
    };
    let Ok(members) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&r.body)
    else {
        return false;
    };
    members.keys().all(|k| k == "code" || k == "retry_after")
        && members.contains_key("retry_after") == body.retry_after.is_some()
        && status != StatusCode::SERVICE_UNAVAILABLE
        && handler_code(body.code)
            == Some(HandlerCode {
                status,
                retry_after: body.retry_after.is_some(),
            })
}

/// `r`, or 500 `internal` when a handler may not reply with it (a programming error of its
/// handler).
fn checked_result(command_id: CommandId, r: CommandResult) -> CommandResult {
    if result_allowed(&r) {
        return r;
    }
    tracing::error!(%command_id, status = r.http_status, "command result outside the handler replies");
    let body = serde_json::to_string(&ApiError::internal().body()).unwrap_or_default();
    CommandResult {
        http_status: 500,
        body,
    }
}

/// A handler's refusal (a `prepare` error, or an intent's, which the job rolls back) as
/// answered: its code must be one a handler may use, at that code's status, with
/// `retry_after` exactly when the code calls for it; any other is 500 `internal`. A
/// transient refusal keeps a code that says nothing happened (`busy`, `shutting_down`) and
/// tells the client when to retry.
fn checked_error(e: ApiError) -> ApiError {
    let reply = HandlerCode {
        status: e.status(),
        retry_after: e.retry_after().is_some(),
    };
    if handler_code(e.code()) == Some(reply) {
        return e;
    }
    tracing::error!(status = e.status().as_u16(), code = ?e.code(), "command refusal outside the handler replies");
    ApiError::internal()
}

/// An intent's refusal as the job's refusal: checked by [`checked_error`]; one that is not
/// an [`ApiError`] is 500 `internal`.
fn checked_refusal(command_id: CommandId, e: JobError) -> JobError {
    let JobError::Refused(r) = e else {
        return e;
    };
    match r.downcast::<ApiError>() {
        Ok(api) => refused(checked_error(*api)),
        Err(_) => {
            tracing::error!(%command_id, "command refusal that is not an ApiError");
            refused(ApiError::internal())
        }
    }
}

/// What an intent closure gets besides the transaction and the verified token.
#[derive(Debug)]
pub struct CommandCtx {
    command_id: CommandId,
    request_start: Instant,
    job_start: Instant,
}

impl CommandCtx {
    /// The request's `command_id` (the events a command writes carry it).
    #[must_use]
    pub fn command_id(&self) -> CommandId {
        self.command_id
    }

    /// The request's [`RequestStart`]: its connection's first head byte, on the server's
    /// clock, before the guard authenticated it.
    #[must_use]
    pub fn request_start(&self) -> Instant {
        self.request_start
    }

    /// The server's clock read in the intent job, right before the intent runs. The writer
    /// runs one job at a time, so a state the intent reads was committed before this instant.
    #[must_use]
    pub fn job_start(&self) -> Instant {
        self.job_start
    }
}

/// The intent closure: object authorization first, then the intent's writes. It runs in
/// the intent job after `receipt_begin` returned `New` and never touches receipts; a
/// refusal rolls back the whole job, the receipt row included. It captures data only
/// (store §5).
pub type IntentFn = Box<
    dyn FnOnce(&mut WriteTx<'_>, &Authorized, &mut CommandCtx) -> Result<Intent, JobError> + Send,
>;

/// A prepared command.
pub struct Prepared {
    /// The canonical form of the handler's parsed arguments; the request identity hashes it.
    pub normalized: CanonicalJson,
    pub intent: IntentFn,
}

impl fmt::Debug for Prepared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Prepared").finish_non_exhaustive()
    }
}

/// What a committed intent leaves to do.
pub enum Intent {
    /// The command finished in the intent job; its receipt completes in the same job.
    Completed(CommandResult),
    /// The effect runs on the runner after the commit; the receipt stays `in_progress`
    /// until it reports its result.
    Pending(EffectRun),
}

/// The data an intent hands to its effect.
pub struct EffectRun(Box<dyn Any + Send>);

impl EffectRun {
    pub fn new<T: Any + Send>(data: T) -> EffectRun {
        EffectRun(Box::new(data))
    }

    /// The data, if it is a `T`.
    ///
    /// # Errors
    ///
    /// `self` unchanged when it holds another type.
    pub fn downcast<T: Any>(self) -> Result<T, EffectRun> {
        self.0.downcast::<T>().map(|b| *b).map_err(EffectRun)
    }
}

impl fmt::Debug for EffectRun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EffectRun").finish_non_exhaustive()
    }
}

/// The handlers of every command name, `Unimplemented` (501) until registered.
pub struct CommandRegistry {
    table: Vec<&'static CommandSpec>,
    handlers: HashMap<&'static str, Arc<dyn CommandHandler>>,
}

impl fmt::Debug for CommandRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut names: Vec<&str> = self.handlers.keys().copied().collect();
        names.sort_unstable();
        f.debug_struct("CommandRegistry")
            .field("commands", &self.table.len())
            .field("registered", &names)
            .finish()
    }
}

impl CommandRegistry {
    /// Every command of `MVP_COMMANDS`, all `Unimplemented` (D9).
    #[must_use]
    pub fn mvp() -> CommandRegistry {
        CommandRegistry {
            table: MVP_COMMANDS.iter().collect(),
            handlers: HashMap::new(),
        }
    }

    /// `MVP_COMMANDS` plus `extra`, all `Unimplemented`: for tests, whose handlers live under
    /// test-only names. The daemon uses [`CommandRegistry::mvp`].
    ///
    /// # Panics
    ///
    /// When a name of `extra` repeats one already in the table.
    #[must_use]
    pub fn mvp_plus(extra: &'static [CommandSpec]) -> CommandRegistry {
        let mut r = CommandRegistry::mvp();
        for spec in extra {
            assert!(
                r.spec(spec.name).is_none(),
                "command {} is in the table twice",
                spec.name
            );
            r.table.push(spec);
        }
        r
    }

    /// Replaces the entry of `h.name()` with `h`.
    ///
    /// # Panics
    ///
    /// When the name is not in the table.
    pub fn register(&mut self, h: Box<dyn CommandHandler>) {
        let name = h.name();
        let spec = self
            .spec(name)
            .unwrap_or_else(|| panic!("command {name} is not in the command table"));
        self.handlers.insert(spec.name, Arc::from(h));
    }

    /// The command named `name`, with its fixed action.
    pub(crate) fn spec(&self, name: &str) -> Option<&'static CommandSpec> {
        self.table.iter().copied().find(|s| s.name == name)
    }

    /// The handler of `name`; `None` while it is unimplemented.
    pub(crate) fn handler(&self, name: &str) -> Option<Arc<dyn CommandHandler>> {
        self.handlers.get(name).cloned()
    }
}

// ---- the request ----------------------------------------------------------------------------

/// The command's budget: its handler's own deadline, or the server's short deadline (the
/// guard measures it from the request origin).
pub(crate) fn budget(shared: &Shared, path: &str) -> Duration {
    path_param(COMMAND_PATH, path, "name")
        .and_then(|name| shared.commands.handler(name))
        .and_then(|h| h.deadline())
        .unwrap_or(shared.cfg.short_deadline)
}

/// Scans the raw `args` for an object with two members of one key (after JSON unescaping),
/// at every depth; text that is not JSON fails too.
pub(crate) fn scan_duplicates(raw: &str) -> Result<(), serde_json::Error> {
    let mut de = serde_json::Deserializer::from_str(raw);
    Scan.deserialize(&mut de)?;
    de.end()
}

/// A seed that walks one JSON value and refuses an object with a repeated key.
#[derive(Clone, Copy)]
struct Scan;

impl<'de> DeserializeSeed<'de> for Scan {
    type Value = ();
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Scan {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_bool<E>(self, _v: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E>(self, _v: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E>(self, _v: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E>(self, _v: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E>(self, _v: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while seq.next_element_seed(self)?.is_some() {}
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut keys = std::collections::HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(de::Error::custom("duplicate member"));
            }
            map.next_value_seed(self)?;
        }
        Ok(())
    }
}

/// The duplicate scan of `args` (400 `bad_args`, before any conversion), the handler's
/// `prepare`, and the request identity of its normalized arguments under `name`.
pub(crate) fn prepare_args(
    handler: &dyn CommandHandler,
    name: &str,
    args: &RawValue,
) -> Result<(Prepared, [u8; 32]), ApiError> {
    scan_duplicates(args.get()).map_err(|_| ApiError::bad_args())?;
    let prepared = handler.prepare(args).map_err(checked_error)?;
    let identity = request_identity(name, &prepared.normalized);
    Ok((prepared, identity))
}

/// `POST /api/commands/{name}` after the guard: the body, the envelope, the duplicate scan,
/// `prepare`, the identity, and the runner; then the reply at `deadline +
/// write_reply_grace` (§2.8 table).
pub(crate) async fn dispatch(ctx: WriteCtx, req: Request<Body>) -> Result<Response, ApiError> {
    let shared = ctx.shared().clone();
    let name = path_param(COMMAND_PATH, req.uri().path(), "name").unwrap_or_default();
    // The guard answered 404 and 501 for these already.
    let handler = shared
        .commands
        .handler(name)
        .ok_or_else(ApiError::not_implemented)?;
    let spec = shared.commands.spec(name).ok_or_else(ApiError::not_found)?;
    let token_sha256 = ctx
        .principal()
        .map(|p| p.token_sha256)
        .ok_or_else(ApiError::internal)?;
    let start = req
        .extensions()
        .get::<RequestStart>()
        .copied()
        .unwrap_or_else(|| RequestStart(shared.cfg.clock.now()));
    let body = ctx
        .body_within(req.into_body(), shared.cfg.max_body_bytes)
        .await?;
    // The envelope and its `args` are objects; the derive alone would also read an array.
    let raw: &RawValue = serde_json::from_slice(&body).map_err(|_| ApiError::bad_request())?;
    if !raw.get().starts_with('{') {
        return Err(ApiError::bad_request());
    }
    let envelope: CommandRequest =
        serde_json::from_str(raw.get()).map_err(|_| ApiError::bad_request())?;
    if !envelope.args.get().starts_with('{') {
        return Err(ApiError::bad_request());
    }
    let (prepared, identity) = prepare_args(handler.as_ref(), spec.name, &envelope.args)?;
    run(
        &shared,
        ctx.deadline(),
        Admitted {
            command_id: envelope.command_id,
            token_sha256,
            identity,
            intent: prepared.intent,
            handler,
            start,
        },
    )
    .await
}

/// A command the request hands to its runner.
pub(crate) struct Admitted {
    pub(crate) command_id: CommandId,
    pub(crate) token_sha256: [u8; 32],
    pub(crate) identity: [u8; 32],
    pub(crate) intent: IntentFn,
    pub(crate) handler: Arc<dyn CommandHandler>,
    /// The request's origin, copied by `dispatch` for [`CommandCtx::request_start`].
    pub(crate) start: RequestStart,
}

/// What the runner has reached; the request reads it at reply time.
#[derive(Debug, Clone)]
pub(crate) enum Progress {
    /// The intent job was handed to the writer, which has not replied.
    Submitted,
    /// The intent committed, and the runner has not published its completion: the effect
    /// or the completion may still be running, or the completion may already have
    /// committed.
    IntentCommitted,
    /// The command's reply.
    Done(Result<CommandResult, ApiError>),
}

/// Registers the runner (closing: 503 `shutting_down`, nothing submitted) and answers from
/// its progress at `deadline + write_reply_grace`. Past the deadline, 408 with nothing
/// submitted.
pub(crate) async fn run(
    shared: &Arc<Shared>,
    deadline: Instant,
    cmd: Admitted,
) -> Result<Response, ApiError> {
    if shared.passed(deadline) {
        return Err(ApiError::request_timeout());
    }
    let reply_by = deadline + shared.cfg.write_reply_grace;
    let (tx, rx) = watch::channel(Progress::Submitted);
    let s = shared.clone();
    shared
        .registry
        .spawn(TaskKind::Runner, runner(s, deadline, cmd, tx))
        .map_err(|_| ApiError::shutting_down())?;
    await_reply(shared, rx, reply_by).await
}

/// The request's wait. Each turn reads the progress, then the server's clock: a `Done`
/// read while the clock is before `reply_by` is the reply. Otherwise the reply time has
/// been reached (the clock read after the progress at or past `reply_by`, or the timer
/// fired), and the progress read then decides: an intent committed is 409 `in_progress`
/// (the receipt exists, and the runner had not yet published its completion when read;
/// the completion may already have committed), anything else 503
/// `outcome_unknown` (a `Done` read at or after `reply_by` included, so a late `Expired`
/// is never 408). A runner gone without a final reply (aborted at the drain deadline) is
/// the same.
async fn await_reply(
    shared: &Shared,
    mut rx: watch::Receiver<Progress>,
    reply_by: Instant,
) -> Result<Response, ApiError> {
    let cutoff = shared.timer(reply_by);
    #[cfg(test)]
    shared
        .faults
        .before_cutoff_wait(|| matches!(*rx.borrow(), Progress::Done(_)))
        .await;
    loop {
        #[cfg(test)]
        shared.faults.before_progress_read().await;
        let progress = rx.borrow_and_update().clone();
        // After the read: a reply taken at or past the reply time is never sent as is.
        let late = shared.passed(reply_by);
        match progress {
            Progress::Done(r) if !late => return r.map(result_response),
            _ if late => return Err(at_reply_time(&progress)),
            _ => {}
        }
        match reply_wake(cutoff, rx.changed()).await {
            Ok(Ok(())) => {}
            Err(_) | Ok(Err(_)) => return Err(at_reply_time(&rx.borrow())),
        }
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "wakes the reply loop only; its progress read and the sample after it decide (http-surface §2.8)"
)]
fn reply_wake<F: Future>(cutoff: tokio::time::Instant, changed: F) -> tokio::time::Timeout<F> {
    tokio::time::timeout_at(cutoff, changed)
}

fn at_reply_time(p: &Progress) -> ApiError {
    match p {
        Progress::IntentCommitted => ApiError::in_progress(),
        Progress::Submitted | Progress::Done(_) => ApiError::outcome_unknown(),
    }
}

/// A stored or fresh command result as its HTTP reply, byte for byte.
fn result_response(r: CommandResult) -> Response {
    let Ok(status) = StatusCode::from_u16(r.http_status) else {
        return ApiError::internal().into_response();
    };
    let mut resp = (status, r.body).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

// ---- the runner -----------------------------------------------------------------------------

/// What the intent job decided.
enum Begun {
    /// A receipt existed: its lookup, with nothing written.
    Lookup(ReceiptLookup),
    /// Completed in the intent job, receipt included.
    Completed(CommandResult),
    /// Committed with the receipt `in_progress`; the effect runs next.
    Pending(EffectRun),
}

/// The registered task that owns a command: it awaits its own writes and never registers
/// another task. It publishes `Done` last; a final result only after its receipt committed.
async fn runner(
    shared: Arc<Shared>,
    deadline: Instant,
    cmd: Admitted,
    progress: watch::Sender<Progress>,
) {
    #[cfg(test)]
    shared.faults.before_runner_submit().await;
    let reply = drive(&shared, deadline, cmd, &progress).await;
    progress.send_replace(Progress::Done(reply));
}

async fn drive(
    shared: &Arc<Shared>,
    deadline: Instant,
    cmd: Admitted,
    progress: &watch::Sender<Progress>,
) -> Result<CommandResult, ApiError> {
    let Admitted {
        command_id,
        token_sha256,
        identity,
        intent,
        handler,
        start,
    } = cmd;
    let has_effect = handler.effect().is_some();
    let clock: Arc<dyn MonotonicClock> = shared.cfg.clock.clone();
    // The single deciding point: the token, the receipt lookup and insert, the object
    // checks and the intent's writes, in one transaction.
    let job: Job<Begun> = Box::new(move |tx| {
        let Some(auth) = verify_opt_in_job(tx, Some(token_sha256))? else {
            return Err(refused(ApiError::internal()));
        };
        let lookup = tx.receipt_begin(command_id, auth.token_id, identity)?;
        if lookup != ReceiptLookup::New {
            return Ok(Begun::Lookup(lookup));
        }
        let mut ctx = CommandCtx {
            command_id,
            request_start: start.0,
            job_start: clock.now(),
        };
        let decided = intent(tx, &auth, &mut ctx).map_err(|e| checked_refusal(command_id, e));
        match decided? {
            Intent::Completed(r) => {
                let r = checked_result(command_id, r);
                tx.receipt_complete(command_id, &r)?;
                Ok(Begun::Completed(r))
            }
            Intent::Pending(run) if has_effect => Ok(Begun::Pending(run)),
            // A programming error: refused, so the receipt row rolls back with the rest.
            Intent::Pending(_) => Err(refused(ApiError::internal())),
        }
    });
    // A revoking intent nudges the sockets once its job committed (design §2.4).
    let nudge: Option<AfterOk> = handler.revokes().then(|| {
        let n = shared.revocation_nudge();
        Box::new(move || n.nudge()) as AfterOk
    });
    let run = match run_write(shared, deadline, job, nudge).await {
        Err(f) => return Err(write_failure(f)),
        Ok(Begun::Lookup(l)) => return lookup_reply(l),
        Ok(Begun::Completed(r)) => return Ok(r),
        Ok(Begun::Pending(run)) => run,
    };
    progress.send_replace(Progress::IntentCommitted);
    // The effect, once, awaited to its terminal result with no timeout of the runner's own.
    let effect = AssertUnwindSafe(async {
        match handler.effect() {
            Some(e) => Some(e.run(run).await),
            None => None,
        }
    })
    .catch_unwind()
    .await;
    // A panic (its payload is not logged) leaves no result to store.
    let result = match effect {
        Ok(Some(EffectOutcome::Done(result))) => result,
        // Shutdown: the receipt stays `in_progress`; nothing is reported, so a graceful stop
        // does not end as a fatal.
        Ok(Some(EffectOutcome::ShuttingDown)) => return Err(ApiError::outcome_unknown()),
        Ok(Some(EffectOutcome::Unrecorded)) => {
            tracing::error!(%command_id, "command effect could not record its work");
            shared
                .sink
                .store_fatal(StoreFatal::ReceiptUncompleted { command_id });
            return Err(ApiError::outcome_unknown());
        }
        Ok(None) | Err(_) => {
            tracing::error!(%command_id, "command effect ended without a result");
            shared
                .sink
                .store_fatal(StoreFatal::ReceiptUncompleted { command_id });
            return Err(ApiError::outcome_unknown());
        }
    };
    let result = checked_result(command_id, result);
    if complete(shared, command_id, &result).await {
        Ok(result)
    } else {
        shared
            .sink
            .store_fatal(StoreFatal::ReceiptUncompleted { command_id });
        Err(ApiError::outcome_unknown())
    }
}

/// An existing receipt's reply (§2.8 table): one `conflict` body whatever its state.
fn lookup_reply(l: ReceiptLookup) -> Result<CommandResult, ApiError> {
    match l {
        ReceiptLookup::Replay(r) => Ok(r),
        ReceiptLookup::InProgress => Err(ApiError::in_progress()),
        ReceiptLookup::Conflict => Err(ApiError::conflict()),
        ReceiptLookup::UnknownAfterCrash => Err(ApiError::unknown_after_crash()),
        ReceiptLookup::New => Err(ApiError::internal()),
    }
}

/// Whether a completion attempt may be retried: nothing was written and the writer is
/// healthy.
pub(crate) fn retryable(f: &WriteFailure) -> bool {
    matches!(
        f,
        WriteFailure::Submit(SubmitError::QueueFull)
            | WriteFailure::Write(
                WriteError::Expired | WriteError::Busy | WriteError::RolledBack(StoreError::Busy)
            )
    )
}

/// Completes the receipt: attempts with `not_after = min(now + 5 s, budget_end)`, retried
/// with backoff on a retryable outcome. The budget (60 s from the first attempt, on the
/// server's clock) bounds the admission of attempts only, checked before each one; an
/// admitted attempt is awaited to its reply. No token check: the effect has run. True once
/// a completion committed.
async fn complete(shared: &Shared, command_id: CommandId, result: &CommandResult) -> bool {
    let budget_end = shared.cfg.clock.now() + COMPLETION_BUDGET;
    let mut backoff = BACKOFF_FIRST;
    loop {
        let now = shared.cfg.clock.now();
        if now >= budget_end {
            return false;
        }
        let not_after = (now + COMPLETION_ATTEMPT).min(budget_end);
        #[cfg(test)]
        shared.faults.completion_attempt(now);
        let r = result.clone();
        #[cfg(test)]
        let hold = shared.faults.take_completion_hold();
        let job: Job<()> = Box::new(move |tx| {
            #[cfg(test)]
            if let Some(h) = hold {
                h.hold();
            }
            Ok(tx.receipt_complete(command_id, &r)?)
        });
        match run_write(shared, not_after, job, None).await {
            Ok(()) => return true,
            Err(f) if retryable(&f) => {}
            Err(_) => return false,
        }
        let from = shared.cfg.clock.now();
        let wake = (from + backoff).min(budget_end);
        #[cfg(test)]
        shared.faults.completion_pause(from, wake);
        backoff_until(shared.timer(wake)).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "completion backoff; the budget is read before each attempt (http-surface §2.8)"
)]
pub(crate) fn backoff_until(wake: tokio::time::Instant) -> tokio::time::Sleep {
    tokio::time::sleep_until(wake)
}
