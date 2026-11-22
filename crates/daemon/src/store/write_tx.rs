//! `WriteTx`: every write operation, inside the job's one transaction
//! (`docs/design/event-store.md` §2.3).

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

use chaldea_core::command::{CommandResult, ReceiptLookup, check_receipt};
use chaldea_core::event::{EventEnvelope, Source, TranscriptPos, TypedEvent};
use chaldea_core::ids::{
    Agent, CommandId, IdempotencyKey, LaunchId, Open, StreamId, ThreadId, Timestamp, TokenId,
};
use chaldea_core::payload::{Content, SourceKind, TranscriptGeneration};
use chaldea_core::state::StatusChanged;
use chaldea_core::sync::StreamHeadEntry;
use rusqlite::{Connection, OptionalExtension};
use serde_json::value::RawValue;
use uuid::Uuid;

use super::IdSource;
use super::annex::{AnnexError, Continuity, EventAnnex};
use super::append::{Candidate, EVENT_COLUMNS, RawRow, StoredEvent, first_difference};
use super::content::{
    ContentError, MAX_BLOB_BYTES, PREVIEW_READ_BYTES, check_against, check_blob_ref, check_inline,
    contents, sha256_hex,
};
use super::error::StoreError;
use super::project::{self, ProjectionTx};
use super::readers::ReadTx;
use super::rows::{
    Binding, BufferedHook, HookDelivery, HookDeliveryLookup, HookReply, ProjectionDiff,
    RebuildReport, Routing, TokenKind, enum_text, to_i64, to_json,
};

/// A new event for `append`.
#[derive(Debug, Clone)]
pub struct NewEvent {
    pub stream_id: StreamId,
    pub idempotency_key: IdempotencyKey,
    /// Known kinds only; the store serializes the payload.
    pub event: TypedEvent,
    pub thread_id: Option<ThreadId>,
    pub launch_id: Option<LaunchId>,
    pub command_id: Option<CommandId>,
    /// `source.kind = transcript` is refused here: use `append_transcript`.
    pub source: Source,
    /// `None`: the source states no time; stored as `recorded_at`.
    pub occurred_at: Option<Timestamp>,
    /// Internal, never on the wire (§2.6).
    pub annex: Option<EventAnnex>,
    /// The bytes of every blob the event's `Content` fields reference that is not stored yet.
    pub blobs: Vec<BlobInput>,
}

/// A blob's bytes; the media type is the referencing `Content`'s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobInput {
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Appended {
    New(EventEnvelope),
    /// The key existed with equal fields: the first insert's event.
    Existing(EventEnvelope),
}

impl Appended {
    #[must_use]
    pub fn envelope(&self) -> &EventEnvelope {
        match self {
            Appended::New(e) | Appended::Existing(e) => e,
        }
    }
}

