//! Projections: the one function that changes them, used by live appends and by rebuild,
//! and the comparison of two projection states (`docs/design/event-store.md` §2.6).

use std::collections::{BTreeMap, BTreeSet};

use chaldea_core::event::TypedEvent;
use chaldea_core::ids::Open;
use chaldea_core::ids::{StreamId, ThreadId};
use chaldea_core::input::InputState;
use chaldea_core::payload::{HumanWriteOrigin, InputInitialState, SourceKind, SurfaceScope};
use chaldea_core::state::{AttentionCause, StatusChanged, ThreadState};
use rusqlite::types::Value;
use rusqlite::{Connection, Params};

use super::annex::EventAnnex;
use super::append::StoredEvent;
use super::error::{ProjectionError, StoreError};
use super::rows::{TableDump, enum_text, to_i64, to_json};

/// The transaction, restricted to statements on `proj_*` tables: `apply` has no access to
/// events, receipts, deliveries, tokens, blobs, or a clock.
pub(crate) struct ProjectionTx<'a> {
    conn: &'a Connection,
}

/// Every table a projection statement names starts with `proj_`.
fn only_projection_tables(sql: &str) -> bool {
    let words: Vec<&str> = sql
        .split(|c: char| c.is_whitespace() || c == '(' || c == ',')
        .filter(|w| !w.is_empty())
        .collect();
    words.windows(2).all(|w| {
        !matches!(
            w[0].to_ascii_uppercase().as_str(),
            "FROM" | "INTO" | "UPDATE" | "JOIN"
        ) || w[1].starts_with("proj_")
            // `ON CONFLICT … DO UPDATE SET` names no table.
            || w[1].eq_ignore_ascii_case("SET")
    })
}

impl<'a> ProjectionTx<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Self {
        ProjectionTx { conn }
    }

    /// Runs a statement and returns the rows it changed.
    fn exec(&mut self, sql: &str, params: impl Params) -> Result<usize, StoreError> {
        debug_assert!(
            only_projection_tables(sql),
            "not a projection statement: {sql}"
        );
        Ok(self.conn.execute(sql, params)?)
    }

    /// Runs a statement that must change exactly one row.
    fn exec_one(&mut self, what: &str, sql: &str, params: impl Params) -> Result<(), StoreError> {
        match self.exec(sql, params)? {
            1 => Ok(()),
            n => Err(ProjectionError(format!("{what}: changed {n} rows, expected 1")).into()),
        }
    }

    fn query_text(&self, sql: &str, params: impl Params) -> Result<Option<String>, StoreError> {
        debug_assert!(
            only_projection_tables(sql),
            "not a projection statement: {sql}"
        );
        let mut stmt = self.conn.prepare(sql)?;
        let mut rows = stmt.query(params)?;
        match rows.next()? {
            Some(r) => Ok(r.get(0)?),
            None => Ok(None),
        }
    }
}

fn seq_of(e: &StoredEvent) -> Result<i64, StoreError> {
    to_i64(e.envelope.seq)
}

fn thread_of(e: &StoredEvent) -> Result<ThreadId, StoreError> {
    match e.envelope.stream_id {
        StreamId::Thread(t) => Ok(t),
        StreamId::Host => {
            Err(ProjectionError(format!("{} on the host stream", e.envelope.kind)).into())
        }
    }
}

fn sorted_attention(v: &[AttentionCause]) -> Result<String, StoreError> {
    let set: BTreeSet<AttentionCause> = v.iter().copied().collect();
    let sorted: Vec<AttentionCause> = set.into_iter().collect();
    to_json(&sorted)
}

