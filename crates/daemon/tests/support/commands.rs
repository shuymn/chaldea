//! The test-only command handlers (`docs/design/http-surface.md` §2.8 "What PR 7 completes
//! end to end"), under test-only names in a test command table. They exist only in test
//! code and reach a server only through [`registry`]; the daemon's registry is
//! `CommandRegistry::mvp()`, every command `Unimplemented`.
//!
//! - `test_counter`: completed in its intent job, which appends one event and counts the
//!   intents run; an optional `target` thread is checked against the `task set` grant.
//! - `test_completed`: completed in its intent job with a fixed body (or an error body with
//!   the `code` the arguments name, or by its text a body that is not JSON or one that
//!   `serde_json::Value` cannot hold) and a status (201 unless the arguments name one); its
//!   `prepare` refuses the text `prepare_timeout` with 408.
//! - `test_slow_effect`: a pending intent whose effect waits for the test's release when
//!   armed, can revoke the caller's token, and answers a status or panics by its tag.
//! - `test_refuse`: an intent that refuses (with a reply a handler may give or one reserved
//!   to the runner), panics, or writes and returns `Pending` without an effect.
//! - `test_worktree_remove`: the `OwnWorktree` object check.
//! - `test_long_deadline`: completed in its intent job, with a budget of its own
//!   ([`LONG_DEADLINE`]).
//! - `test_revoke` and `test_revoke_quiet`: completed in an intent job that revokes the token
//!   the arguments name, after the intent gate; only `test_revoke` declares `revokes()`.
//! - `test_effect_outcome`: a pending intent whose effect, after the effect gate, ends with
//!   the `EffectOutcome` the arguments name.
//! - `test_ctx`: completed in its intent job, which records the `CommandCtx` instants.
//!
//! The gates block inside intent closures and effects on purpose; production intents
//! capture data only (store §5).

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use chaldea_core::auth::{
    Action, CommandSpec, Grant, ThreadGraph, grant, may_act_on_thread, may_remove_worktree,
    self_plus,
};
use chaldea_core::canonical::CanonicalJson;
use chaldea_core::command::CommandResult;
use chaldea_core::http::ErrorCode;
use chaldea_core::ids::{ThreadId, TokenId, WorktreeId};
use chaldea_daemon::http::{
    ApiError, Authorized, CommandEffect, CommandHandler, CommandRegistry, EffectOutcome, EffectRun,
    Intent, Prepared, deny_for,
};
use chaldea_daemon::store::{JobError, ReadTx, Store, StoreError, TokenSelector};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use tokio::sync::oneshot;

use super::waits;
use super::{registered, soon};

/// The test command table: test-only names with the actions their checks use.
pub static TEST_COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "test_counter",
        action: Action::TaskSet,
        owner_pr: 7,
    },
    CommandSpec {
        name: "test_completed",
        action: Action::Send,
        owner_pr: 7,
    },
    CommandSpec {
        name: "test_slow_effect",
        action: Action::Send,
        owner_pr: 7,
    },
    CommandSpec {
        name: "test_refuse",
        action: Action::Send,
        owner_pr: 7,
    },
    CommandSpec {
        name: "test_worktree_remove",
        action: Action::WorktreeRemove,
        owner_pr: 7,
    },
    CommandSpec {
        name: "test_long_deadline",
        action: Action::Send,
        owner_pr: 7,
    },
    CommandSpec {
        name: "test_revoke",
        action: Action::Send,
        owner_pr: 10,
    },
    CommandSpec {
        name: "test_revoke_quiet",
        action: Action::Send,
        owner_pr: 10,
    },
    CommandSpec {
        name: "test_effect_outcome",
        action: Action::Send,
        owner_pr: 10,
    },
    CommandSpec {
        name: "test_ctx",
        action: Action::Send,
        owner_pr: 10,
    },
];

/// `test_long_deadline`'s budget, from its connection's first head byte; longer than the
/// test servers' short deadline.
pub const LONG_DEADLINE: Duration = Duration::from_secs(30);

type SyncGate = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);
type AsyncGate = (oneshot::Sender<()>, oneshot::Receiver<()>);

/// What the test handlers did, and the gates a test arms.
#[derive(Default)]
pub struct Probe {
    intents: AtomicUsize,
    effects: AtomicUsize,
    intent_gate: Mutex<Option<SyncGate>>,
    effect_gate: Mutex<Option<AsyncGate>>,
    ctx_seen: Mutex<Vec<(Instant, Instant)>>,
}

