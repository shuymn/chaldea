//! The delivery job (`docs/design/hook-ingest.md` §2.7): dedup, decide, record, bind, and
//! promote, in the delivery's one store transaction. Every target is the re-verified
//! token's.

use std::collections::HashSet;
use std::sync::Arc;

use axum::body::Bytes;
use chaldea_core::event::{Source, TypedEvent};
use chaldea_core::ids::{Agent, IdempotencyKey, LaunchId, Open, StreamId, ThreadId};
use chaldea_core::payload::{RawUnknown, RawUnknownReason, SessionBound, SourceKind};
use chaldea_core::state::{StateInput, reduce};
use tokio::sync::OwnedSemaphorePermit;
use uuid::Uuid;

use super::HookConfig;
use super::adapter::{AcceptedHook, BindingRule, HookAdapter, HookEvent, Interpretation};
use super::ancestry::{Chain, eligible};
use super::buffer::{self, Meta};
use super::intake::IntakeFacts;
use super::payload::{Decoded, HookHead, decode};
use super::request::HookHeaders;
use crate::http::{ApiError, Authorized};
use crate::store::{
    BufferedHook, EventAnnex, HookDelivery, HookDeliveryLookup, HookReply, JobError, LaunchRow,
    NewEvent, NewHookDelivery, NewRouting, Promotion, StoreError, ThreadRow, WriteTx,
    prepare_content,
};

pub(crate) const JSON: &str = "application/json";
pub(crate) const OCTET: &str = "application/octet-stream";

/// The first reply of every recorded delivery.
pub(crate) fn first_reply() -> HookReply {
    HookReply {
        http_status: 200,
        body: String::new(),
    }
}

/// `hook:<emit_id>` (mvp §2.1).
pub(crate) fn hook_key(emit: Uuid) -> IdempotencyKey {
    IdempotencyKey(format!("hook:{emit}"))
}

/// `derived:hook:<emit_id>:<name>` (mvp §2.1 "Derived event").
pub(crate) fn derived_key(emit: Uuid, name: &str) -> IdempotencyKey {
    IdempotencyKey(format!("derived:hook:{emit}:{name}"))
}

fn agent_name(a: Agent) -> &'static str {
    match a {
        Agent::Claude => "claude",
        Agent::Codex => "codex",
    }
}

/// A hook event's envelope: the thread's stream, the launch, a hook source.
fn envelope(
    thread: ThreadId,
    l: &LaunchRow,
    key: IdempotencyKey,
    event: TypedEvent,
    annex: Option<EventAnnex>,
    blobs: Vec<crate::store::BlobInput>,
) -> NewEvent {
    NewEvent {
        stream_id: StreamId::Thread(thread),
        idempotency_key: key,
        event,
        thread_id: Some(thread),
        launch_id: Some(l.launch_id),
        command_id: None,
        source: Source {
            kind: Open::Known(SourceKind::Hook),
            agent: Some(Open::Known(l.agent)),
            cli_version: l.cli_version.clone(),
            transcript: None,
        },
        occurred_at: None,
        annex,
        blobs,
    }
}

/// A hook's `raw_unknown`, keeping `body` exactly (inline, base64, or a blob above 64 KiB).
pub(crate) fn raw_event(
    thread: ThreadId,
    l: &LaunchRow,
    key: IdempotencyKey,
    reason: RawUnknownReason,
    content_type: &str,
    body: Vec<u8>,
    annex: Option<EventAnnex>,
) -> Result<NewEvent, StoreError> {
    let prepared = prepare_content(body, content_type).map_err(StoreError::Content)?;
    let event = TypedEvent::RawUnknown(RawUnknown {
        source: SourceKind::Hook,
        reason,
        agent: Some(l.agent),
        cli_version: l.cli_version.clone(),
        content_type: content_type.to_owned(),
        raw: prepared.content,
    });
    Ok(envelope(
        thread,
        l,
        key,
        event,
        annex,
        prepared.blob.into_iter().collect(),
    ))
}

