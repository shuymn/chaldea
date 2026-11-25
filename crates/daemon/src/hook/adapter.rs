//! The adapter seam (`docs/design/hook-ingest.md` §2.6): what a selected hook means is the
//! adapter's (PR 10, 13, 14); PR 9 decides only routing, binding, and selection.

use std::borrow::Cow;

use chaldea_core::event::TypedEvent;
use chaldea_core::ids::{Agent, ApprovalId, ThreadId};
use chaldea_core::payload::RawUnknownReason;
use chaldea_pty_runtime::process::Ancestry;
use uuid::Uuid;

use super::intake::IntakeFacts;
use crate::store::{BlobInput, EventAnnex, LaunchRow, ReadTx, StoreError};

/// How an unbound launch binds and how later hooks are selected (§2.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingRule {
    /// The `SessionStart`'s `session_id` equals the expected session (Claude).
    SessionId,
    /// The `SessionStart`'s chain is eligible; later hooks need it too (Codex).
    Ancestry,
}

/// The meaning of selected hooks, decided inside the delivery or binding job. Data only:
/// no blocking, network, PTY, supervisor, or channel handle.
pub trait HookAdapter: Send + Sync + 'static {
    fn rule(&self, agent: Agent) -> BindingRule;

    /// Claude's predetermined session id for this launch, from stored facts only.
    ///
    /// # Errors
    ///
    /// A store error (the job rolls back).
    fn expected_session(
        &self,
        tx: &ReadTx<'_>,
        launch: &LaunchRow,
    ) -> Result<Option<String>, StoreError>;

    /// Events for one selected hook of the current, live, bound launch, with the hook's
    /// turn mark. Reads see the job's earlier appends.
    ///
    /// # Errors
    ///
    /// A store error (the job rolls back).
    fn interpret(
        &self,
        tx: &ReadTx<'_>,
        hook: &AcceptedHook<'_>,
    ) -> Result<(Interpretation, TurnMark), StoreError>;
}

/// What an interpreted hook tells the supervisor once its job committed
/// (supervisor-send §2.4 "Settle completion"): data, handed to
/// [`super::IntakeProbe::finished`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnMark {
    /// A turn started (`UserPromptSubmit`).
    Start,
    /// A turn ended (`Stop`, interrupt); `settle` when the end may start a settle.
    End { settle: bool },
    /// A card opened; `human_at` is the human writer position at its intake, `None` when
    /// it cannot be relied on.
    Card {
        approval_id: ApprovalId,
        human_at: Option<u64>,
    },
    /// A card resolved by this hook's evidence.
    Resolved { approval_id: ApprovalId },
    /// Any other hook.
    Other,
}

/// What `interpret` decided.
#[derive(Debug, Clone)]
pub enum Interpretation {
    /// Non-empty; names distinct and not starting with `bind`.
    Events(Vec<HookEvent>),
    /// Kept as `raw_unknown` with this reason.
    Raw(RawUnknownReason),
}

/// One event of an interpreted hook. PR 9 builds its envelope; `name` gives its key
/// (`derived:hook:<emit_id>:<name>`) unless it is the delivery's first event.
#[derive(Debug, Clone)]
pub struct HookEvent {
    pub event: TypedEvent,
    pub name: Cow<'static, str>,
    pub annex: Option<EventAnnex>,
    pub blobs: Vec<BlobInput>,
}

/// One selected hook, as `interpret` sees it.
#[derive(Debug)]
pub struct AcceptedHook<'a> {
    pub thread: ThreadId,
    pub launch: &'a LaunchRow,
    pub emit_id: Uuid,
    pub receipt_ordinal: u64,
    pub event_name: &'a str,
    pub session_id: &'a str,
    pub json: &'a serde_json::Value,
    pub body: &'a [u8],
    pub intake: Option<&'a IntakeFacts>,
    /// `X-Chaldea-Attempt`; `None` when absent or invalid.
    pub attempt: Option<u8>,
    /// `attempt == Some(1)`: the facts were taken when the CLI's first request arrived.
    pub intake_reliable: bool,
    /// The receipt ordinal of the `SessionStart` that bound the launch (0 when the binding
    /// was not a hook delivery).
    pub binding_ordinal: u64,
    /// Applied from the buffer in the binding job.
    pub promoted: bool,
}

/// The emitter's ancestry, walked once per request on a blocking thread.
pub trait ProcessProbe: Send + Sync + 'static {
    fn chain(&self, emitter_pid: u32, max: usize) -> Ancestry;
}

/// `pty_runtime::process::ancestry`.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsProcesses;

impl ProcessProbe for OsProcesses {
    fn chain(&self, emitter_pid: u32, max: usize) -> Ancestry {
        chaldea_pty_runtime::process::ancestry(emitter_pid, max)
    }
}

/// The adapter with no decoder: §2.5's rules, the expected session from
/// stored facts, and every selected hook kept as `raw_unknown` `unknown_event` (no decoder
/// exists yet).
#[derive(Debug, Default, Clone, Copy)]
pub struct MvpHooks;

impl HookAdapter for MvpHooks {
    fn rule(&self, agent: Agent) -> BindingRule {
        match agent {
            Agent::Claude => BindingRule::SessionId,
            Agent::Codex => BindingRule::Ancestry,
        }
    }

    /// The thread's bound session when it has one (a resume passes `--resume <it>`), else
    /// the launch id in lowercase hyphenated form (a fresh launch passes
    /// `--session-id <launch_id>`).
    fn expected_session(
        &self,
        tx: &ReadTx<'_>,
        launch: &LaunchRow,
    ) -> Result<Option<String>, StoreError> {
        let bound = tx.thread(launch.thread_id)?.and_then(|t| t.session_id);
        Ok(Some(bound.unwrap_or_else(|| launch.launch_id.to_string())))
    }

    fn interpret(
        &self,
        _tx: &ReadTx<'_>,
        _hook: &AcceptedHook<'_>,
    ) -> Result<(Interpretation, TurnMark), StoreError> {
        Ok((
            Interpretation::Raw(RawUnknownReason::UnknownEvent),
            TurnMark::Other,
        ))
    }
}