impl Probe {
    /// Intent closures of `test_counter` and `test_slow_effect` that passed their object
    /// checks (rolled-back runs included).
    pub fn intents(&self) -> usize {
        self.intents.load(Ordering::SeqCst)
    }

    /// `(request_start, job_start)` of each `test_ctx` intent run.
    pub fn ctx_seen(&self) -> Vec<(Instant, Instant)> {
        self.ctx_seen.lock().unwrap().clone()
    }

    /// Effects of `test_slow_effect` started.
    pub fn effects(&self) -> usize {
        self.effects.load(Ordering::SeqCst)
    }

    /// The next intent of `test_counter` or `test_slow_effect` blocks the writer: the
    /// returned receiver gets a message when it is reached, and it goes on once the returned
    /// sender is used or dropped.
    pub fn hold_intent(&self) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *self.intent_gate.lock().unwrap() = Some((reached_tx, release_rx));
        (reached_rx, release_tx)
    }

    /// The next effect waits: the returned receiver fires when it starts, and it goes on
    /// once the returned sender is used or dropped (or the effect's task is aborted).
    pub fn hold_effect(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached_tx, reached_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        *self.effect_gate.lock().unwrap() = Some((reached_tx, release_rx));
        (reached_rx, release_tx)
    }

    fn pass_intent_gate(&self) {
        let gate = self.intent_gate.lock().unwrap().take();
        if let Some((reached, release)) = gate {
            let _ = reached.send(());
            let _ = release.recv();
        }
    }
}

/// `MVP_COMMANDS` (all unimplemented) plus the test commands, registered over `store`.
pub fn registry(store: &Store, probe: &Arc<Probe>) -> CommandRegistry {
    let mut r = CommandRegistry::mvp_plus(TEST_COMMANDS);
    r.register(Box::new(Counter(probe.clone())));
    r.register(Box::new(Completed));
    r.register(Box::new(SlowEffect {
        probe: probe.clone(),
        store: store.clone(),
    }));
    r.register(Box::new(Refuse));
    r.register(Box::new(WorktreeRemove));
    r.register(Box::new(LongDeadline));
    r.register(Box::new(Revoke {
        probe: probe.clone(),
        nudges: true,
    }));
    r.register(Box::new(Revoke {
        probe: probe.clone(),
        nudges: false,
    }));
    r.register(Box::new(Outcome(probe.clone())));
    r.register(Box::new(Ctx(probe.clone())));
    r
}

fn bad_args<E>(_: E) -> ApiError {
    ApiError::bad_args()
}

fn refuse(e: ApiError) -> JobError {
    JobError::Refused(Box::new(e))
}

fn graph(tx: &ReadTx<'_>) -> Result<ThreadGraph, StoreError> {
    Ok(ThreadGraph::new(
        tx.threads()?
            .into_iter()
            .map(|t| (t.thread_id, t.parent_thread_id)),
    ))
}

// ---- test_counter -------------------------------------------------------------------------

/// `test_counter`'s arguments: every field kind whose spelling or order may vary (a map
/// whose order is its hasher's, a float, nested maps in a list).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CounterArgs {
    pub n: u32,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub ratio: f64,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub layers: Vec<BTreeMap<String, u32>>,
    /// A thread the command acts on, checked against the `task set` grant.
    #[serde(default)]
    pub target: Option<ThreadId>,
}

/// Parses `args` as a `T` and returns its canonical form with it.
fn parse<T: Serialize + for<'de> Deserialize<'de>>(
    args: &RawValue,
) -> Result<(T, CanonicalJson), ApiError> {
    let v: T = serde_json::from_str(args.get()).map_err(bad_args)?;
    let canonical = CanonicalJson::of(&v).map_err(bad_args)?;
    Ok((v, canonical))
}

struct Counter(Arc<Probe>);

/// `test_counter` alone, with a probe of its own, for tests that only call `prepare`.
#[must_use]
pub fn counter() -> Box<dyn CommandHandler> {
    Box::new(Counter(Arc::default()))
}

impl CommandHandler for Counter {
    fn name(&self) -> &'static str {
        "test_counter"
    }

    fn prepare(&self, args: &RawValue) -> Result<Prepared, ApiError> {
        let (a, normalized): (CounterArgs, _) = parse(args)?;
        let probe = self.0.clone();
        Ok(Prepared {
            normalized,
            intent: Box::new(move |tx, auth, ctx| {
                probe.pass_intent_gate();
                if let Some(t) = a.target {
                    let g = graph(&tx.read())?;
                    let allowed = grant(auth.kind, Action::TaskSet).is_some_and(|gr| {
                        g.contains(t) && may_act_on_thread(&g, auth.thread, gr, t)
                    });
                    if !allowed {
                        return Err(refuse(deny_for(&g, auth, t)));
                    }
                }
                probe.intents.fetch_add(1, Ordering::SeqCst);
                let id = ctx.command_id();
                let mut e = registered(ThreadId(id.0), false);
                e.command_id = Some(id);
                e.idempotency_key = chaldea_core::ids::IdempotencyKey(format!("command:{id}:1"));
                tx.append(vec![e])?;
                Ok(Intent::Completed(CommandResult {
                    http_status: 200,
                    body: serde_json::json!({ "n": a.n, "label": a.label }).to_string(),
                }))
            }),
        })
    }
}

