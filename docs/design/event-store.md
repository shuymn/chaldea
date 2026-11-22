# Design: Event Store (MVP PR 6)

Status: design, not implemented. Scope is the PR 6 row of `docs/mvp-design.md` §4 (cited as "mvp §N"): the store in a new `crates/daemon`, holding append-only events, projections, command receipts, hook deliveries, tokens, and blobs. Its design obligations: drop-and-rebuild equality for every projection; `INSERT OR REPLACE`, update, and delete on `events` abort; `transcript_generation` before new-generation lines; a stale-cursor batch aborts; blob offload and lossy preview. Plan-layer claims it owns (mvp §6.2): CL-9, CL-10 (store side), CL-11, CL-12 (store side).

The contract implemented is mvp §2.1 ("Store", "Projections", "Transcript cursor rules", "Idempotency keys", "`Content`") and the store side of mvp §2.3 (gate evidence), §2.4 (epoch, `head_seq`, `cum_bytes`, receipts), §2.6 (worktree record), §2.9 (launch reservation, hook deliveries), and §2.11. No public type of `crates/core` and no public contract of mvp changes; §8 lists the two places where the store needs an internal mechanism the mvp text does not name.

**What the store does and does not decide.** The store guarantees ordering, immutability, atomicity, idempotency by key, projection consistency, and durability, and it exposes every read and write the later PRs need as an operation inside one transaction. It does not decide routes, tokens, gate policy, hook binding, reservation preconditions, transcript continuity, or retention; those PRs run their checks inside a store transaction (§2.2), so a check and the write it guards are atomic.

Out of scope, designed in their own PRs: HTTP routes, token issuance and checks, WebSocket, pairing (PR 7); daemon lifecycle, state directory, single-instance lock (PR 8); hook ingest and the emitter (PR 9); the supervisor, reservation checks, and gate decisions (PR 10); transcript tailers and continuity detection (PR 13, 14).

Sources read for this document (paths are relative to each root):

| Source | Revision | Read for |
|---|---|---|
| This repository | `6e72982` | `crates/core/src/{event,payload,state,input,sync,command,ids,lib}.rs`, `crates/core/clippy.toml`, `Cargo.toml`, `Cargo.lock`, `.github/workflows/ci.yml`, `lefthook.yml`, `docs/architecture.md` §3–§4, `docs/design/pty-runtime.md` |
| T3 Code (`pingdotgg/t3code`) | `10586f9e` | `apps/server/src/persistence/Migrations/{001_OrchestrationEvents,055_OrchestrationV2}.ts`, `persistence/{OrchestrationEventStore,OrchestrationCommandReceipts,Sqlite}.ts`, `orchestration-v2/{CommandReceiptStore,EventStore,ProjectionStore,ProjectionMaintenance}.ts` |
| `rusqlite` | crate 0.41.1 | `Cargo.toml` (features), `src/transaction.rs`, `src/config.rs`, `src/busy.rs`, `src/lib.rs` |
| `libsqlite3-sys` | crate 0.39.1 | `build.rs` (bundled compile flags), `sqlite3/sqlite3.c` (version, unix file mode, win32 long paths) |
| SQLite documentation | 3.54 | "Write-Ahead Logging", "PRAGMA synchronous", "PRAGMA recursive_triggers", "CREATE TRIGGER", "The ON CONFLICT Clause", "The RETURNING Clause", "STRICT Tables", "Generated Columns", "Atomic Commit In SQLite", "Database File Format" (application id, user version), "sqlite3_db_config" (`SQLITE_DBCONFIG_DEFENSIVE`) |

**Facts this design relies on** (S). Each is exercised by a named test of §7 on both runners; the tests, not this table, prove what the store does.

| # | Fact | Where |
|---|---|---|
| S1 | The bundled build compiles SQLite 3.54.1 from source with `cc`, with `SQLITE_DEFAULT_FOREIGN_KEYS=1`, `SQLITE_THREADSAFE=1`, `SQLITE_USE_URI`, and the JSON functions. Both runners therefore run the same SQLite, independent of the OS copy. | `libsqlite3-sys` `sqlite3/sqlite3.c:470`, `build.rs:170-187` |
| S2 | `REPLACE` conflict resolution deletes the conflicting row, and delete triggers fire for that deletion only when `recursive_triggers` is on; it is off by default and set per connection. With it off, `INSERT OR REPLACE` on a conflicting unique key silently removes the old row. | "The ON CONFLICT Clause"; "PRAGMA recursive_triggers" |
| S3 | `INSERT … ON CONFLICT DO UPDATE` (upsert) runs an `UPDATE`, which fires update triggers. A `DELETE` without `WHERE` on a table with triggers does not use the truncate optimization, so the delete trigger fires. | "UPSERT"; "DELETE" (truncate optimization) |
| S4 | `RAISE(ROLLBACK, msg)` in a trigger rolls back the whole transaction; `RAISE(ABORT, msg)` and constraint failures undo only the statement and leave the transaction open. | "CREATE TRIGGER" (RAISE) |
| S5 | `BEGIN IMMEDIATE` takes the write lock at `BEGIN`; while it is held no other connection, in this or another process, can write. Every read inside the transaction sees the state the write will commit against. | "BEGIN TRANSACTION"; "File Locking And Concurrency" |
| S6 | In WAL mode a transaction is committed when its commit frame is in the WAL; with `synchronous=FULL` the WAL is synced before `COMMIT` returns. Frames after the last commit frame are ignored on recovery, so a crash mid-transaction leaves no part of it. `synchronous=NORMAL` may roll back recent commits after power loss. | "Write-Ahead Logging"; "PRAGMA synchronous"; "Atomic Commit In SQLite" |
| S7 | WAL readers see a consistent snapshot for the duration of their read transaction and never block the writer; a long read keeps the WAL from being reset. WAL does not work over a network filesystem. | "Write-Ahead Logging" |
| S8 | `application_id` and `user_version` are 32-bit integers in the database header, written in the same transaction as other changes. | "Database File Format" §1.3 |
| S9 | `STRICT` tables (3.37) enforce column types; `RETURNING` (3.35), generated columns (3.31), `json_tree`, `json_extract`, and `octet_length` (3.43) are available, and `json_tree` and `json_extract` are allowed in triggers and generated columns with `trusted_schema=OFF`. | "STRICT Tables"; "The RETURNING Clause"; "Generated Columns"; "JSON Functions" |
| S10 | `SQLITE_DBCONFIG_DEFENSIVE` disables `PRAGMA writable_schema` edits and other direct corruption paths on that connection. rusqlite exposes it as `DbConfig::SQLITE_DBCONFIG_DEFENSIVE` through `Connection::set_db_config`. | "sqlite3_db_config"; `rusqlite` `src/config.rs:38`, `:123` |
| S11 | On Unix, SQLite creates the `-wal` and `-shm` files with the database file's permissions. On Windows, the VFS accepts `\\?\`-prefixed paths and uses wide-character file APIs. | `sqlite3.c:47085-47100` (`findCreateFileMode`), `:53945` (`winIsLongPathPrefix`) |
| S12 | rusqlite: `Transaction` rolls back on drop by default; `TransactionBehavior::Immediate` issues `BEGIN IMMEDIATE`; `busy_timeout` sets the busy handler; the `cache` feature gates prepared-statement caching and pulls `hashlink`. | `rusqlite` `src/transaction.rs:9-28`, `:128`; `src/busy.rs:24`; `src/lib.rs:83-90`, `Cargo.toml` `[features]` |

## 1. Goal and Files

**Goal.** A `chaldea-daemon` library crate whose `store` module owns one SQLite database: one writer thread that runs every write as a `BEGIN IMMEDIATE` transaction, a small pool of read-only connections, and a schema in which SQLite itself enforces append-only events, gapless per-stream `seq`, the running `cum_bytes`, transcript generation order, blob references, and the immutable parts of receipts and hook deliveries. Projections are written by one Rust function, used by live appends and by rebuild alike. Everything runs on `macos-latest` and `windows-latest` CI against temporary files.

**Files.**

```
Cargo.toml                                  # members += "crates/daemon"; [workspace.dependencies] += rusqlite, sha2, tokio, base64, tempfile
crates/daemon/Cargo.toml                    # chaldea-daemon (lib only in PR 6); [lints] workspace = true
crates/daemon/src/lib.rs                    # pub mod store; crate doc: the store rules of §2
crates/daemon/src/store/mod.rs              # Store, StoreConfig, Clock, IdSource, WriteTicket, Committed, errors
crates/daemon/src/store/open.rs             # open, pragmas, application_id / user_version checks, migrations, schema fingerprint
crates/daemon/src/store/schema.rs           # MIGRATIONS: &[&str] (v1), PROJECTION_TABLES, expected-schema fingerprint
crates/daemon/src/store/writer.rs           # writer thread, job queue, deadlines, panics, failed state, notifications
crates/daemon/src/store/readers.rs          # read-only connection pool, ReadTx
crates/daemon/src/store/write_tx.rs         # WriteTx: append, transcript batch, receipts, hook deliveries, tokens, rebuild
crates/daemon/src/store/append.rs           # NewEvent → row; idempotency lookup; envelope assembly
crates/daemon/src/store/project.rs          # apply(&mut ProjectionTx, &StoredEvent): the one projection function
crates/daemon/src/store/rows.rs             # internal row types (ThreadRow, LaunchRow, InputRow, ...) and snapshot assembly
crates/daemon/src/store/content.rs          # prepare_content: inline / blob decision, SHA-256, preview
crates/daemon/src/store/clock.rs            # SystemClock (RFC 3339 UTC ms formatting), RandomIds
crates/daemon/tests/store_{append,triggers,rebuild,transcript,content,receipts,hooks,open,writer,reads,crash}.rs
crates/daemon/tests/support/mod.rs          # temp store, fixed clock and ids, event generators for proptest
```

**Dependencies.** Root `[workspace.dependencies]`, exact pins in the existing style:

```toml
# The store (docs/design/event-store.md): SQLite 3.54.1 compiled from the bundled source, so
# macOS and Windows run the same SQLite. No default features (prepared-statement cache, wasm FFI).
rusqlite = { version = "=0.41.1", default-features = false, features = ["bundled"] }
# Blob content addresses (mvp §2.1 `Content`); the token hashes of PR 7 reuse it.
sha2 = "=0.11.1"
# The writer's reply channel and commit notifications only; PR 7 and PR 8 add the runtime features.
tokio = { version = "=1.54.1", default-features = false, features = ["sync"] }
# Inline `Content` with `encoding: base64` (already in the lock through alacritty_terminal).
base64 = "=0.22.1"
# Test databases in the OS temp directory (already in the lock through proptest).
tempfile = "=3.27.0"
```