/// One transcript batch (§2.3 `append_transcript`).
#[derive(Debug, Clone)]
pub struct TranscriptBatch {
    pub thread_id: ThreadId,
    /// The launch the tailer runs for.
    pub launch_id: LaunchId,
    /// The session it is bound to.
    pub session_id: String,
    /// The bound transcript path it read.
    pub path: String,
    pub agent: Agent,
    pub cli_version: Option<String>,
    /// The cursor the tailer read from; `None`: no cursor yet.
    pub expect: Option<CursorPos>,
    /// Appended as `transcript_generation`; then `lines` must be empty.
    pub new_generation: Option<NewGeneration>,
    pub lines: Vec<TranscriptLine>,
    /// The offset just after the last complete line read.
    pub advance_to: ReadPos,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorPos {
    pub generation: u64,
    pub path: String,
    pub file_id: String,
    pub read_offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewGeneration {
    pub file_id: String,
    pub continuity: Continuity,
}

#[derive(Debug, Clone)]
pub struct TranscriptLine {
    pub offset_start: u64,
    pub offset_end: u64,
    pub event: TypedEvent,
    pub occurred_at: Option<Timestamp>,
    /// Evidence at `offset_end`.
    pub continuity: Continuity,
    pub blobs: Vec<BlobInput>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadPos {
    pub read_offset: u64,
    pub continuity: Continuity,
}

/// A hook delivery to record (§2.3 "Hook deliveries").
#[derive(Debug, Clone)]
pub struct NewHookDelivery {
    pub launch_id: LaunchId,
    pub emit_id: Uuid,
    /// PR 9's intake order for the launch.
    pub receipt_ordinal: u64,
    /// Immutable once stored.
    pub first_reply: HookReply,
    pub routing: NewRouting,
}

/// `(sha256, data)` of the blobs an event stores.
type BlobRows = Vec<(String, Vec<u8>)>;
/// `(sha256, path)` of the blob references an event records.
type RefRows = Vec<(String, String)>;

#[expect(
    clippy::large_enum_variant,
    reason = "a short-lived input value, moved once"
)]
#[derive(Debug, Clone)]
pub enum NewRouting {
    /// Non-empty.
    Appended(Vec<NewEvent>),
    Buffered(BufferedHook),
    /// A `raw_unknown` event.
    RawUnknown(NewEvent),
}

#[expect(
    clippy::large_enum_variant,
    reason = "a short-lived input value, moved once"
)]
#[derive(Debug, Clone)]
pub enum Promotion {
    /// Non-empty.
    Appended(Vec<NewEvent>),
    RawUnknown(NewEvent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewToken {
    pub token_id: TokenId,
    pub kind: TokenKind,
    pub sha256: [u8; 32],
    pub thread_id: Option<ThreadId>,
    pub launch_id: Option<LaunchId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSelector {
    Token(TokenId),
    Launch(LaunchId),
    Thread(ThreadId),
}

/// A `task_set` of this job that may justify one derived `status_changed(TaskState)`.
struct TaskCause {
    thread: ThreadId,
    from: String,
    to: String,
    used: bool,
}

/// What this job did to one thread's transcript.
#[derive(Default, Clone, Copy)]
struct TranscriptJob {
    started_generation: bool,
    continued: bool,
}

/// An event ready to store.
struct Row {
    stream_id: StreamId,
    key: IdempotencyKey,
    typed: TypedEvent,
    thread_id: Option<ThreadId>,
    launch_id: Option<LaunchId>,
    command_id: Option<CommandId>,
    source: Source,
    occurred_at: Option<Timestamp>,
    annex: Option<EventAnnex>,
    blobs: Vec<BlobInput>,
}

impl From<NewEvent> for Row {
    fn from(e: NewEvent) -> Self {
        Row {
            stream_id: e.stream_id,
            key: e.idempotency_key,
            typed: e.event,
            thread_id: e.thread_id,
            launch_id: e.launch_id,
            command_id: e.command_id,
            source: canonical_source(e.source),
            occurred_at: e.occurred_at,
            annex: e.annex,
            blobs: e.blobs,
        }
    }
}

/// The job's transaction. Every method runs inside it; the first error poisons it, and
/// every later call returns that error without touching SQLite.
pub struct WriteTx<'c> {
    conn: &'c Connection,
    recorded_at: Timestamp,
    ids: &'c dyn IdSource,
    projection_version: u32,
    poison: RefCell<Option<StoreError>>,
    heads: BTreeMap<StreamId, u64>,
    transcripts: HashMap<ThreadId, TranscriptJob>,
    task_causes: Vec<TaskCause>,
}

/// The source as a reader of the stored row sees it: an unknown string that spells a known
/// value is that value. On the wire both are the same string, so the store treats them as
/// one value for dispatch, the duplicate check, and the projection.
fn canonical_source(s: Source) -> Source {
    Source {
        kind: canonical(s.kind),
        agent: s.agent.map(canonical),
        ..s
    }
}

fn canonical<T: serde::de::DeserializeOwned>(o: Open<T>) -> Open<T> {
    use serde::Deserialize;
    match o {
        Open::Unknown(v) => {
            let de = serde::de::value::StrDeserializer::<serde::de::value::Error>::new(&v);
            Open::deserialize(de).unwrap_or(Open::Unknown(v))
        }
        known @ Open::Known(_) => known,
    }
}

fn is_transcript(s: &Source) -> bool {
    matches!(s.kind, Open::Known(SourceKind::Transcript))
}

impl<'c> WriteTx<'c> {
    pub(crate) fn new(
        conn: &'c Connection,
        recorded_at: Timestamp,
        ids: &'c dyn IdSource,
        projection_version: u32,
    ) -> Self {
        WriteTx {
            conn,
            recorded_at,
            ids,
            projection_version,
            poison: RefCell::new(None),
            heads: BTreeMap::new(),
            transcripts: HashMap::new(),
            task_causes: Vec::new(),
        }
    }

    pub(crate) fn take_poison(&mut self) -> Option<StoreError> {
        self.poison.get_mut().take()
    }

    pub(crate) fn take_heads(&mut self) -> Vec<StreamHeadEntry> {
        std::mem::take(&mut self.heads)
            .into_iter()
            .map(|(stream_id, head_seq)| StreamHeadEntry {
                stream_id,
                head_seq,
            })
            .collect()
    }

    /// The writer connection, for unit tests that run raw SQL inside a job.
    #[cfg(test)]
    pub(crate) fn raw(&self) -> &Connection {
        self.conn
    }

    /// Runs one operation: refuses at once when poisoned, records its first error.
    fn guard<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        if let Some(e) = self.poison.borrow().as_ref() {
            return Err(e.clone());
        }
        let r = f(self);
        if let Err(e) = &r {
            let mut slot = self.poison.borrow_mut();
            if slot.is_none() {
                *slot = Some(e.clone());
            }
        }
        r
    }

    /// `recorded_at` of every event this job appends.
    #[must_use]
    pub fn recorded_at(&self) -> &Timestamp {
        &self.recorded_at
    }

    /// Every `ReadTx` query, inside the write transaction.
    #[must_use]
    pub fn read(&self) -> ReadTx<'_> {
        ReadTx::new(self.conn, Some(&self.poison))
    }