/// The envelopes of an interpretation's events: the first keyed `first`, the rest by name.
/// A name starting with `bind` or repeated within the delivery refuses the job (500).
fn hook_events(
    thread: ThreadId,
    l: &LaunchRow,
    emit: Uuid,
    first: Option<IdempotencyKey>,
    events: Vec<HookEvent>,
) -> Result<Vec<NewEvent>, JobError> {
    if events.is_empty() {
        return Err(StoreError::BadRouting("an interpretation without events").into());
    }
    let mut taken = HashSet::new();
    let mut first = first;
    let mut out = Vec::with_capacity(events.len());
    for e in events {
        if e.name.starts_with("bind") || !taken.insert(e.name) {
            return Err(super::refused_internal());
        }
        let key = first.take().unwrap_or_else(|| derived_key(emit, e.name));
        out.push(envelope(thread, l, key, e.event, e.annex, e.blobs));
    }
    Ok(out)
}

/// The decoded body as the job keeps it.
#[derive(Debug, Clone)]
pub(crate) enum Head {
    Hook(HookHead<'static>),
    Unparseable,
    SchemaMismatch,
}

impl From<Decoded<'_>> for Head {
    fn from(d: Decoded<'_>) -> Self {
        match d {
            Decoded::Hook(h) => Head::Hook(h.into_owned()),
            Decoded::Unparseable => Head::Unparseable,
            Decoded::SchemaMismatch => Head::SchemaMismatch,
        }
    }
}

/// Everything the job closure captures: data only.
pub(crate) struct Delivery {
    pub(crate) headers: HookHeaders,
    pub(crate) body: Bytes,
    /// The body's permits: held, never read, until the writer drops the closure.
    pub(crate) _permit: OwnedSemaphorePermit,
    pub(crate) head: Head,
    pub(crate) chain: Chain,
    pub(crate) facts: Option<IntakeFacts>,
    pub(crate) ordinal: u64,
    pub(crate) pid_wait_expired: bool,
    pub(crate) adapter: Arc<dyn HookAdapter>,
    pub(crate) cfg: HookConfig,
}

/// The routing a delivery is recorded with.
enum Decision {
    Raw {
        reason: RawUnknownReason,
        content_type: &'static str,
        annex: Option<EventAnnex>,
    },
    Interpret,
    Bind,
    Buffer,
    Overflow,
}

fn raw(reason: RawUnknownReason) -> Decision {
    Decision::Raw {
        reason,
        content_type: JSON,
        annex: None,
    }
}

fn unparseable(truncated: bool) -> Decision {
    Decision::Raw {
        reason: RawUnknownReason::Unparseable,
        content_type: OCTET,
        annex: truncated.then_some(EventAnnex::Hook { truncated: true }),
    }
}