```toml
# crates/daemon/Cargo.toml
[dependencies]
chaldea-core = { path = "../core" }
rusqlite.workspace = true
sha2.workspace = true
tokio.workspace = true
base64.workspace = true
serde.workspace = true
serde_json.workspace = true
# Event ids and the epoch are random v4 UUIDs (getrandom is already in the lock).
uuid = { workspace = true, features = ["v4"] }

[dev-dependencies]
proptest.workspace = true
tempfile.workspace = true

[lints]
workspace = true
```

`libsqlite3-sys` is not a direct dependency: `rusqlite =0.41.1` requires `0.39`, and `Cargo.lock` holds `0.39.1`, so `--locked` fixes the SQLite source. `rusqlite` needs Rust 1.89 (`rust-version` in its manifest); the toolchain is 1.96.0. The bundled build needs a C compiler: Xcode's `cc` on `macos-latest`, MSVC on `windows-latest`, both preinstalled. `sha2` is listed for `core` in mvp §3 but is not yet in `core`'s manifest; PR 6 adds it to the daemon only. The daemon stays `unsafe_code = "forbid"` through `[lints] workspace = true`; the store needs no `unsafe` (file permissions on Unix use `std::os::unix::fs::OpenOptionsExt::mode`).

**Workspace and CI.** The crate joins `members`. The `workspace` job runs unchanged; `cargo test --workspace --locked` now includes the store tests on both runners. No new CI step: the existing `core` dependency check keeps `rusqlite` and `tokio` out of `core`, and the store tests are not `cfg`-gated, so no Windows sentinel list is needed. `lefthook.yml` needs no change (its globs cover `crates/**`).

## 2. Types and Functions

Names may change in review; shapes may not.

### 2.1 The Rules

1. **SQLite is the proof, Rust is the second line.** Every invariant that a schema can express is a constraint or trigger (§2.4): `STRICT` types, `CHECK`, `UNIQUE`, foreign keys, triggers that `RAISE(ROLLBACK)` (S4), and `BEGIN IMMEDIATE` for check-then-write (S5). Rust checks the same conditions first to return a typed error, never as the only guard.
2. **No cross-call state outside the database.** The stream head, `cum_bytes`, cursor, receipt state, delivery routing, and every projection value are read inside the transaction that depends on them. The store keeps only the epoch in memory, read once at open; it cannot change, because the `meta` row is immutable by trigger (§2.4), and `open::epoch_survives_reopen` would fail if the cached value drifted from the row.
3. **One transaction per job.** A caller hands the writer a closure over `WriteTx`; the closure's reads and writes form one `BEGIN IMMEDIATE` transaction that commits only if the closure returns `Ok`. Nothing a caller can do commits part of a job.
4. **Projections change only through `project::apply`**, called for each event inside the transaction that inserts it, and by rebuild on the same events in the same order (§2.6).

**Bounds of these guarantees.** They hold for every connection the store opens. A process with raw access to the database file can still change anything: drop a trigger, edit rows on a connection without `recursive_triggers`, or rewrite pages. The store detects a changed schema at the next open (§2.8, schema fingerprint) but not a row edited by another program while the schema is intact, and it does not detect a change made while it is running. Chaldea's file permissions (§5) limit who can do that to the user's own processes.

### 2.2 `Store`, Writer, Readers

```rust
pub struct StoreConfig {
    pub path: PathBuf,                     // <state_dir>/chaldea.db; the directory exists (PR 8 creates it)
    pub readers: usize,                    // 4
    pub writer_queue: usize,               // 256 jobs
    pub busy_timeout: Duration,            // 5 s
    pub clock: Arc<dyn Clock>,             // recorded_at; tests pass a fixed clock
    pub ids: Arc<dyn IdSource>,            // event ids, the epoch; tests pass a counter
}
pub trait Clock: Send + Sync { fn now(&self) -> (Instant, Timestamp); }      // RFC 3339 UTC, ms
pub trait IdSource: Send + Sync { fn event_id(&self) -> EventId; fn epoch(&self) -> Epoch; }

#[derive(Clone)] pub struct Store { /* Arc: writer handle, reader pool, epoch, notifier */ }
impl Store {
    pub fn open(cfg: StoreConfig) -> Result<Store, OpenError>;
    pub fn epoch(&self) -> &Epoch;
    pub fn paths(&self) -> StorePaths;     // db, -wal, -shm; for doctor and PR 8
    /// Queues a write job. Fails at once, without waiting, when the queue is full or closed.
    pub fn submit<R, F>(&self, not_after: Instant, job: F) -> Result<WriteTicket<R>, SubmitError>
    where F: FnOnce(&mut WriteTx<'_>) -> Result<R, JobError> + Send + 'static, R: Send + 'static;
    /// Runs `f` in one read transaction on a pooled read-only connection.
    pub fn read<R>(&self, wait_until: Instant, f: impl FnOnce(&ReadTx<'_>) -> Result<R, StoreError>) -> Result<R, ReadError>;
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Committed>;
    pub fn shutdown(self, deadline: Instant) -> ShutdownReport;
}

pub struct WriteTicket<R>(tokio::sync::oneshot::Receiver<Result<R, WriteError>>);
impl<R> Future for WriteTicket<R> { type Output = Result<R, WriteError>; }   // async callers
impl<R> WriteTicket<R> { pub fn blocking_wait(self) -> Result<R, WriteError>; } // tests, sync callers

pub enum SubmitError { QueueFull, Closed, Failed /* writer in the failed state, §5 */ }
pub enum WriteError {
    Expired,                 // `not_after` passed before the job started: nothing ran
    Job(JobError),           // the job returned Err: rolled back
    Store(StoreError),       // a statement failed: rolled back
    CommitUnknown(StoreError), // COMMIT itself failed with an I/O error: may or may not be durable
    Panicked,                // the job panicked: rolled back
    WriterGone,              // the writer thread ended before replying
}
pub enum JobError { Store(StoreError), Refused(Box<dyn std::error::Error + Send + Sync>) }
pub struct Committed { pub heads: Vec<StreamHeadEntry> }   // streams this job advanced, after COMMIT
```

**Writer.** One OS thread, `store-writer`, owns the only read-write connection. Jobs arrive over a bounded `std::sync::mpsc::sync_channel(writer_queue)`; `submit` uses `try_send`, so a caller never waits for queue space. For each job the writer:

