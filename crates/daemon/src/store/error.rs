//! Store errors.

use std::fmt;

use chaldea_core::ids::{CommandId, IdempotencyKey, StreamId};
use rusqlite::ffi;

use super::annex::AnnexError;
use super::content::ContentError;
use super::rows::{Binding, TranscriptCursor};

/// A projection change that could not apply: a referenced row is missing, or a
/// compare-and-set matched no row. It poisons the job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionError(pub String);

impl fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An error of one store operation. Cloneable, so a poisoned `WriteTx` can return the
/// recorded error from every later call.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StoreError {
    /// The key exists in the stream with a different value of `field`.
    KeyConflict {
        stream_id: StreamId,
        key: IdempotencyKey,
        field: &'static str,
    },
    Content(ContentError),
    Annex(AnnexError),
    /// A transcript-sourced event or a `transcript_generation` went through `append`.
    TranscriptViaAppend,
    /// A new `status_changed(TaskState)` without an unused matching `task_set` of the same
    /// thread earlier in the job.
    TaskChangeWithoutCause,
    /// The thread's current launch, session, or transcript path is not the batch's.
    StaleBinding {
        stored: Option<Binding>,
    },
    /// The stored transcript cursor is not the batch's expected one.
    StaleCursor {
        stored: Option<TranscriptCursor>,
    },
    /// A generation start and transcript lines or a cursor advance of the same thread in
    /// one batch or one job.
    GenerationWithLines,
    /// A transcript batch whose offsets or shape break §2.3's rules.
    BadBatch(&'static str),
    /// A trigger or constraint refused a statement; the message names it.
    Constraint(String),
    Projection(ProjectionError),
    /// The stored `projection_version` is not this process's.
    ProjectionVersion {
        stored: String,
        code: u32,
    },
    /// `receipt_complete` for a receipt that is not `in_progress`.
    ReceiptNotInProgress(CommandId),
    /// A hook routing or promotion without events, or with the wrong kind.
    BadRouting(&'static str),
    /// `hook_delivery_promote` for a delivery that does not exist or is not buffered.
    NotBuffered,
    /// `SQLITE_FULL`.
    DiskFull,
    /// `SQLITE_BUSY` (another process holds a lock, or WAL recovery runs).
    Busy,
    /// Any other SQLite error, with its extended result code.
    Sqlite {
        extended_code: i32,
        message: String,
    },
    /// A stored value that does not decode into its Rust type.
    Decode(String),
}

impl StoreError {
    /// Corruption, a non-database file, or an I/O error: the writer enters the failed state.
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        match self {
            StoreError::Sqlite { extended_code, .. } => matches!(
                extended_code & 0xff,
                ffi::SQLITE_CORRUPT | ffi::SQLITE_NOTADB | ffi::SQLITE_IOERR | ffi::SQLITE_CANTOPEN
            ),
            _ => false,
        }
    }

    pub(crate) fn decode(what: impl fmt::Display) -> Self {
        StoreError::Decode(what.to_string())
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        match &e {
            rusqlite::Error::SqliteFailure(f, msg) => {
                let message = msg.clone().unwrap_or_else(|| f.to_string());
                match f.extended_code & 0xff {
                    ffi::SQLITE_FULL => StoreError::DiskFull,
                    ffi::SQLITE_BUSY => StoreError::Busy,
                    ffi::SQLITE_CONSTRAINT => StoreError::Constraint(message),
                    _ => StoreError::Sqlite {
                        extended_code: f.extended_code,
                        message: format!("{message} (extended code {})", f.extended_code),
                    },
                }
            }
            other => StoreError::Sqlite {
                extended_code: -1,
                message: other.to_string(),
            },
        }
    }
}

impl From<ContentError> for StoreError {
    fn from(e: ContentError) -> Self {
        StoreError::Content(e)
    }
}

impl From<AnnexError> for StoreError {
    fn from(e: AnnexError) -> Self {
        StoreError::Annex(e)
    }
}