/// The job: §2.7 steps 1–7.
pub(crate) fn run(
    d: &Delivery,
    tx: &mut WriteTx<'_>,
    auth: Option<&Authorized>,
) -> Result<HookReply, JobError> {
    let (Some(thread), Some(launch)) = (auth.and_then(|a| a.thread), auth.and_then(|a| a.launch))
    else {
        return Err(super::refused_internal());
    };
    let emit = d.headers.emit_id;
    // 1. Dedup.
    if let Some(existing) = tx.read().hook_delivery(launch, emit)? {
        return Ok(existing.first_reply);
    }
    // 2. `emit_id` reuse by another launch of the thread: its delivery, buffered ones
    // included, or an event under its key.
    if tx.read().hook_emit_in_thread(thread, emit)?
        || tx
            .read()
            .event_by_key(StreamId::Thread(thread), &hook_key(emit))?
            .is_some()
    {
        return Err(crate::http::refused(ApiError::bad_request()));
    }
    let (Some(l), Some(t)) = (tx.read().launch(launch)?, tx.read().thread(thread)?) else {
        return Err(super::refused_internal());
    };
    // 3. Expire while unbound.
    if l.session_id.is_none() {
        buffer::expire(tx, launch, &d.cfg)?;
    }
    // 4. Expand, then decide.
    let json = match (&d.head, d.headers.truncated) {
        (Head::Hook(_), false) => serde_json::from_slice::<serde_json::Value>(&d.body).ok(),
        _ => None,
    };
    let decision = decide(d, tx, &l, &t, thread, json.is_some())?;
    let overflow = matches!(decision, Decision::Overflow);
    let body = || d.body.to_vec();
    // 5. Record.
    let routing = match decision {
        Decision::Raw {
            reason,
            content_type,
            annex,
        } => NewRouting::RawUnknown(raw_event(
            thread,
            &l,
            hook_key(emit),
            reason,
            content_type,
            body(),
            annex,
        )?),
        Decision::Interpret => {
            let Head::Hook(h) = &d.head else {
                return Err(super::refused_internal());
            };
            let binding_ordinal = tx
                .read()
                .binding_receipt_ordinal(thread, launch)?
                .unwrap_or(0);
            let json = json.as_ref().ok_or_else(super::refused_internal)?;
            let hook = accepted(d, thread, &l, h, json, binding_ordinal);
            let interpretation = d.adapter.interpret(&tx.read(), &hook)?;
            match interpretation {
                Interpretation::Events(events) => NewRouting::Appended(hook_events(
                    thread,
                    &l,
                    emit,
                    Some(hook_key(emit)),
                    events,
                )?),
                Interpretation::Raw(reason) => NewRouting::RawUnknown(raw_event(
                    thread,
                    &l,
                    hook_key(emit),
                    reason,
                    JSON,
                    body(),
                    None,
                )?),
            }
        }
        Decision::Bind => {
            let Head::Hook(h) = &d.head else {
                return Err(super::refused_internal());
            };
            let (_, changes) = reduce(&t.state, &StateInput::SessionBound)
                .map_err(|_| super::refused_internal())?;
            let mut events = vec![envelope(
                thread,
                &l,
                hook_key(emit),
                TypedEvent::SessionBound(SessionBound {
                    launch_id: launch,
                    session_id: h.session_id.to_string(),
                    transcript_path: h.transcript_path.as_deref().map(str::to_owned),
                }),
                None,
                Vec::new(),
            )];
            for (i, c) in changes.into_iter().enumerate() {
                events.push(envelope(
                    thread,
                    &l,
                    derived_key(emit, &format!("bind_status:{i}")),
                    TypedEvent::StatusChanged(c),
                    None,
                    Vec::new(),
                ));
            }
            NewRouting::Appended(events)
        }
        Decision::Buffer => {
            let meta = Meta {
                v: Meta::VERSION,
                thread_header: d.headers.thread,
                agent_header: d.headers.agent.clone(),
                emitter_version: d.headers.emitter_version.clone(),
                attempt: d.headers.attempt,
                chain: d.chain.links.clone(),
                chain_end: d.chain.end.clone(),
                intake: d.facts.clone(),
            };
            let meta =
                serde_json::to_string(&meta).map_err(|e| StoreError::Decode(e.to_string()))?;
            NewRouting::Buffered(BufferedHook { body: body(), meta })
        }
        Decision::Overflow => {
            // Receipt order: the buffered deliveries received before this one first, the
            // later ones after it is recorded.
            buffer::drain_unbound(tx, launch, |o| o < d.ordinal)?;
            NewRouting::RawUnknown(raw_event(
                thread,
                &l,
                hook_key(emit),
                RawUnknownReason::Unbound,
                JSON,
                body(),
                None,
            )?)
        }
    };
    let bind = matches!(routing, NewRouting::Appended(_))
        && matches!(&d.head, Head::Hook(h) if h.is_session_start())
        && l.session_id.is_none();
    let recorded = tx.hook_delivery_record(NewHookDelivery {
        launch_id: launch,
        emit_id: emit,
        receipt_ordinal: d.ordinal,
        first_reply: first_reply(),
        routing,
    })?;
    let reply = match recorded {
        HookDeliveryLookup::New(HookDelivery { first_reply, .. })
        | HookDeliveryLookup::Replay(HookDelivery { first_reply, .. }) => first_reply,
    };
    if overflow {
        buffer::drain_unbound(tx, launch, |_| true)?;
    }
    // 6. Bind.
    if bind {
        bind_and_promote(d, tx, thread, launch, json.as_ref())?;
    }
    Ok(reply)
}

