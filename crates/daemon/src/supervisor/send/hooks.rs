//! The decoder seam and the hook adapter (send §2.5): `SupervisorHooks` asks a per-agent
//! [`HookDecoder`] what a hook says and turns it into state events and a turn mark, data
//! only, inside the delivery or binding job.

use std::borrow::Cow;
use std::sync::Arc;

use chaldea_core::adapter::{HostOs, ReadinessMode, capabilities, evidence_allowed};
use chaldea_core::canonical::CanonicalJson;
use chaldea_core::event::TypedEvent;
use chaldea_core::ids::{Actor, Agent, ApprovalId, InputId, LaunchId, StreamId};
use chaldea_core::input::{InputInput, InputReason, InputState, step};
use chaldea_core::payload::InputStateChanged;
use chaldea_core::payload::{
    ApprovalCorrelation, ApprovalOutcome, ApprovalRequested, ApprovalResolved, Content, Encoding,
    RawUnknownReason, ReadinessEvidence, ReadinessObserved, UserMessage,
};
use chaldea_core::state::{AttentionCause, StateInput, ThreadState, reduce};
use chaldea_pty_runtime::OriginClass;

use super::attribution::attribute;
use super::probe::IntakeFactsV1;
use crate::hook::{
    AcceptedHook, BindingRule, HookAdapter, HookEvent, Interpretation, MvpHooks, TurnMark,
};
use crate::store::{
    ApprovalRow, BlobInput, CountedClasses, EventAnnex, InputRow, LaunchRow, ReadTx, StoreError,
    WriteBasis, prepare_content,
};

/// What one hook says, as a decoder reads its payload.
#[derive(Debug, Clone, PartialEq)]
pub enum HookFact {
    SessionStart,
    PromptSubmitted {
        prompt: String,
    },
    Stop,
    Interrupt,
    PostDecision,
    CardOpened {
        tool_name: String,
        tool_input: serde_json::Value,
        turn_ref: Option<String>,
        correlation: Option<ApprovalCorrelation>,
    },
    /// A `PostToolUse`: a correlated card with its `tool_use_id` ran, else the one open card
    /// with its `tool_name` and canonical `tool_input` (§2.5 `CardEvidence`).
    CardEvidence {
        tool_name: String,
        tool_input: serde_json::Value,
        tool_use_id: Option<String>,
    },
    /// The decoder could not read the payload (a panic in its parser, or a field its fact
    /// needs missing): the body is kept as `raw_unknown` `schema_mismatch`.
    SchemaMismatch,
    Other,
}

/// Reads one agent's hook payloads (PR 13 and PR 14 supply the real ones).
pub trait HookDecoder: Send + Sync + 'static {
    /// Data only, as `interpret`. The events are the adapter's own (assistant text, tool
    /// activity).
    ///
    /// # Errors
    ///
    /// A store error (the job rolls back).
    fn decode(
        &self,
        tx: &ReadTx<'_>,
        hook: &AcceptedHook<'_>,
    ) -> Result<(HookFact, Vec<HookEvent>), StoreError>;
}

/// Every hook is `Other` with no events: until a CLI's hooks are configured, a hook that does
/// arrive stays `raw_unknown`.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoDecoder;

impl HookDecoder for NoDecoder {
    fn decode(
        &self,
        _tx: &ReadTx<'_>,
        _hook: &AcceptedHook<'_>,
    ) -> Result<(HookFact, Vec<HookEvent>), StoreError> {
        Ok((HookFact::Other, Vec::new()))
    }
}

/// The decoder of each agent (send §2.5 "Decoder seam"): `ClaudeDecoder` for Claude and
/// `NoDecoder` for Codex by default; a test may pass one decoder for both.
#[doc(hidden)]
#[derive(Clone)]
pub struct Decoders {
    pub claude: Arc<dyn HookDecoder>,
    pub codex: Arc<dyn HookDecoder>,
}