    // ---- append ---------------------------------------------------------------------------

    /// Appends events in order (§2.3 `append`).
    ///
    /// # Errors
    ///
    /// `KeyConflict`, `Content`, `Annex`, `TranscriptViaAppend`, `TaskChangeWithoutCause`,
    /// `Constraint`, `Projection`, `DiskFull`, or another store error; each poisons the job.
    pub fn append(&mut self, events: Vec<NewEvent>) -> Result<Vec<Appended>, StoreError> {
        self.guard(|tx| tx.append_inner(events))
    }

    fn append_inner(&mut self, events: Vec<NewEvent>) -> Result<Vec<Appended>, StoreError> {
        let mut out = Vec::with_capacity(events.len());
        for e in events {
            let row = Row::from(e);
            if is_transcript(&row.source)
                || matches!(row.typed, TypedEvent::TranscriptGeneration(_))
            {
                return Err(StoreError::TranscriptViaAppend);
            }
            out.push(self.append_row(row)?);
        }
        Ok(out)
    }

    /// The one validation step every path that stores an event runs: typed `Content` and
    /// blob inputs (§2.5), and the annex form the kind requires (§2.6). Returns the blobs
    /// to insert and the refs to write.
    fn validate_event(&self, row: &Row) -> Result<(BlobRows, RefRows), StoreError> {
        // Annex.
        match (&row.typed, &row.annex) {
            (TypedEvent::ReadinessObserved(_), Some(EventAnnex::Readiness(b))) => {
                if Some(b.launch_id) != row.launch_id {
                    return Err(AnnexError::LaunchMismatch.into());
                }
                if to_i64(b.position).is_err() {
                    return Err(AnnexError::PositionOutOfRange.into());
                }
            }
            (TypedEvent::ReadinessObserved(_), _) => {
                return Err(AnnexError::Missing("readiness").into());
            }
            (_, Some(EventAnnex::Readiness(_))) => {
                return Err(AnnexError::Unexpected(row.typed.kind()).into());
            }
            (_, Some(EventAnnex::Transcript(_))) if !is_transcript(&row.source) => {
                return Err(AnnexError::Unexpected(row.typed.kind()).into());
            }
            (_, None) if is_transcript(&row.source) => {
                return Err(AnnexError::Missing("transcript").into());
            }
            _ => {}
        }
        // Blob inputs: the store computes each hash itself.
        let mut inputs: Vec<(String, &[u8], bool)> = Vec::new();
        for b in &row.blobs {
            if b.data.len() > MAX_BLOB_BYTES {
                return Err(ContentError::TooLarge(b.data.len() as u64).into());
            }
            inputs.push((sha256_hex(&b.data), &b.data, false));
        }
        let mut refs: Vec<(String, String)> = Vec::new();
        for c in contents(&row.typed) {
            match c {
                Content::Inline { inline, encoding } => check_inline(inline, *encoding)?,
                Content::Blob {
                    blob,
                    preview,
                    preview_lossy,
                } => {
                    check_blob_ref(blob)?;
                    // Inputs are matched by hash: equal hashes are equal bytes, so a reference
                    // uses every input of its hash, and the body is inserted once below.
                    let mut supplied = None;
                    for i in inputs.iter_mut().filter(|i| i.0 == blob.sha256) {
                        i.2 = true;
                        supplied = Some(i.1);
                    }
                    if let Some(data) = supplied {
                        let head = &data[..data.len().min(PREVIEW_READ_BYTES)];
                        check_against(blob, preview, *preview_lossy, data.len() as u64, head)?;
                    } else {
                        let stored: Option<(i64, Vec<u8>)> = self
                            .conn
                            .query_row(
                                "SELECT size, substr(data, 1, ?2) FROM blobs WHERE sha256 = ?1",
                                rusqlite::params![
                                    blob.sha256,
                                    super::rows::sql_len(PREVIEW_READ_BYTES)
                                ],
                                |r| Ok((r.get(0)?, r.get(1)?)),
                            )
                            .optional()?;
                        let Some((size, head)) = stored else {
                            return Err(ContentError::BlobNotFound(blob.sha256.clone()).into());
                        };
                        check_against(
                            blob,
                            preview,
                            *preview_lossy,
                            super::rows::to_u64(size)?,
                            &head,
                        )?;
                    }
                    let r = (blob.sha256.clone(), blob.media_type.clone());
                    if !refs.contains(&r) {
                        refs.push(r);
                    }
                }
            }
        }
        let mut blobs = Vec::new();
        for (sha, data, used) in inputs {
            if !used {
                return Err(ContentError::UnreferencedBlob(sha).into());
            }
            if !blobs.iter().any(|(s, _): &(String, Vec<u8>)| *s == sha) {
                blobs.push((sha, data.to_vec()));
            }
        }
        Ok((blobs, refs))
    }