/// Applies one stored event to the projections. A `GenericEvent` changes nothing.
pub(crate) fn apply(p: &mut ProjectionTx<'_>, e: &StoredEvent) -> Result<(), StoreError> {
    let Ok(typed) = TypedEvent::decode(&e.envelope) else {
        return Ok(());
    };
    let seq = seq_of(e)?;
    let env = &e.envelope;
    let transcript = matches!(env.source.kind, Open::Known(SourceKind::Transcript));
    match &typed {
        TypedEvent::ThreadRegistered(r) => {
            let state = ThreadState::registered(r.task.is_some());
            p.exec(
                "INSERT INTO proj_threads (thread_id, agent, cwd, workspace_id, parent_thread_id, role, closed,
                   process_state, turn_state, task_state, attention, registered_seq)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8, ?9, '[]', ?10)",
                rusqlite::params![
                    r.thread_id.to_string(),
                    enum_text(&r.agent),
                    r.cwd,
                    r.workspace_id.0,
                    r.parent_thread_id.map(|t| t.to_string()),
                    enum_text(&r.role),
                    enum_text(&state.process),
                    enum_text(&state.turn),
                    enum_text(&state.task),
                    seq,
                ],
            )?;
        }
        TypedEvent::ThreadClosed(c) => {
            p.exec_one(
                "thread_closed",
                "UPDATE proj_threads SET closed = 1 WHERE thread_id = ?1",
                [c.thread_id.to_string()],
            )?;
        }
        TypedEvent::LaunchReserved(r) => {
            let thread = thread_of(e)?;
            p.exec(
                "INSERT INTO proj_launches (launch_id, thread_id, reserved_seq, expected_previous, agent,
                   argv_digest, manual, worktree_id, death_confirmed)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)",
                rusqlite::params![
                    r.launch_id.to_string(),
                    thread.to_string(),
                    seq,
                    r.expected_previous.map(|l| l.to_string()),
                    enum_text(&r.agent),
                    r.argv_digest,
                    r.manual,
                    r.worktree_id.as_ref().map(|w| w.0.clone()),
                ],
            )?;
            p.exec_one(
                "launch_reserved",
                "UPDATE proj_threads SET current_launch_id = ?1 WHERE thread_id = ?2",
                [r.launch_id.to_string(), thread.to_string()],
            )?;
        }
        TypedEvent::LaunchStarted(s) => {
            p.exec_one(
                "launch_started",
                "UPDATE proj_launches SET pid = ?1, start_time = ?2, pgid_or_job = ?3, cli_version = ?4
                 WHERE launch_id = ?5 AND thread_id = ?6",
                rusqlite::params![
                    s.pid,
                    s.start_time,
                    s.pgid_or_job,
                    s.cli_version,
                    s.launch_id.to_string(),
                    thread_of(e)?.to_string(),
                ],
            )?;
        }
        TypedEvent::LaunchExited(x) => {
            p.exec_one(
                "launch_exited",
                "UPDATE proj_launches SET exit_code = ?1, exit_signal = ?2, exit_reason = ?3, death_confirmed = ?4
                 WHERE launch_id = ?5 AND thread_id = ?6",
                rusqlite::params![
                    x.code,
                    x.signal,
                    enum_text(&x.reason),
                    x.death_confirmed,
                    x.launch_id.to_string(),
                    thread_of(e)?.to_string(),
                ],
            )?;
        }
        TypedEvent::LaunchDeathConfirmed(d) => {
            p.exec_one(
                "launch_death_confirmed",
                "UPDATE proj_launches SET death_confirmed = 1, death_method = ?1 WHERE launch_id = ?2 AND thread_id = ?3",
                rusqlite::params![d.method, d.launch_id.to_string(), thread_of(e)?.to_string()],
            )?;
        }
        TypedEvent::SessionBound(b) => {
            let thread = thread_of(e)?;
            p.exec_one(
                "session_bound",
                "UPDATE proj_launches SET session_id = ?1, last_turn_start_seq = ?2 WHERE launch_id = ?3 AND thread_id = ?4",
                rusqlite::params![b.session_id, seq, b.launch_id.to_string(), thread.to_string()],
            )?;
            p.exec_one(
                "session_bound",
                "UPDATE proj_threads SET session_id = ?1, transcript_path = ?2 WHERE thread_id = ?3",
                rusqlite::params![b.session_id, b.transcript_path, thread.to_string()],
            )?;
        }
        TypedEvent::TranscriptGeneration(g) => {
            let continuity = transcript_continuity(e)?;
            p.exec(
                "INSERT INTO proj_transcript_cursors (thread_id, generation, path, file_id, event_offset,
                   event_continuity, read_offset, read_continuity)
                 VALUES (?1, ?2, ?3, ?4, 0, ?5, 0, ?5)
                 ON CONFLICT (thread_id) DO UPDATE SET generation = excluded.generation, path = excluded.path,
                   file_id = excluded.file_id, event_offset = 0, event_continuity = excluded.event_continuity,
                   read_offset = 0, read_continuity = excluded.read_continuity",
                rusqlite::params![
                    thread_of(e)?.to_string(),
                    to_i64(g.generation)?,
                    g.path,
                    g.file_id,
                    continuity,
                ],
            )?;
        }
        TypedEvent::ReadinessObserved(r) => {
            let Some(EventAnnex::Readiness(basis)) = e.annex()? else {
                return Err(
                    ProjectionError("readiness_observed without its write basis".into()).into(),
                );
            };
            p.exec_one(
                "readiness_observed",
                "UPDATE proj_launches SET readiness_seq = ?1, readiness_evidence = ?2, write_basis = ?3
                 WHERE launch_id = ?4 AND thread_id = ?5",
                rusqlite::params![
                    seq,
                    enum_text(&r.evidence),
                    to_json(&basis)?,
                    r.launch_id.to_string(),
                    thread_of(e)?.to_string(),
                ],
            )?;
        }
        TypedEvent::HumanWrite(h) => {
            let closes = matches!(h.origin, HumanWriteOrigin::Attach | HumanWriteOrigin::Key);
            p.exec_one(
                "human_write",
                "UPDATE proj_launches SET last_write_seq = ?1,
                   last_human_write_seq = CASE WHEN ?2 THEN ?1 ELSE last_human_write_seq END
                 WHERE launch_id = ?3 AND thread_id = ?4",
                rusqlite::params![
                    seq,
                    closes,
                    h.launch_id.to_string(),
                    thread_of(e)?.to_string()
                ],
            )?;
        }
        TypedEvent::UserMessage(m) => {
            let thread = thread_of(e)?;
            if let Some(launch) = env.launch_id {
                p.exec_one(
                    "user_message",
                    "UPDATE proj_launches SET last_turn_start_seq = ?1 WHERE launch_id = ?2 AND thread_id = ?3",
                    rusqlite::params![seq, launch.to_string(), thread.to_string()],
                )?;
            }
            // The prompt acknowledges an input only of the launch that submitted it (mvp
            // §2.3); without an envelope launch nothing matches.
            if let Some(input) = m.input_id {
                p.exec_one(
                    "user_message ack",
                    "UPDATE proj_inputs SET ack_seq = ?1
                     WHERE input_id = ?2 AND thread_id = ?3 AND launch_id = ?4",
                    rusqlite::params![
                        seq,
                        input.to_string(),
                        thread.to_string(),
                        env.launch_id.map(|l| l.to_string())
                    ],
                )?;
            }
        }
        TypedEvent::StatusChanged(s) => {
            let thread = thread_of(e)?.to_string();
            match s {
                StatusChanged::ProcessState { from, to, .. } => p.exec_one(
                    "status_changed(process_state)",
                    "UPDATE proj_threads SET process_state = ?1 WHERE thread_id = ?2 AND process_state = ?3",
                    [enum_text(to), thread, enum_text(from)],
                )?,
                StatusChanged::TurnState { from, to, .. } => p.exec_one(
                    "status_changed(turn_state)",
                    "UPDATE proj_threads SET turn_state = ?1 WHERE thread_id = ?2 AND turn_state = ?3",
                    [enum_text(to), thread, enum_text(from)],
                )?,
                StatusChanged::Attention { from, to, .. } => p.exec_one(
                    "status_changed(attention)",
                    "UPDATE proj_threads SET attention = ?1 WHERE thread_id = ?2 AND attention = ?3",
                    [sorted_attention(to)?, thread, sorted_attention(from)?],
                )?,
                // The record of its `task_set`, which alone changes the task (§2.6).
                StatusChanged::TaskState { .. } => {}
            }
        }
        TypedEvent::TaskSet(t) => {
            p.exec_one(
                "task_set",
                "UPDATE proj_threads SET task_state = ?1 WHERE thread_id = ?2",
                [enum_text(&t.to), thread_of(e)?.to_string()],
            )?;
        }
        TypedEvent::ApprovalRequested(a) => {
            p.exec(
                "INSERT INTO proj_approvals (approval_id, thread_id, launch_id, requested, opened_seq)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    a.approval_id.to_string(),
                    thread_of(e)?.to_string(),
                    a.launch_id.to_string(),
                    to_json(a)?,
                    seq,
                ],
            )?;
        }
        TypedEvent::ApprovalResolved(a) => {
            p.exec_one(
                "approval_resolved",
                "UPDATE proj_approvals SET outcome = ?1, resolved_seq = ?2
                 WHERE approval_id = ?3 AND thread_id = ?4 AND outcome IS NULL",
                rusqlite::params![
                    enum_text(&a.outcome),
                    seq,
                    a.approval_id.to_string(),
                    thread_of(e)?.to_string(),
                ],
            )?;
        }
        TypedEvent::InputCreated(i) => {
            let state = match i.initial_state {
                InputInitialState::Queued => InputState::Queued,
                InputInitialState::Sending => InputState::Sending,
            };
            p.exec(
                "INSERT INTO proj_inputs (input_id, thread_id, launch_id, state, text, encoding, origin,
                   created_by, created_seq)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    i.input_id.to_string(),
                    thread_of(e)?.to_string(),
                    i.launch_id.map(|l| l.to_string()),
                    enum_text(&state),
                    to_json(&i.text)?,
                    enum_text(&i.encoding),
                    enum_text(&i.origin),
                    i.created_by.to_string(),
                    seq,
                ],
            )?;
        }
        TypedEvent::InputStateChanged(c) => {
            let thread = thread_of(e)?.to_string();
            let sent_after = c.sent_after_seq.map(to_i64).transpose()?;
            p.exec_one(
                "input_state_changed",
                "UPDATE proj_inputs SET state = ?1, sent_after_seq = coalesce(?2, sent_after_seq)
                 WHERE input_id = ?3 AND thread_id = ?4 AND state = ?5",
                rusqlite::params![
                    enum_text(&c.to),
                    sent_after,
                    c.input_id.to_string(),
                    thread,
                    enum_text(&c.from),
                ],
            )?;
            if c.to == InputState::Sending {
                let launch = p.query_text(
                    "SELECT coalesce(launch_id, ?1) FROM proj_inputs WHERE input_id = ?2",
                    rusqlite::params![env.launch_id.map(|l| l.to_string()), c.input_id.to_string()],
                )?;
                let Some(launch) = launch else {
                    return Err(ProjectionError(
                        "input_state_changed to sending: the input and the envelope have no launch"
                            .into(),
                    )
                    .into());
                };
                p.exec_one(
                    "input attempt",
                    "UPDATE proj_inputs SET launch_id = ?1, attempt_seq = ?2,
                       last_write_before = (SELECT last_write_seq FROM proj_launches
                                            WHERE launch_id = ?1 AND thread_id = ?3)
                     WHERE input_id = ?4",
                    rusqlite::params![launch, seq, thread, c.input_id.to_string()],
                )?;
                p.exec_one(
                    "input write",
                    "UPDATE proj_launches SET last_write_seq = ?1 WHERE launch_id = ?2 AND thread_id = ?3",
                    rusqlite::params![seq, launch, thread],
                )?;
            }
            // A zero-byte attempt (only `RecheckFailed` leads from `sending` to `cancelled`)
            // gives the launch its write position back while no later write moved it.
            if c.from == InputState::Sending && c.to == InputState::Cancelled {
                p.exec(
                    "UPDATE proj_launches SET last_write_seq = (SELECT last_write_before FROM proj_inputs
                                                               WHERE input_id = ?1)
                     WHERE thread_id = ?2
                       AND launch_id = (SELECT launch_id FROM proj_inputs WHERE input_id = ?1)
                       AND last_write_seq IS (SELECT attempt_seq FROM proj_inputs WHERE input_id = ?1)",
                    rusqlite::params![c.input_id.to_string(), thread],
                )?;
            }
        }
        TypedEvent::SurfaceUpdate(s) => {
            if s.components.is_empty() {
                // A surface belongs to the stream whose events record it, so an update on one
                // stream never changes another stream's surface of the same id.
                p.exec(
                    "DELETE FROM proj_surfaces WHERE stream_id = ?1 AND surface_id = ?2",
                    [env.stream_id.to_string(), s.surface_id.0.clone()],
                )?;
            } else {
                let (kind, id) = match &s.scope {
                    SurfaceScope::Host => ("host", None),
                    SurfaceScope::Workspace { workspace_id } => {
                        ("workspace", Some(workspace_id.0.clone()))
                    }
                    SurfaceScope::Thread { thread_id } => ("thread", Some(thread_id.to_string())),
                };
                p.exec(
                    "INSERT INTO proj_surfaces (surface_id, stream_id, scope_kind, scope_id, payload, updated_seq)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT (stream_id, surface_id) DO UPDATE SET
                       scope_kind = excluded.scope_kind, scope_id = excluded.scope_id,
                       payload = excluded.payload, updated_seq = excluded.updated_seq",
                    rusqlite::params![s.surface_id.0, env.stream_id.to_string(), kind, id, to_json(s)?, seq],
                )?;
            }
        }
        TypedEvent::WorktreeChanged(w) => {
            p.exec(
                "INSERT INTO proj_worktrees (worktree_id, record, state, owner_thread_id, workspace_id, updated_seq)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (worktree_id) DO UPDATE SET record = excluded.record, state = excluded.state,
                   owner_thread_id = excluded.owner_thread_id, workspace_id = excluded.workspace_id,
                   updated_seq = excluded.updated_seq",
                rusqlite::params![
                    w.worktree_id.0,
                    to_json(w)?,
                    enum_text(&w.state),
                    w.owner_thread_id.map(|t| t.to_string()),
                    w.workspace_id.0,
                    seq,
                ],
            )?;
        }
        TypedEvent::MailboxMessage(_)
        | TypedEvent::AssistantMessage(_)
        | TypedEvent::ToolActivity(_)
        | TypedEvent::VtSnapshotTaken(_)
        | TypedEvent::RawUnknown(_) => {}
    }
    // Every transcript-sourced line moves the cursor's event position.
    if transcript && !matches!(typed, TypedEvent::TranscriptGeneration(_)) {
        let Some(pos) = &env.source.transcript else {
            return Err(ProjectionError("transcript event without a position".into()).into());
        };
        let continuity = transcript_continuity(e)?;
        p.exec_one(
            "transcript cursor",
            "UPDATE proj_transcript_cursors SET event_offset = ?1, event_continuity = ?2,
               read_continuity = CASE WHEN ?1 >= read_offset THEN ?2 ELSE read_continuity END,
               read_offset = max(read_offset, ?1)
             WHERE thread_id = ?3 AND generation = ?4 AND file_id = ?5",
            rusqlite::params![
                to_i64(pos.offset_end)?,
                continuity,
                thread_of(e)?.to_string(),
                to_i64(pos.generation)?,
                pos.file_id,
            ],
        )?;
    }
    Ok(())
}