1. reads the clock; if `now > not_after`, replies `Expired` and runs nothing. This is the one point where the deadline and the commit are decided together (mvp §2.5 "Deadlines"): a job past its deadline before it starts has no effect; a job that started commits or rolls back on its own merits, and a caller that stopped waiting learns the outcome from the receipt (§2.7);
2. `BEGIN IMMEDIATE` (waits at most `busy_timeout` for another process's lock, S5), stamps the job's `recorded_at` from the same clock read, runs the closure under `catch_unwind`;
3. on `Ok`, `COMMIT`, then replies `Ok(r)`, then publishes `Committed` on a `tokio::sync::broadcast` channel (capacity 1024; a lagging subscriber gets `Lagged` and resyncs from heads, which mvp §2.4's periodic `heads` already covers); on `Err` or panic, `ROLLBACK` and replies the error.

The reply is sent only after `COMMIT` returned, so `Ok` means durable (§2.9). The writer holds no state between jobs except the connection; every value a job needs is read inside its transaction.

**Readers.** `readers` connections opened read-only (`SQLITE_OPEN_READ_ONLY`, plus `PRAGMA query_only=ON`) in a `Mutex<Vec<Connection>>` with a `Condvar`; `read` waits for a free connection until `wait_until`, else `ReadError::Busy`. Each call runs `BEGIN DEFERRED` … `COMMIT`, so everything `f` reads comes from one WAL snapshot (S7): a snapshot and its `as_of_seq`, or events and `head_seq`, are always consistent. `read` is synchronous; async callers run it in `spawn_blocking` (PR 7). Reads must be short: a read transaction held open keeps the WAL from resetting.

**Alternatives** are in §3 ("Writer model").

### 2.3 Write Operations (`WriteTx`)

Every method runs inside the job's transaction; its effects commit with the job. "Error" means the method returns `Err`, the job returns it, and the writer rolls back the whole job.

```rust
impl WriteTx<'_> {
    pub fn recorded_at(&self) -> &Timestamp;
    pub fn read(&self) -> &ReadTx<'_>;                      // every ReadTx query, inside the write transaction

    pub fn append(&mut self, events: Vec<NewEvent>) -> Result<Vec<Appended>, StoreError>;
    pub fn append_transcript(&mut self, b: TranscriptBatch) -> Result<Vec<Appended>, StoreError>;

    pub fn receipt_begin(&mut self, id: CommandId, token: TokenId, request_sha256: [u8; 32]) -> Result<ReceiptLookup, StoreError>;
    pub fn receipt_complete(&mut self, id: CommandId, result: &CommandResult) -> Result<(), StoreError>;
    pub fn receipts_mark_unknown_after_crash(&mut self) -> Result<u64, StoreError>;

    pub fn hook_delivery_record(&mut self, d: NewHookDelivery) -> Result<HookDeliveryLookup, StoreError>;
    pub fn hook_delivery_promote(&mut self, launch: LaunchId, emit: Uuid, to: Promotion) -> Result<Vec<Appended>, StoreError>;

    pub fn token_insert(&mut self, t: NewToken) -> Result<(), StoreError>;
    pub fn token_revoke(&mut self, which: TokenSelector) -> Result<u64, StoreError>;

    pub fn rebuild_projections(&mut self) -> Result<RebuildReport, StoreError>;
    pub fn verify_projections(&mut self) -> Result<ProjectionDiff, StoreError>;   // rebuild in a savepoint, compare, roll back
}

pub struct NewEvent {
    pub stream_id: StreamId,
    pub idempotency_key: IdempotencyKey,
    pub event: TypedEvent,                    // known kinds only; payload serialized by the store
    pub thread_id: Option<ThreadId>, pub launch_id: Option<LaunchId>, pub command_id: Option<CommandId>,
    pub source: Source,
    pub occurred_at: Option<Timestamp>,       // None: recorded_at
    pub annex: Option<EventAnnex>,            // internal, never on the wire (§2.6, D7)
    pub blobs: Vec<NewBlob>,                  // the blobs its Content fields reference (from prepare_content)
}
pub struct EventAnnex { pub write_basis: Option<String>, pub continuity: Option<String> }  // opaque to the store
pub enum Appended { New(EventEnvelope), Existing(EventEnvelope) }
```

**`append`.** Inputs: events in order. For each event, inside the transaction:

1. Look up `(stream_id, idempotency_key)`. If a row exists and its kind, payload (as JSON values), source, and annex equal the new event's, the result is `Existing(stored)`: no row, no `seq`, no projection change. If any differs, error `KeyConflict { stream_id, key }`, which rolls back the whole job (CL-10: a replayed source never creates a second event; a key reused for different content is a producer bug and must not pass silently). Earlier events of the same batch count, so a batch that repeats a key gets `Existing` for the repeat.
2. Insert the blobs (`INSERT … ON CONFLICT(sha256) DO NOTHING`; content addressing makes an existing row the same bytes, §2.5).
3. Insert the row with `seq` and `cum_bytes` computed in the `INSERT` from the stream's last row, `RETURNING global_seq, seq, cum_bytes`. The trigger `events_order` re-checks both (§2.4). `event_id` comes from `IdSource`, `recorded_at` from the job.
4. Call `project::apply` for the event (§2.6). An apply error (`ProjectionError`: a referenced row is missing, a compare-and-set on a state column failed) rolls back the job.

Effects: rows in `events`, `blobs`, `blob_refs` (by trigger), and the projection tables. Errors: `KeyConflict`, `Constraint(String)` (a trigger or `CHECK` refused; the message names it), `Projection(ProjectionError)`, `Sqlite(..)`. The store issues only plain `INSERT` on `events`; no `REPLACE`, `OR IGNORE`, or upsert.

**`append_transcript`.** One transcript batch (mvp §2.1 "Transcript cursor rules"):

```rust
pub struct TranscriptBatch {
    pub thread_id: ThreadId,
    pub expect: Option<CursorPos>,            // the cursor the tailer read from; None: no cursor yet
    pub events: Vec<NewEvent>,                // a transcript_generation first if the generation changes, then lines
    pub advance_to: ReadPos,                  // where the tailer stopped: offset just after the last complete line
}
pub struct CursorPos { pub generation: u64, pub file_id: String, pub read_offset: u64 }
pub struct ReadPos { pub read_offset: u64, pub continuity: Option<String> }
```

Transaction: read `proj_transcript_cursors` for the thread; if it differs from `expect` in any of generation, `file_id`, or `read_offset` (or exists when `expect` is `None`), error `StaleCursor { stored }` before any insert. Then `append(events)` (duplicate keys after a rewind come back `Existing`), then `UPDATE proj_transcript_cursors SET read_offset = ?, read_continuity = ? WHERE thread_id = ? AND generation = ? AND file_id = ? AND read_offset <= ?`, which must change exactly one row, else `StaleCursor`. The stale check is a compare-and-set inside `BEGIN IMMEDIATE`, so no other writer, in any connection or process, can move the cursor between the compare and the set (S5); the conditional `UPDATE` with its row count is the engine-level form of the same check. Generation order, `file_id`, path, and offsets are enforced by triggers (§2.4): a line of a generation that is not the stream's latest recorded `transcript_generation` aborts the batch. A batch with no events only advances `read_offset` past lines that produced none.

**Receipts** (mvp §2.4). `receipt_begin` reads the row for `command_id`, calls `core::command::check_receipt`, and for `New` inserts it `in_progress` with the token and request hash; it returns the `ReceiptLookup`. The caller calls it first in the job that records the command's intent and continues only on `New`; a `Replay` job commits nothing new. Two concurrent duplicates are serialized by the writer and by `BEGIN IMMEDIATE`, so the second reads the first's row; the primary key is the backstop if a second process inserts. `receipt_complete` sets `completed` with the HTTP status and body; `receipts_mark_unknown_after_crash` sets every `in_progress` row to `unknown_after_crash` (PR 8 / PR 10 call it once, before accepting commands). A trigger allows only `in_progress → completed | unknown_after_crash` and keeps `command_id`, token, and hash unchanged (§2.4).

**Hook deliveries** (mvp §2.1 hook keys, §2.9).

```rust
pub struct NewHookDelivery {
    pub launch_id: LaunchId, pub emit_id: Uuid,
    pub first_reply: HookReply,               // { http_status: u16, body: String }, immutable once stored
    pub routing: NewRouting,
}
pub enum NewRouting { Appended(Vec<NewEvent>), Buffered(PendingHook), RawUnknown(NewEvent) }
pub struct PendingHook { pub event: NewEvent, pub note: String }  // note: PR 9's opaque ancestry evidence
pub enum HookDeliveryLookup { New(HookDelivery), Replay(HookDelivery) }
pub struct HookDelivery { pub ordinal: u64, pub first_reply: HookReply, pub routing: Routing }
pub enum Routing { Appended { stream_id: StreamId, seq: u64 }, Buffered, RawUnknown { stream_id: StreamId, seq: u64 } }
pub enum Promotion { Append, RawUnknown(NewEvent) }
```

`hook_delivery_record`: if `(launch_id, emit_id)` exists, returns `Replay` with the stored first reply and the current routing, and changes nothing. Otherwise it appends the routing's events, takes the next ordinal of the launch (computed in the `INSERT`, checked gapless by trigger), and inserts the row with the reply and routing (`seq` of the first event appended for it). `hook_delivery_promote` moves one `buffered` row to `appended` (appending its stored pending event) or to `raw_unknown` (appending the given `raw_unknown` event); PR 9 calls it for each buffered delivery in ordinal order inside the binding job, after `session_bound`. The trigger allows only `buffered → appended | raw_unknown`, and never changes the first reply, ordinal, emit id, or launch (§2.4). Durable before the response: the caller replies after the ticket resolves (§2.9).

**Tokens.** `token_insert` stores `{ token_id, kind, sha256, thread_id, launch_id, created_at }`; `token_revoke` sets `revoked_at` for one token, a launch's tokens, or a thread's tokens. A trigger allows only `revoked_at` from `NULL` to a value. Issuance, hashing of secrets, and comparison are PR 7's; the reservation of mvp §2.9 inserts the launch's hook and agent token hashes in the same job as `launch_reserved`.

**Rebuild** (§2.6).

### 2.4 Schema (Migration v1)

Internal (mvp §2.1). All tables `STRICT`. Every connection sets the pragmas of §2.8 before use. Text columns hold canonical strings (`StreamId`, UUIDs, timestamps) as core serializes them.

```sql
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;   -- 'epoch', 'created_at', 'projection_version'

CREATE TABLE events (
  global_seq      INTEGER PRIMARY KEY,                -- rowid; commit order (one writer, S5)
  stream_id       TEXT NOT NULL CHECK (stream_id = 'host' OR (stream_id GLOB 'thread:*' AND length(stream_id) = 43)),
  seq             INTEGER NOT NULL CHECK (seq >= 1),
  event_id        TEXT NOT NULL UNIQUE,
  schema_version  INTEGER NOT NULL CHECK (schema_version >= 1),
  idempotency_key TEXT NOT NULL CHECK (length(idempotency_key) > 0),
  kind            TEXT NOT NULL CHECK (length(kind) > 0),
  thread_id       TEXT, launch_id TEXT, command_id TEXT,
  source          TEXT NOT NULL CHECK (json_valid(source)),
  occurred_at     TEXT NOT NULL, recorded_at TEXT NOT NULL,
  payload         TEXT NOT NULL CHECK (json_valid(payload) AND json_type(payload) = 'object'),
  payload_bytes   INTEGER NOT NULL CHECK (payload_bytes = octet_length(payload)),
  cum_bytes       INTEGER NOT NULL,
  annex           TEXT CHECK (annex IS NULL OR json_valid(annex)),
  -- generated, for triggers and indexes
  src_kind        TEXT    GENERATED ALWAYS AS (json_extract(source, '$.kind')) VIRTUAL,
  tr_generation   INTEGER GENERATED ALWAYS AS (json_extract(source, '$.transcript.generation')) VIRTUAL,
  tr_path         TEXT    GENERATED ALWAYS AS (json_extract(source, '$.transcript.path')) VIRTUAL,
  tr_file_id      TEXT    GENERATED ALWAYS AS (json_extract(source, '$.transcript.file_id')) VIRTUAL,
  tr_offset_end   INTEGER GENERATED ALWAYS AS (json_extract(source, '$.transcript.offset_end')) VIRTUAL,
  gen_generation  INTEGER GENERATED ALWAYS AS (CASE kind WHEN 'transcript_generation' THEN json_extract(payload, '$.generation') END) VIRTUAL,
  UNIQUE (stream_id, seq),
  UNIQUE (stream_id, idempotency_key),
  CHECK (stream_id = 'host' OR stream_id = 'thread:' || thread_id),
  CHECK (kind NOT IN ('thread_registered', 'thread_closed', 'worktree_changed') OR stream_id = 'host'),
  CHECK (stream_id <> 'host' OR kind IN ('thread_registered', 'thread_closed', 'worktree_changed', 'surface_update', 'raw_unknown'))
) STRICT;
CREATE INDEX events_kind ON events (stream_id, kind, seq);
CREATE INDEX events_generation ON events (stream_id, gen_generation) WHERE kind = 'transcript_generation';
CREATE INDEX events_transcript ON events (stream_id, tr_generation, tr_offset_end) WHERE src_kind = 'transcript';
```

Triggers on `events` (each `RAISE(ROLLBACK, '<name>: …')`, S4, so a refused statement ends the transaction even if a caller ignored the error):

| Trigger | When | Refuses |
|---|---|---|
| `events_no_update` | `BEFORE UPDATE` | every update; with S3 also every upsert's `DO UPDATE` |
| `events_no_delete` | `BEFORE DELETE` | every delete, with or without `WHERE` (S3), and with `recursive_triggers=ON` every `REPLACE` deletion (S2): `INSERT OR REPLACE`, `REPLACE INTO`, `UPDATE OR REPLACE` on any of the four unique keys |
| `events_order` | `BEFORE INSERT` | `NEW.seq` ≠ last `seq` of the stream + 1 (1 for an empty stream), or `NEW.cum_bytes` ≠ last `cum_bytes` + `NEW.payload_bytes`; reads the last row through `UNIQUE (stream_id, seq)` |
| `events_generation` | `BEFORE INSERT WHEN NEW.kind = 'transcript_generation'` | `gen_generation` not greater than every earlier generation of the stream; payload `path` not equal to the `transcript_path` of the stream's latest `session_bound` (none, or `null`, refuses) |
| `events_transcript_line` | `BEFORE INSERT WHEN NEW.src_kind = 'transcript' AND NEW.kind <> 'transcript_generation'` | a line whose `tr_generation`, `tr_file_id`, and `tr_path` do not equal the payload of the stream's latest `transcript_generation`, or whose `tr_offset_end` is not greater than every earlier `tr_offset_end` of that generation |
| `events_blob_refs` | `AFTER INSERT` | inserts `(stream_id, sha256)` into `blob_refs` for every `{"blob": {"sha256": …}}` object found by `json_tree(NEW.payload)`; the foreign key to `blobs` refuses a reference to a blob that does not exist |

```sql
CREATE TABLE blobs (
  sha256     TEXT PRIMARY KEY CHECK (length(sha256) = 64 AND sha256 NOT GLOB '*[^0-9a-f]*'),
  size       INTEGER NOT NULL CHECK (size = length(data) AND size > 65536 AND size <= 67108864),
  media_type TEXT NOT NULL,
  data       BLOB NOT NULL
) STRICT;
CREATE TABLE blob_refs (
  stream_id TEXT NOT NULL, sha256 TEXT NOT NULL REFERENCES blobs (sha256),
  PRIMARY KEY (stream_id, sha256)
) STRICT, WITHOUT ROWID;

CREATE TABLE command_receipts (
  command_id     TEXT PRIMARY KEY,
  token_id       TEXT NOT NULL,
  request_sha256 BLOB NOT NULL CHECK (length(request_sha256) = 32),
  status         TEXT NOT NULL CHECK (status IN ('in_progress', 'completed', 'unknown_after_crash')),
  http_status    INTEGER, body TEXT,
  created_at     TEXT NOT NULL, finished_at TEXT,
  CHECK ((status = 'completed') = (http_status IS NOT NULL AND body IS NOT NULL))
) STRICT;

CREATE TABLE hook_deliveries (
  launch_id   TEXT NOT NULL, emit_id TEXT NOT NULL,
  ordinal     INTEGER NOT NULL CHECK (ordinal >= 1),
  reply_status INTEGER NOT NULL, reply_body TEXT NOT NULL,
  routing     TEXT NOT NULL CHECK (routing IN ('appended', 'buffered', 'raw_unknown')),
  stream_id   TEXT, seq INTEGER,
  pending     TEXT CHECK (pending IS NULL OR json_valid(pending)),   -- the buffered NewEvent and note
  received_at TEXT NOT NULL,
  PRIMARY KEY (launch_id, emit_id),
  UNIQUE (launch_id, ordinal),
  CHECK ((routing = 'buffered') = (stream_id IS NULL AND seq IS NULL)),
  CHECK (routing <> 'buffered' OR pending IS NOT NULL)
) STRICT, WITHOUT ROWID;

CREATE TABLE tokens (
  token_id TEXT PRIMARY KEY,
  kind     TEXT NOT NULL CHECK (kind IN ('owner', 'agent', 'rescue', 'hook', 'extension')),
  sha256   BLOB NOT NULL UNIQUE CHECK (length(sha256) = 32),
  thread_id TEXT, launch_id TEXT,
  created_at TEXT NOT NULL, revoked_at TEXT
) STRICT;
```

Immutability triggers on the other tables (all `RAISE(ROLLBACK)`):

| Table | Update | Delete |
|---|---|---|
| `meta` | refused for `epoch` and `created_at`; only `projection_version` may change (written by `rebuild_projections`) | refused |
| `blobs`, `blob_refs` | refused | refused |
| `command_receipts` | only `status` from `in_progress` to `completed` or `unknown_after_crash` with `http_status`, `body`, `finished_at`; `command_id`, `token_id`, `request_sha256`, `created_at` unchanged | refused |
| `hook_deliveries` | only `routing` from `buffered` to `appended` or `raw_unknown` with `stream_id` and `seq`; key, ordinal, reply, `pending`, `received_at` unchanged | refused |
| `tokens` | only `revoked_at` from `NULL`; every other column unchanged | refused |
| `hook_deliveries` insert | `BEFORE INSERT`: `ordinal` ≠ the launch's last ordinal + 1 | — |

With `recursive_triggers=ON` the delete triggers also refuse `REPLACE` on these tables. Nothing in the MVP deletes from them (§8 D9).

**Projection tables** (prefix `proj_`, which the rebuild and its tests enumerate from `sqlite_schema`, so a new projection table cannot be left out of the equality check):

| Table | Key | Columns (from mvp §2.1 "Projections") |
|---|---|---|
| `proj_threads` | `thread_id` | `agent`, `cwd`, `workspace_id`, `parent_thread_id`, `role`, `closed`, `process_state`, `turn_state`, `task_state`, `attention` (sorted JSON array), `current_launch_id`, `session_id`, `transcript_path`, `registered_seq` |
| `proj_launches` | `launch_id` | `thread_id` (FK), `reserved_seq`, `expected_previous`, `agent`, `argv_digest`, `manual`, `worktree_id`, `pid`, `start_time`, `pgid_or_job`, `cli_version`, `exit_code`, `exit_signal`, `exit_reason`, `death_confirmed`, `death_method`, `session_id`; gate evidence: `readiness_seq`, `readiness_evidence`, `write_basis`, `last_write_seq`, `last_human_write_seq`, `last_turn_start_seq` |
| `proj_inputs` | `input_id` | `thread_id`, `launch_id`, `state`, `text` (`Content` JSON), `encoding`, `origin`, `created_by`, `sent_after_seq`, `ack_seq`, `created_seq` |
| `proj_approvals` | `approval_id` | `thread_id`, `launch_id`, `requested` (payload JSON), `opened_seq`, `outcome`, `resolved_seq` |
| `proj_surfaces` | `surface_id` | `stream_id`, `scope_kind`, `scope_id`, `update` (payload JSON), `updated_seq` |
| `proj_worktrees` | `worktree_id` | `record` (`WorktreeChanged` JSON), `state`, `owner_thread_id`, `workspace_id`, `updated_seq` |
| `proj_transcript_cursors` | `thread_id` | `generation`, `path`, `file_id`, `event_offset`, `event_continuity`, `read_offset`, `read_continuity`; `CHECK (read_offset >= event_offset)` |

State columns carry `CHECK (x IN (…))` with the core enum values; flags are `CHECK (x IN (0, 1))`; foreign keys tie launches, inputs, approvals, and cursors to `proj_threads`. Projection tables have no immutability triggers: rebuild deletes and rewrites them.

**Migrations.** `schema::MIGRATIONS` is an ordered list of SQL scripts; v1 is the schema above. `open` applies each missing script in its own `BEGIN IMMEDIATE` transaction that also sets `PRAGMA user_version` (S8), so a crash leaves the database at the previous version. A projection-only change bumps `meta.projection_version` instead of migrating data: `open` then runs `rebuild_projections` (§2.6), which makes projection changes reversible.

### 2.5 `Content` and Blobs

```rust
pub struct Prepared { pub content: Content, pub blob: Option<NewBlob> }
pub struct NewBlob { pub sha256: String, pub media_type: String, pub data: Vec<u8> }
pub fn prepare_content(bytes: Vec<u8>, media_type: &str) -> Result<Prepared, ContentError>;  // ContentError::TooLarge
pub const MAX_BLOB_BYTES: usize = 64 * 1024 * 1024;
```

- **Inline** when `bytes.len() <= core::payload::INLINE_LIMIT_BYTES` (65 536): `encoding: utf8` with the text when the bytes are valid UTF-8, else `encoding: base64` (standard alphabet, padded). The limit applies to the content's bytes, not to the encoded form.
- **Blob** above it: `sha256` = lowercase hex SHA-256 of the bytes, `size`, `media_type`; `preview` = the first `PREVIEW_LIMIT_BYTES` (1024) bytes decoded with U+FFFD for each invalid sequence, `preview_lossy` = whether any was replaced. A multi-byte character cut by the 1024-byte boundary is dropped from the preview, not replaced, when the bytes after the boundary complete it (an incomplete sequence at the end of the window, `Utf8Error::error_len() == None`, is a cut, not an invalid sequence); see §8 Open Questions.
- Above `MAX_BLOB_BYTES`, `TooLarge`: the producer decides (PR 9's body limit is 4 MiB; PR 13 bounds transcript lines). The bound is also a `CHECK` on `blobs.size`, as is `size > 65 536`, so a blob row below the offload boundary cannot exist.
- **Storage** in the `blobs` table, inside the event's transaction: the blob, the event that references it, and the reference commit together or not at all, and no file outside the database can be orphaned, half-written, or locked by another Windows process. `blob_refs` rows are derived by the `events_blob_refs` trigger from the payload itself, so a stored event cannot reference a blob without a ref row, and the foreign key makes a reference to a missing blob abort the event. `append` also checks that every `NewBlob` it inserted is referenced by its event (a blob that no event references would otherwise be stored forever).
- **Media type.** The row keeps the media type of its first insert; a later identical blob with another type keeps it. PR 7 serves only `text/plain; charset=utf-8` and `application/json` as stored, so a difference changes nothing a client sees but `application/octet-stream`.
- **Deletion.** None in the MVP: events are immutable, so every reference is permanent, and a blob with no reference cannot be created.
- **Reads.** `ReadTx::blob(stream_id, sha256)` returns the bytes and media type only when `(stream_id, sha256)` is in `blob_refs` (the object check of mvp §2.5 is one query). The whole blob is read into memory (at most 64 MiB); a streaming reader (`rusqlite`'s `blob` feature) can be added by PR 7 without a schema change.

### 2.6 Projections and Rebuild

```rust
pub(crate) struct ProjectionTx<'a> { /* the transaction, restricted to proj_* statements */ }
pub(crate) fn apply(p: &mut ProjectionTx<'_>, e: &StoredEvent) -> Result<(), ProjectionError>;
pub(crate) struct StoredEvent { pub envelope: EventEnvelope, pub global_seq: u64, pub annex: Option<EventAnnex> }
```

`apply` decodes the envelope with `TypedEvent::decode` and changes projection rows from the event alone: its payload, envelope, `seq`, and annex. It reads only `proj_*` rows (to check references and compare-and-set states) and has no clock, no configuration, and no access to receipts, deliveries, tokens, or blobs; `ProjectionTx` exposes only statements on `proj_*` tables, so the restriction is a type, not a convention. A `GenericEvent` (unknown kind, newer schema, invalid payload) changes nothing. The mapping is mvp §2.1's table:

| Event | Change |
|---|---|
| `thread_registered` | insert `proj_threads` with `ThreadState::registered(task.is_some())` and the payload; error if it exists |
| `thread_closed` | `closed = 1` |
| `launch_reserved` | insert `proj_launches`; `proj_threads.current_launch_id` = it |
| `launch_started`, `launch_exited`, `launch_death_confirmed` | the launch's process columns; error if the launch is missing or belongs to another thread |
| `session_bound` | launch `session_id` and `last_turn_start_seq = seq`; thread `session_id`, `transcript_path` |
| `transcript_generation` | cursor row = `(generation, path, file_id)`, `event_offset = read_offset = 0`, continuity from the annex |
| any event with `source.kind = transcript` (other kinds) | `event_offset = offset_end`, `event_continuity` from the annex, `read_offset = max(read_offset, offset_end)` |
| `readiness_observed` | launch `readiness_seq = seq`, `readiness_evidence`, `write_basis` from the annex |
| `human_write` | launch `last_write_seq = seq`; also `last_human_write_seq = seq` for `attach` and `key` |
| `user_message` | the envelope launch's `last_turn_start_seq = seq`; with `input_id`, that input's `ack_seq = seq` |
| `status_changed` | the field's column, or `attention`, set to `to`, by `UPDATE … WHERE <column> = from` (the `from` must match: one row changed, else error) |
| `task_set` | `task_state = to` |
| `approval_requested` / `approval_resolved` | insert the card / set `outcome`, `resolved_seq` (only from `outcome IS NULL`) |
| `input_created` | insert the input with `state = initial_state` |
| `input_state_changed` | `state = to` by `UPDATE … WHERE state = from` (one row, else error); `sent_after_seq` when set; for `to = sending`, the input's launch `last_write_seq = seq` |
| `surface_update` | upsert the surface; empty `components` deletes it |
| `worktree_changed` | upsert the record |
| `mailbox_message`, `assistant_message`, `tool_activity`, `vt_snapshot_taken`, `raw_unknown` | none (read from `events` by kind) |

The compare-and-set rows (`status_changed`, `input_state_changed`, approval outcome) turn a lost update into an aborted job: two jobs that both read `queued` cannot both record `queued → sending`, because the second `UPDATE … WHERE state = 'queued'` changes no row.

**Rebuild.** `rebuild_projections` runs in the job's transaction: delete every `proj_*` table (child tables first), read `events` ordered by `global_seq`, and call `apply` on each. Equality with the maintained projections holds by construction: the same function on the same events in the same order, where `global_seq` is the commit order because one transaction writes at a time (S5). The property test (§7) checks it rather than assuming it. The one permitted difference is the transcript cursor's read position: rebuild leaves `read_offset = event_offset` and `read_continuity = event_continuity`, the last event's `offset_end` (or 0 after its `transcript_generation`), which is mvp §2.1's only permitted rewind; transcript idempotency keys turn the re-read lines into `Existing`. Every other column is equal. `rebuild_projections` also writes the current `projection_version` to `meta`.

`verify_projections` dumps every `proj_*` table, rebuilds inside a `SAVEPOINT`, dumps again, rolls back to the savepoint, and returns the differences (empty when equal). PR 21's doctor and the tests use it. Both hold the write lock for the duration of a full replay; they run at open after a projection version change, or on demand.

**Annex.** Two projected values are not in any public payload: the write basis captured with readiness evidence (mvp §2.1 gate evidence row; `ReadinessObserved` has no such field) and the transcript continuity evidence that PR 13 / 14 must keep across a rebuild (mvp §4 PR 13 / 14 obligation). Both are stored in the event row's `annex` column, immutable with the event, never served, and passed to `apply`, so rebuild reproduces them (§8 D7).

### 2.7 Read Operations (`ReadTx`)

All in one read transaction (or the write transaction through `WriteTx::read`).

```rust
impl ReadTx<'_> {
    pub fn stream_exists(&self, s: StreamId) -> Result<bool, StoreError>;          // host: always; thread: registered
    pub fn head(&self, s: StreamId, after_seq: u64) -> Result<StreamHead, StoreError>;  // core::sync::StreamHead
    pub fn heads(&self, s: &[StreamId]) -> Result<Vec<StreamHeadEntry>, StoreError>;
    pub fn events_after(&self, s: StreamId, after_seq: u64, limit: u32) -> Result<Page, StoreError>;     // has_more
    pub fn events_before(&self, s: StreamId, before_seq: u64, limit: u32) -> Result<Page, StoreError>;   // ascending, has_more_before
    pub fn event_by_key(&self, s: StreamId, key: &IdempotencyKey) -> Result<Option<EventEnvelope>, StoreError>;
    pub fn events_of_kind(&self, s: StreamId, kind: &str, after_seq: u64, limit: u32) -> Result<Page, StoreError>;
    pub fn latest_of_kind(&self, s: StreamId, kind: &str) -> Result<Option<EventEnvelope>, StoreError>;
    pub fn thread_snapshot(&self, t: ThreadId) -> Result<Option<(ThreadSnapshot, u64)>, StoreError>;      // with as_of_seq
    pub fn host_snapshot(&self) -> Result<(HostSnapshot, u64), StoreError>;
    pub fn thread(&self, t: ThreadId) -> Result<Option<ThreadRow>, StoreError>;
    pub fn threads(&self) -> Result<Vec<ThreadRow>, StoreError>;
    pub fn launch(&self, l: LaunchId) -> Result<Option<LaunchRow>, StoreError>;
    pub fn open_launches(&self) -> Result<Vec<LaunchRow>, StoreError>;           // not exited, or death unconfirmed
    pub fn launches_using_worktree(&self, w: &WorktreeId) -> Result<Vec<LaunchRow>, StoreError>;
    pub fn input(&self, i: InputId) -> Result<Option<InputRow>, StoreError>;
    pub fn inputs(&self, t: ThreadId, states: &[InputState]) -> Result<Vec<InputRow>, StoreError>;
    pub fn approval(&self, a: ApprovalId) -> Result<Option<ApprovalRow>, StoreError>;
    pub fn open_approvals(&self, t: ThreadId) -> Result<Vec<ApprovalRow>, StoreError>;
    pub fn surface(&self, s: &SurfaceId) -> Result<Option<SurfaceRow>, StoreError>;
    pub fn worktree(&self, w: &WorktreeId) -> Result<Option<WorktreeRow>, StoreError>;
    pub fn worktrees(&self) -> Result<Vec<WorktreeRow>, StoreError>;
    pub fn transcript_cursor(&self, t: ThreadId) -> Result<Option<TranscriptCursor>, StoreError>;
    pub fn blob(&self, s: StreamId, sha256: &str) -> Result<Option<BlobBody>, StoreError>;
    pub fn receipt(&self, c: CommandId) -> Result<Option<CommandReceipt>, StoreError>;   // core::command::CommandReceipt
    pub fn token_by_hash(&self, sha256: &[u8; 32]) -> Result<Option<TokenRow>, StoreError>;
    pub fn hook_delivery(&self, l: LaunchId, emit: Uuid) -> Result<Option<HookDelivery>, StoreError>;
    pub fn buffered_deliveries(&self, l: LaunchId) -> Result<Vec<BufferedDelivery>, StoreError>;  // ordinal order, with sizes
    pub fn buffered_launches(&self) -> Result<Vec<LaunchId>, StoreError>;
}
```

`head` reads the stream's last row (`head_seq`, `cum_bytes_at_head`) and the row at `after_seq` (`cum_bytes_at_after`, 0 for `after_seq` 0 or above the head) in the caller's transaction; `core::sync::decide_replay` takes it unchanged. The snapshots assemble core's `ThreadSnapshot` and `HostSnapshot` from the projection rows and `events` (last `SNAPSHOT_RECENT_EVENTS` events; the latest `vt_snapshot_taken` while an attention cause is active or a card is open) and return the stream head as `as_of_seq`, all from the one transaction.

### 2.8 Open, Pragmas, Epoch

**Per connection**, before any other statement: `journal_mode=WAL` (writer only; persistent; anything but `wal` back, e.g. on a filesystem without shared memory, is `OpenError::NoWal`), `synchronous=FULL`, `fullfsync=ON` and `checkpoint_fullfsync=ON` (macOS `F_FULLFSYNC`; ignored elsewhere), `foreign_keys=ON`, `recursive_triggers=ON`, `trusted_schema=OFF`, `busy_timeout` from the config, `journal_size_limit=67108864`, `SQLITE_DBCONFIG_DEFENSIVE` on (S10); readers add `query_only=ON`. `open` reads each value back and refuses a connection where one did not take effect.

**Open sequence** (writer connection, before readers open):

1. If the file does not exist: create it with `create_new` (mode `0o600` on Unix; S11 gives `-wal` and `-shm` the same mode), then continue as a fresh database. `open` never creates directories.
2. Read the header through SQLite: a file that is not a database (`SQLITE_NOTADB`) is `OpenError::NotADatabase`. `application_id` = `0x4348_4C44` ("CHLD") is Chaldea's; `0` with no schema objects is fresh; anything else (another id, or `0` with tables) is `OpenError::Foreign`.
3. Fresh: one `BEGIN IMMEDIATE` sets `application_id`, runs migration v1, inserts `meta` `epoch` (from `IdSource::epoch`), `created_at`, `projection_version`, sets `user_version = 1`, and commits. The epoch is written only here, so it is created once per database file; its row is immutable by trigger.
4. Ours: `user_version` above the newest migration is `OpenError::NewerSchema { found, supported }` with nothing written; below it, run the missing migrations (§2.4).
5. Compare the schema (`type`, `name`, `sql` of every `sqlite_schema` row, sorted) with the expected fingerprint of the current version; a difference (a dropped or edited trigger, an extra table) is `OpenError::SchemaMismatch` with the names.
6. If `meta.projection_version` is older than the code's, run `rebuild_projections`.
7. Open the readers, start the writer thread.

**Refusal never changes the file.** Every `OpenError` is returned before any write to an existing file, and the store never deletes, renames, or recreates a database. Corruption found later (`SQLITE_CORRUPT`) puts the writer in the failed state (§5); an integrity check (`PRAGMA integrity_check`) is the doctor's (PR 21), not part of `open`, since it reads the whole file.

### 2.9 Durability

| Point | What holds | Guaranteed by |
|---|---|---|
| A ticket resolves `Ok` | The job's transaction is in the synced WAL: it survives a process crash, an OS crash, and power loss on hardware that honors the flush (`F_FULLFSYNC` on macOS, `FlushFileBuffers` on Windows) | `synchronous=FULL` + `fullfsync` (S6) |
| A process crash at any point of a job | No part of the job is visible after restart | WAL recovery ignores frames after the last commit frame (S6) |
| `CommitUnknown` | The outcome is unknown to this process; after restart the database shows it either fully or not at all | S6; the caller treats it as a crash (receipt stays `in_progress`, becomes `unknown_after_crash` at restart) |
| A refused statement | The whole job rolls back | `RAISE(ROLLBACK)` (S4); `WriteTx` returns the error and the writer rolls back on `Err` |
| Disk full | `SQLITE_FULL`: the job rolls back and `Store(DiskFull)` is replied; the store stays usable | S6 |

## 3. Structure Options

| Fork | Option | Precedent | Verdict |
|---|---|---|---|
| **Crate** | A. A separate `chaldea-store` crate | — | Rejected by mvp §3 ("one user"). |
| | B. `crates/daemon` with only the `store` module; later PRs add HTTP, ingest, supervisor | mvp §3 layout | **Adopted.** No binary yet (PR 8 adds `daemon run` in `crates/cli`). |
| **SQLite source** | A. System SQLite | — | Rejected: Windows has no SQLite development package on the runner, and macOS's version differs from any Windows one; `STRICT`, `octet_length`, and the trigger behaviour must be the same on both. |
| | B. `bundled` | `libsqlite3-sys` `build.rs:170-187` | **Adopted**: SQLite 3.54.1 on both runners (S1); costs one C compile per clean build. |
| **Writer model** | A. `Mutex<Connection>` shared by callers | — | Rejected: std `Mutex` is not FIFO and has no deadline; a caller holding it across an `.await` would block the runtime. |
| | B. An async task owning the connection, fed by a channel | T3 Code's writer worker (`persistence/Sqlite.ts:111-115`, `writerWorker: true`) | Rejected: rusqlite calls block; the task would need `spawn_blocking` per job and still serialize. |
| | C. A dedicated writer thread with a bounded job queue and a oneshot reply | — | **Adopted.** One write transaction at a time by construction, a deadline decided at job start, no runtime needed for tests; async callers await the reply. |
| **Readers** | A. All reads through the writer | — | Rejected: reads would wait behind writes; WAL lets them run in parallel (S7). |
| | B. A small pool of read-only connections | T3 Code's reader worker (`Sqlite.ts:111-115`) | **Adopted.** `query_only` makes a write on them fail in the engine. |
| **Per-stream order** | A. Global sequence only | T3 Code v2 events: `sequence INTEGER PRIMARY KEY AUTOINCREMENT`, non-unique `(thread_id, sequence)` index (`Migrations/055_OrchestrationV2.ts:20-39`) | Rejected by mvp §2.1. |
| | B. `stream_version` computed in the `INSERT` from `MAX + 1`, unique per stream | T3 Code v1 (`Migrations/001_OrchestrationEvents.ts:8-28`; `persistence/OrchestrationEventStore.ts:494-501`), relying on the caller's transaction | Partly adopted: the same computation, plus a trigger that refuses any other value, so gaplessness does not rest on the caller. |
| | C. A `streams` table holding `head_seq` and `cum_bytes` | — | Rejected: a second copy of the head that must agree with the last row; the last row is one index lookup away. |
| **Append-only** | A. Convention (the store never issues `UPDATE`) | T3 Code: no trigger; `ProjectionMaintenance.ts` deletes events when pruning (`:172`, `:214`) | Rejected: mvp §2.1 requires the engine to refuse. |
| | B. Update and delete triggers with `RAISE(ROLLBACK)` and `recursive_triggers=ON` | mvp §2.1 | **Adopted**; S2–S4. |
| **Idempotency conflict** | A. `INSERT OR IGNORE` / `ON CONFLICT DO NOTHING` and report "not inserted" | T3 Code receipts (`persistence/OrchestrationCommandReceipts.ts:140-143`) | Rejected for events: a key reused for different content would pass silently, and mvp §2.1 rules out upserts on `events`. |
| | B. Look up the key in the transaction; equal content → the existing event; different → abort | — | **Adopted** (CL-10). The unique index is the backstop. |
| **Projection code** | A. SQL triggers on `events` that maintain projections | — | Rejected: the projection logic (state compare-and-set, `Content` JSON, cursor rules) would be SQL over `json_extract`, untestable without SQLite and hard to keep equal to core's types. |
| | B. A pure reducer plus a separate SQL writer that mirrors it | T3 Code `ProjectionStore.ts`: `applyToProjection` in memory and a parallel SQL `apply` with matching merge helpers (line numbers approximate: ~`:1034-1231`, ~`:2398-2421`, ~`:2460`) | Rejected: two implementations of one rule; rebuild through one can differ from live maintenance through the other. |
| | C. One Rust function over a `proj_*`-only transaction, called by live append and by rebuild | — | **Adopted.** Equality by construction, checked by a property test. A pure function in `core` was considered: `core` has no projection row types, and adding them would change `core`'s public surface for internal tables. |
| **Blob storage** | A. Files named by SHA-256 in `<state_dir>/blobs/` | — | Rejected: a file and the event that references it cannot commit together; Windows sharing violations (antivirus, indexer) and path limits add failure cases; a crash leaves orphans. |
| | B. A `blobs` table in the same database | — | **Adopted.** One transaction, one file to back up, the same fsync path. Costs database size (64 MiB per blob at most). |
| **Transcript order** | A. Rust checks in the tailer | — | Second line only. |
| | B. Triggers over generated columns of `events` | — | **Adopted**: a line of an unrecorded or older generation cannot be stored by any code path. |
| **Migration marker** | A. `PRAGMA user_version` + `application_id` | SQLite file format (S8) | **Adopted**: transactional, in the header, no table needed to read it. |
| | B. A `schema_migrations` table | — | Rejected: needs the schema to exist before it can say which schema exists. |
| **Durability** | A. `synchronous=NORMAL` | T3 Code `Sqlite.ts:82` (with `checkpoint_fullfsync`, `:84`) | Rejected: a hook's first reply must be durable before 200 (mvp §2.1), and NORMAL can lose it on power loss (S6). |
| | B. `synchronous=FULL` + `fullfsync` | mvp §2.1 | **Adopted**; the cost is an `F_FULLFSYNC` per commit on macOS (§8 D5). |

## 4. Same-Class Inventory

Every later PR that reads or writes the store, and what it uses. None needs a store change; each runs its own checks inside one job.

| PR | Needs | Operation |
|---|---|---|
| 7 HTTP | replay, backward pages, head and byte bound, snapshots, 404 for unknown streams | `read`: `stream_exists`, `head` → `decide_replay`, `events_after` / `events_before`, `thread_snapshot` / `host_snapshot` |
| 7 | blobs with the object check | `read`: `blob(stream_id, sha256)` |
| 7 | receipts | `submit`: `receipt_begin` first in the intent job, `receipt_complete`; `read`: `receipt` |
| 7 | tokens, pairing | `submit`: `token_insert`, `token_revoke`; `read`: `token_by_hash` (comparison with `subtle` is PR 7's) |
| 7 | WS heads and `stream_advanced` | `subscribe` (`Committed`), `read`: `heads` |
| 8 lifecycle | open, shutdown, restart recovery entry point | `Store::open`, `shutdown`; `submit`: `receipts_mark_unknown_after_crash` before accepting commands; the single-instance lock and state directory are PR 8's |
| 9 hooks | dedupe before routing, durable first reply, buffered promotion | `submit`: `hook_delivery_record`, `hook_delivery_promote`; `read`: `hook_delivery`, `buffered_deliveries` (count and size bounds are PR 9's), `buffered_launches` (restart turns them into `raw_unknown` through `hook_delivery_promote`) |
| 9 | `raw_unknown`, events from hooks | `append` in the delivery job |
| 10 supervisor | reservation (thread row, expected launch, death confirmed, worktree state, tokens) in one transaction | one job: `read().thread`, `launch`, `worktree`, then `append(launch_reserved, …)`, `token_insert`, `receipt_*` |
| 10 | close racing resume | both jobs read the thread row in `BEGIN IMMEDIATE`; the second sees the first's events |
| 10 | gate inputs, send path | `read`: `thread`, `launch` (gate evidence), `inputs`; `append(input_state_changed)` with `from` compare-and-set in `apply` |
| 10 | restart | `read`: `open_launches`, `inputs(… queued, sending, sent)`; `append(launch_exited, input_state_changed, status_changed)` |
| 10 | readiness write basis | `NewEvent::annex.write_basis` on `readiness_observed` |
| 12 fixtures | captured events for `doctor capture` | `read`: `events_after`, `events_of_kind` |
| 13 / 14 tailers | cursor, generation, batches, rewind after rebuild, continuity evidence | `read`: `transcript_cursor`; `submit`: `append_transcript` (with `annex.continuity` on events and `ReadPos::continuity`) |
| 14 | Codex assistant text from `Stop` | `append` |
| 16 worktrees, mailbox, task | exclusion against reservations; mailbox listing | one job: `read().worktree`, `launches_using_worktree`, `append(worktree_changed)`; `events_of_kind(mailbox_message)`; `append(task_set, status_changed)` |
| 17 `stateright` | the store's atomicity and gapless order as model assumptions | the guarantees of §2.1 and §2.9, cited by claim id |
| 20 extensions | `POST /events` surfaces with the stored scope check | one job: `read().surface`, `append(surface_update)` |
| 21 doctor | integrity, schema version, projection check, sizes | `read` + `PRAGMA integrity_check` on a reader; `submit`: `verify_projections`; `Store::paths` |

## 5. Resource Paths

| Resource | Created by | Bound | Ends on |
|---|---|---|---|
| `chaldea.db`, `chaldea.db-wal`, `chaldea.db-shm` | `open` (db), SQLite (WAL, SHM) | one database per state directory; WAL truncated to 64 MiB after checkpoints | never deleted by the store; WAL checkpointed and the files closed at shutdown |
| `store-writer` thread | `open` | one | queue closed and drained at `shutdown`, or a panic outside a job |
| Writer connection | `open` | one | writer thread exit |
| Reader connections | `open` | `readers` (4) | `shutdown` after the writer |
| Job queue | `open` | `writer_queue` (256) jobs | `shutdown` closes it; `submit` then returns `Closed` |
| Reply oneshot | `submit` | one per job | the writer replies exactly once; a dropped sender (writer gone) is `WriterGone` |
| `Committed` broadcast | `open` | 1024 messages | `shutdown`; lagging receivers get `Lagged` |
| Read transaction | `read` | one per call | `f` returns; rolled back on error |

**Endings.**

| Ending | What happens |
|---|---|
| Normal shutdown | `shutdown(deadline)` closes the queue; the writer runs the jobs already queued (each still deadline-checked), runs `PRAGMA wal_checkpoint(TRUNCATE)`, closes its connection, and signals done; `shutdown` waits on that signal until `deadline`, then closes the readers. `ShutdownReport` says whether the writer finished, how many jobs ran or expired, and whether the checkpoint completed. A writer still busy at the deadline is abandoned (named in the report); its connection closes when the job returns. |
| Job error or panic | `ROLLBACK`, reply the error (`Panicked` for a panic, caught with `catch_unwind`); the writer continues. |
| Writer thread panic outside a job | The queue's receiver drops: `submit` returns `Closed`, pending tickets resolve `WriterGone`. PR 8 stops the daemon. |
| `SQLITE_BUSY` | Only another process holding the database (PR 8 prevents a second daemon): `BEGIN IMMEDIATE` waits `busy_timeout`, then the job fails with `Store(Busy)`, nothing written. Readers in WAL do not wait on the writer (S7). |
| Disk full (`SQLITE_FULL`) | The job rolls back; the reply is `Store(DiskFull)`; later jobs run normally once space exists. |
| Corruption or I/O error (`SQLITE_CORRUPT`, `SQLITE_NOTADB`, `SQLITE_IOERR`) | The job rolls back (or `CommitUnknown` if it happened in `COMMIT`); the writer enters the failed state: every later `submit` returns `Failed`. Reads continue. PR 8 reports and stops; nothing is deleted. |
| Process crash | No partial job (S6); the next `open` recovers the WAL. In-progress receipts and open launches are recovered by PR 8 / PR 10 through the restart operations. |
| Caller stops waiting | The job still runs or expires on its own; the caller recovers through the receipt (mvp §2.4). |

**Every wait has a deadline.** `submit` never waits (`try_send`). A job starts only before its `not_after`. `BEGIN IMMEDIATE` waits at most `busy_timeout`. `read` waits for a connection until `wait_until`. `shutdown` waits until its deadline. A job's own run time is bounded by its statements: jobs must not block on anything but SQLite (no network, no PTY, no channel waits), which the `WriteTx` API enforces by offering nothing else; a caller that captured a blocking handle in its closure breaks the bound, and PR 7–10 reviews check for it. `WriteTicket::blocking_wait` has no deadline of its own, because the writer replies to every accepted job (run, expired, failed, or `WriterGone`).

**Windows and macOS differences.**

- **File locking.** SQLite uses `LockFileEx` on Windows and POSIX advisory locks on macOS. A second `Store` on the same file, in this or another process, serializes through them (S5) and gets `Busy` after `busy_timeout`; tested on both runners. On Windows, a handle left open keeps the files from being deleted, so `open::drop_releases_every_file` deletes all three files after `shutdown`.
- **Paths.** On Windows the store passes the canonical path (`\\?\` form from `std::fs::canonicalize` of the directory), which SQLite's VFS accepts (S11), so a long state directory is not limited to 260 characters; `open::long_path` runs on both runners with a directory path above 260 characters. If that test fails on Windows, the documented limit becomes "state directory under 200 characters" and PR 8 checks it at startup.
- **fsync.** `F_FULLFSYNC` on macOS (slower, flushes the drive cache), `FlushFileBuffers` on Windows. Neither is observable in CI; the durability claims rest on S6 and are tested for process crashes only (§6.2).
- **Permissions.** Unix: the database is created `0600` and SQLite gives WAL and SHM the same mode (S11). Windows: the files inherit the state directory's ACL; PR 8 places the state directory under the user's profile, which is user-only.
- **Network filesystems.** WAL needs shared memory on a local filesystem (S7); `journal_mode` returning anything but `wal` refuses the open.

## 6. Purpose Layer and Plan Layer

### 6.1 Purpose Layer: Acceptance Criteria

Checked by running the store tests on `macos-latest` and `windows-latest` CI against real database files in the OS temp directory.

| ID | Criterion (EARS) | Source | How to observe |
|---|---|---|---|
| PR6-P-1 | When projections are dropped and rebuilt from `events`, they shall equal the maintained projections. | mvp §4 PR 6 obligation; CL-11 | `verify_projections` returns no difference after random histories (PR6-CL-6). |
| PR6-P-2 | When any statement tries to update or delete an event, including `INSERT OR REPLACE`, the database shall refuse it and keep the event. | mvp §4 PR 6 obligation; CL-9 | Each forbidden statement on the store's own connection fails and the row is unchanged (PR6-CL-2). |
| PR6-P-3 | When a transcript generation changes, its `transcript_generation` event shall be stored before any line of that generation. | mvp §4 PR 6 obligation; CL-12 | A line before its generation is refused (PR6-CL-9). |
| PR6-P-4 | If the stored transcript cursor differs from the batch's expected cursor, then the whole batch shall abort with nothing stored. | mvp §4 PR 6 obligation; CL-12 | Stream heads and cursor unchanged after a stale batch (PR6-CL-8). |
| PR6-P-5 | When content above 64 KiB is stored, it shall become a blob with a 1 KiB lossy preview, and content at or below 64 KiB shall stay inline. | mvp §4 PR 6 obligation; mvp §2.1 `Content`; P-21 | Boundary values and blob reads (PR6-CL-11, CL-12). |
| PR6-P-6 | The store shall give PR 7–21 every operation of §4, each as one atomic transaction with a stated failure contract. | brief; mvp §4 | §4 covers every row; the operations exist with the signatures of §2. |

### 6.2 Plan Layer: Claims

| ID | Claim (EARS) | Source | Falsifying test |
|---|---|---|---|
| PR6-CL-1 | When events are appended to a stream, their `seq` shall be 1, 2, 3, … with no gap or repeat, `cum_bytes` shall be the running sum of `payload_bytes`, and `global_seq` shall increase in commit order, including when batches fail and roll back. | CL-9; mvp §2.1, §2.4 | proptest `store_append::seq_is_gapless_under_random_batches` (random streams, batch sizes, and injected failures); `store_triggers::out_of_order_seq_is_refused` (gap, repeat, 0); `store_triggers::wrong_cum_bytes_is_refused`; `store_triggers::wrong_payload_bytes_is_refused` |
| PR6-CL-2 | If any statement on a store connection would update or delete an `events` row (`UPDATE`, `DELETE` with or without `WHERE`, `INSERT OR REPLACE` and `REPLACE INTO` on each of the four unique keys, upsert `DO UPDATE`, `UPDATE OR REPLACE`), then the transaction shall roll back and the row shall be unchanged. | CL-9; mvp §2.1 | `store_triggers::each_forbidden_statement_aborts` (table-driven, run on the writer connection through a `#[cfg(test)]` raw handle, which also proves its `recursive_triggers`; the `INSERT OR REPLACE` cases fail with it off) |
| PR6-CL-3 | If a statement would change or delete a blob, a blob ref, the epoch, a completed or crashed receipt, a delivery's first reply, ordinal, or key, or any token column but an unset `revoked_at`, then the transaction shall roll back. | mvp §2.1, §2.4, §2.9 | `store_triggers::immutable_tables_refuse_changes` (one case per row of §2.4's table) |
| PR6-CL-4 | When an event is appended with a key its stream already holds, the store shall return the stored event without a new `seq` if the content is equal, and roll back the job if it differs; distinct keys shall each create one event. | CL-10; mvp §2.1 | `store_append::replayed_key_returns_the_existing_event`, `store_append::conflicting_key_aborts_the_job`, `store_append::repeated_key_within_a_batch`, `store_append::distinct_emit_ids_create_distinct_events` |
| PR6-CL-5 | When a hook delivery is recorded, its first reply, ordinal, routing, and events shall commit together; a retry with the same `(launch_id, emit_id)` shall get the first reply and the current routing and change nothing; promotion shall change only the routing, from `buffered` only. | CL-10 (store side); mvp §2.1, §2.9 | `store_hooks::retry_replays_the_first_reply`, `store_hooks::retry_after_promotion_sees_new_routing`, `store_hooks::promotion_changes_only_routing`, `store_hooks::ordinals_follow_receipt_order`, `store_hooks::failed_routing_stores_no_delivery` |
| PR6-CL-6 | When the projections are rebuilt from `events`, every `proj_*` table shall equal the maintained one, except that each transcript cursor's read position shall equal its last event position. | CL-11; mvp §2.1 | proptest `store_rebuild::rebuild_equals_maintained` (generated valid histories over every kind, with unknown-kind and blob-bearing events and failed jobs mixed in; the dump enumerates `proj_*` from `sqlite_schema`); `store_rebuild::cursor_rewinds_to_the_last_event` |
| PR6-CL-7 | If any event of a job fails its insert or its projection change, then no event, blob, ref, or projection change of that job shall remain. | mvp §2.1 "change only with the event … in its transaction" | `store_append::failed_job_changes_nothing` (second event fails `apply`; tables dumped before and after), `store_append::lost_update_aborts` (two jobs record the same `queued → sending`) |
| PR6-CL-8 | When a transcript batch commits, its events and the cursor advance shall commit together, and only if the stored generation, `file_id`, and read offset equal the batch's expected cursor; otherwise nothing of the batch shall be stored. | CL-12 (store side); mvp §2.1 | `store_transcript::stale_cursor_aborts_the_batch` (one case per field, plus `None` with a cursor), `store_transcript::batch_and_cursor_commit_together`, `store_transcript::rewound_lines_are_existing` |
| PR6-CL-9 | If a transcript line event names a generation other than the stream's latest recorded `transcript_generation`, another `file_id` or path, or an offset not after the previous line of its generation, or a `transcript_generation` is not newer or its path is not the bound session's, then the insert shall be refused. | CL-12 (store side); mvp §2.1 | `store_transcript::line_before_its_generation_is_refused`, `store_transcript::older_generation_line_is_refused`, `store_transcript::offsets_must_increase`, `store_transcript::generation_path_must_match_the_session` |
| PR6-CL-10 | When a rebuild runs, each transcript cursor's read position shall rewind to its last transcript event's `offset_end` (0 after its `transcript_generation`), and re-read lines shall be returned as existing events. | mvp §2.1 rewind rule | `store_rebuild::cursor_rewinds_to_the_last_event`, `store_transcript::rewound_lines_are_existing` |
| PR6-CL-11 | When content is prepared, at most 65 536 bytes shall stay inline (`utf8` for valid UTF-8, else `base64`), and above it the content shall be a blob with its SHA-256, size, and the first 1024 bytes decoded with U+FFFD for each invalid sequence and `preview_lossy` set exactly when one was replaced. | mvp §2.1 `Content`; P-21 | `store_content::boundary_inline_and_blob` (65 535, 65 536, 65 537 bytes, UTF-8 and binary), `store_content::preview_marks_invalid_bytes`, `store_content::preview_cut_inside_a_character`, `store_triggers::blob_rows_are_checked` (size below the boundary, wrong length, bad hash shape) |
| PR6-CL-12 | When an event referencing a blob commits, the blob and its `(stream_id, sha256)` ref shall commit with it; a reference to a missing blob shall abort the event; a blob shall be readable only through a stream that references it. | mvp §2.1, §2.5 | `store_content::blob_round_trip`, `store_content::refs_come_from_the_payload`, `store_content::missing_blob_aborts_the_event`, `store_content::unreferenced_stream_cannot_read`, `store_content::unreferenced_blob_is_refused` |
| PR6-CL-13 | When `receipt_begin` runs, it shall return core's `ReceiptLookup` for the stored receipt and insert `in_progress` only for `New`; concurrent duplicates shall insert once; a restart shall turn every `in_progress` receipt into `unknown_after_crash`. | CL-8 (storage); mvp §2.4 | `store_receipts::lookup_cases`, `store_receipts::concurrent_duplicates_insert_once` (threads on one store, and two `Store`s on one file), `store_receipts::restart_marks_unknown_after_crash` |
| PR6-CL-14 | When a job starts after its `not_after`, it shall change nothing and reply `Expired`; a ticket shall resolve `Ok` only after `COMMIT`; a job error or panic shall roll back the whole job; a store whose writer is gone shall reply `WriterGone` or `Closed`. | mvp §2.5 deadlines; §2.1 one writer | `store_writer::expired_job_changes_nothing`, `store_writer::ok_is_visible_to_a_new_reader`, `store_writer::job_panic_rolls_back`, `store_writer::full_queue_fails_at_once`, `store_writer::closed_store_refuses` |
| PR6-CL-15 | If the process is killed during a job, then after reopening no part of that job shall be visible and every earlier committed job shall be. | mvp §2.1; S6 | `store_crash::killed_writer_leaves_no_partial_job` (the test binary re-runs itself as a child that commits N jobs, prints a marker inside a large job, and waits; the parent kills it by `Child::kill` and reopens; both OSes) |
| PR6-CL-16 | The epoch shall be created once per database and survive every reopen; `open` shall refuse a non-database, a foreign database, a newer schema, or a changed schema, and leave the file unchanged. | mvp §2.4 epoch; brief decision 8 | `store_open::epoch_survives_reopen`, `store_open::refuses_not_a_database`, `store_open::refuses_foreign_database`, `store_open::refuses_newer_schema`, `store_open::refuses_altered_schema` (a dropped trigger); each compares the file's bytes before and after |
| PR6-CL-17 | When a read returns a snapshot or a page, its `as_of_seq` or `head_seq` shall come from the same transaction as its contents, while writes continue. | mvp §2.4 | `store_reads::snapshot_and_head_agree_under_writes` (a writer thread appends while readers check `head_seq` = last event's `seq`) |
| PR6-CL-18 | When two `Store`s open one file, their writes shall serialize, and after `shutdown` no file of the database shall stay open. | §5 Windows and macOS | `store_open::second_store_serializes_writes`, `store_open::drop_releases_every_file`, `store_open::long_path` |

**Mapping.** Obligations: rebuild equality → PR6-CL-6, CL-7, CL-10; forbidden statements → CL-2 (and CL-3 for the other tables); generation before lines → CL-9; stale-cursor batch → CL-8; blob offload and lossy preview → CL-11, CL-12. mvp §6.2: CL-9 → PR6-CL-1, CL-2; CL-10 (store side) → PR6-CL-4, CL-5; CL-11 → PR6-CL-6, CL-7, CL-10; CL-12 (store side) → PR6-CL-8, CL-9 (the "only from the bound session's path" part through `events_generation`); CL-8's storage → PR6-CL-13. CL-13 and CL-17 rest on PR6-CL-1, CL-14, CL-15, CL-17.

**Known limits** (no test, by design): a process with raw file access can drop a trigger and edit rows while the store runs (§2.1); `open` detects only the schema change, at the next open. Power-loss durability is not observable in CI. `ProjectionTx`'s restriction to `proj_*` is a type boundary; a projection that read the clock would still pass every test that does not vary the clock, so `store_rebuild::rebuild_equals_maintained` runs the rebuild with a different fixed clock than the live appends.

## 7. Tests and Completion Commands

All tests are integration tests in `crates/daemon/tests/` and run in `cargo test --workspace --locked` on both runners. Each test opens a store in its own `tempfile::TempDir` (OS temp directory) with a fixed `Clock` and a counter `IdSource`, and calls `shutdown` before the directory drops (Windows cannot delete open files). Event generators live in `tests/support/mod.rs`: a proptest strategy that walks a small model of threads, launches, inputs, approvals, surfaces, worktrees, and transcripts to emit valid event sequences, plus noise (unknown kinds through a raw-insert helper, `raw_unknown`, events with blob content, jobs that fail midway). Property tests use proptest's default 256 cases, with the history length bounded (≤ 200 events) so a case runs in milliseconds. Raw SQL for trigger tests goes through a `#[cfg(test)]`-only `Store::with_raw_writer` that runs a closure on the writer thread's own connection, so the pragmas under test are the production ones.

Completion commands (mvp §5, extended):

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
deps="$(cargo tree -p chaldea-core -e normal --prefix none)" && ! grep -Eq '^(tokio|rusqlite|axum) ' <<<"$deps"
deps="$(cargo tree -p chaldea-pty-runtime -e normal --prefix none)" && ! grep -Eq '^tokio ' <<<"$deps"
```

plus the `workspace` job green on both runners and the `pty-hooks` job unchanged.

## 8. Decisions

**D1. `crates/daemon` starts with the store only.** No HTTP, no binary; PR 7 and PR 8 add theirs. Dependencies: `rusqlite =0.41.1` (`bundled`, no defaults), `sha2 =0.11.1`, `tokio =1.54.1` (`sync` only), `base64 =0.22.1`, `uuid` with `v4`, dev `proptest` and `tempfile =3.27.0`.

**D2. Bundled SQLite 3.54.1.** One SQLite on both runners; the schema relies on `STRICT`, generated columns, `RETURNING`, `octet_length`, JSON functions in triggers, and `trusted_schema=OFF` with them (S9).

**D3. One writer thread, jobs as closures over one `BEGIN IMMEDIATE` transaction, oneshot replies; a pool of four read-only connections.** The smallest form PR 7–10 can use: a check and its write are one job, async callers await the ticket, sync tests call `blocking_wait`.

**D4. Invariants live in the schema.** Gapless `seq`, `cum_bytes`, append-only events, transcript generation order and path, blob refs and sizes, receipt and delivery transitions, token revocation: triggers and constraints with `RAISE(ROLLBACK)`; Rust checks first for typed errors. Compare-and-set `UPDATE`s with row counts guard projection state transitions.

**D5. `synchronous=FULL` with `fullfsync=ON`.** A hook's first reply and a command's intent are durable before the response, including on macOS power loss. Cost: one `F_FULLFSYNC` per job on macOS (milliseconds to tens of milliseconds); the writer's batching per job (one transaction for a hook's events) keeps it to one per request. Reversible by a pragma change if commits prove too slow.

**D6. One projection function, `project::apply`, for live appends and rebuild**, restricted to `proj_*` tables by type; rebuild replays by `global_seq`. A projection-only change bumps `projection_version` and rebuilds at open instead of migrating.

**D7. The `events.annex` column carries projected values that are not in public payloads**: the readiness write basis (PR 10) and transcript continuity evidence (PR 13 / 14). It is internal, immutable with the event, and never served, so rebuild reproduces both without changing a public type.

**D8. Blobs in the database, never deleted in the MVP**, capped at 64 MiB each; refs derived from payloads by trigger.

**D9. Receipts, hook deliveries, and tokens are kept forever in the MVP.** Deletion would need retention rules (a receipt deleted while a client may still retry would re-run the command), which no MVP PR owns. A later pruning migration can add them; it must keep the receipt semantics of mvp §2.4.

**D10. `open` refuses and never repairs.** Not a database, foreign, newer schema, or changed schema: an error with the file untouched. Corruption at run time stops writes. Integrity checks are the doctor's.

### Open Questions

**Q1. Is an incomplete UTF-8 sequence cut by the 1 KiB preview window "invalid"?** This sets the public meaning of `preview_lossy`. The core doc says "the first 1024 bytes decoded as UTF-8, invalid sequences replaced by U+FFFD".

| Option | Gain | Loss |
|---|---|---|
| A. Drop the cut character (no U+FFFD, not lossy) | Valid UTF-8 content never shows `preview_lossy: true`; a reader can trust the flag to mean "the content is not text" | The preview can be up to 3 bytes shorter than 1024 |
| B. Replace it (literal reading) | Exactly the first 1024 bytes | Most Japanese text above 64 KiB gets `preview_lossy: true` and a trailing U+FFFD by accident of the boundary |

Reversible: no for stored events (a preview is persisted in the payload); clients can recompute from the blob. **Recommendation: A**, and add one clause to the `Content` doc comment in `core` and mvp §2.1 ("a character cut by the window is dropped") when PR 6 lands.

**Q2. Where does the readiness write basis live?** mvp §2.1's projection table says `readiness_observed` "also stores the write basis", but `core::payload::ReadinessObserved` has no such field.

| Option | Gain | Loss |
|---|---|---|
| A. Internal `events.annex` (D7) | No public change; rebuild reproduces it | The event row holds data no client sees; PR 17's model must treat annex as part of the event |
| B. Add an optional `write_basis` to the public payload | One place for everything an event says | Exposes a daemon-internal PTY writer position to clients and the relay; additive, but public |
| C. Keep it only in the supervisor's memory | Nothing stored | Contradicts mvp §2.1, and the gate's basis would not survive a projection rebuild (it is only meaningful for a live launch, but the projection would differ) |

Reversible: A → B is an additive payload change plus a migration that copies the annex. **Recommendation: A** (decided as D7 unless review prefers B); the same column also carries PR 13 / 14's continuity evidence.

## Related Text in Other Documents

- `docs/mvp-design.md` §2.1 "Store": the trigger set here adds `RAISE(ROLLBACK)` and engine checks for `seq`, `cum_bytes`, and transcript order; the projection table's "write basis" is stored in the internal annex (D7, Q2).
- `docs/mvp-design.md` §2.1 `Content` and `crates/core/src/payload.rs` `Content::Blob::preview`: the cut-character rule (Q1).
- `docs/mvp-design.md` §3: `sha2` is listed among `core`'s dependencies but `core` does not use it yet; PR 6 adds it to `daemon`.
- `docs/mvp-design.md` §4 PR 8: the single-instance lock and the state directory location, which this store assumes (§5).
