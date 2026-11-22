//! Event rows: reading them back into envelopes, and the idempotency comparison
//! (`docs/design/event-store.md` §2.3 step 2).

use std::str::FromStr;

use chaldea_core::event::{EventEnvelope, Source};
use chaldea_core::ids::{
    CommandId, EventId, EventKindTag, IdempotencyKey, LaunchId, StreamId, ThreadId, Timestamp,
};
use rusqlite::Row;
use serde_json::value::RawValue;

use super::annex::EventAnnex;
use super::error::StoreError;
use super::rows::{opt_uuid, parse_json, parse_uuid, to_u64};

/// The `events` columns `RawRow::read` expects, in order.
pub(crate) const EVENT_COLUMNS: &str = "stream_id, seq, event_id, schema_version, \
     idempotency_key, kind, thread_id, launch_id, command_id, source, occurred_at, \
     occurred_explicit, recorded_at, payload, annex";

/// One `events` row as stored.
#[derive(Debug, Clone)]
pub(crate) struct RawRow {
    stream_id: String,
    seq: i64,
    event_id: String,
    schema_version: i64,
    key: String,
    kind: String,
    thread_id: Option<String>,
    launch_id: Option<String>,
    command_id: Option<String>,
    source: String,
    occurred_at: String,
    occurred_explicit: i64,
    recorded_at: String,
    payload: String,
    annex: Option<String>,
}

impl RawRow {
    pub(crate) fn read(r: &Row<'_>) -> rusqlite::Result<RawRow> {
        Ok(RawRow {
            stream_id: r.get(0)?,
            seq: r.get(1)?,
            event_id: r.get(2)?,
            schema_version: r.get(3)?,
            key: r.get(4)?,
            kind: r.get(5)?,
            thread_id: r.get(6)?,
            launch_id: r.get(7)?,
            command_id: r.get(8)?,
            source: r.get(9)?,
            occurred_at: r.get(10)?,
            occurred_explicit: r.get(11)?,
            recorded_at: r.get(12)?,
            payload: r.get(13)?,
            annex: r.get(14)?,
        })
    }

    pub(crate) fn decode(self) -> Result<StoredEvent, StoreError> {
        let envelope = EventEnvelope {
            stream_id: StreamId::from_str(&self.stream_id).map_err(StoreError::decode)?,
            seq: to_u64(self.seq)?,
            event_id: EventId(parse_uuid(&self.event_id)?),
            schema_version: u32::try_from(self.schema_version).map_err(StoreError::decode)?,
            idempotency_key: IdempotencyKey(self.key),
            kind: EventKindTag(self.kind),
            thread_id: opt_uuid(self.thread_id.as_deref())?.map(ThreadId),
            launch_id: opt_uuid(self.launch_id.as_deref())?.map(LaunchId),
            command_id: opt_uuid(self.command_id.as_deref())?.map(CommandId),
            source: parse_json::<Source>(&self.source)?,
            occurred_at: Timestamp(self.occurred_at),
            recorded_at: Timestamp(self.recorded_at),
            payload: RawValue::from_string(self.payload).map_err(StoreError::decode)?,
        };
        Ok(StoredEvent {
            envelope,
            occurred_explicit: self.occurred_explicit != 0,
            annex_text: self.annex,
        })
    }
}

/// An event as `project::apply` and the idempotency check see it.
#[derive(Debug, Clone)]
pub(crate) struct StoredEvent {
    pub(crate) envelope: EventEnvelope,
    pub(crate) occurred_explicit: bool,
    pub(crate) annex_text: Option<String>,
}

impl StoredEvent {
    /// The annex, decoded; `Err` when the stored text is not an annex.
    pub(crate) fn annex(&self) -> Result<Option<EventAnnex>, StoreError> {
        self.annex_text.as_deref().map(parse_json).transpose()
    }
}

/// A new event's producer-supplied fields, as the idempotency check compares them.
pub(crate) struct Candidate<'a> {
    pub(crate) kind: &'a str,
    pub(crate) thread_id: Option<ThreadId>,
    pub(crate) launch_id: Option<LaunchId>,
    pub(crate) command_id: Option<CommandId>,
    pub(crate) source: &'a Source,
    pub(crate) annex: Option<&'a EventAnnex>,
    pub(crate) payload: &'a str,
    pub(crate) occurred_at: Option<&'a Timestamp>,
}

/// The first producer-supplied field in which `stored` differs from `new`, if any.
pub(crate) fn first_difference(stored: &StoredEvent, new: &Candidate<'_>) -> Option<&'static str> {
    let e = &stored.envelope;
    if e.kind.as_str() != new.kind {
        return Some("kind");
    }
    if e.thread_id != new.thread_id {
        return Some("thread_id");
    }
    if e.launch_id != new.launch_id {
        return Some("launch_id");
    }
    if e.command_id != new.command_id {
        return Some("command_id");
    }
    if &e.source != new.source {
        return Some("source");
    }
    match stored.annex() {
        Ok(a) if a.as_ref() == new.annex => {}
        _ => return Some("annex"),
    }
    let same_payload = matches!(
        (
            serde_json::from_str::<serde_json::Value>(e.payload.get()),
            serde_json::from_str::<serde_json::Value>(new.payload),
        ),
        (Ok(a), Ok(b)) if a == b
    );
    if !same_payload {
        return Some("payload");
    }
    let same_time = match new.occurred_at {
        None => !stored.occurred_explicit,
        Some(t) => stored.occurred_explicit && &e.occurred_at == t,
    };
    if !same_time {
        return Some("occurred_at");
    }
    None
}