fn accepted<'a>(
    d: &'a Delivery,
    thread: ThreadId,
    l: &'a LaunchRow,
    h: &'a HookHead<'static>,
    json: &'a serde_json::Value,
    binding_ordinal: u64,
) -> AcceptedHook<'a> {
    AcceptedHook {
        thread,
        launch: l,
        emit_id: d.headers.emit_id,
        receipt_ordinal: d.ordinal,
        event_name: &h.event,
        session_id: &h.session_id,
        json,
        body: &d.body,
        intake: d.facts.as_ref(),
        attempt: d.headers.attempt,
        intake_reliable: d.headers.attempt == Some(1),
        binding_ordinal,
        promoted: false,
    }
}

/// Whether the chain is eligible for `l`'s recorded identity.
fn chain_eligible(links: &[super::ancestry::Link], l: &LaunchRow, cfg: &HookConfig) -> bool {
    match (l.pid, l.start_time.as_deref()) {
        (Some(pid), Some(start)) => {
            eligible(links, pid, start, agent_name(l.agent), &cfg.emitter_name)
        }
        _ => false,
    }
}

/// The routing table (§2.7), first matching row.
fn decide(
    d: &Delivery,
    tx: &WriteTx<'_>,
    l: &LaunchRow,
    t: &ThreadRow,
    thread: ThreadId,
    expanded: bool,
) -> Result<Decision, JobError> {
    use RawUnknownReason as R;
    // Rows 1–2.
    if d.headers.truncated {
        return Ok(unparseable(true));
    }
    let h = match &d.head {
        Head::Unparseable => return Ok(unparseable(false)),
        Head::Hook(_) if !expanded => return Ok(unparseable(false)),
        Head::SchemaMismatch => return Ok(raw(R::SchemaMismatch)),
        Head::Hook(h) => h,
    };
    // Row 3.
    if t.current_launch_id != Some(l.launch_id) {
        return Ok(raw(R::Unbound));
    }
    // Row 4.
    if d.headers.thread != Some(thread) {
        return Ok(raw(R::ForeignSession));
    }
    // Rows 5–6.
    if l.exit_reason.is_some() || l.pid.is_none() || d.pid_wait_expired {
        return Ok(raw(R::Unbound));
    }
    let rule = d.adapter.rule(l.agent);
    let eligible = || chain_eligible(&d.chain.links, l, &d.cfg);
    // Rows 7–8.
    if let Some(s) = &l.session_id {
        if h.session_id != s.as_str() || (rule == BindingRule::Ancestry && !eligible()) {
            return Ok(raw(R::ForeignSession));
        }
        return Ok(Decision::Interpret);
    }
    let selected = match rule {
        BindingRule::SessionId => {
            let expected = d.adapter.expected_session(&tx.read(), l)?;
            expected.as_deref() == Some(&*h.session_id)
        }
        BindingRule::Ancestry => eligible(),
    };
    // Rows 9–11.
    if h.is_session_start() {
        if !selected {
            return Ok(raw(R::ForeignSession));
        }
        return Ok(match reduce(&t.state, &StateInput::SessionBound) {
            Ok(_) => Decision::Bind,
            Err(_) => raw(R::Unbound),
        });
    }
    // Rows 12–13.
    if !selected {
        return Ok(raw(R::ForeignSession));
    }
    let summary = tx.read().buffered_summary(l.launch_id)?;
    Ok(if buffer::fits(&summary, d.body.len() as u64, &d.cfg) {
        Decision::Buffer
    } else {
        Decision::Overflow
    })
}