// ---- test_completed -----------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TextArgs {
    text: String,
    /// The status to answer; 201 when absent.
    #[serde(default)]
    status: Option<u16>,
    /// When present, the body is `{"code": code}` (with `retry_after` when given) instead
    /// of [`completed_body`].
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    retry_after: Option<u32>,
}

/// The body `test_completed` answers for `text`: status 201, non-ASCII bytes included.
#[must_use]
pub fn completed_body(text: &str) -> String {
    format!(
        "{{\"echo\":{},\"note\":\"é😀\"}}",
        serde_json::to_string(text).unwrap()
    )
}

struct Completed;

impl CommandHandler for Completed {
    fn name(&self) -> &'static str {
        "test_completed"
    }

    fn prepare(&self, args: &RawValue) -> Result<Prepared, ApiError> {
        let (a, normalized): (TextArgs, _) = parse(args)?;
        if a.text == "prepare_timeout" {
            return Err(ApiError::request_timeout());
        }
        Ok(Prepared {
            normalized,
            intent: Box::new(move |_tx, _auth, _ctx| {
                let body = match (a.text.as_str(), &a.code) {
                    ("not_json", _) => "not json".to_owned(),
                    ("large_number", _) => {
                        r#"{"code":"in_progress","retry_after":1,"details":1e400}"#.to_owned()
                    }
                    ("null_retry_after", _) => {
                        r#"{"code":"forbidden","retry_after":null}"#.to_owned()
                    }
                    ("extra_member", _) => r#"{"code":"forbidden","extra":1}"#.to_owned(),
                    (_, Some(code)) => match a.retry_after {
                        Some(secs) => {
                            serde_json::json!({ "code": code, "retry_after": secs }).to_string()
                        }
                        None => serde_json::json!({ "code": code }).to_string(),
                    },
                    (_, None) => completed_body(&a.text),
                };
                Ok(Intent::Completed(CommandResult {
                    http_status: a.status.unwrap_or(201),
                    body,
                }))
            }),
        })
    }
}

// ---- test_slow_effect ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SlowArgs {
    tag: String,
    #[serde(default)]
    revoke_self: bool,
}

struct SlowRun {
    tag: String,
    revoke_self: bool,
    token_id: TokenId,
}

/// The body `test_slow_effect`'s effect answers.
#[must_use]
pub fn slow_body(tag: &str) -> String {
    serde_json::json!({ "tag": tag, "effect": "done" }).to_string()
}

struct SlowEffect {
    probe: Arc<Probe>,
    store: Store,
}

impl CommandHandler for SlowEffect {
    fn name(&self) -> &'static str {
        "test_slow_effect"
    }

    fn prepare(&self, args: &RawValue) -> Result<Prepared, ApiError> {
        let (a, normalized): (SlowArgs, _) = parse(args)?;
        let probe = self.probe.clone();
        Ok(Prepared {
            normalized,
            intent: Box::new(move |_tx, auth: &Authorized, _ctx| {
                probe.pass_intent_gate();
                probe.intents.fetch_add(1, Ordering::SeqCst);
                Ok(Intent::Pending(EffectRun::new(SlowRun {
                    tag: a.tag,
                    revoke_self: a.revoke_self,
                    token_id: auth.token_id,
                })))
            }),
        })
    }

    fn effect(&self) -> Option<&dyn CommandEffect> {
        Some(self)
    }
}