impl Decoders {
    /// The daemon's: `ClaudeDecoder` and `NoDecoder`.
    #[must_use]
    pub fn mvp() -> Decoders {
        Decoders {
            claude: Arc::new(super::claude::ClaudeDecoder::default()),
            codex: Arc::new(NoDecoder),
        }
    }

    /// `d` for every agent.
    #[must_use]
    pub fn all(d: Arc<dyn HookDecoder>) -> Decoders {
        Decoders {
            claude: d.clone(),
            codex: d,
        }
    }

    #[must_use]
    pub fn of(&self, agent: Agent) -> &Arc<dyn HookDecoder> {
        match agent {
            Agent::Claude => &self.claude,
            Agent::Codex => &self.codex,
        }
    }
}

/// `c`'s `tool_input` as JSON: inline UTF-8, or its blob from `thread`'s stream; `None` when
/// neither reads.
fn card_input(
    tx: &ReadTx<'_>,
    thread: chaldea_core::ids::ThreadId,
    c: &ApprovalRow,
) -> Result<Option<serde_json::Value>, StoreError> {
    let bytes = match &c.requested.tool_input {
        Content::Inline {
            inline,
            encoding: Encoding::Utf8,
        } => inline.as_bytes().to_vec(),
        Content::Inline { .. } => return Ok(None),
        Content::Blob { blob, .. } => match tx.blob(StreamId::Thread(thread), &blob.sha256)? {
            Some(b) => b.data,
            None => return Ok(None),
        },
    };
    Ok(serde_json::from_slice(&bytes).ok())
}