impl From<ProjectionError> for StoreError {
    fn from(e: ProjectionError) -> Self {
        StoreError::Projection(e)
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::KeyConflict {
                stream_id,
                key,
                field,
            } => write!(
                f,
                "idempotency key {key} of {stream_id} is stored with a different {field}"
            ),
            StoreError::Content(e) => write!(f, "content: {e}"),
            StoreError::Annex(e) => write!(f, "annex: {e}"),
            StoreError::TranscriptViaAppend => {
                f.write_str("transcript events are stored only through append_transcript")
            }
            StoreError::TaskChangeWithoutCause => f.write_str(
                "status_changed(task_state) needs an unused matching task_set earlier in the job",
            ),
            StoreError::StaleBinding { stored } => {
                write!(f, "the batch's binding is not the thread's: {stored:?}")
            }
            StoreError::StaleCursor { stored } => {
                write!(
                    f,
                    "the stored transcript cursor is not the expected one: {stored:?}"
                )
            }
            StoreError::GenerationWithLines => f.write_str(
                "a generation start and lines or a cursor advance of one thread in one job",
            ),
            StoreError::BadBatch(why) => write!(f, "transcript batch: {why}"),
            StoreError::Constraint(m) => write!(f, "constraint: {m}"),
            StoreError::Projection(e) => write!(f, "projection: {e}"),
            StoreError::ProjectionVersion { stored, code } => write!(
                f,
                "the stored projection version {stored} is not this process's {code}"
            ),
            StoreError::ReceiptNotInProgress(id) => {
                write!(f, "receipt {id} is not in progress")
            }
            StoreError::BadRouting(why) => write!(f, "hook routing: {why}"),
            StoreError::NotBuffered => f.write_str("the hook delivery is not buffered"),
            StoreError::DiskFull => f.write_str("disk full (SQLITE_FULL)"),
            StoreError::Busy => f.write_str("database busy (SQLITE_BUSY)"),
            StoreError::Sqlite { message, .. } => write!(f, "sqlite: {message}"),
            StoreError::Decode(m) => write!(f, "stored value does not decode: {m}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// Why `read` returned no result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// No reader connection was free before `wait_until`.
    Busy,
    /// `shutdown` began.
    Closed,
    Store(StoreError),
}

impl From<StoreError> for ReadError {
    fn from(e: StoreError) -> Self {
        ReadError::Store(e)
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::Busy => f.write_str("no reader connection was free in time"),
            ReadError::Closed => f.write_str("the store is shutting down"),
            ReadError::Store(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ReadError {}

/// Why `open` failed. Every error from the checks of an existing file comes before Chaldea
/// changes it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum OpenError {
    /// A `StoreConfig` value is out of range; nothing was touched.
    Config(String),
    /// Creating or opening the file failed.
    Io(String),
    NotADatabase,
    /// Another application's database (`application_id`), or `0` with schema objects.
    Foreign {
        application_id: i32,
    },
    NewerSchema {
        found: u32,
        supported: u32,
    },
    /// The schema differs from the one this code ships for `version`.
    SchemaMismatch {
        version: u32,
        names: Vec<String>,
    },
    /// The database's text encoding is not UTF-8 (SQLite fixes it at creation); the store
    /// binds UTF-8 byte counts that `octet_length` must agree with.
    Encoding(String),
    /// `journal_mode=WAL` returned another mode.
    NoWal(String),
    /// A connection setting did not take effect.
    Setup(String),
    Store(StoreError),
}

impl From<StoreError> for OpenError {
    fn from(e: StoreError) -> Self {
        OpenError::Store(e)
    }
}

impl From<rusqlite::Error> for OpenError {
    fn from(e: rusqlite::Error) -> Self {
        match &e {
            // The first statement that reads the file (a pragma during setup, or a check)
            // finds that it is not a database.
            rusqlite::Error::SqliteFailure(f, _)
                if f.extended_code & 0xff == ffi::SQLITE_NOTADB =>
            {
                OpenError::NotADatabase
            }
            _ => OpenError::Store(e.into()),
        }
    }
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenError::Config(m) => write!(f, "store configuration: {m}"),
            OpenError::Io(m) => write!(f, "io: {m}"),
            OpenError::NotADatabase => f.write_str("the file is not a database"),
            OpenError::Foreign { application_id } => {
                write!(
                    f,
                    "not a Chaldea database (application_id {application_id:#x})"
                )
            }
            OpenError::NewerSchema { found, supported } => write!(
                f,
                "schema version {found} is newer than this binary's {supported}"
            ),
            OpenError::SchemaMismatch { version, names } => write!(
                f,
                "the schema differs from version {version}'s: {}",
                names.join(", ")
            ),
            OpenError::Encoding(e) => write!(f, "the database encoding is {e}, not UTF-8"),
            OpenError::NoWal(mode) => write!(f, "journal_mode=WAL returned {mode}"),
            OpenError::Setup(m) => write!(f, "connection setup: {m}"),
            OpenError::Store(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for OpenError {}