/// §2.7 step 6: interpret the binding `SessionStart` with the launch bound, then promote
/// the buffer in receipt order. Any error rolls back the whole job, binding included.
fn bind_and_promote(
    d: &Delivery,
    tx: &mut WriteTx<'_>,
    thread: ThreadId,
    launch: LaunchId,
    json: Option<&serde_json::Value>,
) -> Result<(), JobError> {
    let Head::Hook(h) = &d.head else {
        return Err(super::refused_internal());
    };
    let json = json.ok_or_else(super::refused_internal)?;
    let emit = d.headers.emit_id;
    let l = tx
        .read()
        .launch(launch)?
        .ok_or_else(super::refused_internal)?;
    let session = l.session_id.clone().ok_or_else(super::refused_internal)?;
    let hook = accepted(d, thread, &l, h, json, d.ordinal);
    let interpretation = d.adapter.interpret(&tx.read(), &hook)?;
    let events = match interpretation {
        Interpretation::Events(events) => hook_events(thread, &l, emit, None, events)?,
        Interpretation::Raw(reason) => vec![raw_event(
            thread,
            &l,
            derived_key(emit, "binding_raw"),
            reason,
            JSON,
            d.body.to_vec(),
            None,
        )?],
    };
    tx.append(events)?;
    let rule = d.adapter.rule(l.agent);
    let buffered = tx.read().buffered_deliveries(launch)?;
    for b in buffered {
        // Re-read per promotion: earlier promotions may have changed the launch's row.
        let l = tx
            .read()
            .launch(launch)?
            .ok_or_else(super::refused_internal)?;
        let emit_b = b.emit_id;
        let to = promotion(d, tx, thread, &l, &session, rule, b)?;
        tx.hook_delivery_promote(launch, emit_b, to)?;
    }
    Ok(())
}

/// The promotion table (§2.7) for one buffered delivery of the just-bound launch.
fn promotion(
    d: &Delivery,
    tx: &WriteTx<'_>,
    thread: ThreadId,
    l: &LaunchRow,
    session: &str,
    rule: BindingRule,
    b: crate::store::BufferedDelivery,
) -> Result<Promotion, JobError> {
    use RawUnknownReason as R;
    let emit = b.emit_id;
    let body = b.hook.body;
    let raw_of = |reason, content_type, body| -> Result<Promotion, JobError> {
        Ok(Promotion::RawUnknown(raw_event(
            thread,
            l,
            hook_key(emit),
            reason,
            content_type,
            body,
            None,
        )?))
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return raw_of(R::Unparseable, OCTET, body);
    };
    let Some(meta) = Meta::parse(&b.hook.meta) else {
        return raw_of(R::SchemaMismatch, JSON, body);
    };
    let Decoded::Hook(h) = decode(&body) else {
        return raw_of(R::SchemaMismatch, JSON, body);
    };
    let h = h.into_owned();
    if h.session_id != session
        || meta.thread_header != Some(thread)
        || (rule == BindingRule::Ancestry && !chain_eligible(&meta.chain, l, &d.cfg))
    {
        return raw_of(R::ForeignSession, JSON, body);
    }
    let hook = AcceptedHook {
        thread,
        launch: l,
        emit_id: emit,
        receipt_ordinal: b.receipt_ordinal,
        event_name: &h.event,
        session_id: &h.session_id,
        json: &json,
        body: &body,
        intake: meta.intake.as_ref(),
        attempt: meta.attempt,
        intake_reliable: meta.attempt == Some(1),
        binding_ordinal: d.ordinal,
        promoted: true,
    };
    match d.adapter.interpret(&tx.read(), &hook)? {
        Interpretation::Events(events) => Ok(Promotion::Appended(hook_events(
            thread,
            l,
            emit,
            Some(hook_key(emit)),
            events,
        )?)),
        Interpretation::Raw(reason) => raw_of(reason, JSON, body),
    }
}
