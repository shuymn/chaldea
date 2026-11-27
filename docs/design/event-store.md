# Design: Event Store (MVP PR 6)

Status: implemented in `crates/daemon/src/store`. Scope is the PR 6 row of `docs/mvp-design.md` §4 (cited as "mvp §N"): the store in a new `crates/daemon`, holding append-only events, projections, command receipts, hook deliveries, tokens, and blobs. Its design obligations: drop-and-rebuild equality for every projection; `INSERT OR REPLACE`, update, and delete on `events` abort; `transcript_generation` before new-generation lines; a stale-cursor batch aborts; blob offload and lossy preview. Plan-layer claims it owns (mvp §6.2): CL-9, CL-10 (store side), CL-11, CL-12 (store side).

The contract implemented is mvp §2.1 ("Store", "Projections", "Transcript cursor rules", "Idempotency keys", "`Content`") and the store side of mvp §2.3 (gate evidence), §2.4 (epoch, `head_seq`, `cum_bytes`, receipts), §2.6 (worktree record), §2.9 (launch reservation, hook deliveries), and §2.11. No public type of `crates/core` changes. PR 6 clarifies one public sentence, the `Content` preview rule (mvp §2.1 and the doc comment of `core::payload::Content::Blob`, §2.5 below); no producer has persisted events yet, so no stored value changes meaning.

**What the store does and does not decide.** The store guarantees ordering, immutability, atomicity, idempotency by key, projection consistency, and durability, and it exposes every read and write the later PRs need as an operation inside one transaction. It does not decide routes, tokens, gate policy, hook binding and matching, reservation preconditions, transcript continuity, or retention; those PRs run their checks inside a store job (§2.2), so a check and the write it guards are atomic, and the store returns error kinds they map to their own outcomes.

Out of scope, designed in their own PRs: HTTP routes and status mapping, token issuance and checks, WebSocket, pairing (PR 7); daemon lifecycle, state directory, single-instance lock (PR 8); hook ingest, the emitter, and buffered-hook matching (PR 9); the supervisor, reservation checks, and gate decisions (PR 10); transcript tailers and continuity detection (PR 13, 14).

Sources read for this document (paths are relative to each root):

| Source | Revision | Read for |
|---|---|---|
| This repository | `f019dee` | `crates/core/src/{event,payload,state,input,sync,command,ids,lib}.rs`, `crates/core/clippy.toml`, `crates/pty-runtime/src/{writer,origin}.rs`, `Cargo.toml`, `Cargo.lock`, `.github/workflows/ci.yml`, `lefthook.yml`, `docs/architecture.md` §3–§4, `docs/design/pty-runtime.md` |
| T3 Code (`pingdotgg/t3code`) | `10586f9e` | `apps/server/src/persistence/Migrations/{001_OrchestrationEvents,055_OrchestrationV2}.ts`, `persistence/{OrchestrationEventStore,OrchestrationCommandReceipts,Sqlite}.ts`, `orchestration-v2/{CommandReceiptStore,EventStore,ProjectionStore,ProjectionMaintenance}.ts` |
| `rusqlite` | crate 0.41.1 | `Cargo.toml` (features), `src/transaction.rs`, `src/config.rs`, `src/busy.rs`, `src/lib.rs` |
| `libsqlite3-sys` | crate 0.39.1 | `build.rs` (bundled compile flags), `sqlite3/sqlite3.c` (version, unix file mode, win32 VFSs) |
| SQLite documentation | 3.54 | "Write-Ahead Logging", "PRAGMA synchronous", "PRAGMA recursive_triggers", "PRAGMA journal_size_limit", "CREATE TRIGGER", "CREATE TABLE" (CHECK constraints), "The ON CONFLICT Clause", "UPSERT", "DELETE", "The RETURNING Clause", "STRICT Tables", "Generated Columns", "SQLite Foreign Key Support", "Atomic Commit In SQLite", "Database File Format", "sqlite3_get_autocommit", "sqlite3_busy_timeout", "sqlite3_db_config" |

**Facts this design relies on.** T-facts are exercised by named tests of §7 on both runners; D-facts rest on SQLite's documentation or the bundled source and are not re-proven here.

| # | Fact | Where | Kind |
|---|---|---|---|
| S1 | The bundled build compiles SQLite 3.54.1 with `cc`, with `SQLITE_DEFAULT_FOREIGN_KEYS=1`, `SQLITE_THREADSAFE=1`, `SQLITE_USE_URI`, and the JSON functions, so both runners run the same SQLite. | `libsqlite3-sys` `sqlite3/sqlite3.c:470`, `build.rs:170-187` | D (`store_open::sqlite_version_is_bundled` asserts the version) |
| S2 | `REPLACE` deletes the conflicting row; delete triggers fire for that deletion only with `recursive_triggers` on, which is per connection and off by default. With it off, `INSERT OR REPLACE` on a duplicate `event_id` silently removes the old event. | "The ON CONFLICT Clause"; "PRAGMA recursive_triggers" | T (PR6-CL-2) |
| S3 | An upsert's `DO UPDATE` runs an `UPDATE` and fires update triggers. A `DELETE` on a table with triggers does not use the truncate optimization. | "UPSERT"; "DELETE" | T (PR6-CL-2) |
| S4 | `RAISE(ROLLBACK)` ends the whole transaction and returns the connection to autocommit, so a later statement on it commits on its own; `RAISE(ABORT)` and constraint failures undo only the statement. Some I/O and `SQLITE_FULL` errors also end the transaction; `sqlite3_get_autocommit` tells which happened. | "CREATE TRIGGER" (RAISE); "sqlite3_get_autocommit"; "BEGIN TRANSACTION" (response to errors) | T (PR6-CL-4) |
| S5 | `BEGIN IMMEDIATE` takes the write lock at `BEGIN`; while it is held no other connection, in this or another process, can write. | "BEGIN TRANSACTION"; "File Locking And Concurrency" | T (PR6-CL-20) |
| S6 | In WAL mode a transaction is committed when its commit frame is in the WAL; with `synchronous=FULL` the WAL is synced before `COMMIT` returns. Frames after the last commit frame are ignored on recovery. `synchronous=NORMAL` may lose recent commits on power loss. | "Write-Ahead Logging"; "PRAGMA synchronous"; "Atomic Commit In SQLite" | T for a kill before `COMMIT` and after it returned (PR6-CL-16); D for a kill during `COMMIT` and for power loss |
| S7 | WAL readers see one snapshot for their read transaction and do not block the writer; a long read keeps the WAL from being reset; a WAL reader can still get `SQLITE_BUSY` during recovery. WAL needs a local filesystem. | "Write-Ahead Logging" (concurrency, "Sometimes queries return SQLITE_BUSY") | T for the snapshot (PR6-CL-18); D otherwise |
| S8 | `application_id` and `user_version` are 32-bit integers in the database header, written transactionally. `PRAGMA journal_mode=WAL` on a rollback-journal database rewrites header bytes 18–19. | "Database File Format" §1.3; "Write-Ahead Logging" (persistence of WAL mode) | T (PR6-CL-17) |
| S9 | `STRICT` tables (3.37), `RETURNING` (3.35), generated columns (3.31), `json_extract`, and `octet_length` (3.43) exist; JSON functions may be used in triggers and generated columns with `trusted_schema=OFF`. | "STRICT Tables"; "The RETURNING Clause"; "Generated Columns"; "JSON Functions" | T (every schema test runs them) |
| S10 | A `CHECK` whose expression is `NULL` passes. | "CREATE TABLE" (CHECK constraints) | T (PR6-CL-3, CL-10) |
| S11 | Foreign keys are checked regardless of the statement's conflict clause: `INSERT … ON CONFLICT DO NOTHING` skips a duplicate key and still refuses a missing parent. A composite foreign key may reference a `UNIQUE (stream_id, seq)`. | "SQLite Foreign Key Support"; "The ON CONFLICT Clause" | T (PR6-CL-12, CL-6) |
| S12 | `SQLITE_DBCONFIG_DEFENSIVE` disables `writable_schema` edits and other direct corruption paths on that connection; rusqlite exposes it through `Connection::set_db_config`. | "sqlite3_db_config"; `rusqlite` `src/config.rs:38`, `:123` | D |
| S13 | On Unix, SQLite creates `-wal` and `-shm` with the database file's permissions. On Windows, the default `win32` VFS limits a path to `MAX_PATH × 4` = 1040 UTF-8 bytes; the `win32-longpath` VFS allows `UNICODE_STRING_MAX_CHARS`; rusqlite selects a VFS by name with `Connection::open_with_flags_and_vfs`. | `sqlite3.c:47085-47100`, `:49286`, `:49309-49310`, `:54486-54488`, `:54534`, `:54512`; `rusqlite` `src/lib.rs:493` | T for Windows paths (PR6-CL-21); D for Unix modes |
| S14 | rusqlite: `Connection::is_autocommit`, `busy_timeout`; the `cache` feature gates prepared-statement caching and pulls `hashlink`. | `rusqlite` `src/lib.rs:83-90`, `:1071`; `src/busy.rs:24` | D |
| S15 | The PTY writer's readiness basis is `Basis { seq, counted: ClassSet }`, where `seq` is `last_started(counted)` and `ClassSet` is a subset of `{Input, Human, Reply}`; the default counts all three. | `crates/pty-runtime/src/writer.rs:102-107`, `src/origin.rs:22-57` | D (PR 10 tests it) |
| S16 | A trigger can use `json_tree`, joins, and subqueries, including a recursive CTE inside a subquery. | "CREATE TRIGGER" (syntax restrictions) | D |

## 1. Goal and Dependencies

**Goal.** A `chaldea-daemon` library crate whose `store` module owns one SQLite database: one writer thread that runs every write as one `BEGIN IMMEDIATE` transaction per job, a small pool of read-only connections, and a schema in which SQLite itself enforces append-only events, gapless per-stream `seq`, global commit order, the running `cum_bytes`, stream membership, transcript generation order, blob references, and the immutable parts of receipts, hook deliveries, and tokens. Projections are written by one Rust function used by live appends and by rebuild alike. Everything runs on `macos-latest` and `windows-latest` CI against real files in the OS temp directory.

**Files.**

```
Cargo.toml                                  # members += "crates/daemon"; [workspace.dependencies] += rusqlite, sha2, tokio, base64, tempfile
crates/daemon/Cargo.toml                    # chaldea-daemon (lib only in PR 6); [lints] workspace = true
crates/daemon/src/lib.rs                    # pub mod store; crate doc: the store rules of §2.1
crates/daemon/src/store/mod.rs              # Store, StoreConfig, Clock, IdSource, WriteTicket, Committed, errors
crates/daemon/src/store/open.rs             # open sequence, connection setup, header checks, migrations, fingerprints
crates/daemon/src/store/schema.rs           # MIGRATIONS (v1 DDL to v4), shipped fingerprints per version, PROJECTION_VERSION, PART_BYTES
crates/daemon/src/store/writer.rs           # writer thread, job queue, deadlines, poisoning, failed state, shutdown
crates/daemon/src/store/readers.rs          # read-only connection pool, ReadTx, Page
crates/daemon/src/store/write_tx.rs         # WriteTx: append, transcript batch, receipts, hook deliveries, tokens, rebuild
crates/daemon/src/store/append.rs           # NewEvent → row; idempotency comparison; envelope assembly
crates/daemon/src/store/content.rs          # prepare_content, the typed Content walker, blob validation, preview
crates/daemon/src/store/spool.rs            # BlobSpool, SpooledBlob: values above MAX_BLOB_BYTES in a state-directory file
crates/daemon/src/store/annex.rs            # EventAnnex, WriteBasis, CountedClass
crates/daemon/src/store/project.rs          # apply(&mut ProjectionTx, &StoredEvent); compare_projections
crates/daemon/src/store/rows.rs             # internal row types and snapshot assembly
crates/daemon/src/store/clock.rs            # SystemClock (RFC 3339 UTC ms), RandomIds
crates/daemon/src/store/tests/              # unit tests with raw SQL on production-configured connections (§7)
crates/daemon/tests/store_*.rs              # integration tests through the public API (§7)
crates/daemon/tests/support/mod.rs          # temp store, fixed clock and ids, history model and generators
```

**Dependencies.** Root `[workspace.dependencies]`, exact pins in the existing style:

```toml
# The store (docs/design/event-store.md): SQLite 3.54.1 compiled from the bundled source, so
# macOS and Windows run the same SQLite. No default features (prepared-statement cache, wasm FFI).
rusqlite = { version = "=0.41.1", default-features = false, features = ["bundled"] }
# Blob content addresses (mvp §2.1 `Content`); PR 7's token hashes reuse it.
sha2 = "=0.11.1"
# The writer's reply channel and commit notifications only; PR 7 and PR 8 add runtime features.
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
# Event ids and the epoch are random v4 UUIDs.
uuid = { workspace = true, features = ["v4"] }

[dev-dependencies]
proptest.workspace = true
tempfile.workspace = true

[lints]
workspace = true
```

`Cargo.lock` resolves `libsqlite3-sys` to `0.39.1` (`rusqlite =0.41.1` requires `0.39`). `rusqlite` needs Rust 1.89; the toolchain is 1.96.0. The bundled build needs a C compiler: Xcode's `cc` on `macos-latest`, MSVC on `windows-latest`. The daemon stays `unsafe_code = "forbid"`; the store needs no `unsafe` (Unix file mode through `std::os::unix::fs::OpenOptionsExt::mode`).

**Workspace and CI.** The crate joins `members`. The `workspace` job runs unchanged; `cargo test --workspace --locked` now includes the store's unit and integration tests on both runners. No new CI step: the existing `core` dependency check keeps `rusqlite` and `tokio` out of `core`, and the only `cfg(windows)` store test (`store_open::long_path_vfs_is_selected`) is listed in the Windows sentinel step. `lefthook.yml` needs no change.

## 2. Types and Functions

Names may change in review; shapes may not.

### 2.1 The Rules

1. **SQLite is the proof, Rust is the second line.** Every invariant a schema can express is a constraint or trigger (§2.4): `STRICT` types, `CHECK` written so `NULL` cannot pass it (S10), `UNIQUE`, foreign keys, triggers that `RAISE(ROLLBACK)` (S4), and `BEGIN IMMEDIATE` for check-then-write (S5). Rust checks the same conditions first to return a typed error, never as the only guard. Where SQLite cannot express a rule (SHA-256 of a blob: SQLite has no built-in hash function), the rule is enforced at the store's boundary and named as such.
2. **No cross-call state outside the database.** The stream head, `cum_bytes`, cursor, binding, receipt state, delivery routing, and every projection value are read inside the transaction that depends on them. The store keeps only the epoch in memory, read once at open; the `meta` row is immutable by trigger, and `store_open::epoch_survives_reopen` fails if the cached value differs from the row.
3. **One transaction per job, and nothing commits after an error.** A caller hands the writer a closure over `WriteTx`; the closure reaches the database only through `WriteTx`. The first statement error poisons the `WriteTx`: every later call returns that error without touching SQLite, and the writer rolls back however the closure returns (§2.2).
4. **Projections change only through `project::apply`**, called for each event inside the transaction that inserts it, and by rebuild on the same events in the same order (§2.6).

**Bounds of these guarantees.** They hold for every connection the store opens. A process with raw access to the database file can still change anything: drop a trigger, edit rows on a connection without `recursive_triggers`, or rewrite pages. The store detects a changed schema at the next open (§2.8) but not a row edited by another program while the schema is intact, nor a change made while it runs. Chaldea's file permissions (§5) limit who can do that to the user's own processes.

### 2.2 `Store`, Writer, Readers

```rust
pub struct StoreConfig {
    pub path: PathBuf,                     // <state_dir>/chaldea.db; the directory exists (PR 8 creates it)
    pub readers: usize,                    // 4; 1..=MAX_READERS (64)
    pub writer_queue: usize,               // 256 jobs; 1..=MAX_WRITER_QUEUE (65 536), allocated up front
    pub busy_timeout: Duration,            // 5 s; at most i32::MAX ms (SQLite's range)
    pub clock: Arc<dyn Clock>,             // recorded_at; tests pass a fixed clock
    pub ids: Arc<dyn IdSource>,            // event ids, the epoch; tests pass a counter
}
pub trait Clock: Send + Sync { fn now(&self) -> (Instant, Timestamp); }      // RFC 3339 UTC, ms
pub trait IdSource: Send + Sync { fn event_id(&self) -> EventId; fn epoch(&self) -> Epoch; }

#[derive(Clone)] pub struct Store { /* Arc<Inner>: queue sender in Mutex<Option<_>>, reader pool, epoch, notifier */ }
impl Store {
    pub fn open(cfg: StoreConfig) -> Result<Store, OpenError>;
    pub fn epoch(&self) -> &Epoch;
    pub fn paths(&self) -> StorePaths;                       // db, -wal, -shm
    /// Queues a write job. Fails at once, without waiting, when the queue is full, closed, or failed.
    pub fn submit<R, F>(&self, not_after: Instant, job: F) -> Result<WriteTicket<R>, SubmitError>
    where F: FnOnce(&mut WriteTx<'_>) -> Result<R, JobError> + Send + 'static, R: Send + 'static;
    /// Runs `f` in one read transaction on a pooled read-only connection.
    pub fn read<R>(&self, wait_until: Instant, f: impl FnOnce(&ReadTx<'_>) -> Result<R, StoreError>) -> Result<R, ReadError>;
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Committed>;
    /// Stops intake for every clone, drains, closes; see §5.
    pub fn shutdown(&self, deadline: Instant) -> ShutdownReport;
}

pub struct WriteTicket<R>(tokio::sync::oneshot::Receiver<Result<R, WriteError>>);
impl<R> Future for WriteTicket<R> { type Output = Result<R, WriteError>; }   // async callers
impl<R> WriteTicket<R> { pub fn blocking_wait(self) -> Result<R, WriteError>; } // tests, sync callers
impl WriteTx<'_> { pub fn commit_stamp(&mut self, now: impl FnOnce() -> Instant + Send + 'static) -> CommitStamp; }
impl CommitStamp { pub fn get(&self) -> Option<Instant>; }  // `now()` read right after this job's COMMIT; empty otherwise

pub enum SubmitError { QueueFull, Closed, Failed }
/// What happened to a job. Each variant says whether the job's effects are in the database.
pub enum WriteError {
    Expired,                   // not_after passed before the job ran: nothing written
    Busy,                      // the write lock was not obtained (another process): nothing written
    Rejected(JobError),        // the job returned Err (a refusal or a store error it passed on): rolled back
    RolledBack(StoreError),    // a statement failed (constraint, trigger, disk full, ...): rolled back
    Panicked,                  // the job panicked: rolled back
    CommitUnknown(StoreError), // COMMIT returned an I/O error: may or may not be durable
    Failed(StoreError),        // the connection failed (corrupt, I/O, failed rollback): not run, or rolled back
    WriterGone,                // the writer thread ended without replying: outcome unknown
    WouldDeadlock,             // blocking_wait called inside a job (on a writer thread): refused at once
}
pub enum JobError { Store(StoreError), Refused(Box<dyn std::error::Error + Send + Sync>) }
pub struct Committed { pub heads: Vec<StreamHeadEntry> }   // streams this job advanced, after COMMIT
```