fn transcript_continuity(e: &StoredEvent) -> Result<String, StoreError> {
    match e.annex()? {
        Some(EventAnnex::Transcript(c)) => Ok(c.0),
        _ => Err(ProjectionError("transcript event without its continuity annex".into()).into()),
    }
}

/// The current `task_state` of a thread, read before a `task_set` applies.
pub(crate) fn task_state(
    conn: &Connection,
    thread: ThreadId,
) -> Result<Option<String>, StoreError> {
    let p = ProjectionTx::new(conn);
    p.query_text(
        "SELECT task_state FROM proj_threads WHERE thread_id = ?1",
        [thread.to_string()],
    )
}

/// The `proj_*` tables, from `sqlite_schema`, so a new one cannot be left out.
pub(crate) fn projection_tables(conn: &Connection) -> Result<Vec<String>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_schema WHERE type = 'table' AND substr(name, 1, 5) = 'proj_' ORDER BY name",
    )?;
    let names = stmt.query_map([], |r| r.get(0))?;
    Ok(names.collect::<Result<_, _>>()?)
}

/// Deletes every projection row, children before their parents.
pub(crate) fn clear_projections(conn: &Connection) -> Result<(), StoreError> {
    let mut left = projection_tables(conn)?;
    let mut parents: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for t in &left {
        let mut stmt = conn.prepare("SELECT \"table\" FROM pragma_foreign_key_list(?1)")?;
        let ps = stmt.query_map([t], |r| r.get::<_, String>(0))?;
        parents.insert(t.clone(), ps.collect::<Result<_, _>>()?);
    }
    while !left.is_empty() {
        // A table no remaining table references.
        let Some(i) = left
            .iter()
            .position(|t| !left.iter().any(|o| o != t && parents[o].contains(t)))
        else {
            return Err(ProjectionError("projection foreign keys form a cycle".into()).into());
        };
        let t = left.remove(i);
        conn.execute(&format!("DELETE FROM \"{t}\""), [])?;
    }
    Ok(())
}