/// Whether `a` and `b` have one canonical JSON form.
fn same_json(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    match (CanonicalJson::of(a), CanonicalJson::of(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// The adapter: `MvpHooks`'s binding rules, the decoder's facts as state.
pub(crate) struct SupervisorHooks {
    pub host_os: HostOs,
    /// The send side: the decoder, the test's readiness mode, and each launch's turn
    /// watermark.
    pub side: std::sync::Arc<super::SendSide>,
}

/// A delivery's events under construction, with the thread's state as they change it.
struct Events {
    launch: LaunchId,
    state: ThreadState,
    out: Vec<HookEvent>,
}

impl Events {
    fn push(&mut self, name: impl Into<Cow<'static, str>>, event: TypedEvent) {
        self.push_full(name, event, None, Vec::new());
    }

    fn push_full(
        &mut self,
        name: impl Into<Cow<'static, str>>,
        event: TypedEvent,
        annex: Option<EventAnnex>,
        blobs: Vec<BlobInput>,
    ) {
        self.out.push(HookEvent {
            event,
            name: name.into(),
            annex,
            blobs,
        });
    }

    /// Applies `input` through the reducer and queues its `status_changed` events; a
    /// refusal changes nothing.
    fn apply(&mut self, name: &str, input: &StateInput) {
        if let Ok((next, changes)) = reduce(&self.state, input) {
            for (i, c) in changes.into_iter().enumerate() {
                self.push(format!("status:{name}:{i}"), TypedEvent::StatusChanged(c));
            }
            self.state = next;
        }
    }

    /// Moves `input` by `by` (and its attention), keyed `input:<id>:<n>`.
    fn step(&mut self, input: &mut InputRow, by: &InputInput, n: &str) {
        let Ok(to) = step(input.state, by) else {
            return;
        };
        let id = input.input_id;
        self.push(
            format!("input:{id}:{n}"),
            TypedEvent::InputStateChanged(InputStateChanged {
                input_id: id,
                from: input.state,
                to,
                reason: InputReason::from(by),
                sent_after_seq: None,
                by: Some(Actor::Daemon),
            }),
        );
        let unconfirmed = AttentionCause::InputUnconfirmed { input_id: id };
        if to == InputState::Unconfirmed && !self.state.attention.contains(&unconfirmed) {
            self.apply(
                &format!("unconfirmed:{id}"),
                &StateInput::AttentionRaised(unconfirmed),
            );
        }
        if input.state == InputState::Unconfirmed && self.state.attention.contains(&unconfirmed) {
            self.apply(
                &format!("unconfirmed_cleared:{id}"),
                &StateInput::AttentionCleared(unconfirmed),
            );
        }
        input.state = to;
    }

    /// Closes every open card of the launch as `superseded`, each named
    /// `card:<approval_id>`.
    fn close_cards(
        &mut self,
        tx: &ReadTx<'_>,
        thread: chaldea_core::ids::ThreadId,
    ) -> Result<(), StoreError> {
        for c in tx.open_approvals(thread)? {
            if c.launch_id != self.launch {
                continue;
            }
            self.push(
                format!("card:{}", c.approval_id),
                TypedEvent::ApprovalResolved(ApprovalResolved {
                    approval_id: c.approval_id,
                    outcome: ApprovalOutcome::Superseded,
                }),
            );
        }
        Ok(())
    }

    /// `readiness_observed` with its basis and its `status_changed`.
    fn readiness(&mut self, kind: ReadinessEvidence, position: u64) {
        self.push_full(
            "readiness",
            TypedEvent::ReadinessObserved(ReadinessObserved {
                launch_id: self.launch,
                evidence: kind,
                by: Actor::Daemon,
            }),
            Some(EventAnnex::Readiness(WriteBasis {
                launch_id: self.launch,
                position,
                counted: CountedClasses::ALL,
            })),
            Vec::new(),
        );
        self.apply("readiness", &StateInput::ReadinessObserved);
    }
}

impl SupervisorHooks {
    fn mode(&self, agent: Agent) -> ReadinessMode {
        self.side
            .mode
            .unwrap_or_else(|| capabilities(agent, self.host_os).readiness_mode())
    }

    /// Hook evidence of `kind` is recorded: the row allows it, the delivery's facts are
    /// reliable and present, and no input unit may still write.
    fn evidence(
        &self,
        l: &LaunchRow,
        kind: ReadinessEvidence,
        hook: &AcceptedHook<'_>,
        facts: Option<&IntakeFactsV1>,
    ) -> Option<u64> {
        let f = facts?;
        (evidence_allowed(l.agent, self.mode(l.agent), kind) && hook.intake_reliable && !f.running)
            .then(|| f.position())
    }
}

impl SupervisorHooks {
    /// The coarse rule (C1 D15): the one open card of the launch with an equal `tool_name` and
    /// canonical `tool_input`, uncorrelated, when this `PostToolUse` was received above the
    /// turn watermark and above that card's own delivery. Anything else resolves nothing; the
    /// turn boundary closes the card.
    fn coarse<'a>(
        &self,
        tx: &ReadTx<'_>,
        hook: &AcceptedHook<'_>,
        open: &'a [ApprovalRow],
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) -> Result<Option<&'a ApprovalRow>, StoreError> {
        if hook.receipt_ordinal <= hook.turn_watermark {
            return Ok(None);
        }
        let mut equal = Vec::new();
        for c in open {
            if c.requested.tool_name == tool_name
                && card_input(tx, hook.thread, c)?.is_some_and(|v| same_json(&v, tool_input))
            {
                equal.push(c);
            }
        }
        let [c] = equal.as_slice() else {
            return Ok(None);
        };
        if c.requested.correlation.is_some() {
            return Ok(None);
        }
        let Some(card) = tx.hook_delivery(hook.launch.launch_id, c.approval_id.0)? else {
            return Ok(None);
        };
        Ok((hook.receipt_ordinal > card.receipt_ordinal).then_some(*c))
    }
}

impl HookAdapter for SupervisorHooks {
    fn rule(&self, agent: Agent) -> BindingRule {
        MvpHooks.rule(agent)
    }

    fn expected_session(
        &self,
        tx: &ReadTx<'_>,
        launch: &LaunchRow,
    ) -> Result<Option<String>, StoreError> {
        MvpHooks.expected_session(tx, launch)
    }

    fn turn_watermark(
        &self,
        launch: LaunchId,
    ) -> Option<std::sync::Arc<std::sync::atomic::AtomicU64>> {
        // The send side exists from the reservation job until the launch left the live map,
        // whether or not its exit job committed. Without it no boundary is folded: every
        // one counts as stale (§2.4 "Turn boundaries in receipt order").
        Some(self.side.launch(launch).map_or_else(
            || std::sync::Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX)),
            |sl| sl.turn_watermark.clone(),
        ))
    }

    fn interpret(
        &self,
        tx: &ReadTx<'_>,
        hook: &AcceptedHook<'_>,
    ) -> Result<(Interpretation, TurnMark), StoreError> {
        let l = hook.launch;
        let (fact, extra) = self.side.decoders.of(l.agent).decode(tx, hook)?;
        if fact == HookFact::SchemaMismatch {
            return Ok((
                Interpretation::Raw(RawUnknownReason::SchemaMismatch),
                TurnMark::Other,
            ));
        }
        let thread = hook.thread;
        let Some(t) = tx.thread(thread)? else {
            return Ok((
                Interpretation::Raw(RawUnknownReason::UnknownEvent),
                TurnMark::Other,
            ));
        };
        let facts = IntakeFactsV1::parse(hook.intake);
        let facts = facts.as_ref();
        let mut ev = Events {
            launch: l.launch_id,
            state: t.state,
            out: Vec::new(),
        };
        // A turn boundary below the launch's turn watermark committed after a later boundary:
        // its events stay, but it changes neither the turn, the cards, nor the turn-start
        // basis (§2.4 "Turn boundaries in receipt order").
        let stale = hook.receipt_ordinal < hook.turn_watermark;
        let mark = match fact {
            HookFact::SessionStart => {
                // Only the binding `SessionStart` is evidence.
                let binding = !hook.promoted && hook.binding_ordinal == hook.receipt_ordinal;
                if binding
                    && let Some(p) = self.evidence(l, ReadinessEvidence::SessionStart, hook, facts)
                {
                    ev.readiness(ReadinessEvidence::SessionStart, p);
                }
                TurnMark::Other
            }
            HookFact::PromptSubmitted { prompt } => {
                let a = attribute(
                    tx,
                    thread,
                    l.launch_id,
                    l.agent,
                    &prompt,
                    facts,
                    hook.intake_reliable,
                )?;
                let prepared = prepare_content(prompt.into_bytes(), "text/plain; charset=utf-8")
                    .map_err(StoreError::Content)?;
                let acked: Option<InputId> = a
                    .attributable
                    .then(|| a.matched.as_ref().map(|x| x.input_id))
                    .flatten();
                ev.push_full(
                    "prompt",
                    TypedEvent::UserMessage(UserMessage {
                        prompt: prepared.content,
                        input_id: acked,
                    }),
                    stale.then_some(EventAnnex::Prompt { superseded: true }),
                    prepared.blob.into_iter().collect(),
                );
                if let Some(mut x) = a.matched {
                    match (a.attributable, x.state) {
                        (true, InputState::Sending) => {
                            ev.step(&mut x, &InputInput::WriteCompleted, "written");
                            ev.step(&mut x, &InputInput::AckAttributed, "acked");
                        }
                        (true, _) => ev.step(&mut x, &InputInput::AckAttributed, "acked"),
                        (false, InputState::Sent) => {
                            ev.step(&mut x, &InputInput::AckNotAttributable, "unattributed");
                        }
                        (false, _) => {}
                    }
                }
                if !stale {
                    ev.apply("prompt", &StateInput::PromptSubmitted);
                    ev.close_cards(tx, thread)?;
                }
                TurnMark::Start
            }
            HookFact::Stop => {
                if !stale {
                    ev.apply("stop", &StateInput::TurnStopped);
                    ev.close_cards(tx, thread)?;
                }
                // A unit that ran to `Written` before the intake, with its input already acked
                // by a hook (its own turn's `UserPromptSubmit`: `acked` with an `ack_seq`; an
                // owner's resolution sets none), ended before this `Stop`; the settle waits for
                // its record (§2.4 step 3). Any other running unit's turn may still run.
                let written = facts
                    .filter(|f| f.running)
                    .and_then(|f| f.inflight.as_ref().filter(|i| i.written));
                let settle = match (hook.intake_reliable, facts, written) {
                    (true, Some(f), _) if !f.running => true,
                    (true, Some(_), Some(i)) => tx
                        .input(i.input_id)?
                        .is_some_and(|r| r.state == InputState::Acked && r.ack_seq.is_some()),
                    _ => false,
                };
                TurnMark::End { settle }
            }
            HookFact::Interrupt => {
                if !stale {
                    ev.apply("interrupt", &StateInput::Interrupted);
                    ev.close_cards(tx, thread)?;
                }
                TurnMark::End { settle: false }
            }
            HookFact::CardOpened {
                tool_name,
                tool_input,
                turn_ref,
                correlation,
            } => {
                let approval_id = ApprovalId(hook.emit_id);
                let input = serde_json::to_vec(&tool_input)
                    .map_err(|e| StoreError::Decode(e.to_string()))?;
                let prepared =
                    prepare_content(input, "application/json").map_err(StoreError::Content)?;
                ev.push_full(
                    "card",
                    TypedEvent::ApprovalRequested(ApprovalRequested {
                        approval_id,
                        launch_id: l.launch_id,
                        turn_ref,
                        tool_name,
                        tool_input: prepared.content,
                        correlation,
                    }),
                    None,
                    prepared.blob.into_iter().collect(),
                );
                if hook.receipt_ordinal < hook.turn_watermark {
                    // Taken in before a committed turn boundary: its turn already ended, so the
                    // card is closed at open and keeps no memory entry (its key answers
                    // `card_closed`).
                    ev.push(
                        format!("card:{approval_id}"),
                        TypedEvent::ApprovalResolved(ApprovalResolved {
                            approval_id,
                            outcome: ApprovalOutcome::Superseded,
                        }),
                    );
                    TurnMark::Other
                } else {
                    ev.apply("card", &StateInput::ApprovalOpened);
                    let human_at = facts
                        .filter(|f| hook.intake_reliable && !f.human_running)
                        .map(|f| f.at(OriginClass::Human));
                    TurnMark::Card {
                        approval_id,
                        human_at,
                    }
                }
            }
            HookFact::CardEvidence {
                tool_name,
                tool_input,
                tool_use_id,
            } => {
                let open: Vec<_> = tx
                    .open_approvals(thread)?
                    .into_iter()
                    .filter(|c| c.launch_id == l.launch_id)
                    .collect();
                let correlated = tool_use_id.as_ref().and_then(|id| {
                    open.iter().find(|c| {
                        c.requested
                            .correlation
                            .as_ref()
                            .is_some_and(|k| &k.tool_use_id == id)
                    })
                });
                let hit = match correlated {
                    Some(c) => Some(c),
                    None => self.coarse(tx, hook, &open, &tool_name, &tool_input)?,
                };
                let mark = hit.map_or(TurnMark::Other, |c| TurnMark::Resolved {
                    approval_id: c.approval_id,
                });
                if let Some(c) = hit {
                    ev.push(
                        format!("card:{}", c.approval_id),
                        TypedEvent::ApprovalResolved(ApprovalResolved {
                            approval_id: c.approval_id,
                            outcome: ApprovalOutcome::Ran,
                        }),
                    );
                    if open.len() == 1 {
                        ev.apply("resolved", &StateInput::ApprovalsAllResolved);
                    }
                }
                mark
            }
            HookFact::PostDecision => {
                if let Some(p) = self.evidence(l, ReadinessEvidence::PostDecision, hook, facts) {
                    ev.readiness(ReadinessEvidence::PostDecision, p);
                }
                TurnMark::Other
            }
            HookFact::Other | HookFact::SchemaMismatch => TurnMark::Other,
        };
        let _ = StreamId::Thread(thread);
        let mut events = ev.out;
        events.extend(extra);
        if events.is_empty() {
            return Ok((Interpretation::Raw(RawUnknownReason::UnknownEvent), mark));
        }
        Ok((Interpretation::Events(events), mark))
    }
}