impl CommandEffect for SlowEffect {
    fn run(&self, run: EffectRun) -> BoxFuture<'static, EffectOutcome> {
        // Counted before the data is read: every start is an effect started.
        self.probe.effects.fetch_add(1, Ordering::SeqCst);
        let run = run.downcast::<SlowRun>().unwrap_or_else(|_| SlowRun {
            tag: "<another run>".into(),
            revoke_self: false,
            token_id: TokenId::from_u128(0),
        });
        let gate = self.probe.effect_gate.lock().unwrap().take();
        let store = self.store.clone();
        Box::pin(async move {
            if let Some((reached, release)) = gate {
                let _ = reached.send(());
                let _ = release.await;
            }
            if run.revoke_self {
                let id = run.token_id;
                let ticket = store
                    .submit(soon(), move |tx| {
                        Ok(tx.token_revoke(TokenSelector::Token(id))?)
                    })
                    .unwrap();
                waits::bounded(Duration::from_secs(10), ticket)
                    .await
                    .expect("the revocation within 10 s")
                    .unwrap();
            }
            // Tags that answer a status outside the result set, or panic.
            let http_status = match run.tag.as_str() {
                "effect_408" => 408,
                "effect_42" => 42,
                "effect_panic" => panic!("test_slow_effect panics in its effect"),
                _ => 200,
            };
            EffectOutcome::Done(CommandResult {
                http_status,
                body: slow_body(&run.tag),
            })
        })
    }
}

// ---- test_refuse --------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
enum How {
    /// Refused with 403 `forbidden`.
    Forbidden,
    /// Refused with 408, outside the refusal set.
    Timeout,
    /// Refused with 200, outside the refusal set.
    Success,
    /// Refused with 503 `outcome_unknown`, a reply reserved to the runner.
    OutcomeUnknown,
    /// Refused with 503 `busy`: a transient refusal whose outcome is known.
    Busy,
    /// Refused with the `code` and `status` of the arguments.
    Code,
    /// Appends one event, then returns `Intent::Pending` from a handler without an effect.
    Pending,
    /// The intent panics.
    Panic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RefuseArgs {
    how: How,
    /// For `How::Code`: the refusal's code, status, and `retry_after`.
    #[serde(default)]
    code: Option<ErrorCode>,
    #[serde(default)]
    status: Option<u16>,
    #[serde(default)]
    retry_after: Option<u32>,
}

struct Refuse;

impl CommandHandler for Refuse {
    fn name(&self) -> &'static str {
        "test_refuse"
    }

    fn prepare(&self, args: &RawValue) -> Result<Prepared, ApiError> {
        let (a, normalized): (RefuseArgs, _) = parse(args)?;
        Ok(Prepared {
            normalized,
            intent: Box::new(move |tx, _auth, ctx| match a.how {
                How::Forbidden => Err(refuse(ApiError::forbidden())),
                How::Timeout => Err(refuse(ApiError::request_timeout())),
                How::Success => Err(refuse(ApiError::new(StatusCode::OK, ErrorCode::Internal))),
                How::OutcomeUnknown => Err(refuse(ApiError::outcome_unknown())),
                How::Busy => Err(refuse(ApiError::busy())),
                How::Code => {
                    let status = StatusCode::from_u16(a.status.unwrap_or(400)).unwrap();
                    let e = ApiError::new(status, a.code.unwrap_or(ErrorCode::Internal));
                    Err(refuse(match a.retry_after {
                        Some(secs) => e.with_retry_after(secs),
                        None => e,
                    }))
                }
                How::Pending => {
                    let id = ctx.command_id();
                    let mut e = registered(ThreadId(id.0), false);
                    e.command_id = Some(id);
                    e.idempotency_key =
                        chaldea_core::ids::IdempotencyKey(format!("command:{id}:1"));
                    tx.append(vec![e])?;
                    Ok(Intent::Pending(EffectRun::new(())))
                }
                How::Panic => {
                    chaldea_pty_runtime::fake::inject_panic("test_refuse panics in its intent")
                }
            }),
        })
    }
}

// ---- test_worktree_remove -----------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorktreeArgs {
    worktree_id: WorktreeId,
}

struct WorktreeRemove;

impl CommandHandler for WorktreeRemove {
    fn name(&self) -> &'static str {
        "test_worktree_remove"
    }

    fn prepare(&self, args: &RawValue) -> Result<Prepared, ApiError> {
        let (a, normalized): (WorktreeArgs, _) = parse(args)?;
        Ok(Prepared {
            normalized,
            intent: Box::new(move |tx, auth, _ctx| {
                let r = tx.read();
                let Some(row) = r.worktree(&a.worktree_id)? else {
                    return Err(refuse(ApiError::not_found()));
                };
                let owner = row.record.owner_thread_id;
                let may = grant(auth.kind, Action::WorktreeRemove)
                    .is_some_and(|g| may_remove_worktree(auth.thread, g, owner));
                if may {
                    return Ok(Intent::Completed(CommandResult {
                        http_status: 200,
                        body: serde_json::json!({ "removed": a.worktree_id }).to_string(),
                    }));
                }
                // Hiding: a worktree the token cannot list is 404, one it can is 403.
                let readable = match grant(auth.kind, Action::ReadWorktrees) {
                    Some(Grant::All) => true,
                    Some(Grant::SelfPlus) => match (auth.thread, owner) {
                        (Some(me), Some(o)) => self_plus(&graph(&r)?, me).contains(&o),
                        _ => false,
                    },
                    _ => false,
                };
                Err(refuse(if readable {
                    ApiError::forbidden()
                } else {
                    ApiError::not_found()
                }))
            }),
        })
    }
}