/// Every row of every projection table, keyed by its primary key.
pub(crate) fn dump_projections(
    conn: &Connection,
) -> Result<BTreeMap<String, TableDump>, StoreError> {
    let mut out = BTreeMap::new();
    for t in projection_tables(conn)? {
        let mut stmt = conn.prepare(&format!("SELECT * FROM \"{t}\""))?;
        let columns: Vec<String> = stmt.column_names().map(str::to_owned).collect();
        let n = columns.len();
        // The primary-key columns in key order (`pk` is the 1-based position in the key).
        let mut pk: Vec<(i64, usize)> = Vec::new();
        let mut info = conn.prepare(&format!("PRAGMA table_info(\"{t}\")"))?;
        let mut q = info.query([])?;
        while let Some(r) = q.next()? {
            let (name, at): (String, i64) = (r.get("name")?, r.get("pk")?);
            if at > 0
                && let Some(i) = columns.iter().position(|c| *c == name)
            {
                pk.push((at, i));
            }
        }
        pk.sort_unstable();
        if pk.is_empty() {
            return Err(ProjectionError(format!("{t} has no primary key")).into());
        }
        let mut rows = BTreeMap::new();
        let mut q = stmt.query([])?;
        while let Some(r) = q.next()? {
            let mut values = Vec::with_capacity(n);
            for i in 0..n {
                values.push(r.get::<_, Value>(i)?);
            }
            let key: Vec<&Value> = pk.iter().map(|&(_, i)| &values[i]).collect();
            let key = format!("{key:?}");
            rows.insert(key, values);
        }
        out.insert(t, TableDump { columns, rows });
    }
    Ok(out)
}