    /// Steps 1–5 of `append` for one event whose boundary checks passed.
    fn append_row(&mut self, row: Row) -> Result<Appended, StoreError> {
        // 1. Validation.
        let (blobs, refs) = self.validate_event(&row)?;
        let kind = row.typed.kind();
        let payload = row
            .typed
            .to_payload()
            .map_err(StoreError::decode)?
            .get()
            .to_owned();
        let annex_text = row.annex.as_ref().map(to_json).transpose()?;
        // 2. Idempotency.
        let existing: Option<RawRow> = self
            .conn
            .query_row(
                &format!("SELECT {EVENT_COLUMNS} FROM events WHERE stream_id = ?1 AND idempotency_key = ?2"),
                [row.stream_id.to_string(), row.key.0.clone()],
                RawRow::read,
            )
            .optional()?;
        if let Some(raw) = existing {
            let stored = raw.decode()?;
            let candidate = Candidate {
                kind,
                thread_id: row.thread_id,
                launch_id: row.launch_id,
                command_id: row.command_id,
                source: &row.source,
                annex: row.annex.as_ref(),
                payload: &payload,
                occurred_at: row.occurred_at.as_ref(),
            };
            return match first_difference(&stored, &candidate) {
                None => Ok(Appended::Existing(stored.envelope)),
                Some(field) => Err(StoreError::KeyConflict {
                    stream_id: row.stream_id,
                    key: row.key,
                    field,
                }),
            };
        }
        // The derived status_changed(TaskState) needs its task_set earlier in the job.
        if let TypedEvent::StatusChanged(StatusChanged::TaskState { from, to, .. }) = &row.typed {
            let thread = row.thread_id.ok_or(StoreError::TaskChangeWithoutCause)?;
            let (from, to) = (enum_text(from), enum_text(to));
            let cause = self
                .task_causes
                .iter_mut()
                .find(|c| !c.used && c.thread == thread && c.from == from && c.to == to)
                .ok_or(StoreError::TaskChangeWithoutCause)?;
            cause.used = true;
        }
        let task_before = match (&row.typed, row.thread_id) {
            (TypedEvent::TaskSet(_), Some(t)) => project::task_state(self.conn, t)?,
            _ => None,
        };
        // 3. Blobs and refs.
        for (sha, data) in &blobs {
            self.conn.execute(
                "INSERT INTO blobs (sha256, size, data) VALUES (?1, ?2, ?3) ON CONFLICT (sha256) DO NOTHING",
                rusqlite::params![sha, super::rows::sql_len(data.len()), data],
            )?;
        }
        for (sha, media_type) in &refs {
            self.conn.execute(
                "INSERT INTO blob_refs (stream_id, sha256, media_type) VALUES (?1, ?2, ?3) ON CONFLICT DO NOTHING",
                rusqlite::params![row.stream_id.to_string(), sha, media_type],
            )?;
        }
        // 4. The row, with seq and cum_bytes computed from the stream's last row.
        let event_id = self.ids.event_id();
        let occurred = row
            .occurred_at
            .clone()
            .unwrap_or_else(|| self.recorded_at.clone());
        let stream = row.stream_id.to_string();
        let seq: i64 = self.conn.query_row(
            "INSERT INTO events (stream_id, seq, event_id, schema_version, idempotency_key, kind, thread_id,
               launch_id, command_id, source, occurred_at, occurred_explicit, recorded_at, payload, payload_bytes,
               cum_bytes, annex)
             VALUES (?1,
               coalesce((SELECT max(seq) FROM events WHERE stream_id = ?1), 0) + 1,
               ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
               coalesce((SELECT cum_bytes FROM events WHERE stream_id = ?1 ORDER BY seq DESC LIMIT 1), 0) + ?14,
               ?15)
             RETURNING seq",
            rusqlite::params![
                stream,
                event_id.to_string(),
                chaldea_core::SCHEMA_VERSION,
                row.key.0,
                kind,
                row.thread_id.map(|t| t.to_string()),
                row.launch_id.map(|l| l.to_string()),
                row.command_id.map(|c| c.to_string()),
                to_json(&row.source)?,
                occurred.0,
                row.occurred_at.is_some(),
                self.recorded_at.0,
                payload,
                super::rows::sql_len(payload.len()),
                annex_text,
            ],
            |r| r.get(0),
        )?;
        let stored = StoredEvent {
            envelope: EventEnvelope {
                stream_id: row.stream_id,
                seq: super::rows::to_u64(seq)?,
                event_id,
                schema_version: chaldea_core::SCHEMA_VERSION,
                idempotency_key: row.key,
                kind: chaldea_core::ids::EventKindTag(kind.to_owned()),
                thread_id: row.thread_id,
                launch_id: row.launch_id,
                command_id: row.command_id,
                source: row.source,
                occurred_at: occurred,
                recorded_at: self.recorded_at.clone(),
                payload: RawValue::from_string(payload).map_err(StoreError::decode)?,
            },
            occurred_explicit: row.occurred_at.is_some(),
            annex_text,
        };
        // 5. Projection.
        project::apply(&mut ProjectionTx::new(self.conn), &stored)?;
        if let (TypedEvent::TaskSet(t), Some(thread), Some(from)) =
            (&row.typed, row.thread_id, task_before)
        {
            let to = enum_text(&t.to);
            if from != to {
                self.task_causes.push(TaskCause {
                    thread,
                    from,
                    to,
                    used: false,
                });
            }
        }
        let head = self.heads.entry(row.stream_id).or_insert(0);
        *head = (*head).max(stored.envelope.seq);
        Ok(Appended::New(stored.envelope))
    }

    // ---- transcript ------------------------------------------------------------------------

    /// Stores one transcript batch: binding and cursor checks first, then the generation or
    /// the lines, then the cursor advance (§2.3 `append_transcript`).
    ///
    /// # Errors
    ///
    /// `StaleBinding`, `StaleCursor`, `GenerationWithLines`, `BadBatch`, or any `append`
    /// error; each poisons the job.
    pub fn append_transcript(&mut self, b: TranscriptBatch) -> Result<Vec<Appended>, StoreError> {
        self.guard(|tx| tx.append_transcript_inner(b))
    }

    fn binding(&self, thread: ThreadId) -> Result<Option<Binding>, StoreError> {
        let row: Option<(Option<String>, Option<String>)> = self
            .conn
            .query_row(
                "SELECT current_launch_id, transcript_path FROM proj_threads WHERE thread_id = ?1",
                [thread.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((launch, path)) = row else {
            return Ok(None);
        };
        let session: Option<String> = match &launch {
            Some(l) => self
                .conn
                .query_row(
                    "SELECT session_id FROM proj_launches WHERE launch_id = ?1",
                    [l],
                    |r| r.get(0),
                )
                .optional()?
                .flatten(),
            None => None,
        };
        Ok(Some(Binding {
            launch_id: super::rows::opt_uuid(launch.as_deref())?.map(LaunchId),
            session_id: session,
            transcript_path: path,
        }))
    }

    fn append_transcript_inner(&mut self, b: TranscriptBatch) -> Result<Vec<Appended>, StoreError> {
        // 1. Binding: the batch's new state against the thread's current binding.
        let binding = self.binding(b.thread_id)?;
        let bound = binding.as_ref().is_some_and(|x| {
            x.launch_id == Some(b.launch_id)
                && x.session_id.as_deref() == Some(b.session_id.as_str())
                && x.transcript_path.as_deref() == Some(b.path.as_str())
        });
        if !bound {
            return Err(StoreError::StaleBinding { stored: binding });
        }
        // 2. Cursor: the batch's old state against the stored cursor.
        let stored = self.read().cursor_row(b.thread_id)?;
        let matches = match (&stored, &b.expect) {
            (None, None) => true,
            (Some(s), Some(e)) => {
                s.generation == e.generation
                    && s.path == e.path
                    && s.file_id == e.file_id
                    && s.read_offset == e.read_offset
            }
            _ => false,
        };
        if !matches {
            return Err(StoreError::StaleCursor { stored });
        }
        let job = self
            .transcripts
            .get(&b.thread_id)
            .copied()
            .unwrap_or_default();
        let source = |generation: u64, file_id: &str, offset_end: u64| Source {
            kind: Open::Known(SourceKind::Transcript),
            agent: Some(Open::Known(b.agent)),
            cli_version: b.cli_version.clone(),
            transcript: Some(TranscriptPos {
                generation,
                path: b.path.clone(),
                file_id: file_id.to_owned(),
                offset_end,
            }),
        };
        let stream = StreamId::Thread(b.thread_id);
        let mut out = Vec::new();
        let (generation, file_id) = if let Some(g) = &b.new_generation {
            // 3. A generation commits alone at offset 0.
            if !b.lines.is_empty() || job.continued {
                return Err(StoreError::GenerationWithLines);
            }
            if b.advance_to.read_offset != 0 {
                return Err(StoreError::BadBatch(
                    "a generation start advances to offset 0",
                ));
            }
            let generation = b.expect.as_ref().map_or(1, |e| e.generation + 1);
            self.transcripts
                .entry(b.thread_id)
                .or_default()
                .started_generation = true;
            out.push(self.append_row(Row {
                stream_id: stream,
                key: IdempotencyKey(format!("transcript_generation:{generation}")),
                typed: TypedEvent::TranscriptGeneration(TranscriptGeneration {
                    generation,
                    path: b.path.clone(),
                    file_id: g.file_id.clone(),
                }),
                thread_id: Some(b.thread_id),
                launch_id: Some(b.launch_id),
                command_id: None,
                source: source(generation, &g.file_id, 0),
                occurred_at: None,
                annex: Some(EventAnnex::Transcript(g.continuity.clone())),
                blobs: Vec::new(),
            })?);
            (generation, g.file_id.clone())
        } else {
            // 4. A continuation of the expected generation.
            if job.started_generation {
                return Err(StoreError::GenerationWithLines);
            }
            let Some(expect) = &b.expect else {
                return Err(StoreError::BadBatch("a continuation needs a cursor"));
            };
            if expect.path != b.path {
                return Err(StoreError::BadBatch(
                    "a continuation reads the cursor's path",
                ));
            }
            let mut at = expect.read_offset;
            for l in &b.lines {
                if l.offset_start >= l.offset_end {
                    return Err(StoreError::BadBatch("a line ends after it starts"));
                }
                if l.offset_start < at {
                    return Err(StoreError::BadBatch(
                        "lines start at the read offset and ascend",
                    ));
                }
                at = l.offset_end;
            }
            // A batch without lines that moves backwards is left to the conditional cursor
            // update below (`StaleCursor`).
            if b.lines
                .last()
                .is_some_and(|l| l.offset_end > b.advance_to.read_offset)
            {
                return Err(StoreError::BadBatch("the last line ends after advance_to"));
            }
            self.transcripts.entry(b.thread_id).or_default().continued = true;
            for l in b.lines {
                if matches!(l.event, TypedEvent::TranscriptGeneration(_)) {
                    return Err(StoreError::BadBatch(
                        "a line cannot be a transcript_generation",
                    ));
                }
                out.push(self.append_row(Row {
                    stream_id: stream,
                    key: IdempotencyKey(format!(
                        "transcript:{}:{}",
                        expect.generation, l.offset_start
                    )),
                    typed: l.event,
                    thread_id: Some(b.thread_id),
                    launch_id: Some(b.launch_id),
                    command_id: None,
                    source: source(expect.generation, &expect.file_id, l.offset_end),
                    occurred_at: l.occurred_at,
                    annex: Some(EventAnnex::Transcript(l.continuity)),
                    blobs: l.blobs,
                })?);
            }
            (expect.generation, expect.file_id.clone())
        };
        // The cursor advance, bound to the post-append generation and file_id.
        let changed = self.conn.execute(
            "UPDATE proj_transcript_cursors SET read_offset = ?1, read_continuity = ?2
             WHERE thread_id = ?3 AND generation = ?4 AND file_id = ?5 AND read_offset <= ?1",
            rusqlite::params![
                to_i64(b.advance_to.read_offset)?,
                b.advance_to.continuity.0,
                b.thread_id.to_string(),
                to_i64(generation)?,
                file_id,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::StaleCursor {
                stored: self.read().cursor_row(b.thread_id)?,
            });
        }
        Ok(out)
    }

    // ---- receipts --------------------------------------------------------------------------

    /// Classifies the request against its receipt, inserting `in_progress` for `New`.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn receipt_begin(
        &mut self,
        id: CommandId,
        token: TokenId,
        request_sha256: [u8; 32],
    ) -> Result<ReceiptLookup, StoreError> {
        self.guard(|tx| {
            let existing = tx.read().receipt_row(id)?;
            let lookup = check_receipt(existing.as_ref(), token, &request_sha256);
            if lookup == ReceiptLookup::New {
                tx.conn.execute(
                    "INSERT INTO command_receipts (command_id, token_id, request_sha256, status, created_at)
                     VALUES (?1, ?2, ?3, 'in_progress', ?4)",
                    rusqlite::params![id.to_string(), token.to_string(), request_sha256.as_slice(), tx.recorded_at.0],
                )?;
            }
            Ok(lookup)
        })
    }

    /// Stores the HTTP status and body of an `in_progress` receipt, byte for byte.
    ///
    /// # Errors
    ///
    /// `ReceiptNotInProgress`, or a store error.
    pub fn receipt_complete(
        &mut self,
        id: CommandId,
        result: &CommandResult,
    ) -> Result<(), StoreError> {
        self.guard(|tx| {
            let n = tx.conn.execute(
                "UPDATE command_receipts SET status = 'completed', http_status = ?1, body = ?2, finished_at = ?3
                 WHERE command_id = ?4 AND status = 'in_progress'",
                rusqlite::params![result.http_status, result.body, tx.recorded_at.0, id.to_string()],
            )?;
            if n == 1 {
                Ok(())
            } else {
                Err(StoreError::ReceiptNotInProgress(id))
            }
        })
    }

    /// Turns every `in_progress` receipt into `unknown_after_crash` (once, at restart).
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn receipts_mark_unknown_after_crash(&mut self) -> Result<u64, StoreError> {
        self.guard(|tx| {
            let n = tx.conn.execute(
                "UPDATE command_receipts SET status = 'unknown_after_crash', finished_at = ?1 WHERE status = 'in_progress'",
                [tx.recorded_at.0.clone()],
            )?;
            Ok(n as u64)
        })
    }

    // ---- hook deliveries -------------------------------------------------------------------

    fn routing_events(
        &mut self,
        events: Vec<NewEvent>,
    ) -> Result<(Vec<Appended>, (StreamId, u64)), StoreError> {
        let appended = self.append_inner(events)?;
        let first = appended
            .first()
            .map(|a| (a.envelope().stream_id, a.envelope().seq))
            .ok_or(StoreError::BadRouting("no events"))?;
        Ok((appended, first))
    }

    fn raw_unknown(&mut self, e: NewEvent) -> Result<(Vec<Appended>, (StreamId, u64)), StoreError> {
        if !matches!(e.event, TypedEvent::RawUnknown(_)) {
            return Err(StoreError::BadRouting(
                "a raw_unknown routing needs a raw_unknown event",
            ));
        }
        self.routing_events(vec![e])
    }

    /// Records a hook delivery, or replays the stored one (§2.3 "Hook deliveries").
    ///
    /// # Errors
    ///
    /// `BadRouting`, any `append` error, or a store error.
    pub fn hook_delivery_record(
        &mut self,
        d: NewHookDelivery,
    ) -> Result<HookDeliveryLookup, StoreError> {
        self.guard(|tx| {
            if let Some(existing) = tx.read().delivery_row(d.launch_id, d.emit_id)? {
                return Ok(HookDeliveryLookup::Replay(existing));
            }
            let (routing, name, position, pending) = match d.routing {
                NewRouting::Appended(events) => {
                    if events.is_empty() {
                        return Err(StoreError::BadRouting("no events"));
                    }
                    let (_, (s, q)) = tx.routing_events(events)?;
                    (Routing::Appended { stream_id: s, seq: q }, "appended", Some((s, q)), None)
                }
                NewRouting::RawUnknown(e) => {
                    let (_, (s, q)) = tx.raw_unknown(e)?;
                    (Routing::RawUnknown { stream_id: s, seq: q }, "raw_unknown", Some((s, q)), None)
                }
                NewRouting::Buffered(h) => (Routing::Buffered, "buffered", None, Some(h)),
            };
            let commit_ordinal: i64 = tx.conn.query_row(
                "INSERT INTO hook_deliveries (launch_id, emit_id, receipt_ordinal, commit_ordinal, reply_status,
                   reply_body, routing, stream_id, seq, pending_body, pending_meta, received_at)
                 VALUES (?1, ?2, ?3,
                   coalesce((SELECT max(commit_ordinal) FROM hook_deliveries WHERE launch_id = ?1), 0) + 1,
                   ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 RETURNING commit_ordinal",
                rusqlite::params![
                    d.launch_id.to_string(),
                    d.emit_id.to_string(),
                    to_i64(d.receipt_ordinal)?,
                    d.first_reply.http_status,
                    d.first_reply.body,
                    name,
                    position.map(|p| p.0.to_string()),
                    position.map(|p| to_i64(p.1)).transpose()?,
                    pending.as_ref().map(|h| h.body.clone()),
                    pending.as_ref().map(|h| h.meta.clone()),
                    tx.recorded_at.0,
                ],
                |r| r.get(0),
            )?;
            Ok(HookDeliveryLookup::New(HookDelivery {
                receipt_ordinal: d.receipt_ordinal,
                commit_ordinal: super::rows::to_u64(commit_ordinal)?,
                first_reply: d.first_reply,
                routing,
            }))
        })
    }

    /// Promotes a buffered delivery with the caller's final events.
    ///
    /// # Errors
    ///
    /// `NotBuffered`, `BadRouting`, any `append` error, or a store error.
    pub fn hook_delivery_promote(
        &mut self,
        launch: LaunchId,
        emit: Uuid,
        to: Promotion,
    ) -> Result<Vec<Appended>, StoreError> {
        self.guard(|tx| {
            match tx.read().delivery_row(launch, emit)? {
                Some(HookDelivery {
                    routing: Routing::Buffered,
                    ..
                }) => {}
                _ => return Err(StoreError::NotBuffered),
            }
            let (appended, (stream, seq), name) = match to {
                Promotion::Appended(events) => {
                    if events.is_empty() {
                        return Err(StoreError::BadRouting("no events"));
                    }
                    let (a, p) = tx.routing_events(events)?;
                    (a, p, "appended")
                }
                Promotion::RawUnknown(e) => {
                    let (a, p) = tx.raw_unknown(e)?;
                    (a, p, "raw_unknown")
                }
            };
            let n = tx.conn.execute(
                "UPDATE hook_deliveries SET routing = ?1, stream_id = ?2, seq = ?3, pending_body = NULL,
                   pending_meta = NULL
                 WHERE launch_id = ?4 AND emit_id = ?5 AND routing = 'buffered'",
                rusqlite::params![name, stream.to_string(), to_i64(seq)?, launch.to_string(), emit.to_string()],
            )?;
            if n != 1 {
                return Err(StoreError::NotBuffered);
            }
            Ok(appended)
        })
    }

    // ---- tokens ----------------------------------------------------------------------------

    /// # Errors
    ///
    /// `Constraint` for a duplicate id or hash, or a store error.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "WriteTx methods take their inputs by value"
    )]
    pub fn token_insert(&mut self, t: NewToken) -> Result<(), StoreError> {
        let NewToken {
            token_id,
            kind,
            sha256,
            thread_id,
            launch_id,
        } = t;
        self.guard(|tx| {
            tx.conn.execute(
                "INSERT INTO tokens (token_id, kind, sha256, thread_id, launch_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    token_id.to_string(),
                    enum_text(&kind),
                    sha256.as_slice(),
                    thread_id.map(|x| x.to_string()),
                    launch_id.map(|x| x.to_string()),
                    tx.recorded_at.0,
                ],
            )?;
            Ok(())
        })
    }

    /// Revokes the selected tokens that are not revoked yet; returns how many.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn token_revoke(&mut self, which: TokenSelector) -> Result<u64, StoreError> {
        self.guard(|tx| {
            let (column, id) = match which {
                TokenSelector::Token(t) => ("token_id", t.to_string()),
                TokenSelector::Launch(l) => ("launch_id", l.to_string()),
                TokenSelector::Thread(t) => ("thread_id", t.to_string()),
            };
            let n = tx.conn.execute(
                &format!(
                    "UPDATE tokens SET revoked_at = ?1 WHERE {column} = ?2 AND revoked_at IS NULL"
                ),
                [tx.recorded_at.0.clone(), id],
            )?;
            Ok(n as u64)
        })
    }

    // ---- rebuild ---------------------------------------------------------------------------

    /// Clears every projection, replays `events` by `global_seq` through `apply`, and writes
    /// this process's projection version (§2.6).
    ///
    /// # Errors
    ///
    /// A `Projection` error or a store error.
    pub fn rebuild_projections(&mut self) -> Result<RebuildReport, StoreError> {
        self.guard(WriteTx::rebuild_inner)
    }

    fn rebuild_inner(&mut self) -> Result<RebuildReport, StoreError> {
        project::clear_projections(self.conn)?;
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM events ORDER BY global_seq"
        ))?;
        let mut rows = stmt.query([])?;
        let mut p = ProjectionTx::new(self.conn);
        let mut n = 0u64;
        while let Some(r) = rows.next()? {
            let stored = RawRow::read(r)?.decode()?;
            project::apply(&mut p, &stored)?;
            n += 1;
        }
        drop(rows);
        drop(stmt);
        self.conn.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'projection_version'",
            [self.projection_version.to_string()],
        )?;
        Ok(RebuildReport { events: n })
    }

    /// Rebuilds inside a savepoint, compares with the maintained projections, and rolls the
    /// rebuild back.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn verify_projections(&mut self) -> Result<ProjectionDiff, StoreError> {
        self.guard(|tx| {
            let maintained = project::dump_projections(tx.conn)?;
            tx.conn.execute_batch("SAVEPOINT verify_projections")?;
            let rebuilt = tx
                .rebuild_inner()
                .and_then(|_| project::dump_projections(tx.conn));
            if !tx.conn.is_autocommit() {
                tx.conn
                    .execute_batch("ROLLBACK TO verify_projections; RELEASE verify_projections")?;
            }
            Ok(ProjectionDiff {
                differences: project::compare_projections(&maintained, &rebuilt?),
            })
        })
    }
}