// ---- test_long_deadline -------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

struct LongDeadline;

impl CommandHandler for LongDeadline {
    fn name(&self) -> &'static str {
        "test_long_deadline"
    }

    fn deadline(&self) -> Option<Duration> {
        Some(LONG_DEADLINE)
    }

    fn prepare(&self, args: &RawValue) -> Result<Prepared, ApiError> {
        let (NoArgs {}, normalized) = parse(args)?;
        Ok(Prepared {
            normalized,
            intent: Box::new(move |_tx, _auth, _ctx| {
                Ok(Intent::Completed(CommandResult {
                    http_status: 200,
                    body: r#"{"long":true}"#.to_owned(),
                }))
            }),
        })
    }
}

// ---- test_revoke, test_revoke_quiet -------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokeArgs {
    token: TokenId,
}

struct Revoke {
    probe: Arc<Probe>,
    nudges: bool,
}

impl CommandHandler for Revoke {
    fn name(&self) -> &'static str {
        if self.nudges {
            "test_revoke"
        } else {
            "test_revoke_quiet"
        }
    }

    fn revokes(&self) -> bool {
        self.nudges
    }

    fn prepare(&self, args: &RawValue) -> Result<Prepared, ApiError> {
        let (a, normalized): (RevokeArgs, _) = parse(args)?;
        let probe = self.probe.clone();
        Ok(Prepared {
            normalized,
            intent: Box::new(move |tx, _auth, _ctx| {
                probe.pass_intent_gate();
                tx.token_revoke(TokenSelector::Token(a.token))?;
                Ok(Intent::Completed(CommandResult {
                    http_status: 200,
                    body: "{}".to_owned(),
                }))
            }),
        })
    }
}

// ---- test_effect_outcome ------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OutcomeKind {
    Unrecorded,
    ShuttingDown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutcomeArgs {
    outcome: OutcomeKind,
}

struct Outcome(Arc<Probe>);

impl CommandHandler for Outcome {
    fn name(&self) -> &'static str {
        "test_effect_outcome"
    }

    fn prepare(&self, args: &RawValue) -> Result<Prepared, ApiError> {
        let (a, normalized): (OutcomeArgs, _) = parse(args)?;
        Ok(Prepared {
            normalized,
            intent: Box::new(move |_tx, _auth, _ctx| {
                Ok(Intent::Pending(EffectRun::new(a.outcome)))
            }),
        })
    }

    fn effect(&self) -> Option<&dyn CommandEffect> {
        Some(self)
    }
}

impl CommandEffect for Outcome {
    fn run(&self, run: EffectRun) -> BoxFuture<'static, EffectOutcome> {
        self.0.effects.fetch_add(1, Ordering::SeqCst);
        let kind = run.downcast::<OutcomeKind>().ok();
        let gate = self.0.effect_gate.lock().unwrap().take();
        Box::pin(async move {
            if let Some((reached, release)) = gate {
                let _ = reached.send(());
                let _ = release.await;
            }
            match kind {
                Some(OutcomeKind::Unrecorded) => EffectOutcome::Unrecorded,
                Some(OutcomeKind::ShuttingDown) => EffectOutcome::ShuttingDown,
                None => EffectOutcome::Done(CommandResult {
                    http_status: 200,
                    body: "{}".to_owned(),
                }),
            }
        })
    }
}

// ---- test_ctx -----------------------------------------------------------------------------

struct Ctx(Arc<Probe>);

impl CommandHandler for Ctx {
    fn name(&self) -> &'static str {
        "test_ctx"
    }

    fn prepare(&self, args: &RawValue) -> Result<Prepared, ApiError> {
        let (NoArgs {}, normalized) = parse(args)?;
        let probe = self.0.clone();
        Ok(Prepared {
            normalized,
            intent: Box::new(move |_tx, _auth, ctx| {
                probe
                    .ctx_seen
                    .lock()
                    .unwrap()
                    .push((ctx.request_start(), ctx.job_start()));
                Ok(Intent::Completed(CommandResult {
                    http_status: 200,
                    body: "{}".to_owned(),
                }))
            }),
        })
    }
}