/// Compares maintained with rebuilt projections. Every column must be equal, except a
/// transcript cursor's read position: the rebuilt one must equal its event position
/// exactly, and the maintained one may be past it (mvp §2.1's only permitted rewind).
pub(crate) fn compare_projections(
    maintained: &BTreeMap<String, TableDump>,
    rebuilt: &BTreeMap<String, TableDump>,
) -> Vec<String> {
    let mut diff = Vec::new();
    let tables: BTreeSet<&String> = maintained.keys().chain(rebuilt.keys()).collect();
    for t in tables {
        let (Some(m), Some(r)) = (maintained.get(t), rebuilt.get(t)) else {
            diff.push(format!("{t}: table missing on one side"));
            continue;
        };
        if m.columns != r.columns {
            diff.push(format!("{t}: columns differ"));
            continue;
        }
        let col = |name: &str| m.columns.iter().position(|c| c == name);
        let rewind = if t == "proj_transcript_cursors" {
            match (
                col("event_offset"),
                col("event_continuity"),
                col("read_offset"),
                col("read_continuity"),
            ) {
                (Some(eo), Some(ec), Some(ro), Some(rc)) => Some((eo, ec, ro, rc)),
                _ => None,
            }
        } else {
            None
        };
        let keys: BTreeSet<&String> = m.rows.keys().chain(r.rows.keys()).collect();
        for k in keys {
            let (Some(mr), Some(rr)) = (m.rows.get(k), r.rows.get(k)) else {
                diff.push(format!("{t} {k}: row missing on one side"));
                continue;
            };
            for (i, name) in m.columns.iter().enumerate() {
                if let Some((eo, ec, ro, rc)) = rewind {
                    if i == ro {
                        if rr[ro] != rr[eo] {
                            diff.push(format!(
                                "{t} {k}: rebuilt read_offset is not its event_offset"
                            ));
                        }
                        let ahead = matches!(
                            (&mr[ro], &mr[eo]),
                            (Value::Integer(a), Value::Integer(b)) if a >= b
                        );
                        if !ahead {
                            diff.push(format!(
                                "{t} {k}: maintained read_offset is behind event_offset"
                            ));
                        }
                        continue;
                    }
                    if i == rc {
                        if rr[rc] != rr[ec] {
                            diff.push(format!(
                                "{t} {k}: rebuilt read_continuity is not its event_continuity"
                            ));
                        }
                        continue;
                    }
                }
                if mr[i] != rr[i] {
                    diff.push(format!(
                        "{t} {k}: {name} {:?} != rebuilt {:?}",
                        mr[i], rr[i]
                    ));
                }
            }
        }
    }
    diff
}
