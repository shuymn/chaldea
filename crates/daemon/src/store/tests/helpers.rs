//! Helpers for the privileged unit tests: stores with fault points, production-configured raw
//! connections, and raw event inserts.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use tempfile::TempDir;

use super::support::{FixedClock, T0, config};
use crate::store::open::{Role, connect};
use crate::store::writer::Faults;
use crate::store::{Store, StoreError};

/// Opens a store in `dir` with the given fault points.
pub(crate) fn open_with(dir: &Path, faults: Faults) -> Result<Store, crate::store::OpenError> {
    crate::store::open::open(
        config(dir.join("chaldea.db"), FixedClock::new(T0)),
        Arc::new(faults),
    )
}

/// A temp directory and a store in it with the given fault points.
pub(crate) fn store_with(faults: Faults) -> (TempDir, Store, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let store = open_with(dir.path(), faults).unwrap();
    let path = dir.path().join("chaldea.db");
    (dir, store, path)
}

/// A read-write connection with the production settings.
pub(crate) fn raw(path: &Path) -> Connection {
    connect(path, Role::Writer, Duration::from_secs(5)).unwrap()
}

/// A production connection with `recursive_triggers` turned off afterwards (the control).
pub(crate) fn raw_without_recursive_triggers(path: &Path) -> Connection {
    let c = raw(path);
    c.pragma_update(None, "recursive_triggers", 0).unwrap();
    c
}

/// The fields of a raw `events` insert; `None` for `seq`, `cum_bytes`, and `payload_bytes`
/// computes the valid value.
#[derive(Debug, Clone)]
pub(crate) struct RawEvent {
    pub(crate) stream: String,
    pub(crate) key: String,
    pub(crate) kind: String,
    pub(crate) thread: Option<String>,
    pub(crate) launch: Option<String>,
    pub(crate) source: String,
    pub(crate) payload: String,
    pub(crate) annex: Option<String>,
    pub(crate) schema_version: i64,
    pub(crate) seq: Option<i64>,
    pub(crate) cum_bytes: Option<i64>,
    pub(crate) payload_bytes: Option<i64>,
    pub(crate) global_seq: Option<i64>,
    pub(crate) event_id: Option<String>,
}

pub(crate) const DAEMON_SOURCE: &str =
    r#"{"kind":"daemon","agent":null,"cli_version":null,"transcript":null}"#;

/// A transcript source.
pub(crate) fn transcript_source(
    generation: u64,
    path: &str,
    file_id: &str,
    offset_end: u64,
) -> String {
    format!(
        r#"{{"kind":"transcript","agent":"claude","cli_version":null,"transcript":{{"generation":{generation},"path":"{path}","file_id":"{file_id}","offset_end":{offset_end}}}}}"#
    )
}

impl RawEvent {
    pub(crate) fn new(stream: &str, key: &str, kind: &str, payload: &str) -> Self {
        let thread = stream.strip_prefix("thread:").map(str::to_owned);
        RawEvent {
            stream: stream.to_owned(),
            key: key.to_owned(),
            kind: kind.to_owned(),
            thread,
            launch: None,
            source: DAEMON_SOURCE.to_owned(),
            payload: payload.to_owned(),
            annex: None,
            schema_version: 1,
            seq: None,
            cum_bytes: None,
            payload_bytes: None,
            global_seq: None,
            event_id: None,
        }
    }

    pub(crate) fn launch(mut self, l: &str) -> Self {
        self.launch = Some(l.to_owned());
        self
    }

    pub(crate) fn source(mut self, s: String) -> Self {
        self.source = s;
        self
    }

    pub(crate) fn annex(mut self, a: &str) -> Self {
        self.annex = Some(a.to_owned());
        self
    }

    /// Inserts the row.
    pub(crate) fn insert(&self, c: &Connection) -> rusqlite::Result<usize> {
        let event_id = self
            .event_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        c.execute(
            "INSERT INTO events (global_seq, stream_id, seq, event_id, schema_version, idempotency_key, kind,
               thread_id, launch_id, command_id, source, occurred_at, occurred_explicit, recorded_at, payload,
               payload_bytes, cum_bytes, annex)
             VALUES (?1, ?2,
               coalesce(?3, coalesce((SELECT max(seq) FROM events WHERE stream_id = ?2), 0) + 1),
               ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10, ?11, 0, ?11, ?12,
               coalesce(?13, octet_length(?12)),
               coalesce(?14, coalesce((SELECT cum_bytes FROM events WHERE stream_id = ?2 ORDER BY seq DESC LIMIT 1), 0)
                 + coalesce(?13, octet_length(?12))),
               ?15)",
            rusqlite::params![
                self.global_seq,
                self.stream,
                self.seq,
                event_id,
                self.schema_version,
                self.key,
                self.kind,
                self.thread,
                self.launch,
                self.source,
                "2026-11-22T00:00:00.000Z",
                self.payload,
                self.payload_bytes,
                self.cum_bytes,
                self.annex,
            ],
        )
    }
}

/// Whether `e` is a statement-only failure (a `CHECK`, `UNIQUE`, or foreign key).
pub(crate) fn is_constraint(e: &rusqlite::Error) -> bool {
    matches!(StoreError::from(clone_err(e)), StoreError::Constraint(_))
}

fn clone_err(e: &rusqlite::Error) -> rusqlite::Error {
    match e {
        rusqlite::Error::SqliteFailure(f, m) => rusqlite::Error::SqliteFailure(*f, m.clone()),
        other => rusqlite::Error::InvalidParameterName(other.to_string()),
    }
}

/// The message of a refused statement.
pub(crate) fn message(e: &rusqlite::Error) -> String {
    e.to_string()
}

/// Every row of `table`, as text, for before/after comparisons.
pub(crate) fn dump(c: &Connection, table: &str) -> Vec<String> {
    let mut stmt = c
        .prepare(&format!("SELECT * FROM \"{table}\" ORDER BY 1"))
        .unwrap();
    let n = stmt.column_count();
    stmt.query_map([], |r| {
        Ok((0..n)
            .map(|i| format!("{:?}", r.get::<_, rusqlite::types::Value>(i).unwrap()))
            .collect::<Vec<_>>()
            .join("|"))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}