**Writer.** One OS thread, `store-writer`, owns the only read-write connection. Jobs arrive over a bounded `std::sync::mpsc::sync_channel(writer_queue)`; `submit` uses `try_send`, so a caller never waits for queue space. For each job the writer:

1. reads the clock; if `now > not_after`, replies `Expired`;
2. runs `BEGIN IMMEDIATE`; SQLite's busy handler retries for about `busy_timeout` while another process holds the lock. If the lock is not obtained, the reply is `Busy`, whatever the deadline did meanwhile (nothing was written either way, and `Busy` tells the caller why). `busy_timeout` is SQLite's retry budget, not a wall-clock bound the store promises;
3. reads the clock again; if `now > not_after`, `ROLLBACK` and replies `Expired`. This second check is the boundary between "no effect" and "may commit" (mvp §2.5 "Deadlines"): a job runs its closure only if its deadline had not passed once it held the lock. `recorded_at` is stamped from this clock read;
4. reads `meta.projection_version` (one primary-key read); if it differs from the code's, `ROLLBACK`, replies `Failed(ProjectionVersion)`, and enters the failed state: another process rebuilt the projections with another `apply`, and this one must not write with its own (§2.4);
5. runs the closure under `catch_unwind` with a fresh `WriteTx`;
6. if the closure returned `Ok` and the `WriteTx` is not poisoned, checks `!is_autocommit()` (S14) and runs `COMMIT`; the outcomes are in the table below. After a successful `COMMIT` and before the reply it fills each `CommitStamp` the closure took with `WriteTx::commit_stamp(now)`, calling that `now` (the caller's clock), so a caller's timer can start at the commit itself; a job that does not commit leaves its stamps empty;
7. otherwise (closure `Err`, panic, or a poisoned `WriteTx` whatever the closure returned), rolls back: if `is_autocommit()` already holds (S4: `RAISE(ROLLBACK)`, some `SQLITE_FULL`), there is nothing to roll back; else `ROLLBACK`. The reply is the poisoning error when there is one (`RolledBack`), else `Rejected` or `Panicked`.

| Point | Outcome | Reply | Writer afterwards |
|---|---|---|---|
| Before `COMMIT`: expired, busy, job error, panic, or a statement failing during the job (constraint, trigger, `SQLITE_FULL`) | nothing of the job is in the database | `Expired`, `Busy`, `Rejected`, `RolledBack` (`DiskFull` for `SQLITE_FULL`), `Panicked` | continues; the next job may succeed |
| `ROLLBACK` fails | the transaction state is unknown | `Failed` | retires the connection; failed state |
| `COMMIT` fails and `is_autocommit()` is false: the transaction is still open, so it did not commit (e.g. `SQLITE_BUSY`, or `SQLITE_FULL` reported with the transaction kept) | nothing; the writer runs `ROLLBACK` | `RolledBack` (`DiskFull` for `SQLITE_FULL`) | continues; the next job may succeed (a failed `ROLLBACK`: row above) |
| `COMMIT` fails and `is_autocommit()` is true: the transaction ended (an I/O error, or `SQLITE_FULL` that ended it) | all or nothing after recovery, never part (S6: recovery keeps a transaction only with its commit frame) | `CommitUnknown` | failed state |
| `COMMIT` returned | all of the job, durable | `Ok` | continues; `Committed` published after the reply |
| Process killed during `COMMIT` | all or nothing after recovery | (no reply) | — |

**Poisoning.** `WriteTx` records the first `StoreError` any of its statements returned (constraint, trigger, `SQLITE_FULL`, I/O). After that every `WriteTx` method returns the recorded error without running SQL, so a closure that catches an error and keeps writing can neither add to the transaction nor, after a transaction-ending error, write in autocommit mode. Errors the store raises in Rust before any statement (for example `KeyConflict`, `StaleCursor`) also poison, so a job is all or nothing whichever line refused it.

**Failed state.** On `SQLITE_CORRUPT`, `SQLITE_NOTADB`, an I/O error, a failed `ROLLBACK`, `CommitUnknown`, or a projection version mismatch, the writer first publishes the failed state (under the sender lock, where `submit` decides admission), then replies to the job, so a caller that sees the reply can no longer submit; then it closes its connection, replies `Failed` to every queued job, and refuses later submissions with `SubmitError::Failed`. Reads continue on the reader connections. PR 8 reports and stops the daemon; nothing is deleted or repaired.

**Notifications.** `Committed` goes out on a `tokio::sync::broadcast` channel (capacity 1024) after the reply. The owning `shutdown` call drops the store's own sender and the writer thread drops its sender before it publishes its report, so once a report says `writer_exited` every receiver gets `Closed` after draining the notifications still buffered for it; `subscribe` after shutdown returns a closed receiver. A lagging subscriber gets `Lagged`, then the notifications still buffered, then `Closed`, and resyncs from heads, which mvp §2.4's subscribe-time and periodic `heads` already cover. Poll commit notifications in ordinary Tokio tasks; custom wakers must return promptly and must not panic or wait for shutdown progress, because the owning `shutdown` wakes pending receivers inline when it drops the last sender (§5).

**What callers learn** (for PR 7's mapping, which this PR does not design): `Ok` means committed and durable (§2.9); `Expired`, `Busy`, `Rejected`, `RolledBack`, `Panicked` mean the job left nothing; `CommitUnknown` and `WriterGone` mean all or nothing, unknown until a later read; `Failed` before the job ran means nothing, and the store is unusable for writes. For a command whose intent job ended `CommitUnknown`, the receipt after a restart is absent (the job did not commit), `completed` (it did, and completed in that job, as `send` does), or `in_progress` (it did, and completes later); only the last becomes `unknown_after_crash` (mvp §2.4 "Restart").

**Readers.** `readers` connections opened read-only (`SQLITE_OPEN_READ_ONLY`, `PRAGMA query_only=ON`) in a pool with a `Condvar`; `read` waits for a free connection until `wait_until`, else `ReadError::Busy`; after `shutdown` began it returns `ReadError::Closed`. Each call runs `BEGIN DEFERRED`, `f`, `COMMIT`, so everything `f` reads comes from one WAL snapshot (S7): a snapshot and its `as_of_seq`, or events and `head_seq`, are consistent however many statements `f` runs. A reader can get `SQLITE_BUSY` during WAL recovery (S7), returned as `ReadError::Store(Busy)`. `read` is synchronous; async callers run it in `spawn_blocking` (PR 7). Reads must be short: an open read transaction keeps the WAL from resetting.

Alternatives are in §3 ("Writer model").

### 2.3 Write Operations (`WriteTx`)

Every method runs inside the job's transaction; its effects commit with the job. "Error" means the method returns `Err` and poisons the `WriteTx` (§2.2), so the job rolls back whatever the closure does next.

```rust
impl WriteTx<'_> {
    pub fn recorded_at(&self) -> &Timestamp;
    pub fn read(&self) -> ReadTx<'_>;                       // every ReadTx query, inside the write transaction

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
    pub source: Source,                       // source.kind = transcript is refused here: use append_transcript
    pub occurred_at: Option<Timestamp>,       // None: the source states no time; stored as recorded_at
    pub annex: Option<EventAnnex>,            // internal, never on the wire (§2.6)
    pub blobs: Vec<BlobInput>,                // the bytes of every blob its Content fields reference that is not stored yet
}
pub enum BlobInput {                          // the media type is the referencing Content's
    Bytes(Vec<u8>),                           // at most MAX_BLOB_BYTES, in memory; stored in `blobs`
    Spooled(SpooledBlob),                     // above it, in a spool file (§2.5); stored as `large_blob_parts`
}
pub enum Appended { New(EventEnvelope), Existing(EventEnvelope) }
```

**`append`.** For each event in order, inside the transaction:

1. **Boundary checks.** The source is first made canonical: an `Open::Unknown` string that spells a known value (`"transcript"`, `"hook"`, an agent name) becomes that value, because both are the same stored string and a reader of the row sees the known value; dispatch, the duplicate check, the projection, and the returned envelope all use the canonical source. Then `source.kind` is not `transcript` and the kind is not `transcript_generation` (those go through `append_transcript`, so no transcript event bypasses the binding and cursor checks). Then `validate_event`, the one validation step every path that stores an event runs (`append`, `append_transcript` for each line, hook delivery and promotion): the typed `Content` fields are valid and every blob they reference is in `blobs`, in `large_blobs`, or in this event's `BlobInput`s, with matching hash, size, and preview (§2.5); the annex has the form its kind requires (§2.6).
2. **Idempotency.** Look up `(stream_id, idempotency_key)`. If a row exists, compare every producer-supplied field: `kind`, `thread_id`, `launch_id`, `command_id`, `source`, `annex`, `payload` (as JSON values), and the time: `occurred_at: None` matches a row stored without a source time (`occurred_explicit = 0`), whatever its value; `Some(t)` matches a row with `occurred_explicit = 1` and `occurred_at = t`. All equal: the result is `Existing(stored)`, carrying the first insert's `event_id`, `seq`, and `recorded_at`; no row, no projection change. Any difference: error `KeyConflict { stream_id, key, field }` (CL-10: a replayed source never creates a second event; a key reused for a different event is a producer bug and must not pass as a retry). Earlier events of the same batch count, so a batch that repeats a key gets `Existing` for the repeat.
3. **Blobs and refs.** Insert each `BlobInput` the event references: the store computes SHA-256 and size from the bytes itself (`INSERT … ON CONFLICT(sha256) DO NOTHING`), so no caller can store bytes under another content's hash. A `Spooled` input whose hash is already in `large_blobs` is reused (refs only); otherwise the store inserts its `large_blobs` row and then its parts from the spool file in order, through one `PART_BYTES` buffer, hashing what it inserts, and before the event row checks that the SHA-256 and length of the inserted bytes equal the reference's and that the stored parts are contiguous from 0 with that total length; any mismatch is `ContentError::BlobMismatch` and the job rolls back with nothing advanced. Then insert a `blob_refs` row (a `large_blob_refs` row for a large blob) `(stream_id, sha256, media_type)` for each typed reference, with the media type its `Content` declares (`ON CONFLICT DO NOTHING`; the foreign key still refuses a missing blob, S11).
4. **Row.** Insert the row with `seq` and `cum_bytes` computed in the `INSERT` from the stream's last row, `RETURNING global_seq, seq, cum_bytes`; the triggers re-check order, membership, and refs (§2.4; `events_blob_refs` refuses the row if a typed reference has no ref from step 3). `event_id` comes from `IdSource`, `recorded_at` from the job.
5. **Projection.** Call `project::apply` (§2.6). Its errors (`ProjectionError`: a referenced row missing, a compare-and-set that matched no row) poison the job.

Errors: `KeyConflict`, `Content(ContentError)`, `Annex(AnnexError)`, `TranscriptViaAppend`, `TaskChangeWithoutCause`, `Constraint(String)` (a trigger or `CHECK` refused; the message names it), `Projection(ProjectionError)`, `DiskFull`, `Sqlite(..)`. The store issues only plain `INSERT` on `events`; never `REPLACE`, `OR IGNORE`, or an upsert.

**`append_transcript`.** The only way to store a transcript-sourced event (mvp §2.1 "Transcript cursor rules").

```rust
pub struct TranscriptBatch {
    pub thread_id: ThreadId,
    pub launch_id: LaunchId,                  // the launch the tailer runs for
    pub session_id: String,                   // the session it is bound to
    pub path: String,                         // the bound transcript_path it read
    pub agent: Agent, pub cli_version: Option<String>,
    pub expect: Option<CursorPos>,            // the cursor the tailer read from; None: no cursor yet
    pub new_generation: Option<NewGeneration>,// appended as transcript_generation; then `lines` must be empty
    pub lines: Vec<TranscriptLine>,
    pub advance_to: ReadPos,                  // offset just after the last complete line read
}
pub struct CursorPos { pub generation: u64, pub path: String, pub file_id: String, pub read_offset: u64 }
pub struct NewGeneration { pub file_id: String, pub continuity: Continuity }   // generation = stored + 1, or 1
pub struct TranscriptLine {
    pub offset_start: u64, pub offset_end: u64,
    pub event: TypedEvent, pub occurred_at: Option<Timestamp>,
    pub continuity: Continuity,               // evidence at offset_end (its content: §2.6 Annex)
    pub blobs: Vec<BlobInput>,
}
pub struct ReadPos { pub read_offset: u64, pub continuity: Continuity }
pub struct Continuity(pub String);           // opaque to the store
```

The store builds every envelope itself: stream `thread:<thread_id>`, `thread_id`, `launch_id`, source `{ kind: transcript, agent, cli_version, transcript: { generation, path, file_id, offset_end } }`, and key `transcript:<generation>:<offset_start>` (mvp §2.1), so a line cannot name another thread, path, or generation. The `transcript_generation` event's key is `transcript_generation:<generation>` (an internal format, mvp §2.1), so it cannot collide with `transcript:<generation>:0` of the generation's first line. Inside the transaction, **before any duplicate detection or insert**:

1. **Binding** (the batch's new state against the thread's current binding). The thread's `current_launch_id` equals `launch_id`, that launch's `session_id` equals `session_id`, and the thread's `transcript_path` is non-null and equals `path`. Otherwise `StaleBinding { stored }`. A batch the tailer read before a rebind or a resume is refused even if its cursor still matches. The check does not require the launch to be running: after `launch_exited` the launch stays current until the next reservation, so a trailing `partial_line` `raw_unknown` (mvp §2.1) still commits.
2. **Cursor** (the batch's old state against the stored cursor, independently of step 1). The stored `proj_transcript_cursors` row equals `expect` in generation, `path`, `file_id`, and `read_offset`; `expect: None` requires no row. Otherwise `StaleCursor { stored }`. Because `expect` carries the old path, a batch can start a generation on the newly bound path B while the cursor still names path A.
3. **Generation start.** A batch with `new_generation` carries no lines, and a job that starts a generation for a thread may append no transcript line of that thread, in either call order: `WriteTx` records, per thread, whether this job started a generation and whether it appended lines, and refuses the second of the two with `GenerationWithLines`, which poisons the job; a second generation start for the same thread in one job is `BadBatch`. The store appends `transcript_generation { generation, path, file_id }` with generation = the stored one + 1, or 1 for the thread's first, and `source.transcript.offset_end = 0`; the cursor becomes `(generation, path, file_id)` with `event_offset = read_offset = 0` and both continuities from the generation's annex (`new_generation.continuity`), live and in rebuild; `advance_to.read_offset` must be 0 and `advance_to.continuity` is not used. This is the one rule for a new generation, whether the path changed (rebind, resume) or the same path holds another file (`file_id` changed, truncate-and-regrow): the generation commits alone at offset 0, and its first line comes in the next batch.
4. **Continuation.** Without `new_generation`, `expect` must be `Some` with `expect.path == path`; the lines belong to `expect.generation` and `expect.file_id`. Offsets satisfy `offset_start < offset_end`, ascending; the first `offset_start ≥ expect.read_offset` (the tailer reads from its read position, which after a rebuild equals the last event's position); the last `offset_end ≤ advance_to.read_offset`.

Then each line goes through `validate_event` and the same steps as `append` (2–5 there). The cursor checks above run before the duplicate check, so they decide what a retry gets: a committed batch that advanced the cursor, sent again, names the old cursor and gets `StaleCursor` (an empty continuation that left the read offset where it was matches the cursor again and commits again, appending no events); after a rebuild rewind every stored line lies before the rebuilt `read_offset`, so a batch resubmitting one gets `BadBatch`. Neither stores anything twice; a line's `Existing` is reached only when its batch passes the cursor checks. Last, `UPDATE proj_transcript_cursors SET read_offset = ?, read_continuity = ? WHERE thread_id = ? AND generation = ? AND file_id = ? AND read_offset <= ?`, bound to the **post-append** generation and `file_id`, must change exactly one row, else `StaleCursor`. Everything happens in one `BEGIN IMMEDIATE` transaction, so no other writer can move the binding or the cursor between a check and the write (S5). Batches by kind:

| Batch | Effect |
|---|---|
| `new_generation`, no lines | commits the generation alone at offset 0; the tailer does this before reading past a generation change (mvp §2.1) |
| `new_generation` with lines, or a generation start and lines of the same thread in one job | refused (`GenerationWithLines`) |
| lines | events and cursor together |
| no generation, no lines | advances `read_offset` past lines that produced no event |

The checks against `session_id` and the expected cursor are the API's; the triggers of §2.4 enforce in the engine the generation order, that a line or generation names the latest `session_bound`'s launch and transcript path, and the line's `file_id` and offsets, so a raw insert of a line for launch L1 is refused once the thread rebound to B, and also once a launch L2 was reserved and is not bound yet. The triggers read `events` only (the latest `launch_reserved` is the current launch), so they hold while projections are dropped. A `partial_line` of L1 after its exit and before the next reservation still passes.

**Receipts** (mvp §2.4). `receipt_begin` reads the row for `command_id`, calls `core::command::check_receipt`, and for `New` inserts it `in_progress` with the token and request hash; it returns the `ReceiptLookup`. A command job calls it first and records its intent (events) in the same job, so the receipt and the intent commit together or not at all; on any other lookup the job writes nothing. `send` calls `receipt_complete` in that same job as well (mvp §2.4 "Completion"); synchronous side-effect commands complete in a later job. Two concurrent duplicates are serialized by the writer and by `BEGIN IMMEDIATE`, so the second reads the first's row; the primary key is the backstop for a second process. `receipt_complete` stores the HTTP status and the body byte for byte; a later `Replay` returns exactly them. `receipts_mark_unknown_after_crash` sets every `in_progress` row to `unknown_after_crash` (PR 8 / PR 10 call it once, before accepting commands). A trigger allows only `in_progress → completed | unknown_after_crash` and keeps `command_id`, token, hash, and `created_at` unchanged.

**Hook deliveries** (mvp §2.1 hook keys, §2.9). The store keeps PR 9's buffered data opaque; matching and the events it produces are PR 9's.

```rust
pub struct NewHookDelivery {
    pub launch_id: LaunchId, pub emit_id: Uuid,
    pub receipt_ordinal: u64,                 // PR 9's intake order for the launch, assigned when the request arrived
    pub first_reply: HookReply,               // { http_status: u16, body: String }, immutable once stored
    pub routing: NewRouting,
}
pub enum NewRouting {
    Appended(Vec<NewEvent>),                  // non-empty
    Buffered(BufferedHook),
    RawUnknown(NewEvent),                     // a raw_unknown event
}
pub struct BufferedHook { pub body: Vec<u8>, pub meta: String }  // PR 9's raw payload and its metadata (JSON)
pub enum HookDeliveryLookup { New(HookDelivery), Replay(HookDelivery) }
pub struct HookDelivery { pub receipt_ordinal: u64, pub commit_ordinal: u64, pub first_reply: HookReply, pub routing: Routing }
pub enum Routing { Appended { stream_id: StreamId, seq: u64 }, Buffered, RawUnknown { stream_id: StreamId, seq: u64 } }
pub enum Promotion { Appended(Vec<NewEvent>), RawUnknown(NewEvent) }   // Appended non-empty
pub struct BufferedDelivery { pub emit_id: Uuid, pub receipt_ordinal: u64, pub received_at: Timestamp, pub hook: BufferedHook }
```

`hook_delivery_record`: if `(launch_id, emit_id)` exists, returns `Replay` with the stored first reply and the current routing, and changes nothing. Otherwise it appends the routing's events (if any), takes the launch's next commit ordinal (computed in the `INSERT`, checked gapless by trigger), and inserts the row with the reply, both ordinals, the routing, and for a terminal routing the `(stream_id, seq)` of its first event. Two orders are kept because they can differ: the **receipt ordinal** is the order in which the daemon received the requests, assigned by PR 9 at intake (per launch, increasing, unique by `UNIQUE (launch_id, receipt_ordinal)`; gaps are allowed for requests that never recorded a delivery), and is the order mvp §2.9 requires for buffered hooks; the **commit ordinal** is the order the record jobs committed, kept for audit. A launch never outlives a daemon run (restart ends every open launch, mvp §2.9), so PR 9's per-launch counter can live in memory. `hook_delivery_promote` takes the final events from the caller: PR 9 reads `buffered_deliveries(launch)` (in receipt order) in the binding job, after `session_bound`, decides each delivery's events against the current projections in that order, and calls `hook_delivery_promote` for each, in that order, in the same job. Promotion appends the events, sets the routing to `appended` or `raw_unknown` with `(stream_id, seq)`, and clears `pending_body` and `pending_meta`. The trigger allows only `buffered → appended | raw_unknown`, with `stream_id` and `seq` set and the pending columns cleared, and never changes the first reply, either ordinal, key, or `received_at`; terminal rows reference `events (stream_id, seq)` by foreign key. The caller replies only after the ticket resolves `Ok`, so the first reply is durable before the response (§2.9).

**Tokens.** `token_insert` stores `{ token_id, kind, sha256, thread_id, launch_id, created_at }`, where `kind` is `chaldea_core::auth::TokenKind`, which the store re-exports as `rows::TokenKind`; `token_revoke` sets `revoked_at` for one token, a launch's tokens, or a thread's tokens, returning the count. A trigger allows only `revoked_at` from `NULL` to a value. Issuance, hashing of secrets, and comparison are PR 7's; the reservation of mvp §2.9 inserts the launch's hook and agent token hashes in the job that appends `launch_reserved`. PR 20 (deferred) may add a scope column for extension tokens in a later migration.

**Rebuild and verify** (§2.6).

### 2.4 Schema (Migration v1)

Internal (mvp §2.1). All tables `STRICT`. Text columns hold canonical strings (`StreamId`, UUIDs, timestamps) as core serializes them. Every `CHECK` is written so that a `NULL` operand fails it (S10): comparisons that can meet `NULL` use `IS` / `IS NOT`, and a disjunction names `IS NOT NULL` explicitly.

```sql
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;   -- 'epoch', 'created_at', 'projection_version'

CREATE TABLE events (
  global_seq      INTEGER PRIMARY KEY,                -- rowid; commit order
  stream_id       TEXT NOT NULL,
  seq             INTEGER NOT NULL CHECK (seq >= 1),
  event_id        TEXT NOT NULL UNIQUE,
  schema_version  INTEGER NOT NULL CHECK (schema_version >= 1),
  idempotency_key TEXT NOT NULL CHECK (length(idempotency_key) > 0),
  kind            TEXT NOT NULL CHECK (length(kind) > 0),
  thread_id       TEXT, launch_id TEXT, command_id TEXT,
  source          TEXT NOT NULL CHECK (json_valid(source)),
  occurred_at     TEXT NOT NULL, occurred_explicit INTEGER NOT NULL CHECK (occurred_explicit IN (0, 1)),
  recorded_at     TEXT NOT NULL,
  payload         TEXT NOT NULL CHECK (json_valid(payload) AND json_type(payload) = 'object'),
  payload_bytes   INTEGER NOT NULL CHECK (payload_bytes = octet_length(payload)),
  cum_bytes       INTEGER NOT NULL,
  annex           TEXT CHECK (annex IS NULL OR json_valid(annex)),
  src_kind        TEXT    GENERATED ALWAYS AS (json_extract(source, '$.kind')) VIRTUAL,
  tr_generation   INTEGER GENERATED ALWAYS AS (json_extract(source, '$.transcript.generation')) VIRTUAL,
  tr_path         TEXT    GENERATED ALWAYS AS (json_extract(source, '$.transcript.path')) VIRTUAL,
  tr_file_id      TEXT    GENERATED ALWAYS AS (json_extract(source, '$.transcript.file_id')) VIRTUAL,
  tr_offset_end   INTEGER GENERATED ALWAYS AS (json_extract(source, '$.transcript.offset_end')) VIRTUAL,
  UNIQUE (stream_id, seq),
  UNIQUE (stream_id, idempotency_key),
  -- stream membership
  CHECK (stream_id = 'host' OR (thread_id IS NOT NULL AND stream_id = 'thread:' || thread_id)),
  CHECK (kind NOT IN ('thread_registered', 'thread_closed', 'worktree_changed') OR stream_id = 'host'),
  CHECK (stream_id <> 'host' OR kind IN ('thread_registered', 'thread_closed', 'worktree_changed', 'surface_update', 'raw_unknown')),
  CHECK (json_extract(payload, '$.thread_id') IS NULL OR json_extract(payload, '$.thread_id') IS thread_id),
  CHECK (json_extract(payload, '$.launch_id') IS NULL OR json_extract(payload, '$.launch_id') IS launch_id),
  CHECK (kind <> 'mailbox_message' OR stream_id IS 'thread:' || json_extract(payload, '$.to_thread_id')),
  CHECK (kind <> 'surface_update'
         OR (json_extract(payload, '$.scope.kind') IS 'thread' AND stream_id IS 'thread:' || json_extract(payload, '$.scope.thread_id'))
         OR ((json_extract(payload, '$.scope.kind') IS 'host' OR json_extract(payload, '$.scope.kind') IS 'workspace') AND stream_id = 'host')),
  -- transcript positions are complete whenever the source is a transcript
  CHECK (src_kind IS NOT 'transcript' OR (tr_generation IS NOT NULL AND tr_generation >= 1 AND tr_path IS NOT NULL
         AND tr_file_id IS NOT NULL AND tr_offset_end IS NOT NULL AND tr_offset_end >= 0)),
  CHECK (kind <> 'transcript_generation' OR (src_kind IS 'transcript' AND tr_offset_end IS 0
         AND json_extract(payload, '$.generation') IS tr_generation AND json_extract(payload, '$.path') IS tr_path
         AND json_extract(payload, '$.file_id') IS tr_file_id)),
  -- readiness evidence carries its writer basis for its own launch (§2.6)
  CHECK (kind <> 'readiness_observed' OR (json_extract(annex, '$.readiness.launch_id') IS launch_id AND launch_id IS NOT NULL
         AND json_type(annex, '$.readiness.position') IS 'integer' AND json_extract(annex, '$.readiness.position') >= 0
         AND json_type(annex, '$.readiness.counted') IS 'array'
         AND coalesce(json_extract(annex, '$.readiness.counted'), '') IN ('[]', '["input"]', '["human"]', '["reply"]', '["input","human"]',
             '["input","reply"]', '["human","reply"]', '["input","human","reply"]')))
) STRICT;
CREATE INDEX events_kind ON events (stream_id, kind, seq);
CREATE INDEX events_transcript ON events (stream_id, tr_generation, tr_offset_end) WHERE src_kind = 'transcript';
```

The payload `thread_id` and `launch_id` checks hold for every kind that has those top-level keys (`thread_registered`, `thread_closed`, the launch kinds, `session_bound`, `readiness_observed`, `human_write`, `approval_requested`, `input_created` when its launch is set); no kind uses either key for anything else.

Triggers on `events` (each `RAISE(ROLLBACK, '<name>: …')`, S4):

| Trigger | When | Refuses |
|---|---|---|
| `events_no_update` | `BEFORE UPDATE` | every update, and with S3 every upsert's `DO UPDATE` |
| `events_no_delete` | `BEFORE DELETE` | every delete, and with `recursive_triggers=ON` every `REPLACE` deletion (S2): `INSERT OR REPLACE`, `REPLACE INTO`, `UPDATE OR REPLACE` that collide on `event_id`, the idempotency key, `(stream_id, seq)`, or `global_seq` |
| `events_order` | `BEFORE INSERT` | `NEW.seq IS NOT` the stream's last `seq` + 1 (1 for an empty stream), or `NEW.cum_bytes IS NOT` the last `cum_bytes` + `NEW.payload_bytes` |
| `events_global_order` | `AFTER INSERT` | `NEW.global_seq IS NOT (SELECT max(global_seq) FROM events)`: an explicit `global_seq` below an existing one; the store never names the column, so SQLite assigns max + 1 |
| `events_generation` | `BEFORE INSERT WHEN NEW.kind = 'transcript_generation'` | `NEW.tr_generation IS NOT` the stream's highest `tr_generation` of a `transcript_generation` + 1 (1 when none); `NEW.launch_id IS NOT` the `launch_id` of the stream's latest `launch_reserved` (the thread's current launch), or `NEW.tr_path IS NOT` the `transcript_path` of that launch's latest `session_bound` (none or a `null` path refuses) |
| `events_transcript_line` | `BEFORE INSERT WHEN NEW.src_kind = 'transcript' AND NEW.kind <> 'transcript_generation'` | a line whose `tr_generation`, `tr_path`, or `tr_file_id` `IS NOT` the stream's latest `transcript_generation`; whose `launch_id IS NOT` the stream's latest `launch_reserved`'s, or whose `tr_path IS NOT` the `transcript_path` of that launch's latest `session_bound`; or whose `tr_offset_end` is not positive (the generation marker sits at 0, so the first line ends past it) or not greater than every earlier `tr_offset_end` of that generation |
| `events_blob_refs` | `AFTER INSERT` | an event with a blob reference at a typed path (table below) that has neither a `blob_refs` nor (v4) a `large_blob_refs` row `(NEW.stream_id, sha256, media_type)`; the store inserts the refs before the event row (§2.3 step 3) |

Typed `Content` paths (from `core::payload`; the walker in `content.rs` matches every `TypedEvent` variant without a wildcard, so a new kind with a `Content` field does not compile until it is listed):

| Kind | Paths |
|---|---|
| `user_message` | `$.prompt` |
| `assistant_message`, `input_created` | `$.text` |
| `tool_activity` | `$.input`, `$.output` |
| `approval_requested` | `$.tool_input` |
| `mailbox_message` | `$.body` |
| `vt_snapshot_taken` | `$.grid` |
| `raw_unknown` | `$.raw` |
| `surface_update` | `.grid` of each `vt_snapshot` component reached through `$.components[i]` and `.items[j]` of `list` components only; nothing under `button.action.args`, which is free JSON |

The roles are split. The typed Rust walker establishes which values are references (it follows `items` only of `list` components and reads `grid` only of `vt_snapshot` components) and writes their refs. The trigger checks that each reference it can see has a ref: the fixed paths with `json_extract`, and for `surface_update` every `json_tree(NEW.payload, '$.components')` node with key `grid` and an object value whose parent is an object with `type` `vt_snapshot`, excluding any node under an `args` value, object or array (`fullkey NOT GLOB '*.action.args*'`). The trigger does not follow the list-parent chain, although a recursive CTE in a subquery could (S16): keeping the membership rule in one typed walker is simpler, and the trigger's set exceeds the walker's only through objects shaped like a `vt_snapshot` component inside `args`, which it excludes, since no other typed field of a component holds an object. A `{"blob": …}` object anywhere else in a payload is plain data.

```sql
CREATE TABLE blobs (                                -- the body, shared by SHA-256; no media type
  sha256     TEXT PRIMARY KEY CHECK (length(sha256) = 64 AND sha256 NOT GLOB '*[^0-9a-f]*'),
  size       INTEGER NOT NULL CHECK (size = length(data) AND size > 65536 AND size <= 67108864),
  data       BLOB NOT NULL
) STRICT;
CREATE TABLE blob_refs (                            -- one row per stream and declared media type
  stream_id  TEXT NOT NULL, sha256 TEXT NOT NULL REFERENCES blobs (sha256),
  media_type TEXT NOT NULL CHECK (length(media_type) > 0),
  PRIMARY KEY (stream_id, sha256, media_type)
) STRICT, WITHOUT ROWID;

-- v4: a value above MAX_BLOB_BYTES, as ordered parts under its whole content's SHA-256
CREATE TABLE large_blobs (
  sha256     TEXT PRIMARY KEY CHECK (length(sha256) = 64 AND sha256 NOT GLOB '*[^0-9a-f]*'),
  size       INTEGER NOT NULL CHECK (size > 67108864 AND size <= 9007199254740991),  -- disjoint from blobs
  part_count INTEGER NOT NULL CHECK (part_count >= 2)
) STRICT;
CREATE TABLE large_blob_parts (                     -- PART_BYTES = 4 MiB; part 0 holds the 1027 bytes the preview reads
  sha256     TEXT NOT NULL REFERENCES large_blobs (sha256),
  part_index INTEGER NOT NULL CHECK (part_index >= 0),
  data       BLOB NOT NULL CHECK (length(data) > 0 AND length(data) <= 4194304
                                  AND (part_index > 0 OR length(data) >= 1027)),
  PRIMARY KEY (sha256, part_index)
) STRICT;
CREATE INDEX large_blob_parts_no_blob_write ON large_blob_parts (data) WHERE 0;  -- empty; sqlite3_blob_open, which bypasses UPDATE triggers, cannot open an indexed column for writing
CREATE TABLE large_blob_refs (                      -- as blob_refs
  stream_id  TEXT NOT NULL, sha256 TEXT NOT NULL REFERENCES large_blobs (sha256),
  media_type TEXT NOT NULL CHECK (length(media_type) > 0),
  PRIMARY KEY (stream_id, sha256, media_type)
) STRICT, WITHOUT ROWID;
CREATE INDEX large_blob_refs_sha256 ON large_blob_refs (sha256);

CREATE TABLE command_receipts (
  command_id     TEXT PRIMARY KEY,
  token_id       TEXT NOT NULL,
  request_sha256 BLOB NOT NULL CHECK (length(request_sha256) = 32),
  status         TEXT NOT NULL CHECK (status IN ('in_progress', 'completed', 'unknown_after_crash')),
  http_status    INTEGER, body TEXT,
  created_at     TEXT NOT NULL, finished_at TEXT,
  CHECK ((status = 'completed' AND http_status IS NOT NULL AND body IS NOT NULL AND finished_at IS NOT NULL)
      OR (status = 'unknown_after_crash' AND http_status IS NULL AND body IS NULL AND finished_at IS NOT NULL)
      OR (status = 'in_progress' AND http_status IS NULL AND body IS NULL AND finished_at IS NULL))
) STRICT;

CREATE TABLE hook_deliveries (
  launch_id    TEXT NOT NULL, emit_id TEXT NOT NULL,
  receipt_ordinal INTEGER NOT NULL CHECK (receipt_ordinal >= 1),   -- PR 9's intake order
  commit_ordinal  INTEGER NOT NULL CHECK (commit_ordinal >= 1),    -- record-job commit order
  reply_status INTEGER NOT NULL, reply_body TEXT NOT NULL,
  routing      TEXT NOT NULL CHECK (routing IN ('appended', 'buffered', 'raw_unknown')),
  stream_id    TEXT, seq INTEGER,
  pending_body BLOB, pending_meta TEXT CHECK (pending_meta IS NULL OR json_valid(pending_meta)),
  received_at  TEXT NOT NULL,
  PRIMARY KEY (launch_id, emit_id),
  UNIQUE (launch_id, receipt_ordinal),
  UNIQUE (launch_id, commit_ordinal),
  FOREIGN KEY (stream_id, seq) REFERENCES events (stream_id, seq),
  CHECK ((routing = 'buffered' AND stream_id IS NULL AND seq IS NULL AND pending_body IS NOT NULL AND pending_meta IS NOT NULL)
      OR (routing <> 'buffered' AND stream_id IS NOT NULL AND seq IS NOT NULL AND pending_body IS NULL AND pending_meta IS NULL))
) STRICT, WITHOUT ROWID;

CREATE TABLE tokens (
  token_id   TEXT PRIMARY KEY,
  kind       TEXT NOT NULL CHECK (kind IN ('owner', 'agent', 'rescue', 'hook', 'extension')),
  sha256     BLOB NOT NULL UNIQUE CHECK (length(sha256) = 32),
  thread_id  TEXT, launch_id TEXT,
  created_at TEXT NOT NULL, revoked_at TEXT
) STRICT;
```

Immutability triggers on the other tables (all `RAISE(ROLLBACK)`):

| Table | Update | Delete |
|---|---|---|
| `meta` | refused for `epoch` and `created_at`; only `projection_version` may change | refused |
| `blobs`, `blob_refs`, `large_blobs`, `large_blob_parts`, `large_blob_refs` | refused | refused |
| `large_blob_refs` insert | `BEFORE INSERT` (`large_blob_refs_complete`): the blob's parts are not complete, that is `(count, min(part_index), max(part_index), sum(length(data)))` `IS NOT (part_count, 0, part_count - 1, size)` (or there is no `large_blobs` row) | — |
| `large_blob_parts` insert | `BEFORE INSERT` (`large_blob_parts_sealed`): the blob already has a ref, so no part is added after a referenced blob committed | — |
| `command_receipts` | only `status` from `in_progress` to `completed` or `unknown_after_crash`, with the columns the `CHECK` requires; `command_id`, `token_id`, `request_sha256`, `created_at` unchanged | refused |
| `hook_deliveries` | only `routing` from `buffered` to `appended` or `raw_unknown`, setting `stream_id`, `seq` and clearing the pending columns; key, both ordinals, reply, `received_at` unchanged | refused |
| `tokens` | only `revoked_at` from `NULL`; every other column unchanged | refused |
| `hook_deliveries` insert | `BEFORE INSERT`: `NEW.commit_ordinal IS NOT` the launch's last commit ordinal + 1 | — |

With `recursive_triggers=ON` the delete triggers also refuse `REPLACE` on these tables. Nothing in the MVP deletes from them (§8 D9).

**Projection tables** (prefix `proj_`; rebuild and its comparison enumerate them from `sqlite_schema`, so a new projection table cannot be left out):

| Table | Key | Columns (from mvp §2.1 "Projections") |
|---|---|---|
| `proj_threads` | `thread_id` | `agent`, `cwd`, `workspace_id`, `parent_thread_id`, `role`, `closed`, `process_state`, `turn_state`, `task_state`, `attention` (sorted JSON array), `current_launch_id`, `session_id`, `transcript_path`, `registered_seq` |
| `proj_launches` | `launch_id` | `thread_id` (FK), `reserved_seq`, `expected_previous`, `agent`, `argv_digest`, `manual`, `worktree_id`, `pid`, `start_time`, `pgid_or_job`, `cli_version`, `exit_code`, `exit_signal`, `exit_reason`, `death_confirmed`, `death_method`, `session_id`; gate evidence: `readiness_seq`, `readiness_evidence`, `write_basis` (the annex JSON), `last_write_seq`, `last_human_write_seq`, `last_turn_start_seq` |
| `proj_inputs` | `input_id` | `thread_id`, `launch_id`, `state`, `text` (`Content` JSON), `encoding`, `origin`, `created_by`, `sent_after_seq`, `ack_seq`, `created_seq`, `attempt_seq` (v3; the seq of its latest move to `sending`), `last_write_before` (v3; its launch's `last_write_seq` just before that move) |
| `proj_approvals` | `approval_id` | `thread_id`, `launch_id`, `requested` (payload JSON), `opened_seq`, `outcome`, `resolved_seq` |
| `proj_surfaces` | `(stream_id, surface_id)` | `scope_kind`, `scope_id`, `payload` (the update's JSON), `updated_seq` |
| `proj_worktrees` | `worktree_id` | `record` (`WorktreeChanged` JSON), `state`, `owner_thread_id`, `workspace_id`, `updated_seq` |
| `proj_transcript_cursors` | `thread_id` | `generation`, `path`, `file_id`, `event_offset`, `event_continuity`, `read_offset`, `read_continuity`; `CHECK (read_offset >= event_offset)` |

State columns carry `CHECK (x IN (…))` with the core enum values; flags `CHECK (x IN (0, 1))`; foreign keys tie launches, inputs, approvals, and cursors to `proj_threads`. Projection tables have no immutability triggers: rebuild clears and rewrites them.

**Migrations and projection versions.** `schema::MIGRATIONS` is an ordered list of SQL scripts; v1 is the schema above, v2 adds the trigger `events_session_bound_once` (PR 9, hook-ingest §2.12): a `session_bound` needs a non-`NULL` launch that is the stream's latest `launch_reserved`, with no earlier `session_bound` of that launch in the stream, and v3 adds `proj_inputs.attempt_seq` and `last_write_before` (PR 10b, `docs/design/supervisor-send.md` §2.9 E1) with `PROJECTION_VERSION` 2, so a v2 database rebuilds its projections on open, and v4 adds `large_blobs`, `large_blob_parts`, `large_blob_refs`, their triggers, and an `events_blob_refs` recreated to accept either ref table (C1c1, `docs/design/c1-claude-adapter.md` D14; no table is rebuilt, and the new tables are not projections, so `PROJECTION_VERSION` stays and rebuild leaves them alone), and v5 adds the index `proj_inputs_launch_attempt` on `proj_inputs (launch_id, attempt_seq)`, which the attach interval reads in every attach `human_write` job (`docs/design/t1-attach.md` D11; rebuild clears rows, not indexes, so `PROJECTION_VERSION` stays). `PROJECTION_VERSION` 3 has a `user_message` carrying the `Prompt { superseded: true }` annex leave its launch's `last_turn_start_seq` (`docs/design/supervisor-send.md` §2.4 "Turn boundaries in receipt order"); no event stored by an earlier version carries that annex, so the rebuild on open gives the same rows. Each script runs in its own `BEGIN IMMEDIATE` transaction that also sets `PRAGMA user_version` (S8), so a crash leaves the previous version. Any physical DDL change, projection tables included, is a migration. A shipped migration never changes, and neither does its fingerprint in `schema::FINGERPRINTS`: every physical change is a new version. `meta.projection_version` covers only a change in how `apply` interprets events on the same DDL; `open` rebuilds when the stored value differs from the code's, in either direction (a downgrade rebuilds too). Because two processes of different versions can have the file open (PR 8's lock covers one daemon, not a doctor or an older binary), every write job reads `meta.projection_version` after taking the lock and refuses when it differs from its own (§2.2 step 4): once one version rebuilt, a process with another `apply` cannot write.

### 2.5 `Content` and Blobs

```rust
pub struct Prepared { pub content: Content, pub blob: Option<BlobInput> }
pub fn prepare_content(bytes: Vec<u8>, media_type: &str) -> Result<Prepared, ContentError>;  // ContentError::TooLarge
pub const MAX_BLOB_BYTES: usize = 64 * 1024 * 1024;

impl Store { pub fn blob_spool(&self) -> io::Result<BlobSpool>; }   // its file in the state directory
impl BlobSpool {                                                    // also io::Write
    pub fn append(&mut self, bytes: &[u8]) -> io::Result<()>;
    pub fn finish(self, media_type: &str) -> io::Result<Prepared>;  // Bytes or inline at or below MAX_BLOB_BYTES, Spooled above
}
```

- **Inline** when `bytes.len() <= core::payload::INLINE_LIMIT_BYTES` (65 536): `encoding: utf8` with the text when the bytes are valid UTF-8, else `encoding: base64` (standard alphabet, padded). The limit applies to the content's bytes, not the encoded form.
- **Blob** above it: `sha256` = lowercase hex SHA-256 of the bytes, `size`, `media_type`, `preview`, `preview_lossy`.
- **Preview** (mvp §2.1 as clarified by this PR). Decode `bytes[..min(len, 1024)]` as UTF-8, replacing each invalid sequence with U+FFFD and setting `preview_lossy`. If the window ends inside a sequence that is incomplete only because of the window (`Utf8Error::error_len() == None` at the end), look at up to 3 bytes past byte 1024: if they complete a valid scalar, that scalar is left out of the preview and does not set `preview_lossy`; if they do not (or the content ends), the partial sequence is invalid, is replaced with U+FFFD, and sets `preview_lossy`. A U+FFFD present in the content itself is valid UTF-8 and does not set it. Reason: the flag tells a reader that the preview misrepresents the content; a character cut only by the window misrepresents nothing, and Japanese text (AGENTS.md invariant 4) would otherwise be flagged lossy on most large blobs.
- **Store boundary** (`append`, `append_transcript`, promotion). For every typed `Content` field of the event (the walker of §2.4): an `Inline` value must be at most 65 536 bytes (for `base64`, after decoding; the decoded bytes must be valid base64 and must not be valid UTF-8, so each content has one form); a `Blob` value must match the bytes it will name, with equal `sha256`, `size`, preview, and `preview_lossy` (recomputed from those bytes), and a non-empty `media_type`: the blob already stored under its hash when there is one (in `blobs`, or `large_blobs` with the preview from part 0; an input of that hash is then not inserted), else the first of this event's `BlobInput`s of that hash, the one step 3 inserts. Any mismatch is `ContentError`. SHA-256 of a stored blob is computed by the store from the bytes (§2.3 step 3); this rule rests on Rust, because SQLite has no built-in hash function (§2.1). `BlobInput`s are matched by hash (the store's own for in-memory bytes, the producer's for a spooled input, which step 3 recomputes): several inputs of one hash (two separately prepared references, under different media types) are all used by any reference to that hash, and only the first is checked and inserted. Every `BlobInput` must be referenced by the event; an unreferenced one is refused.
- **Size bound.** A blob has no size cap beyond `MAX_SAFE_INTEGER` bytes (`BlobRef.size` on the wire). `MAX_BLOB_BYTES` (64 MiB) bounds a value held in memory and one `blobs` row: above it `prepare_content` returns `TooLarge`, and a producer of a value of unknown or larger size spools it instead. The bound and `size > 65 536` are `CHECK`s on `blobs`; `size > 67 108 864` and `size <= 2^53 − 1` are `CHECK`s on `large_blobs`, so a hash is in at most one of the two tables.
- **Spooling** (D14). `Store::blob_spool` creates a temporary file the producer owns in the state directory (never `TMPDIR`); `append` fills a 64 KiB buffer and writes it to the file, computing the whole SHA-256, the size, and the first 1027 bytes for the preview as it goes, so the producer never holds the value in memory. `finish` returns, at or below `MAX_BLOB_BYTES`, what `prepare_content` returns for the bytes read back; above it a `Content::Blob` with that hash, size, and preview and a `BlobInput::Spooled` that owns the file handle. The file is closed and then removed (closing first, for Windows) when the last owner drops: after the job's append, whether it commits or rolls back, or with a job that never runs, before its reply. The file is created owner-only like the state directory's other files (`host::create_private_file`: mode `0600`, on Windows a protected DACL). A spool file left by a process crash stays in the state directory; nothing removes it.
- **Storage** in `blobs` (at most `MAX_BLOB_BYTES`) or as `large_blob_parts` (above it, `PART_BYTES` = 4 MiB each, in order), inside the event's transaction: the blob, the event, and its refs commit together or not at all, so the database never holds a half-written blob. The only file outside the database is a producer's spool, which is temporary input, never stored content. Completeness of a large blob is enforced in SQL as well as in Rust: a ref is refused for a blob whose parts are incomplete, and a referenced blob takes no more parts (§2.4).
- **Refs.** The store writes `(stream_id, sha256, media_type)` for each typed blob reference, with the type its `Content` declares, using `INSERT … ON CONFLICT DO NOTHING`: a repeat in the same event or a later event of the stream is skipped, and the foreign key still refuses a missing blob (S11). `events_blob_refs` refuses an event whose typed reference has no ref, and only typed paths count, so a `{"blob": …}` object in `SurfaceAction.args` neither aborts an event nor grants a stream read access to a blob.
- **Media type.** The body is shared by SHA-256; the media type belongs to each reference. Every event keeps the type its `Content` declared (in its payload and in its `blob_refs` row), and no event is ever refused because the same bytes were stored under another type: a 70 KiB JSON document can be a `text/plain; charset=utf-8` prompt in one thread and an `application/json` tool input in another, and both events and the full bytes are kept. The download type for `(stream_id, sha256)` is the type of that stream's refs when they all agree, else `application/octet-stream`; it does not depend on insertion order, and PR 7 then applies mvp §2.5's serving rule (only `text/plain; charset=utf-8` and `application/json` are served as stored).
- **Deletion.** None in the MVP, for either table: events are immutable, so every reference is permanent, and no unreferenced blob can be stored. There is no blob garbage collection.
- **Reads.** `ReadTx::blob(stream_id, sha256)` returns the whole bytes and the download type above only when the stream has a ref to `sha256` in either ref table (a large blob is read whole into memory). `ReadTx::blob_ref_exists(stream_id, sha256)` answers the same ref check without loading the bytes; PR 7 calls it before admitting a blob download. `ReadTx::blob_download` returns what a download serves: a `blobs` blob's bytes, or a large blob's download type, size, and part count; `ReadTx::large_blob_part(sha256, index)` reads one part, so a download reads each part in its own short read transaction.

### 2.6 Projections, Annex, and Rebuild

```rust
pub(crate) struct ProjectionTx<'a> { /* the transaction, restricted to proj_* statements */ }
pub(crate) fn apply(p: &mut ProjectionTx<'_>, e: &StoredEvent) -> Result<(), ProjectionError>;
pub(crate) struct StoredEvent { pub envelope: EventEnvelope, pub occurred_explicit: bool, pub annex_text: Option<String> } // annex() decodes the annex
```

`apply` decodes the envelope with `TypedEvent::decode` and changes projection rows from the event alone: payload, envelope, `seq`, and annex. It reads only `proj_*` rows and has no clock, configuration, or access to receipts, deliveries, tokens, or blobs; `ProjectionTx` exposes only statements on `proj_*` tables. A `GenericEvent` changes nothing. The mapping follows mvp §2.1:

| Event | Change |
|---|---|
| `thread_registered` | insert `proj_threads` with `ThreadState::registered(task.is_some())` and the payload; error if it exists |
| `thread_closed` | `closed = 1` |
| `launch_reserved` | insert `proj_launches`; `proj_threads.current_launch_id` = it |
| `launch_started`, `launch_exited`, `launch_death_confirmed` | the launch's process columns; error if the launch is missing or of another thread |
| `session_bound` | launch `session_id`, `last_turn_start_seq = seq`; thread `session_id`, `transcript_path` |
| `transcript_generation` | cursor row = `(generation, path, file_id)`, `event_offset = read_offset = 0`, both continuities from the annex |
| other transcript-sourced events | `event_offset = offset_end`, `event_continuity` from the annex, `read_offset = max(read_offset, offset_end)` |
| `readiness_observed` | launch `readiness_seq = seq`, `readiness_evidence`, `write_basis` from the annex |
| `human_write` | launch `last_write_seq = seq`; also `last_human_write_seq = seq` for `attach` and `key` |
| `user_message` | with an envelope `launch_id`: that launch's `last_turn_start_seq = seq`, except with the annex `Prompt { superseded: true }` (a start that committed after a later turn boundary), which leaves it; without one (a hook that bound no launch cannot produce it, but a stored event may lack it), no launch change; with `input_id`, that input's `ack_seq = seq`, only when the input belongs to the thread and to the envelope's launch (mvp §2.3: an acknowledgement is a `UserPromptSubmit` of the same launch); otherwise, or without an envelope launch, the job rolls back. Matching the prompt to its input stays the producer's |
| `status_changed` (`process_state`, `turn_state`, `attention`) | the column set to `to` by `UPDATE … WHERE <column> = from` (one row, else error); the only event that changes these after registration |
| `task_set` | `task_state = to` |
| `status_changed` (`task_state`) | none: it stays in the public history as the record of its `task_set` |
| `approval_requested` / `approval_resolved` | insert the card / set `outcome`, `resolved_seq` (only from `outcome IS NULL`) |
| `input_created` | insert the input with `state = initial_state`, `launch_id` from the payload (may be null) |
| `input_state_changed` | `state = to` by `UPDATE … WHERE state = from` (one row, else error); `sent_after_seq` when set; for `to = sending`: the input's `launch_id = COALESCE(launch_id, envelope launch_id)` (error if both are null), its `attempt_seq = seq` and `last_write_before` = that launch's `last_write_seq`, and then that launch's `last_write_seq = seq`; for `sending → cancelled` (a zero-byte attempt): the launch's `last_write_seq = last_write_before` when it still equals the input's `attempt_seq` |
| `surface_update` | upsert the surface of the event's stream; empty `components` deletes it (scope and stream agree by `CHECK`, §2.4). A surface's identity is `(stream_id, surface_id)`, so an update on one stream never changes or removes another stream's surface of the same id, and every change a snapshot shows is an event of that snapshot's stream |
| `worktree_changed` | upsert the record |
| `mailbox_message`, `assistant_message`, `tool_activity`, `vt_snapshot_taken`, `raw_unknown`, `launch_evidence` | none (read from `events` by kind; `launch_evidence` is an observation that changes no state, `docs/design/supervisor-recovery.md` §2.4) |

**Who changes an axis.** `process_state`, `turn_state`, and attention have no event of their own: their `status_changed` is the only record of a change, so it is what `apply` applies. `task_state` has an explicit cause, `task_set` (mvp §2.2, CL-2: after registration it changes only on `task_set`), so `task_set` is the only event `apply` uses to change it. The `status_changed(TaskState)` that `core::state::reduce` emits for `StateInput::TaskSet` (`crates/core/src/state.rs`, the `TaskSet` arm and `diff`) is a derived record: `apply` does nothing for it. `append` checks it at write time, after the idempotency step, so an equal retry of a stored one returns `Existing` first: a new `status_changed(TaskState)` must follow, in the same job, a `task_set` of the same thread whose `to` equals its `to`, and its `from` must equal the thread's `task_state` before that `task_set` (`WriteTx` records both when it appends the `task_set`); otherwise `TaskChangeWithoutCause`. A `task_set` to the same state emits no `status_changed` and needs none. This rule is Rust-only: a raw insert of a standalone `status_changed(TaskState)` changes no projection, so it cannot change the task either. Compare-and-set rows turn a lost update into an aborted job: two jobs that both read `queued` cannot both record `queued → sending`.

**Annex.** Internal values that projections need but no public payload carries. Stored in `events.annex`, immutable with the event, never served, passed to `apply`, so rebuild reproduces them.

```rust
#[derive(Serialize, Deserialize)] #[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum EventAnnex {
    Readiness(WriteBasis),                               // required on readiness_observed (CHECK in §2.4)
    Transcript(Continuity),                              // on every transcript-sourced event
    Hook { truncated: bool },                            // only on a hook-sourced raw_unknown (hook-ingest §2.12)
    Prompt { superseded: bool },                         // only on a hook-sourced user_message (supervisor-send §2.4)
}
#[derive(Serialize, Deserialize)] #[serde(deny_unknown_fields)]
pub struct WriteBasis { pub launch_id: LaunchId, pub position: u64, pub counted: CountedClasses }
/// A set of `CountedClass`, serialized as a JSON array in the fixed order input, human, reply,
/// without repeats; deserializing any other array (unknown class, other order, repeat) fails.
pub struct CountedClasses(u8);
pub enum CountedClass { Input, Human, Reply }              // mirrors pty_runtime::origin::OriginClass
```

The persisted JSON, exactly (serde's externally tagged form of the tuple variants; `annex.rs` has a golden test for each):

```json
{"readiness":{"launch_id":"0190c6f2-0000-7000-8000-000000000001","position":42,"counted":["input","human","reply"]}}
{"transcript":"sha256:<64 lowercase hex digits>"}
{"hook":{"truncated":true}}
{"prompt":{"superseded":true}}
```

The `CHECK` of §2.4 reads these paths: `$.readiness.launch_id`, a non-negative integer `$.readiness.position`, and `$.readiness.counted` of JSON type array (a string spelling an array is refused) and equal to one of the eight canonical arrays, so a set has one spelling and two equal sets compare equal for idempotency.

`WriteBasis` is the PTY writer's `Basis { seq, counted }` captured at the evidence's intake (S15): `position` is `last_started(counted)`, and `counted` is stored with it in `ClassSet::iter()`'s order (`crates/pty-runtime/src/origin.rs`), so the gate checks the same origin set the basis was computed over even if the adapter policy changes later. `launch_id` must equal the event's launch. Transcript continuity keeps two pieces of evidence apart: the event position's (`proj_transcript_cursors.event_continuity`, from the annex of the last transcript event, restored by rebuild) and the read position's (`read_continuity`, from `ReadPos`, kept in the projection across restarts and replaced by the event position's evidence on rebuild, which is the permitted rewind). The store treats both as opaque text. For Claude (`docs/design/c1-claude-adapter.md` D10) each is `sha256:` and the lowercase hex SHA-256 of the transcript's bytes `[0, offset)` at that position: a `transcript_generation` carries the digest of no bytes, a line the digest up to its `offset_end`, `ReadPos` the digest up to the read offset. The tailer verifies the stored read continuity against the file at every open (mvp §2.1 "Transcript cursor rules").

**Rebuild.** `rebuild_projections` runs in the job's transaction: `DELETE` from every `proj_*` table (children first), read `events` ordered by `global_seq`, call `apply` on each, and write the code's `projection_version` to `meta`. Equality with the maintained projections holds by construction: the same function on the same events in the same order, where `global_seq` is the commit order (one write transaction at a time, S5, and `events_global_order`). The tests check it instead of assuming it (§7).

**Comparison** (`compare_projections(maintained, rebuilt) -> ProjectionDiff`, used by `verify_projections` and the tests). Every column of every `proj_*` row must be equal, except `proj_transcript_cursors.read_offset` and `read_continuity`, where it requires the rebuilt `read_offset = event_offset` and `read_continuity = event_continuity` exactly, with `event_offset` and `event_continuity` equal between the two and the maintained `read_offset ≥ event_offset`. This is mvp §2.1's only permitted rewind: the rebuilt cursor reads from the end of the last stored event, so the tailer re-reads only the suffix after it (bytes that produced no event, such as a partial line or skipped records); every stored line lies before the rebuilt `read_offset`, and a batch resubmitting one is refused (`BadBatch`, lines start at the read offset) rather than deduplicated.

`verify_projections` dumps every `proj_*` table, rebuilds inside a `SAVEPOINT`, dumps again, rolls back to the savepoint, and returns `compare_projections`. Both hold the write lock for a full replay; they run at open after a projection version change, or on demand (PR 21, deferred).

### 2.7 Read Operations (`ReadTx`)

All in one read transaction (or the write transaction through `WriteTx::read`).

```rust
pub struct Page { pub events: Vec<EventEnvelope>, pub has_more: bool, pub has_more_before: bool }

impl ReadTx<'_> {
    pub fn stream_exists(&self, s: StreamId) -> Result<bool, StoreError>;          // host: always; thread: registered
    pub fn head(&self, s: StreamId, after_seq: u64) -> Result<StreamHead, StoreError>;  // core::sync::StreamHead
    pub fn heads(&self, s: &[StreamId]) -> Result<Vec<StreamHeadEntry>, StoreError>;
    pub fn events_after(&self, s: StreamId, after_seq: u64, limit: u32) -> Result<Page, StoreError>;
    pub fn events_before(&self, s: StreamId, before_seq: u64, limit: u32) -> Result<Page, StoreError>;
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
    pub fn surface(&self, stream: StreamId, s: &SurfaceId) -> Result<Option<SurfaceRow>, StoreError>;
    pub fn worktree(&self, w: &WorktreeId) -> Result<Option<WorktreeRow>, StoreError>;
    pub fn worktrees(&self) -> Result<Vec<WorktreeRow>, StoreError>;
    pub fn transcript_cursor(&self, t: ThreadId) -> Result<Option<TranscriptCursor>, StoreError>;
    pub fn blob(&self, s: StreamId, sha256: &str) -> Result<Option<BlobBody>, StoreError>;
    pub fn blob_ref_exists(&self, s: StreamId, sha256: &str) -> Result<bool, StoreError>;
    pub fn blob_download(&self, s: StreamId, sha256: &str) -> Result<Option<BlobDownload>, StoreError>;  // Whole(BlobBody) | Parts(LargeBlob { media_type, size, part_count })
    pub fn large_blob_part(&self, sha256: &str, index: u64) -> Result<Option<Vec<u8>>, StoreError>;
    pub fn receipt(&self, c: CommandId) -> Result<Option<CommandReceipt>, StoreError>;   // core::command::CommandReceipt
    pub fn token_by_hash(&self, sha256: &[u8; 32]) -> Result<Option<TokenRow>, StoreError>;
    pub fn hook_delivery(&self, l: LaunchId, emit: Uuid) -> Result<Option<HookDelivery>, StoreError>;
    pub fn buffered_deliveries(&self, l: LaunchId) -> Result<Vec<BufferedDelivery>, StoreError>;  // receipt order
    pub fn buffered_launches(&self) -> Result<Vec<LaunchId>, StoreError>;
    pub fn buffered_summary(&self, l: LaunchId) -> Result<BufferedSummary, StoreError>;  // count, body bytes, MIN(received_at)
    pub fn max_receipt_ordinal(&self, l: LaunchId) -> Result<u64, StoreError>;           // 0 when none
    pub fn binding_receipt_ordinal(&self, t: ThreadId, l: LaunchId) -> Result<Option<u64>, StoreError>;  // the delivery recorded with the launch's latest session_bound in t's stream
    pub fn hook_emit_in_thread(&self, t: ThreadId, emit: Uuid) -> Result<bool, StoreError>;  // a delivery of any launch of `t` took `emit`
    pub fn schema_info(&self) -> Result<SchemaInfo, StoreError>;   // application_id, user_version, projection_version, SQLite version
    pub fn integrity_check(&self, max_errors: u32) -> Result<Vec<String>, StoreError>;  // PRAGMA integrity_check(N); empty = ok
}
```

- **Pages** match `core::sync::EventsResponse::Replay`. `events_after` returns events with `seq > after_seq` ascending, at most `limit`; `has_more` = an event beyond the page exists; `has_more_before` = `after_seq ≥ 1` and the stream has an event with `seq ≤ after_seq` (the page does not start at the stream's first event). `events_before` returns the nearest `limit` events with `seq < before_seq`, ascending; `has_more_before` = an older event exists; `has_more` = an event with `seq ≥ before_seq` exists. A cursor above `i64::MAX` (SQLite's integer range) is clamped to `i64::MAX` before binding, so `before_seq = u64::MAX` returns the latest page; `after_seq` above the head is a snapshot by `core::sync::decide_replay` before the store is asked. Events whose kind is unknown, whose `schema_version` is newer, or whose payload fails its schema are returned like any other; decoding them into `GenericEvent` is the reader's (mvp §2.1).
- `head` reads the stream's last row (`head_seq`, `cum_bytes_at_head`) and the row at `after_seq` (`cum_bytes_at_after`, 0 for `after_seq` 0 or above the head) in the caller's transaction; `decide_replay` takes it unchanged.
- **Snapshots** assemble `ThreadSnapshot` and `HostSnapshot` from projection rows and `events` and return the stream head as `as_of_seq`, all in one transaction. The snapshot's inputs (the non-terminal ones) and `inputs(t, states)` are selected by state in SQL, so the work does not grow with a thread's finished inputs. `ThreadSnapshot.worktree` is the worktree of the current launch's `worktree_id` (none when the launch has none); worktrees a thread owns through ownership transfer are in the host snapshot's list. `recent_events` are the last `SNAPSHOT_RECENT_EVENTS` events in ascending `seq`. `vt_snapshot` is the latest `vt_snapshot_taken` while an attention cause is active or a card is open.

### 2.8 Open, Pragmas, Epoch

**Connection setup** (writer and readers, before any statement that reads the database): on Windows the `win32-longpath` VFS through `open_with_flags_and_vfs` (S13); `busy_timeout` first, then `foreign_keys=ON`, `recursive_triggers=ON`, `trusted_schema=OFF`, `SQLITE_DBCONFIG_DEFENSIVE` (S12), `synchronous=FULL`, `fullfsync=ON`, `checkpoint_fullfsync=ON` (macOS `F_FULLFSYNC`; ignored elsewhere), `journal_size_limit=67108864`; readers add `query_only=ON`. None of these writes the file. `open` reads each back and refuses a connection where one did not take effect (`OpenError::Setup`).

**Open sequence.**

0. **Configuration.** Each `StoreConfig` range above is checked before the path is touched; a value outside it is `OpenError::Config`, never a panic.
1. **Create if missing.** If the path does not exist, create it with `create_new` (mode `0o600` on Unix; S13 gives `-wal` and `-shm` the same mode); if another opener created it first, continue with that file. `open` never creates directories.
2. **Writer connection** with the setup above (no `SQLITE_OPEN_CREATE`).
3. **Read-only checks** in a read transaction: a file that is not a database is `NotADatabase`; a text encoding other than UTF-8 is `Encoding` (SQLite fixes it at creation, and the `payload_bytes = octet_length(payload)` check needs the UTF-8 byte count the store binds; an empty file reports UTF-8, the encoding it is created with); `application_id` = `0x4348_4C44` ("CHLD") is ours, `0` with no schema objects is fresh, anything else (another id, or `0` with objects) is `Foreign`; for ours, `user_version` above the newest migration is `NewerSchema { found, supported }`, and the schema (`type`, `name`, `tbl_name`, `sql` of every `sqlite_schema` row except SQLite's internal objects, selected by the literal prefix `lower(substr(name, 1, 7)) = 'sqlite_'`, sorted; `LIKE 'sqlite_%'` is not used, since its `_` matches any character and would hide a table such as `sqliteXextra`. SQLite refuses user objects whose name starts with `sqlite_` in any case, so the prefix selects internal objects only) must equal the fingerprint the code ships for that version, else `SchemaMismatch { names }`. Every check of an existing database happens here, before any change.
4. **`journal_mode=WAL`**, which must return `wal` (else `NoWal`, e.g. a filesystem without shared memory). For a rollback-journal database this is the first write (S8).
5. **Initialize or migrate** under `BEGIN IMMEDIATE`, re-reading `application_id`, `user_version`, and the schema count inside the lock (another process may have initialized the file since step 3): fresh → set `application_id`, run v1, insert `meta` `epoch` (from `IdSource::epoch`), `created_at`, `projection_version`, set `user_version = 1`, commit; ours and older → each missing migration in its own transaction, then the fingerprint check again for the new version.
6. **Projections.** If `meta.projection_version` differs from the code's, run `rebuild_projections` in one transaction.
7. Open the readers and start the writer thread.

**What `open` promises.** Every `OpenError` from steps 1–3 is returned before Chaldea changes the file. SQLite itself may still write on open, independently of Chaldea: it rolls back a hot rollback journal left by a crashed writer on first read, and it creates or recovers `-wal` and `-shm` for a database already in WAL mode. `open` never deletes a file, including one it created in step 1: between its create and its failure another opener may have initialized the same file and committed to it, and on macOS an unlink succeeds even while that opener holds it. A failed open can therefore leave an empty or uninitialized file; the next `open` classifies it as fresh in step 3 and initializes it under the lock in step 5, which re-checks that nobody initialized it meanwhile. The store never renames, repairs, or recreates a database. The epoch is written only in step 5 for a fresh file, so it is created once per database; its row is immutable by trigger.

Corruption found later (`SQLITE_CORRUPT`) puts the writer in the failed state (§2.2); `integrity_check` is offered to the doctor (PR 21, deferred), not run at open, since it reads the whole file.

### 2.9 Durability

| Point | What holds | Guaranteed by |
|---|---|---|
| A ticket resolves `Ok` | The job's transaction is in the synced WAL: it survives a process crash, an OS crash, and power loss on hardware that honors the flush (`F_FULLFSYNC` on macOS, `FlushFileBuffers` on Windows) | `synchronous=FULL` + `fullfsync` (S6) |
| A process crash during a job, before `COMMIT` began | No part of the job is visible after restart | WAL recovery ignores frames without a commit frame (S6) |
| A process crash during `COMMIT` | All or none of the job is visible after restart, never part | S6 (a D-fact; not timed in CI) |
| A process crash after `COMMIT` returned, before the reply | The whole job is visible after restart | S6 |
| `CommitUnknown` | Unknown to this process; after restart the database shows it fully or not at all, and the writer is retired | S6; for a command, the receipt after restart is absent, `completed`, or `in_progress`, and only `in_progress` becomes `unknown_after_crash` (§2.2) |
| A refused statement or a poisoned job | The whole job rolls back | `RAISE(ROLLBACK)` (S4) and poisoning (§2.2) |
| Disk full during the job, or at `COMMIT` with the transaction still open | The job rolls back; `RolledBack(DiskFull)`; later jobs run normally once space exists | S4, S6 |
| Disk full at `COMMIT` with the transaction ended | `CommitUnknown`, as above | S6 |

## 3. Structure Options

| Fork | Option | Precedent | Verdict |
|---|---|---|---|
| **Crate** | A. A separate `chaldea-store` crate | — | Rejected by mvp §3 ("one user"). |
| | B. `crates/daemon` with only the `store` module | mvp §3 layout | **Adopted.** No binary yet (PR 8 adds `daemon run` in `crates/cli`). |
| **SQLite source** | A. System SQLite | — | Rejected: no SQLite development package on the Windows runner, and versions differ per OS; `STRICT`, `octet_length`, and trigger behaviour must be the same on both. |
| | B. `bundled` | `libsqlite3-sys` `build.rs:170-187` | **Adopted**: SQLite 3.54.1 on both runners (S1). |
| **Writer model** | A. `Mutex<Connection>` shared by callers | — | Rejected: std `Mutex` is not FIFO and has no deadline; a caller holding it across an `.await` would block the runtime. |
| | B. An async task owning the connection | T3 Code's writer worker (`persistence/Sqlite.ts:111-115`, `writerWorker: true`) | Rejected: rusqlite calls block; each job would need `spawn_blocking` and still serialize. |
| | C. A dedicated writer thread, bounded job queue, oneshot reply | — | **Adopted.** One write transaction at a time by construction, deadlines decided under the lock, no runtime needed for tests. |
| **Atomicity after a caught error** | A. Trust the closure to return `Err` | rusqlite `Transaction` rolls back on drop (`src/transaction.rs:128`) | Rejected: a closure that catches an error and returns `Ok` would commit a partial job, and after `RAISE(ROLLBACK)` its next statement autocommits (S4). |
| | B. Poison `WriteTx` on the first error and check `is_autocommit` before `COMMIT`/`ROLLBACK` | — | **Adopted.** |
| **Readers** | A. All reads through the writer | — | Rejected: reads would wait behind writes. |
| | B. A small pool of read-only connections | T3 Code's reader worker (`Sqlite.ts:111-115`) | **Adopted.** `query_only` makes a write on them fail in the engine. |
| **Per-stream order** | A. Global sequence only | T3 Code v2 events: `sequence INTEGER PRIMARY KEY AUTOINCREMENT`, non-unique `(thread_id, sequence)` index (`Migrations/055_OrchestrationV2.ts:20-39`) | Rejected by mvp §2.1. |
| | B. `stream_version` computed in the `INSERT` from `MAX + 1`, unique per stream | T3 Code v1 (`Migrations/001_OrchestrationEvents.ts:8-28`; `persistence/OrchestrationEventStore.ts:494-501`), relying on the caller's transaction | Partly adopted: the same computation, plus a trigger that refuses any other value. |
| | C. A `streams` table holding `head_seq` and `cum_bytes` | — | Rejected: a second copy of the head that must agree with the last row. |
| **Append-only** | A. Convention (the store never issues `UPDATE`) | T3 Code: no trigger; `ProjectionMaintenance.ts` deletes events when pruning (`:172`, `:214`) | Rejected: mvp §2.1 requires the engine to refuse. |
| | B. Update and delete triggers with `RAISE(ROLLBACK)` and `recursive_triggers=ON` | mvp §2.1 | **Adopted**; S2–S4. |
| **Idempotency conflict** | A. `INSERT OR IGNORE` / `ON CONFLICT DO NOTHING`, report "not inserted" | T3 Code receipts (`persistence/OrchestrationCommandReceipts.ts:140-143`) | Rejected for events: a key reused for a different event would pass, and mvp §2.1 rules out upserts on `events`. |
| | B. Look up the key; every producer-supplied field equal → the existing event; else abort | — | **Adopted** (CL-10). The unique index is the backstop. |
| **Projection code** | A. SQL triggers on `events` that maintain projections | — | Rejected: compare-and-set rules, `Content` JSON, and cursor rules in SQL over `json_extract`, hard to keep equal to core's types. |
| | B. A pure reducer plus a separate SQL writer that mirrors it | T3 Code `ProjectionStore.ts`: `applyToProjection` in memory and a parallel SQL `apply` with matching merge helpers (line numbers approximate: ~`:1034-1231`, ~`:2398-2421`, ~`:2460`) | Rejected: two implementations of one rule. |
| | C. One Rust function over a `proj_*`-only transaction, used by live append and rebuild | — | **Adopted.** A pure function in `core` was considered: `core` has no projection row types, and adding them would widen its public surface for internal tables. |
| **`task_state` owner** | A. Only `status_changed` changes axes | — | Rejected: a standalone `status_changed(TaskState)` would change the task with no `task_set`, against mvp CL-2. |
| | B. `task_set` sets it; `apply` ignores the paired `status_changed`, which `append` checks against the `task_set` of the same job | mvp §2.2 (task changes only on `task_set`) | **Adopted.** No projection columns for the pairing; the other axes have no event of their own, so `status_changed` stays their only cause. |
| **Blob storage** | A. Files named by SHA-256 in `<state_dir>/blobs/` | — | Rejected: a file and its event cannot commit together; Windows sharing violations and path limits; orphans after a crash. |
| | B. A `blobs` table in the same database | — | **Adopted.** One transaction, the same fsync path; costs database size. |
| **Blob refs** | A. A trigger derives refs from every `{"blob": …}` object in the payload | — | Rejected: free JSON (`SurfaceAction.args`) would abort valid events or grant read access to another stream's blob. |
| | B. Refs from the typed `Content` walker in Rust, checked by a trigger over the same typed paths | — | **Adopted.** |
| **Blob media type** | A. One type per blob (the first insert's), refusing a reference with another type | — | Rejected: a producer cannot see other threads' blobs and cannot change verbatim bytes, so it would lose an event because unrelated content happened to be equal. |
| | B. One type per blob, first insert wins, no refusal | — | Rejected: the served type would depend on insertion order and could differ from the event's own `BlobRef.media_type`. |
| | C. Body shared by SHA-256, media type per reference; the download type is the stream's agreed type or `application/octet-stream` | — | **Adopted.** |
| **Failed first open** | A. Delete the file this call created | — | Rejected: another opener may have initialized and committed to it meanwhile, and macOS unlinks open files. |
| | B. Never delete; reuse an uninitialized file at the next open | — | **Adopted.** |
| **Hook order** | A. The order record jobs commit | — | Rejected as the buffered order: a slow ancestry check reorders it; kept for audit. |
| | B. PR 9's intake ordinal, stored with each delivery | mvp §2.9 "receipt order at the daemon" | **Adopted.** |
| **Buffered hooks** | A. Store a pre-built `NewEvent` and append it on promotion | — | Rejected: the events of a buffered hook depend on the state after binding (attribution, status changes), and a hook can produce several. |
| | B. Opaque buffered bytes and metadata; the caller passes the final events at promotion | — | **Adopted.** |
| **Transcript order** | A. Rust checks in the tailer | — | Second line only. |
| | B. A batch operation that builds the envelopes and checks binding and cursor, plus triggers over generated columns | — | **Adopted**: no code path stores a line of an unrecorded generation or a stale binding. |
| **Migration marker** | A. `PRAGMA user_version` + `application_id` | SQLite file format (S8) | **Adopted**: transactional, in the header, readable before any schema exists. |
| | B. A `schema_migrations` table | — | Rejected: needs the schema to exist before it can say which schema exists. |
| **Durability** | A. `synchronous=NORMAL` | T3 Code `Sqlite.ts:82` (with `checkpoint_fullfsync`, `:84`) | Rejected: a hook's first reply must be durable before 200 (mvp §2.1). |
| | B. `synchronous=FULL` + `fullfsync` | mvp §2.1 | **Adopted** (§8 D5). |

## 4. Same-Class Inventory

Every later PR that reads or writes the store, and what it uses. None needs a store change; each runs its own checks inside one job.

| PR | Needs | Operation |
|---|---|---|
| 7 HTTP | replay, backward pages, head and byte bound, snapshots, 404 for unknown streams | `read`: `stream_exists`, `head` → `decide_replay`, `events_after` / `events_before` (`Page`), `thread_snapshot` / `host_snapshot` |
| 7 | blobs with the object check | `read`: `blob_ref_exists(stream_id, sha256)`, `blob(stream_id, sha256)` |
| 7 | receipts and deadline outcome | `submit`: `receipt_begin` first in the intent job, `receipt_complete`; `read`: `receipt`; the `WriteError` variants of §2.2 say whether the intent committed |
| 7 | tokens, pairing | `submit`: `token_insert`, `token_revoke`; `read`: `token_by_hash` |
| 7 | WS heads and `stream_advanced` | `subscribe` (`Committed`), `read`: `heads` |
| 8 lifecycle | open, shutdown, restart entry point, lock release condition | `Store::open`, `shutdown` (`ShutdownReport::writer_exited`), `paths`; `submit`: `receipts_mark_unknown_after_crash` before accepting commands. `shutdown` blocks the calling thread (run it off the async executor's worker threads, e.g. `spawn_blocking`; a timed-out or dropped `spawn_blocking` task keeps running and runtime shutdown still waits for it); the first call's deadline is the only drain deadline (one shutdown budget); stop accepting new work and give in-flight reads a chance to finish within the same budget, then call it even if readers remain (the store abandons and reports them); process exit need not drain notification receivers; release the single-instance lock only when `writer_exited`, else exit holding it; touch the DB files in-process only when `readers_returned` and `writer_exited` are both true, else only after process exit; committed transactions are recovered from the WAL even if the checkpoint was skipped; stop the daemon on `Failed`, `WriterGone`, or `CommitUnknown` (the writer retires after `CommitUnknown`, so no later `Failed` may arrive; the unknown outcome is kept for restart reconciliation) |
| 9 hooks | dedupe before routing, durable first reply, receipt order, buffered storage and promotion | `submit`: `hook_delivery_record` (with PR 9's `receipt_ordinal`), `hook_delivery_promote`; `read`: `hook_delivery`, `buffered_deliveries`, `buffered_summary`, `max_receipt_ordinal`, `binding_receipt_ordinal`, `hook_emit_in_thread`, `buffered_launches` (restart turns them into `raw_unknown` through `hook_delivery_promote`) |
| 9 | `raw_unknown`, events from hooks | `append` in the delivery job |
| 10 supervisor | reservation (thread row, expected launch, death confirmed, worktree state, tokens) in one transaction | one job: `read().thread`, `launch`, `worktree`, then `append(launch_reserved, …)`, `token_insert`, `receipt_*` |
| 10 | close racing resume | both jobs read the thread row under `BEGIN IMMEDIATE`; the second sees the first's events |
| 10 | gate inputs, send path | `read`: `thread`, `launch` (gate evidence, `write_basis`), `inputs`; `append(input_state_changed)` with the compare-and-set of `apply` |
| 10 | readiness write basis | `NewEvent::annex = Readiness { write_basis }` on `readiness_observed` |
| 10 | restart | `read`: `open_launches`, `inputs(… queued, sending, sent)`; `append(launch_exited, input_state_changed, status_changed)` |
| 12 fixtures (`doctor capture` deferred; hand-captured fixtures are mvp C1, X1) | captured events for `doctor capture` | `read`: `events_after`, `events_of_kind` |
| 13 / 14 tailers | binding, cursor, generation, batches, rewind after rebuild, continuity | `read`: `transcript_cursor`, `thread`, `launch`; `submit`: `append_transcript` |
| 14 | Codex assistant text from `Stop` | `append` |
| 16 worktrees, mailbox, task | exclusion against reservations; mailbox listing; task changes | one job: `read().worktree`, `launches_using_worktree`, `append(worktree_changed)`; `events_of_kind(mailbox_message)`; `append(task_set, status_changed)` |
| 17 `stateright` (deferred) | the store's atomicity and gapless order as model assumptions | the guarantees of §2.1 and §2.9, cited by claim id |
| 20 extensions (deferred) | `POST /events` surfaces with the stored scope check | one job: `read().surface`, `append(surface_update)` |
| 21 doctor (deferred) | integrity, schema version, projection check, file sizes | `read`: `integrity_check`, `schema_info`; `submit`: `verify_projections`; `Store::paths` |

## 5. Resource Paths

| Resource | Created by | Bound | Ends on |
|---|---|---|---|
| `chaldea.db`, `chaldea.db-wal`, `chaldea.db-shm` | `open` (db), SQLite (WAL, SHM) | one database per state directory | never deleted by the store once initialized; the writer connection closes before the writer's exit report is published, and the store's handles are released once the writer and every reader connection have closed (a reader held past the shutdown deadline can keep the files open on Windows); SQLite may delete `-wal` and `-shm` as the last connection closes |
| `store-writer` thread | `open` | one | exits after shutdown's checkpoint, after a panic outside a job, or in the failed state after draining; in each case its last statement publishes its report; the owning `shutdown` detaches its handle after its report wait and never joins it (thread-local teardown may still run) |
| Writer connection | `open` | one | writer exit, or retired in the failed state |
| Reader connections | `open` | `readers` (4) | closed by shutdown once returned to the pool |
| Job queue | `open` | `writer_queue` (256) jobs | shutdown takes the shared sender (every clone's `submit` then returns `Closed`) |
| Reply oneshot | `submit` | one per job | the writer replies once; a dropped sender is `WriterGone` |
| `Committed` broadcast | `open` | 1024 messages | the owning `shutdown` drops the store's sender outside the intake locks and then publishes the close; the writer thread drops its own before publishing its report; then each receiver gets `Closed` once it has drained the notifications still buffered for it; a lagging receiver gets `Lagged` first, then the rest, then `Closed` |
| Read transaction | `read` | one per call | `f` returns |
| Spool file `chaldea-spool-<uuid>.tmp` | `Store::blob_spool` | one per spooled value; disk only, a 64 KiB buffer in memory | closed, then removed, when its last owner drops (`finish` at or below the cap, the job's append, a job that never runs, or a spool dropped unfinished); a process crash leaves it |
| Part buffer | `append` of a `Spooled` input | one `PART_BYTES` (4 MiB) buffer per spooled input being inserted | the insert returns |

`journal_size_limit` is the size SQLite truncates the WAL back to after a checkpoint resets it, not a cap: a long read transaction can let the WAL grow past it. A backup copies the database through SQLite (the backup API or `VACUUM INTO`) or copies all three files with the store shut down; a copy of a live `.db` file alone can miss committed transactions still in the WAL.

**Endings.**

| Ending | What happens |
|---|---|
| Normal shutdown | One shape: **the first `shutdown` call owns the shutdown**, and every later call does no work. The owner, in order: (1) closes intake in one transition: under the queue-sender lock it takes the sender, closes the pool, marks itself owner, and takes the store's notification sender out, so once any clone's `submit` returns `Closed` its `read` does too; after releasing the locks it drops the notification sender (dropping the last sender wakes receivers inline, and a waker may call `submit`) and then publishes the close, and no later caller reports the writer's exit before that publication (this drop wakes pending receivers inline, so the waker rule of §2.2 applies: a waker that panics or blocks here can leave the shutdown unfinished); (2) the writer runs the jobs already queued (each still deadline-checked) and then waits for the owner to finish with the readers; (3) the owner closes every idle reader connection and each lent one as it comes back, waiting until its deadline (a connection counts as open until its handle is dropped); readers still lent then are abandoned and reported; then, and only then, it releases the writer; (4) the writer runs `PRAGMA wal_checkpoint(TRUNCATE)`, which can complete only once no reader holds a snapshot, closes its connection, and its thread ends: as the thread's last statement, after the connection, the queue receiver, and the notification sender are dropped, it publishes its report (a panic outside a job publishes an empty report the same way); (5) the owner waits for that report until its deadline and, like every caller, counts it only if it was published by that deadline; then it detaches the thread's handle without joining it (`join` would also wait for thread-local destructors), and a report published later still reaches the shared state for later calls. The owner's deadline limits the owner's timed waits for shutdown progress (lock acquisition, the synchronous cleanup in (1) and (3), and scheduling can still delay its return), and it is the shutdown's single deadline: no other caller can release the writer or abandon readers. A **later call** waits until its own deadline for the shutdown to complete (the owner's report and the writer's report) and returns what it observed by that deadline (the completion or timeout is decided and the reports read under one lock acquisition, and on every return a report published after the deadline is left out): the owner's reader outcome once the owner returned (else `readers_returned` false and the connections still open as abandoned), and the writer's report if it was published and the owner had published the notification close (else `writer_exited` false). Only these timestamped publications are cut off at the deadline: the owner cuts off the writer's report only, while a later call also requires the owner's report and the notification close by its own deadline. The reader fields are observations at the check that produced them: the owner's `readers_returned` can include a reader returned just after its deadline but before its last pool check, and a later call without the owner's report takes `readers_abandoned` from the pool's count at its check. `ShutdownReport { readers_returned, readers_abandoned, writer_exited, jobs_run, jobs_expired, checkpoint: Complete \| Busy \| Error \| NotRun }` holds these facts separately: `writer_exited` means the writer thread published its exit report as its last statement by the caller's deadline (its connection already closed; its thread-local teardown may still run), so every notification receiver sees `Closed` after the notifications still buffered for it (`Lagged` first for a lagging one); `checkpoint` is `NotRun` if the report had not arrived. The order "readers closed, then checkpoint, then exit" holds whenever the owner closed every reader by its deadline; at the deadline the owner stops waiting. A reader held past the owner's deadline is abandoned, so the report has `readers_returned` false and, typically, `writer_exited` false and `checkpoint` `NotRun`; a writer blocked in a long job leaves `writer_exited` false while every reader may have returned. PR 8 may release the single-instance lock only when `writer_exited` is true; otherwise an abandoned writer might still commit, and the daemon exits the process with the lock held until the OS releases it. |
| Job error, panic, or poisoned job | Rolled back as in §2.2; the writer continues. |
| Writer thread panic outside a job | The queue's receiver drops: `submit` returns `Closed`, pending tickets resolve `WriterGone`. PR 8 stops the daemon. |
| `SQLITE_BUSY` | Only another process holding the database (PR 8 prevents a second daemon): the job ends `Busy` with nothing written. Readers do not wait on the writer (S7) but can get `Busy` during WAL recovery. |
| Disk full (`SQLITE_FULL`) | During the job, or at `COMMIT` with the transaction still open: the job rolls back, `RolledBack(DiskFull)`, and later jobs run normally once space exists. At `COMMIT` with the transaction ended: `CommitUnknown` and the failed state. |
| Corruption, I/O error outside `COMMIT`, failed `ROLLBACK` | The failed state (§2.2): queued jobs get `Failed`, new submissions `SubmitError::Failed`, reads continue. Nothing is deleted. |
| `COMMIT` fails with the transaction ended (I/O error, or `SQLITE_FULL` that ended it) | `CommitUnknown` for that job; the failed state follows. |
| Process crash | No partial job (S6); the next `open` recovers the WAL. In-progress receipts and open launches are recovered by PR 8 / PR 10 through the restart operations. |
| Caller stops waiting | The job still runs or expires on its own; the caller recovers through the receipt (mvp §2.4). |

**Waits.** `submit` never waits (`try_send`). A job runs only if its deadline had not passed once the lock was held. `BEGIN IMMEDIATE` waits on SQLite's busy handler, about `busy_timeout`, not a hard bound. `read` waits for a connection until `wait_until`. `shutdown` waits until its deadline for readers and for the writer's report, and never joins the writer thread. A started job's run time is not bounded by the store, and neither is a read closure's: a closure can do anything it captured, `rebuild_projections` runs as long as a full replay, and a read closure that never returns keeps its connection and snapshot (shutdown then counts it abandoned). Bounding either would need another execution boundary; the store states the limit instead. The `WriteTx` API offers nothing but database work, and PR 7–10 reviews check that closures capture no blocking handle (network, PTY, channels). `WriteTicket::blocking_wait` has no deadline of its own, because the writer replies to every accepted job (run, expired, busy, failed, or `WriterGone` when it ends); a job that calls `blocking_wait` on another job's ticket, a wait that could never end, gets `WouldDeadlock` instead (a thread-local flag marks writer threads; the waited-for job still runs later). Polling a ticket as a future inside a job is not guarded; a job has no runtime to poll it with, and PR 7–10 reviews check that closures do not.

**Windows and macOS differences.**

- **File locking.** SQLite uses `LockFileEx` on Windows and POSIX advisory locks on macOS. A second `Store` on the same file, in this or another process, serializes through them (S5). On Windows a handle left open keeps a file from being deleted, so `store_open::shutdown_releases_every_file` proves on Windows that the store closed its handles by deleting all three files after `shutdown`; on macOS, where unlink succeeds with open handles, the same test checks `ShutdownReport` and reopens the files exclusively through a new `Store`.
- **Paths.** On Windows writer and readers use the `win32-longpath` VFS (S13), since the default `win32` VFS limits a path to 1040 UTF-8 bytes (`MAX_PATH × 4`) while Windows accepts much longer `\\?\` paths; a 260-unit path of BMP characters is at most 780 bytes and fits, so the limit bites only on paths beyond `MAX_PATH`, which a deep multibyte state directory can reach. The store passes the canonical `\\?\` path of the state directory. `store_open::long_multibyte_path` creates a store several directory levels deep under names of Japanese characters, totalling more than 1100 UTF-8 bytes and 300 UTF-16 units, writes until `-wal` and `-shm` exist, shuts down, and reopens; on macOS the same test stays just below 512 bytes, because the `unix` VFS caps a path at `MAX_PATHNAME` (512) bytes. If it fails on Windows, the documented limit becomes "state directory path under 200 characters" and PR 8 checks it at startup.
- **fsync.** `F_FULLFSYNC` on macOS, `FlushFileBuffers` on Windows. Neither is observable in CI; durability claims are tested for process kills only.
- **Permissions.** Unix: the database is created `0600` and SQLite gives WAL and SHM the same mode (S13). Windows: the files inherit the state directory's ACL; PR 8 places the state directory under the user's profile, which is user-only.
- **Network filesystems.** WAL needs a local filesystem (S7); `journal_mode` returning anything but `wal` refuses the open.

## 6. Purpose Layer and Plan Layer

### 6.1 Purpose Layer: Acceptance Criteria

Checked by running the store tests on `macos-latest` and `windows-latest` CI against real database files in the OS temp directory.

| ID | Criterion (EARS) | Source | How to observe |
|---|---|---|---|
| PR6-P-1 | When projections are dropped and rebuilt from `events`, they shall equal the maintained projections, except for the permitted transcript read rewind. | mvp §4 PR 6 obligation; CL-11 | PR6-CL-7. |
| PR6-P-2 | When any statement tries to update or delete an event, including `INSERT OR REPLACE`, the database shall refuse it, keep the event, and end the transaction. | mvp §4 PR 6 obligation; CL-9 | PR6-CL-2. |
| PR6-P-3 | When a transcript generation changes, its `transcript_generation` event shall be stored before any line of that generation. | mvp §4 PR 6 obligation; CL-12 | PR6-CL-10. |
| PR6-P-4 | If the stored transcript cursor or binding differs from the batch's, then the whole batch shall abort with nothing stored. | mvp §4 PR 6 obligation; CL-12 | PR6-CL-9. |
| PR6-P-5 | When content above 64 KiB is stored, it shall become a blob with a 1 KiB preview that marks invalid sequences, and content at or below 64 KiB shall stay inline. | mvp §4 PR 6 obligation; mvp §2.1 `Content`; P-21 | PR6-CL-11, CL-12. |
| PR6-P-6 | The store shall provide the operations listed in §4, each in one transaction, with the return and error values §2 states. | mvp §4 (PRs 7–21) | The operations of §4 exist with the signatures of §2.2–§2.7. Each write operation and the reads that later PRs decide on (`head`, `events_after`, `events_before`, the snapshots, `blob`, `receipt`, `token_by_hash`, `hook_delivery`, `buffered_deliveries`, `transcript_cursor`) has a test among PR6-CL-1 to CL-24; the plain lookups without a claim (`stream_exists`, `heads`, `event_by_key`, `events_of_kind`, `latest_of_kind`, `thread`, `threads`, `launch`, `open_launches`, `launches_using_worktree`, `input`, `inputs`, `approval`, `open_approvals`, `surface`, `worktree`, `worktrees`, `buffered_launches`, `schema_info`, `integrity_check`) are exercised only by `store_reads::lookups_smoke` (each returns what one known history stored), which is not a proof of their later use. |

### 6.2 Plan Layer: Claims

| ID | Claim (EARS) | Source | Falsifying test |
|---|---|---|---|
| PR6-CL-1 | When events are appended, each stream's `seq` shall be 1, 2, 3, … with no gap or repeat, `cum_bytes` shall be the running sum of `payload_bytes`, and `global_seq` shall increase in commit order, including after failed batches. | CL-9; mvp §2.1, §2.4 | proptest `store_append::seq_is_gapless_under_random_batches` (random streams and batches with injected failures; after a failed batch the next event reuses the same `seq` and `cum_bytes`); unit `triggers::out_of_order_seq_is_refused` (gap, repeat, 0), `triggers::wrong_cum_bytes_is_refused`, `triggers::wrong_payload_bytes_is_refused`, `triggers::explicit_lower_global_seq_is_refused` |
| PR6-CL-2 | If a statement on a store connection would update or delete an `events` row, then the transaction shall end with every earlier write of it gone and the row unchanged. | CL-9; mvp §2.1 | unit `triggers::each_forbidden_statement_aborts`: `UPDATE`; `DELETE` on a non-empty table, with and without `WHERE`; `INSERT OR REPLACE` and `REPLACE INTO` at the next valid `seq` colliding only on `event_id`, and only on the idempotency key; an upsert `DO UPDATE` on the key; `UPDATE OR REPLACE`. Each case first writes a row to another table in the same transaction and asserts it is gone and the connection is in autocommit (fails if `RAISE(ROLLBACK)` became `RAISE(ABORT)`). Control `triggers::replace_passes_without_recursive_triggers` runs the `REPLACE` cases on a connection with `recursive_triggers=OFF` and sees them succeed |
| PR6-CL-3 | If a statement would change or delete a blob, a blob ref, the epoch, a finished receipt, a delivery's reply, ordinals, or key, or a token column other than an unset `revoked_at`, then the transaction shall roll back (an immutability trigger, `RAISE(ROLLBACK)`); if a plain raw `INSERT` (no conflict clause) adds a row violating a `CHECK` of §2.4, including through a `NULL` operand, then that statement shall fail (the transaction stays open, S4; the job-level effect is PR6-CL-4); a conflict clause changes this as SQLite defines (`OR IGNORE` skips the row with no error, `OR ROLLBACK` ends the transaction), and the store issues none. | mvp §2.1, §2.4, §2.9 | unit `triggers::immutable_tables_refuse_changes` (one case per row of §2.4's table; each asserts autocommit afterwards), `triggers::null_operands_fail_checks` (thread stream with null `thread_id`; half-null routing; partial receipt fields; transcript source with null positions; readiness annex without `counted`; each asserts the statement failed and an earlier write of the transaction is still there) |
| PR6-CL-4 | If any statement of a job fails, then no write of that job shall remain, even when the closure catches the error and continues or returns `Ok`. | mvp §2.1 "in its transaction" | unit `writer::caught_statement_error_still_rolls_back` (a `CHECK` failure, which ends only the statement), `writer::caught_transaction_error_does_not_autocommit` (a `RAISE(ROLLBACK)`; the closure then writes again and returns `Ok`); both assert an earlier valid insert of the job is gone and the reply is `RolledBack`; `store_append::failed_job_changes_nothing` (second event fails `apply`; tables dumped before and after) |
| PR6-CL-5 | When an event is appended with a key its stream already holds, the store shall return the stored event if every producer-supplied field is equal, and otherwise roll back the job; distinct keys shall each create one event. | CL-10; mvp §2.1 | `store_append::replayed_key_returns_the_existing_event` (same event submitted later with `occurred_at: None`: `Existing` with the first `event_id`, `seq`, `recorded_at`), `store_append::each_changed_field_conflicts` (one case per field: `kind`, `thread_id`, `launch_id`, `command_id`, `source`, `annex`, `payload`, explicit `occurred_at`, `None` vs explicit), `store_append::repeated_key_within_a_batch`, `store_append::distinct_emit_ids_create_distinct_events` |
| PR6-CL-6 | When a hook delivery is recorded, its first reply, both ordinals, routing, and events shall commit together; a retry with the same `(launch_id, emit_id)` shall get the first reply and the current routing and change nothing; buffered deliveries shall be read in receipt order; promotion shall take the caller's events, change only the routing from `buffered`, and clear the buffered data. | CL-10 (store side); mvp §2.1, §2.9 | `store_hooks::retry_replays_the_first_reply`, `store_hooks::retry_after_promotion_sees_new_routing`, `store_hooks::promotion_appends_the_given_events` (two events for one delivery), `store_hooks::buffered_read_in_receipt_order` (receipt ordinals 1 and 2 recorded in commit order 2, 1: read 1, 2), `store_hooks::failed_routing_stores_no_delivery`, `store_hooks::empty_promotion_is_refused`; unit `triggers::terminal_routing_needs_an_event` (missing `(stream_id, seq)` refused by the foreign key) |
| PR6-CL-7 | When the projections are dropped and rebuilt from `events`, every `proj_*` row shall equal the maintained one, except that each transcript cursor's read position and read evidence shall equal its event position and event evidence exactly. | CL-11; mvp §2.1 | unit proptest `rebuild::rebuild_equals_maintained`: the test drops every `proj_*` table, recreates it from the shipped DDL, rebuilds, and compares with `compare_projections`; histories come from the model generator (every kind, annexes, no-event cursor advances, re-read lines, generation changes across paths, empty-component `surface_update`, `input_created` with `launch_id: None`, `task_set` pairs, unknown kinds inserted raw, blob content, failed jobs); the rebuild runs with a different fixed clock than the live appends. `store_rebuild::model_matches_projection` (integration): the generator's model computes expected values independently for the axes, current launch, input state and launch, gate evidence and `write_basis`, and cursor event position; they are compared with the store after live appends and after `rebuild_projections`, so a bug shared by both paths fails. `store_rebuild::verify_allows_only_the_rewind` (a maintained `read_offset` past the event offset passes; any other changed column is reported) |
| PR6-CL-8 | When `status_changed` or `input_state_changed` names a `from` the projection does not hold, the job shall roll back; `task_state` shall change only through `task_set`, and a new `status_changed(TaskState)` shall be accepted only as the record of a `task_set` of the same thread earlier in the same job, with matching `from` and `to`, and each `task_set` shall justify at most one. `from` is compared with the current projection for the other axes and with the state before the causing `task_set` for `task_state`. | mvp §2.1 projections; mvp §2.2, CL-2 | `store_append::lost_update_aborts` (two jobs record the same `queued → sending`), `store_rebuild::task_set_with_reducer_output` (`task_set` then the `status_changed` that `core::state::reduce` returns for `TaskSet`: `completed` after commit and after rebuild), `store_append::stored_task_status_change_retry_is_existing` (an equal retry of the stored derived event, alone in a later job: `Existing`), `store_append::standalone_task_status_change_is_refused` (a new key without a `task_set` in the job; one whose `from` or `to` does not match it; a second new key with the same `from` and `to` after one `task_set`; an equal retry of the stored change after them is still `Existing`), `store_append::sending_sets_a_missing_input_launch` (`input_created` without a launch, then `sending` with an envelope launch: input launch and that launch's `last_write_seq` set; both null refused) |
| PR6-CL-9 | When a transcript batch commits, its events and cursor advance shall commit together, and only if, before any duplicate check, the thread's current launch, session, and transcript path equal the batch's new state and the stored cursor equals the batch's expected old state (generation, path, `file_id`, read offset); a new generation shall commit alone at offset 0, with its annex as the evidence of both positions, and a job that starts a generation for a thread shall refuse that thread's lines, cursor advances, and further generations in either order; every line shall pass the same content validation as `append`; otherwise nothing of the batch shall be stored. | CL-12 (store side); mvp §2.1 | `store_transcript::stale_cursor_aborts_the_batch` (one case per field, plus `None` with a cursor), `store_transcript::rebind_starts_a_generation_on_the_new_path` (cursor on path A, `session_bound` to B, generation start on B with `expect` naming A, then the first B line), `store_transcript::same_path_swapped_file` (new `file_id` on the same path: generation start, then a line from offset 0), `store_transcript::old_batch_after_rebind_is_refused`, `store_transcript::old_batch_after_resume_is_refused`, `store_transcript::generation_with_lines_is_refused` (one batch), `store_transcript::generation_and_lines_in_one_job_are_refused` (two calls in one job, both orders, once with lines and once with a continuation of no lines and a later `read_offset`), `store_transcript::two_generations_for_one_thread_do_not_share_a_commit`, `store_transcript::generation_zero_uses_the_annex_evidence`, `store_transcript::first_line_in_the_next_job`, `store_transcript::continuation_starts_at_the_read_offset`, `store_transcript::batch_and_cursor_commit_together`, `store_transcript::advance_without_events`, `store_transcript::partial_line_after_exit_commits`, `store_transcript::invalid_content_line_is_refused` (inline above 64 KiB), `store_transcript::transcript_source_via_append_is_refused` |
| PR6-CL-10 | If a transcript line names a generation other than the stream's latest `transcript_generation`, another `file_id` or path, a launch other than the latest `launch_reserved`'s or a path other than that launch's bound path, a missing position, or an offset not after the previous line of its generation (for the first line, not after 0), or a `transcript_generation` is not the next number from 1 or names another launch than the latest `launch_reserved`'s or another path than that launch's bound path, then the insert shall be refused. | CL-12 (store side); mvp §2.1 | unit `triggers::line_before_its_generation_is_refused`, `triggers::older_generation_line_is_refused`, `triggers::offsets_must_increase`, `triggers::first_line_of_a_generation_ends_past_zero`, `triggers::generation_must_be_next`, `triggers::generation_path_must_match_the_session`, `triggers::raw_line_after_rebind_is_refused` (latest generation on path A, then `session_bound` to B, then a raw insert of an A line), `triggers::raw_line_after_a_new_reservation_is_refused` (L1 bound to A, L2 reserved and unbound, raw L1 line), `triggers::partial_line_before_the_next_reservation_passes`, `triggers::transcript_positions_are_required` |
| PR6-CL-11 | When content is prepared, at most 65 536 bytes shall stay inline (`utf8` for valid UTF-8, else `base64`), and above it the content shall be a blob with its SHA-256, size, and a preview of the first 1024 bytes in which each invalid sequence is U+FFFD and sets `preview_lossy`, and a valid character crossing byte 1024 is left out without setting it; the store shall refuse any `Content` that breaks these rules. | mvp §2.1 `Content`; P-21 | `store_content::boundary_inline_and_blob` (65 535, 65 536, 65 537 bytes, UTF-8 and binary), `store_content::preview_cut_valid_character` (1023 ASCII bytes then `あ`), `store_content::preview_invalid_byte_at_the_boundary` (`0xE3 0x20` at 1023–1024), `store_content::preview_invalid_byte_before_the_boundary`, `store_content::preview_keeps_a_literal_replacement_character`, `store_content::store_refuses_bad_content` (inline above 64 KiB, base64 of valid UTF-8, forged hash, size mismatch, wrong preview, unreferenced `BlobInput`) |
| PR6-CL-12 | When an event referencing a blob commits, the blob and a `(stream_id, sha256, media_type)` ref for each typed reference shall commit with it; a missing blob shall abort the event; free JSON shall never create or require a ref; no event shall be refused because the same bytes were stored under another media type; a blob shall be readable only through a stream that references it, with the stream's agreed type or `application/octet-stream`. | mvp §2.1, §2.5 | `store_content::blob_round_trip`, `store_content::same_blob_twice_in_one_event` (`tool_activity` input and output), `store_content::same_blob_across_events`, `store_content::missing_blob_aborts_the_event`, `store_content::blob_shape_in_surface_args_is_data` (unknown hash and another stream's hash in `SurfaceAction.args`: event stored, no ref, no read access), `store_content::nested_list_grid_needs_a_ref`, `store_content::unreferenced_stream_cannot_read`, `store_content::media_types_per_reference` (same bytes as `text/plain; charset=utf-8` and `application/json`: in different streams, in one stream, in one job, each in both insertion orders; every event and the full bytes kept; each stream's download type is its agreed type or `application/octet-stream`) |
| PR6-CL-13 | When `receipt_begin` runs, it shall return core's `ReceiptLookup` for the stored receipt and insert `in_progress` only for `New`, in the same transaction as the command's intent; a completed receipt shall replay its status and body exactly; concurrent duplicates shall insert once; a restart shall turn every `in_progress` receipt into `unknown_after_crash`. | CL-8 (storage); mvp §2.4 | `store_receipts::lookup_cases`, `store_receipts::intent_and_receipt_commit_together` (intent job fails after `receipt_begin`: no receipt, no event), `store_receipts::replay_is_exact` (a body with non-ASCII and trailing whitespace), `store_receipts::concurrent_duplicates_insert_once` (threads on one store, and two `Store`s on one file), `store_receipts::restart_marks_unknown_after_crash` |
| PR6-CL-14 | When a job's deadline passes before the writer starts it, or while it waits for the lock that it then obtains, the job shall change nothing and reply `Expired`; when the lock is not obtained, the reply shall be `Busy` whatever the deadline; a ticket shall resolve `Ok` only after `COMMIT`; a job error or panic shall roll back the whole job. A job that calls `WriteTicket::blocking_wait` shall get `WouldDeadlock` at once (polling a ticket as a future inside a job is not guarded). | mvp §2.5 deadlines; §2.1 one writer | `store_writer::expired_in_queue_changes_nothing`, `store_writer::expired_while_waiting_for_the_lock` (a second connection holds `BEGIN IMMEDIATE` past the deadline, then releases within the busy budget), `store_writer::busy_wins_over_the_deadline` (the lock held past both), `store_writer::ok_is_visible_to_a_new_reader`, `store_writer::job_panic_rolls_back`, `store_writer::full_queue_fails_at_once`, `store_writer::waiting_on_a_ticket_inside_a_job_is_refused` (a job submits through a captured clone and waits: `WouldDeadlock`, no hang) |
| PR6-CL-15 | If another connection holds the write lock past the busy budget, the job shall reply `Busy` with nothing written; if a statement fails with `SQLITE_FULL` during the job, or `COMMIT` fails with the transaction still open, the job shall reply `RolledBack` and the next job shall succeed once space exists; if `COMMIT` fails with the transaction ended, the reply shall be `CommitUnknown` and the writer shall be retired (failed state) before that reply; if the writer fails or ends, queued jobs shall get `Failed` or `WriterGone` and new submissions shall be refused. | §2.2, §2.9, §5 | `store_writer::busy_from_another_connection`; unit `writer::disk_full_rolls_back_and_recovers` (the writer connection's `PRAGMA max_page_count` lowered so an insert fails, then raised), `writer::commit_failure_with_open_transaction_rolls_back` (a test-only fault point returns `SQLITE_BUSY` from the commit step without committing: `RolledBack`, next job succeeds), `writer::commit_failure_with_ended_transaction_is_unknown` (the fault point returns `SQLITE_IOERR` after the real `COMMIT`: `CommitUnknown`; with the writer held at the start of its failed state, the next `submit` is already `Failed`; tests the mapping, not SQLite), `writer::failed_state_drains_the_queue`, `writer::writer_gone_resolves_tickets` (test-only panic outside a job) |
| PR6-CL-16 | If the process is killed during a job before `COMMIT` began, then after reopening no part of that job shall be visible and every earlier committed job shall be; if it is killed after `COMMIT` returned, the whole job shall be visible; if it is killed during `COMMIT`, all or none of the job shall be visible. | mvp §2.1; S6 | `store_crash::kill_before_commit_leaves_no_partial_job`, `store_crash::kill_after_commit_keeps_the_job` (the test binary re-runs itself as a child that commits N jobs, then prints a marker inside a large job or after a `COMMIT`, and waits; the parent kills it with `Child::kill` and reopens; both OSes). The during-`COMMIT` case cannot be timed portably; it rests on SQLite's WAL commit-frame rule (S6, a D-fact) |
| PR6-CL-17 | The epoch shall be created once per database and survive every reopen; `open` shall refuse a non-database, a database not encoded in UTF-8, a foreign database, a newer schema, or a changed schema before Chaldea changes the file, and never delete a file; a projection version different from the code's shall rebuild at open, and a store of another projection version shall not write after it. | mvp §2.4 epoch; §2.4, §2.8 | `store_open::epoch_survives_reopen`, `store_open::refuses_not_a_database`, `store_open::refuses_a_non_utf8_database` (an empty UTF-16 database: `Encoding`, file unchanged), `store_open::refuses_foreign_rollback_journal_database` (header bytes 18–19 and file bytes unchanged, no `-wal`), `store_open::refuses_newer_schema`, `store_open::refuses_altered_schema` (a dropped trigger), `store_open::refuses_an_extra_table` (`sqliteXextra`), `store_open::uninitialized_file_is_reused`; unit `open::concurrent_open_failure_keeps_the_other_stores_data` (A creates the file; B opens, initializes, and commits an event; A then fails at an injected fault after step 5; reopening shows B's event), `open::projection_version_change_rebuilds` (higher and lower), `open::stale_projection_version_cannot_write` (two `Store`s on one file built with different versions; after the second rebuilds, the first's next job gets `Failed(ProjectionVersion)`) |
| PR6-CL-18 | When a read runs several statements, all shall see one snapshot: a commit by another connection between two of them shall not be visible, and a new read shall see it; pages shall carry `has_more` and `has_more_before` per §2.7, `head` its `cum_bytes`, and unknown kinds, newer versions, and invalid payloads as stored. | mvp §2.4 | unit `readers::commit_between_selects_is_invisible` (first `SELECT` of head and bytes, then a commit forced through the writer, then a second `SELECT` of head, bytes, and events: old values; a new `ReadTx`: new values), `store_reads::page_flags` (head 3 with `limit` 1 forward and backward, `before_seq` 1, `before_seq = u64::MAX`), `store_reads::head_and_cum_bytes_agree_with_the_page`, unit `readers::undecodable_events_are_returned` (rows inserted raw) |
| PR6-CL-19 | When tokens are inserted and revoked, `token_by_hash` shall return the row with its revocation, a revoked token shall stay revoked, and a second insert of a hash shall fail. | mvp §2.5 (storage) | `store_tokens::insert_revoke_lookup`, unit `triggers::revocation_is_final`, `store_tokens::duplicate_hash_is_refused` |
| PR6-CL-20 | When two `Store`s open one file, their writes shall serialize, and a job of one shall see the other's committed rows. | §5 file locking | `store_open::second_store_serializes_writes` |
| PR6-CL-21 | On Windows, the store shall open, write, and reopen a database under a multi-level multibyte path longer than the `win32` VFS limit. | §5 paths | `store_open::long_multibyte_path` (both OSes), `store_open::long_path_vfs_is_selected` (Windows only, in the sentinel list) |
| PR6-CL-22 | When `shutdown` begins, no clone shall be able to submit or borrow a reader, and both shall close in one transition; the first call shall own the shutdown and every later call shall do no work; each call's deadline shall limit its timed waits for shutdown progress (lock acquisition, synchronous cleanup, and scheduling can delay its return), and the owner shall detach the writer thread without joining it; the writer shall checkpoint only after the owner finished with the readers (closed, or abandoned at the owner's deadline); a report with `writer_exited` shall mean the writer thread published its exit report as its last statement by the caller's deadline (its connection already closed; its thread-local teardown may still run), with every commit notification receiver `Closed` once it drains its buffered notifications (a lagging one gets `Lagged` first); each caller shall count a timestamped publication only if it was published by its deadline (the writer's report for every caller; the owner's report and the notification close for a later call), and the reader fields shall be observations at the caller's checks; when the owner closed every reader in time, on the normal writer path and if the report is available, the writer shall have run its checkpoint after the readers were closed and shall not commit again. | §5 shutdown | `store_open::shutdown_stops_every_clone`, unit `open::in_time_shutdown_checkpoints_after_readers` (a reader held while `shutdown` runs: the writer's checkpoint call has not begun; after the reader returns, fault-point marks record the release, the readers closing, and the checkpoint call's start and end in that order; `readers_returned`, checkpoint `Complete`, `writer_exited`), `store_open::deadline_with_a_held_reader` (a reader held past the deadline: `readers_returned` false, and `writer_exited` and `checkpoint` reported as observed, `NotRun` when not reached); unit `writer::writer_blocked_past_the_deadline` (a job blocks past the deadline: `writer_exited` false, `readers_returned` true; a later call reports the exit); unit `shutdown::intake_closes_for_submit_and_read_together` (gated right after the intake transition: `submit` and `read` both `Closed`), `shutdown::checkpoint_waits_for_the_owner_to_finish_with_the_readers` (the owner gated while it holds the idle readers it took: a later call with an expired deadline returns with both flags false, and the checkpoint gate stays unreached until the owner is released), `shutdown::shorter_shutdown_returns_by_its_deadline` (the owner waits for a writer blocked in a job; a later call with a 1 s deadline returns within it), `shutdown::reported_writer_exit_has_closed_notifications` (the writer held at its last step: the owner returns by its deadline; any report with `writer_exited` comes with `Closed` notifications); `store_open::shutdown_closes_commit_notifications_with_a_live_clone`, unit `shutdown::later_report_of_an_early_writer_exit_has_closed_notifications` (the writer ends on its own while the owner is held after its intake transition: a later call that reports the exit finds notifications `Closed`), `shutdown::owner_detaches_a_writer_still_running` (the owner returns before the writer's last step: the store keeps no handle; a later call reports the exit), `shutdown::later_call_reports_only_progress_by_its_deadline` (a later call that timed out, held while the writer then exits: `writer_exited` false, checkpoint `NotRun`), `shutdown::later_call_past_its_deadline_leaves_out_late_reports` (a later call held before it observes, past its deadline, while the writer then exits: both reports present, still `writer_exited` false and checkpoint `NotRun`), `shutdown::closing_notifications_leaves_the_intake_locks_free` (a receiver's waker, run inline when the owner drops the last notification sender, finds the queue-sender and shutdown-state locks free), `shutdown::later_call_waits_for_the_notification_close_to_report_the_exit` (the writer ended on its own and the owner is held after claiming the shutdown: a later call that reports the exit finds notifications `Closed`), `shutdown::timed_out_owner_detaches_a_finished_writer` (the owner's exit wait timed out and the writer's main returned while a thread-local destructor still runs: the owner returns without joining and the store keeps no handle), `shutdown::later_call_leaves_out_a_notification_close_after_its_deadline` (the writer exited in time, a later call held past its deadline while the owner then closes notifications: `writer_exited` false), `shutdown::later_call_leaves_out_an_owner_report_after_its_deadline` (a later call held past its deadline while the owner then reports: `readers_returned` false), `shutdown::owner_detaches_a_reported_writer_still_tearing_down` (the writer reported in time and its main returned while a thread-local destructor still runs: the owner returns by its deadline with `writer_exited` and keeps no handle), `shutdown::owner_leaves_out_a_writer_report_after_its_deadline` (the owner held after releasing the writer until the writer reported after the owner's deadline: `writer_exited` false, checkpoint `NotRun`) |
| PR6-CL-23 | Every `readiness_observed` shall carry a write basis for its own launch with a non-negative position and its counted classes in the one canonical form, and a retry and a rebuild shall restore it; transcript continuity evidence shall be restored per position: the event position's by rebuild, the read position's across reopen. | mvp §2.1 gate evidence; §4 PR 13 / 14 | unit `annex::golden_json` (both variants, the exact JSON of §2.6), `store_append::readiness_basis_round_trip` (insert, retry: `Existing`, rebuild: same `write_basis`), `store_append::readiness_basis_is_refused` (missing, other launch), unit `triggers::readiness_annex_check` (raw inserts: `counted` as a string spelling an array, negative position, unknown class, non-canonical order, repeated class, missing `counted`), `store_rebuild::event_continuity_restored_by_rebuild`, `store_transcript::read_continuity_survives_reopen` |
| PR6-CL-24 | When `shutdown` returns with `readers_returned`, every reader connection shall be closed (by the owning call, which counts a connection until its handle is dropped) and, once `writer_exited` is also true (the writer's exit report published, its connection closed before), no handle of the store shall keep a database file open. | §5 shutdown | `store_open::shutdown_waits_for_lent_readers` (a reader returned before the deadline: `readers_returned` true), `store_open::shutdown_releases_every_file` (both facts true; on Windows the three files are deleted; on macOS reopened through a new `Store`) |

**Mapping.** Obligations: rebuild equality → PR6-CL-7, CL-8; forbidden statements → CL-2 (CL-3 for the other tables); generation before lines → CL-10; stale-cursor batch → CL-9; blob offload and lossy preview → CL-11, CL-12. mvp §6.2: CL-9 → PR6-CL-1, CL-2; CL-10 (store side) → PR6-CL-5, CL-6; CL-11 → PR6-CL-7, CL-8, CL-23; CL-12 (store side) → PR6-CL-9, CL-10; CL-8's storage → PR6-CL-13. mvp CL-2 (task state) → PR6-CL-8 on the store side. mvp CL-13 (sync) rests on PR6-CL-1, CL-14, CL-16, CL-18.

**Known limits** (no test, by design): a process with raw file access can drop a trigger and edit rows while the store runs (§2.1); `open` detects only the schema change, at the next open. SHA-256 of a blob is the store's Rust guarantee, not the engine's. Power-loss durability is not observable in CI. `ProjectionTx`'s restriction to `proj_*` is a type boundary; a projection that read the clock would pass a test that does not vary it, which is why the rebuild runs with a different clock. A commit-notification waker must return promptly and must not panic or wait for shutdown progress (§2.2): the owning `shutdown` wakes pending receivers inline as it drops the last sender, and a waker that panics or blocks there can leave the shutdown unfinished.

## 7. Tests and Completion Commands

**Two kinds of tests.** A test that needs privileged access is a unit test in `crates/daemon/src/store/tests/` (`triggers`, `writer`, `readers`, `rebuild`, `open`, `annex`): raw SQL, the writer connection's `max_page_count`, raw inserts of undecodable rows, dropping projection tables, fault points and panics outside the closure, a second `Store` built with another projection version. These open connections with the production `open::configure` and the shipped DDL, so the pragmas and triggers under test are the production ones. A test that needs only the public API is an integration test in `crates/daemon/tests/store_*.rs`. Both run in `cargo test --workspace --locked` on both runners.

Each test opens a store in its own `tempfile::TempDir` (OS temp directory) with a fixed `Clock` and a counter `IdSource`, and calls `shutdown` before the directory drops (Windows cannot delete open files). `tests/support/mod.rs` holds a history model: a proptest strategy that walks threads, launches, sessions, inputs, approvals, surfaces, worktrees, and transcripts, emits valid jobs (with annexes, no-event advances, re-reads, empty surfaces, launchless inputs, blob content, unknown kinds through the unit-test raw helper, and jobs that fail midway), and computes the expected projection values itself. Property tests use proptest's default 256 cases with histories of at most 200 events.

Completion commands (mvp §5, extended):

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
deps="$(cargo tree -p chaldea-core -e normal --prefix none)" && ! grep -Eq '^(tokio|rusqlite|axum) ' <<<"$deps"
deps="$(cargo tree -p chaldea-pty-runtime -e normal --prefix none)" && ! grep -Eq '^tokio ' <<<"$deps"
```

plus the `workspace` job green on both runners with `store_open::long_path_vfs_is_selected` added to the Windows sentinel list.

## 8. Decisions

**D1. `crates/daemon` starts with the store only.** No HTTP, no binary. Dependencies: `rusqlite =0.41.1` (`bundled`, no defaults), `sha2 =0.11.1`, `tokio =1.54.1` (`sync` only), `base64 =0.22.1`, `uuid` with `v4`, dev `proptest` and `tempfile =3.27.0`.

**D2. Bundled SQLite 3.54.1.** One SQLite on both runners; the schema relies on `STRICT`, generated columns, `RETURNING`, `octet_length`, JSON functions in triggers and checks, and `trusted_schema=OFF` with them (S9).

**D3. One writer thread, jobs as closures over one `BEGIN IMMEDIATE` transaction, poisoned on the first error, deadline checked again under the lock; a pool of four read-only connections.** The smallest form PR 7–10 can use: a check and its write are one job, async callers await the ticket, sync tests call `blocking_wait`.

**D4. Invariants live in the schema.** Gapless `seq`, global order, `cum_bytes`, stream membership, append-only events, transcript generation order and path, typed blob refs and sizes, receipt and delivery transitions, token revocation: triggers and `NULL`-safe constraints with `RAISE(ROLLBACK)`; Rust checks first for typed errors. Compare-and-set `UPDATE`s guard projection state transitions.

**D5. `synchronous=FULL` with `fullfsync=ON` is the MVP contract.** A hook's first reply and a command's intent are durable before the response, including on macOS power loss. Cost: one `F_FULLFSYNC` per job on macOS; one job per request keeps it to one flush per request.

**D6. One projection function, `project::apply`, for live appends and rebuild**, restricted to `proj_*` tables by type; rebuild replays by `global_seq`. `status_changed` changes `process_state`, `turn_state`, and attention; `task_set` changes `task_state`, and the paired `status_changed(TaskState)` is checked at write time and not applied. Every write job refuses when the stored `projection_version` is not its own. A physical DDL change is a migration; a change of interpretation on the same DDL bumps `projection_version` and rebuilds at open.

**D7. The `events.annex` column carries the typed internal values projections need**: the readiness `WriteBasis` (launch, position, counted classes), required on every `readiness_observed`, and transcript continuity per event. It is immutable with the event and never served, so rebuild reproduces both without a public type change. Rejected: a public `write_basis` payload field (exposes a daemon-internal PTY position to clients and the relay), and keeping the basis only in the supervisor's memory (contradicts mvp §2.1 and breaks rebuild equality).

**D8. Blobs in the database, verified at the store boundary, never deleted in the MVP**, of any size up to `MAX_SAFE_INTEGER` bytes (`MAX_BLOB_BYTES`, 64 MiB, bounds a value held in memory and one `blobs` row; a larger one is spooled and stored as parts, C1 D14); refs from typed `Content` fields only; the body shared by SHA-256 and the media type kept per reference, so the same bytes under two types never cost an event.

**D9. Receipts, hook deliveries, and tokens are kept forever in the MVP.** Deletion needs retention rules (a receipt deleted while a client may still retry would re-run the command) that no MVP PR owns; a later migration can add them and must keep mvp §2.4's receipt semantics.

**D10. `open` refuses before it changes anything, never deletes, and never repairs.** Not a database, not UTF-8, foreign, newer schema, or changed schema: an error with the file as found (apart from what SQLite itself does on open, §2.8). A failed first open may leave an uninitialized file, which the next open reuses. Corruption at run time stops writes. Integrity checks are the doctor's (deferred, mvp §4 "Deferred").

**D11. `Content` previews leave out a valid character cut by the 1 KiB window** without setting `preview_lossy`, and replace every invalid sequence, including one at the boundary, with U+FFFD and set it (§2.5). mvp §2.1 and the `core::payload::Content::Blob` doc comment say so; no type changes and no stored value existed before. Rejected: decoding exactly the first 1024 bytes lossily, under which most Japanese text above 64 KiB would be flagged lossy because of where the window ends.

### Open Questions

None that change a public contract or are irreversible.

## Related Text in Other Documents

- `docs/mvp-design.md` §2.1 `Content` and `crates/core/src/payload.rs` (`PREVIEW_LIMIT_BYTES`, `Content::Blob::preview`, `preview_lossy`), with the generated `packages/contracts/src/generated/Content.ts` and `packages/contracts/schema/{TypedEvent,EventsResponse}.schema.json`: the cut-character rule (D11).
- `docs/mvp-design.md` §2.1 "Store": the trigger set here adds `RAISE(ROLLBACK)` and engine checks for `seq`, global order, `cum_bytes`, membership, and transcript order; the projection table's "write basis" lives in the internal annex (D7).
- `docs/mvp-design.md` §2.1 "Projections" row "axes, attention set | `status_changed` (plus `task_set`)": `task_set` changes `task_state`; the paired `status_changed(TaskState)` is checked at write time and changes nothing (§2.6).
- `docs/mvp-design.md` §2.9 "ordered by receipt order at the daemon": PR 9 assigns the receipt ordinal at intake and the store keeps it (§2.3).
- `docs/mvp-design.md` §4 PR 8: the single-instance lock, released only after `ShutdownReport::writer_exited`, the store shutdown rules (blocking call, one budget, file access, stop triggers), and the state directory location (§5).
