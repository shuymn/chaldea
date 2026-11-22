//! The schema: migrations, the shipped fingerprint of each version, and the projection
//! version (`docs/design/event-store.md` §2.4).
//!
//! Every invariant the schema can express is a constraint or a trigger. Each `CHECK` is
//! written so that a `NULL` operand fails it (a `CHECK` whose expression is `NULL` passes),
//! and every trigger refuses with `RAISE(ROLLBACK, '<name>: …')`, which ends the whole
//! transaction.

/// `application_id` of a Chaldea database: "CHLD".
pub(crate) const APPLICATION_ID: i32 = 0x4348_4C44;

/// How `project::apply` interprets events on the current DDL. A change of interpretation
/// without a DDL change bumps it; `open` then rebuilds the projections, and a process with
/// another value refuses to write (§2.2 step 4).
pub(crate) const PROJECTION_VERSION: u32 = 1;

/// Ordered migration scripts; `MIGRATIONS[i]` brings `user_version` from `i` to `i + 1`.
/// Any physical DDL change, projection tables included, is a new migration.
pub(crate) const MIGRATIONS: &[&str] = &[V1];

/// SHA-256 (lowercase hex) of the schema dump of each version (`open::schema_fingerprint`),
/// in the order of `MIGRATIONS`. `schema_tests::shipped_fingerprints_match_the_migrations`
/// recomputes them, so a DDL edit without a migration fails a test.
pub(crate) const FINGERPRINTS: &[&str] =
    &["5edc542e0d4bec60b31ea9bb397bde06bae509e4366ac30c27fffdb9a812f536"];

const V1: &str = r#"
CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
) STRICT;

CREATE TABLE events (
  global_seq        INTEGER PRIMARY KEY,
  stream_id         TEXT NOT NULL,
  seq               INTEGER NOT NULL CHECK (seq >= 1),
  event_id          TEXT NOT NULL UNIQUE,
  schema_version    INTEGER NOT NULL CHECK (schema_version >= 1),
  idempotency_key   TEXT NOT NULL CHECK (length(idempotency_key) > 0),
  kind              TEXT NOT NULL CHECK (length(kind) > 0),
  thread_id         TEXT,
  launch_id         TEXT,
  command_id        TEXT,
  source            TEXT NOT NULL CHECK (json_valid(source)),
  occurred_at       TEXT NOT NULL,
  occurred_explicit INTEGER NOT NULL CHECK (occurred_explicit IN (0, 1)),
  recorded_at       TEXT NOT NULL,
  payload           TEXT NOT NULL CHECK (json_valid(payload) AND json_type(payload) = 'object'),
  payload_bytes     INTEGER NOT NULL CHECK (payload_bytes = octet_length(payload)),
  cum_bytes         INTEGER NOT NULL,
  annex             TEXT CHECK (annex IS NULL OR json_valid(annex)),
  src_kind          TEXT    GENERATED ALWAYS AS (json_extract(source, '$.kind')) VIRTUAL,
  tr_generation     INTEGER GENERATED ALWAYS AS (json_extract(source, '$.transcript.generation')) VIRTUAL,
  tr_path           TEXT    GENERATED ALWAYS AS (json_extract(source, '$.transcript.path')) VIRTUAL,
  tr_file_id        TEXT    GENERATED ALWAYS AS (json_extract(source, '$.transcript.file_id')) VIRTUAL,
  tr_offset_end     INTEGER GENERATED ALWAYS AS (json_extract(source, '$.transcript.offset_end')) VIRTUAL,
  UNIQUE (stream_id, seq),
  UNIQUE (stream_id, idempotency_key),
  CHECK (stream_id = 'host' OR (thread_id IS NOT NULL AND stream_id = 'thread:' || thread_id)),
  CHECK (kind NOT IN ('thread_registered', 'thread_closed', 'worktree_changed') OR stream_id = 'host'),
  CHECK (stream_id <> 'host'
         OR kind IN ('thread_registered', 'thread_closed', 'worktree_changed', 'surface_update', 'raw_unknown')),
  CHECK (json_extract(payload, '$.thread_id') IS NULL OR json_extract(payload, '$.thread_id') IS thread_id),
  CHECK (json_extract(payload, '$.launch_id') IS NULL OR json_extract(payload, '$.launch_id') IS launch_id),
  CHECK (kind <> 'mailbox_message' OR stream_id IS 'thread:' || json_extract(payload, '$.to_thread_id')),
  CHECK (kind <> 'surface_update'
         OR (json_extract(payload, '$.scope.kind') IS 'thread'
             AND stream_id IS 'thread:' || json_extract(payload, '$.scope.thread_id'))
         OR ((json_extract(payload, '$.scope.kind') IS 'host' OR json_extract(payload, '$.scope.kind') IS 'workspace')
             AND stream_id = 'host')),
  CHECK (src_kind IS NOT 'transcript'
         OR (tr_generation IS NOT NULL AND tr_generation >= 1 AND tr_path IS NOT NULL
             AND tr_file_id IS NOT NULL AND tr_offset_end IS NOT NULL AND tr_offset_end >= 0)),
  CHECK (kind <> 'transcript_generation'
         OR (src_kind IS 'transcript' AND tr_offset_end IS 0
             AND json_extract(payload, '$.generation') IS tr_generation
             AND json_extract(payload, '$.path') IS tr_path
             AND json_extract(payload, '$.file_id') IS tr_file_id)),
  CHECK (kind <> 'readiness_observed'
         OR (json_extract(annex, '$.readiness.launch_id') IS launch_id AND launch_id IS NOT NULL
             AND json_type(annex, '$.readiness.position') IS 'integer'
             AND json_extract(annex, '$.readiness.position') >= 0
             AND json_type(annex, '$.readiness.counted') IS 'array'
             AND coalesce(json_extract(annex, '$.readiness.counted'), '') IN (
               '[]', '["input"]', '["human"]', '["reply"]', '["input","human"]',
               '["input","reply"]', '["human","reply"]', '["input","human","reply"]')))
) STRICT;
CREATE INDEX events_kind ON events (stream_id, kind, seq);
CREATE INDEX events_transcript ON events (stream_id, tr_generation, tr_offset_end) WHERE src_kind = 'transcript';

CREATE TRIGGER events_no_update BEFORE UPDATE ON events
BEGIN
  SELECT RAISE(ROLLBACK, 'events_no_update: events are append-only');
END;

CREATE TRIGGER events_no_delete BEFORE DELETE ON events
BEGIN
  SELECT RAISE(ROLLBACK, 'events_no_delete: events are append-only');
END;

CREATE TRIGGER events_order BEFORE INSERT ON events
WHEN NEW.seq IS NOT coalesce((SELECT max(seq) FROM events WHERE stream_id = NEW.stream_id), 0) + 1
  OR NEW.cum_bytes IS NOT coalesce((SELECT cum_bytes FROM events WHERE stream_id = NEW.stream_id
                                    ORDER BY seq DESC LIMIT 1), 0) + NEW.payload_bytes
BEGIN
  SELECT RAISE(ROLLBACK, 'events_order: seq or cum_bytes is not the stream''s next');
END;

CREATE TRIGGER events_global_order AFTER INSERT ON events
WHEN NEW.global_seq IS NOT (SELECT max(global_seq) FROM events)
BEGIN
  SELECT RAISE(ROLLBACK, 'events_global_order: global_seq is below an existing one');
END;

CREATE TRIGGER events_generation BEFORE INSERT ON events
WHEN NEW.kind = 'transcript_generation'
BEGIN
  SELECT RAISE(ROLLBACK, 'events_generation: not the next generation, or not the current launch and its bound path')
  WHERE NEW.tr_generation IS NOT coalesce((SELECT max(tr_generation) FROM events
                                           WHERE stream_id = NEW.stream_id AND kind = 'transcript_generation'), 0) + 1
     OR NEW.launch_id IS NOT (SELECT launch_id FROM events
                              WHERE stream_id = NEW.stream_id AND kind = 'launch_reserved'
                              ORDER BY seq DESC LIMIT 1)
     OR NEW.tr_path IS NOT (SELECT json_extract(payload, '$.transcript_path') FROM events
                            WHERE stream_id = NEW.stream_id AND kind = 'session_bound' AND launch_id IS NEW.launch_id
                            ORDER BY seq DESC LIMIT 1);
END;

CREATE TRIGGER events_transcript_line BEFORE INSERT ON events
WHEN NEW.src_kind = 'transcript' AND NEW.kind <> 'transcript_generation'
BEGIN
  SELECT RAISE(ROLLBACK, 'events_transcript_line: not the latest generation, launch, or bound path, or the offset does not increase')
  WHERE NEW.tr_generation IS NOT (SELECT tr_generation FROM events
                                  WHERE stream_id = NEW.stream_id AND kind = 'transcript_generation'
                                  ORDER BY seq DESC LIMIT 1)
     OR NEW.tr_path IS NOT (SELECT tr_path FROM events
                            WHERE stream_id = NEW.stream_id AND kind = 'transcript_generation'
                            ORDER BY seq DESC LIMIT 1)
     OR NEW.tr_file_id IS NOT (SELECT tr_file_id FROM events
                               WHERE stream_id = NEW.stream_id AND kind = 'transcript_generation'
                               ORDER BY seq DESC LIMIT 1)
     OR NEW.launch_id IS NOT (SELECT launch_id FROM events
                              WHERE stream_id = NEW.stream_id AND kind = 'launch_reserved'
                              ORDER BY seq DESC LIMIT 1)
     OR NEW.tr_path IS NOT (SELECT json_extract(payload, '$.transcript_path') FROM events
                            WHERE stream_id = NEW.stream_id AND kind = 'session_bound' AND launch_id IS NEW.launch_id
                            ORDER BY seq DESC LIMIT 1)
     OR EXISTS (SELECT 1 FROM events
                WHERE stream_id = NEW.stream_id AND src_kind = 'transcript' AND kind <> 'transcript_generation'
                  AND tr_generation = NEW.tr_generation AND tr_offset_end >= NEW.tr_offset_end);
END;

CREATE TABLE blobs (
  sha256 TEXT PRIMARY KEY CHECK (length(sha256) = 64 AND sha256 NOT GLOB '*[^0-9a-f]*'),
  size   INTEGER NOT NULL CHECK (size = length(data) AND size > 65536 AND size <= 67108864),
  data   BLOB NOT NULL
) STRICT;

CREATE TABLE blob_refs (
  stream_id  TEXT NOT NULL,
  sha256     TEXT NOT NULL REFERENCES blobs (sha256),
  media_type TEXT NOT NULL CHECK (length(media_type) > 0),
  PRIMARY KEY (stream_id, sha256, media_type)
) STRICT, WITHOUT ROWID;

CREATE TRIGGER events_blob_refs AFTER INSERT ON events
BEGIN
  SELECT RAISE(ROLLBACK, 'events_blob_refs: a typed blob reference has no blob_refs row')
  WHERE EXISTS (
    SELECT 1 FROM (
      SELECT json_extract(NEW.payload, p.path || '.blob.sha256') AS sha,
             json_extract(NEW.payload, p.path || '.blob.media_type') AS media_type
      FROM (SELECT '$.prompt' AS path WHERE NEW.kind = 'user_message'
            UNION ALL SELECT '$.text' WHERE NEW.kind IN ('assistant_message', 'input_created')
            UNION ALL SELECT '$.input' WHERE NEW.kind = 'tool_activity'
            UNION ALL SELECT '$.output' WHERE NEW.kind = 'tool_activity'
            UNION ALL SELECT '$.tool_input' WHERE NEW.kind = 'approval_requested'
            UNION ALL SELECT '$.body' WHERE NEW.kind = 'mailbox_message'
            UNION ALL SELECT '$.grid' WHERE NEW.kind = 'vt_snapshot_taken'
            UNION ALL SELECT '$.raw' WHERE NEW.kind = 'raw_unknown') AS p
      WHERE json_type(NEW.payload, p.path || '.blob') IS 'object'
      UNION ALL
      SELECT json_extract(t.value, '$.blob.sha256'), json_extract(t.value, '$.blob.media_type')
      FROM json_tree(NEW.payload, '$.components') AS t
      WHERE NEW.kind = 'surface_update' AND t.key IS 'grid' AND t.type IS 'object'
        AND json_type(t.value, '$.blob') IS 'object'
        AND json_extract(NEW.payload, t.path || '.type') IS 'vt_snapshot'
        AND t.fullkey NOT GLOB '*.action.args*'
    ) AS r
    WHERE NOT EXISTS (SELECT 1 FROM blob_refs
                      WHERE stream_id = NEW.stream_id AND sha256 IS r.sha AND media_type IS r.media_type)
  );
END;

CREATE TABLE command_receipts (
  command_id     TEXT PRIMARY KEY,
  token_id       TEXT NOT NULL,
  request_sha256 BLOB NOT NULL CHECK (length(request_sha256) = 32),
  status         TEXT NOT NULL CHECK (status IN ('in_progress', 'completed', 'unknown_after_crash')),
  http_status    INTEGER,
  body           TEXT,
  created_at     TEXT NOT NULL,
  finished_at    TEXT,
  CHECK ((status = 'completed' AND http_status IS NOT NULL AND body IS NOT NULL AND finished_at IS NOT NULL)
      OR (status = 'unknown_after_crash' AND http_status IS NULL AND body IS NULL AND finished_at IS NOT NULL)
      OR (status = 'in_progress' AND http_status IS NULL AND body IS NULL AND finished_at IS NULL))
) STRICT;

CREATE TABLE hook_deliveries (
  launch_id       TEXT NOT NULL,
  emit_id         TEXT NOT NULL,
  receipt_ordinal INTEGER NOT NULL CHECK (receipt_ordinal >= 1),
  commit_ordinal  INTEGER NOT NULL CHECK (commit_ordinal >= 1),
  reply_status    INTEGER NOT NULL,
  reply_body      TEXT NOT NULL,
  routing         TEXT NOT NULL CHECK (routing IN ('appended', 'buffered', 'raw_unknown')),
  stream_id       TEXT,
  seq             INTEGER,
  pending_body    BLOB,
  pending_meta    TEXT CHECK (pending_meta IS NULL OR json_valid(pending_meta)),
  received_at     TEXT NOT NULL,
  PRIMARY KEY (launch_id, emit_id),
  UNIQUE (launch_id, receipt_ordinal),
  UNIQUE (launch_id, commit_ordinal),
  FOREIGN KEY (stream_id, seq) REFERENCES events (stream_id, seq),
  CHECK ((routing = 'buffered' AND stream_id IS NULL AND seq IS NULL
          AND pending_body IS NOT NULL AND pending_meta IS NOT NULL)
      OR (routing <> 'buffered' AND stream_id IS NOT NULL AND seq IS NOT NULL
          AND pending_body IS NULL AND pending_meta IS NULL))
) STRICT, WITHOUT ROWID;

CREATE TABLE tokens (
  token_id   TEXT PRIMARY KEY,
  kind       TEXT NOT NULL CHECK (kind IN ('owner', 'agent', 'rescue', 'hook', 'extension')),
  sha256     BLOB NOT NULL UNIQUE CHECK (length(sha256) = 32),
  thread_id  TEXT,
  launch_id  TEXT,
  created_at TEXT NOT NULL,
  revoked_at TEXT
) STRICT;

CREATE TRIGGER meta_no_update BEFORE UPDATE ON meta
WHEN OLD.key IS NOT 'projection_version' OR NEW.key IS NOT OLD.key
BEGIN
  SELECT RAISE(ROLLBACK, 'meta_no_update: only projection_version may change');
END;

CREATE TRIGGER meta_no_delete BEFORE DELETE ON meta
BEGIN
  SELECT RAISE(ROLLBACK, 'meta_no_delete: meta rows are permanent');
END;

CREATE TRIGGER blobs_no_update BEFORE UPDATE ON blobs
BEGIN
  SELECT RAISE(ROLLBACK, 'blobs_no_update: blobs are immutable');
END;

CREATE TRIGGER blobs_no_delete BEFORE DELETE ON blobs
BEGIN
  SELECT RAISE(ROLLBACK, 'blobs_no_delete: blobs are permanent');
END;

CREATE TRIGGER blob_refs_no_update BEFORE UPDATE ON blob_refs
BEGIN
  SELECT RAISE(ROLLBACK, 'blob_refs_no_update: blob refs are immutable');
END;

CREATE TRIGGER blob_refs_no_delete BEFORE DELETE ON blob_refs
BEGIN
  SELECT RAISE(ROLLBACK, 'blob_refs_no_delete: blob refs are permanent');
END;

CREATE TRIGGER command_receipts_finish BEFORE UPDATE ON command_receipts
WHEN OLD.status IS NOT 'in_progress'
  OR NEW.status NOT IN ('completed', 'unknown_after_crash')
  OR NEW.command_id IS NOT OLD.command_id
  OR NEW.token_id IS NOT OLD.token_id
  OR NEW.request_sha256 IS NOT OLD.request_sha256
  OR NEW.created_at IS NOT OLD.created_at
BEGIN
  SELECT RAISE(ROLLBACK, 'command_receipts_finish: only in_progress may finish, and the key, token, hash, and created_at never change');
END;

CREATE TRIGGER command_receipts_no_delete BEFORE DELETE ON command_receipts
BEGIN
  SELECT RAISE(ROLLBACK, 'command_receipts_no_delete: receipts are permanent');
END;

CREATE TRIGGER hook_deliveries_order BEFORE INSERT ON hook_deliveries
WHEN NEW.commit_ordinal IS NOT coalesce((SELECT max(commit_ordinal) FROM hook_deliveries
                                         WHERE launch_id = NEW.launch_id), 0) + 1
BEGIN
  SELECT RAISE(ROLLBACK, 'hook_deliveries_order: commit_ordinal is not the launch''s next');
END;

CREATE TRIGGER hook_deliveries_promote BEFORE UPDATE ON hook_deliveries
WHEN OLD.routing IS NOT 'buffered'
  OR NEW.routing NOT IN ('appended', 'raw_unknown')
  OR NEW.launch_id IS NOT OLD.launch_id
  OR NEW.emit_id IS NOT OLD.emit_id
  OR NEW.receipt_ordinal IS NOT OLD.receipt_ordinal
  OR NEW.commit_ordinal IS NOT OLD.commit_ordinal
  OR NEW.reply_status IS NOT OLD.reply_status
  OR NEW.reply_body IS NOT OLD.reply_body
  OR NEW.received_at IS NOT OLD.received_at
BEGIN
  SELECT RAISE(ROLLBACK, 'hook_deliveries_promote: only buffered may be promoted, and the key, ordinals, reply, and received_at never change');
END;

CREATE TRIGGER hook_deliveries_no_delete BEFORE DELETE ON hook_deliveries
BEGIN
  SELECT RAISE(ROLLBACK, 'hook_deliveries_no_delete: deliveries are permanent');
END;

CREATE TRIGGER tokens_revoke BEFORE UPDATE ON tokens
WHEN OLD.revoked_at IS NOT NULL
  OR NEW.revoked_at IS NULL
  OR NEW.token_id IS NOT OLD.token_id
  OR NEW.kind IS NOT OLD.kind
  OR NEW.sha256 IS NOT OLD.sha256
  OR NEW.thread_id IS NOT OLD.thread_id
  OR NEW.launch_id IS NOT OLD.launch_id
  OR NEW.created_at IS NOT OLD.created_at
BEGIN
  SELECT RAISE(ROLLBACK, 'tokens_revoke: only an unset revoked_at may be set');
END;

CREATE TRIGGER tokens_no_delete BEFORE DELETE ON tokens
BEGIN
  SELECT RAISE(ROLLBACK, 'tokens_no_delete: tokens are permanent');
END;

CREATE TABLE proj_threads (
  thread_id         TEXT PRIMARY KEY,
  agent             TEXT NOT NULL CHECK (agent IN ('claude', 'codex')),
  cwd               TEXT NOT NULL,
  workspace_id      TEXT NOT NULL,
  parent_thread_id  TEXT,
  role              TEXT NOT NULL CHECK (role IN ('standard', 'rescue')),
  closed            INTEGER NOT NULL CHECK (closed IN (0, 1)),
  process_state     TEXT NOT NULL CHECK (process_state IN ('starting', 'running', 'stopping', 'exited')),
  turn_state        TEXT NOT NULL CHECK (turn_state IN ('unknown', 'idle', 'running', 'awaiting_approval')),
  task_state        TEXT NOT NULL
                    CHECK (task_state IN ('none', 'active', 'blocked', 'completed', 'failed', 'cancelled')),
  attention         TEXT NOT NULL CHECK (json_valid(attention) AND json_type(attention) = 'array'),
  current_launch_id TEXT,
  session_id        TEXT,
  transcript_path   TEXT,
  registered_seq    INTEGER NOT NULL CHECK (registered_seq >= 1)
) STRICT;

CREATE TABLE proj_launches (
  launch_id            TEXT PRIMARY KEY,
  thread_id            TEXT NOT NULL REFERENCES proj_threads (thread_id),
  reserved_seq         INTEGER NOT NULL CHECK (reserved_seq >= 1),
  expected_previous    TEXT,
  agent                TEXT NOT NULL CHECK (agent IN ('claude', 'codex')),
  argv_digest          TEXT NOT NULL,
  manual               INTEGER NOT NULL CHECK (manual IN (0, 1)),
  worktree_id          TEXT,
  pid                  INTEGER,
  start_time           TEXT,
  pgid_or_job          TEXT,
  cli_version          TEXT,
  exit_code            INTEGER,
  exit_signal          INTEGER,
  exit_reason          TEXT CHECK (exit_reason IS NULL
                                   OR exit_reason IN ('exit', 'stopped', 'spawn_failed', 'daemon_restart', 'unknown_after_crash')),
  death_confirmed      INTEGER NOT NULL CHECK (death_confirmed IN (0, 1)),
  death_method         TEXT,
  session_id           TEXT,
  readiness_seq        INTEGER,
  readiness_evidence   TEXT CHECK (readiness_evidence IS NULL
                                   OR readiness_evidence IN ('session_start', 'stop_settled', 'post_decision', 'owner_asserted')),
  write_basis          TEXT CHECK (write_basis IS NULL OR json_valid(write_basis)),
  last_write_seq       INTEGER,
  last_human_write_seq INTEGER,
  last_turn_start_seq  INTEGER
) STRICT;

CREATE TABLE proj_inputs (
  input_id       TEXT PRIMARY KEY,
  thread_id      TEXT NOT NULL REFERENCES proj_threads (thread_id),
  launch_id      TEXT,
  state          TEXT NOT NULL CHECK (state IN ('queued', 'sending', 'sent', 'acked', 'unconfirmed', 'rejected',
                                                'cancelled', 'dismissed')),
  text           TEXT NOT NULL CHECK (json_valid(text)),
  encoding       TEXT NOT NULL CHECK (encoding IN ('bracketed_paste', 'win_keys', 'positional')),
  origin         TEXT NOT NULL CHECK (origin IN ('cli', 'pwa', 'surface_action')),
  created_by     TEXT NOT NULL,
  sent_after_seq INTEGER,
  ack_seq        INTEGER,
  created_seq    INTEGER NOT NULL CHECK (created_seq >= 1)
) STRICT;

CREATE TABLE proj_approvals (
  approval_id  TEXT PRIMARY KEY,
  thread_id    TEXT NOT NULL REFERENCES proj_threads (thread_id),
  launch_id    TEXT NOT NULL,
  requested    TEXT NOT NULL CHECK (json_valid(requested)),
  opened_seq   INTEGER NOT NULL CHECK (opened_seq >= 1),
  outcome      TEXT CHECK (outcome IS NULL OR outcome IN ('ran', 'expired', 'superseded')),
  resolved_seq INTEGER,
  CHECK ((outcome IS NULL) = (resolved_seq IS NULL))
) STRICT;

CREATE TABLE proj_surfaces (
  stream_id   TEXT NOT NULL,
  surface_id  TEXT NOT NULL,
  scope_kind  TEXT NOT NULL CHECK (scope_kind IN ('host', 'workspace', 'thread')),
  scope_id    TEXT,
  payload     TEXT NOT NULL CHECK (json_valid(payload)),
  updated_seq INTEGER NOT NULL CHECK (updated_seq >= 1),
  CHECK ((scope_kind = 'host' AND scope_id IS NULL) OR (scope_kind <> 'host' AND scope_id IS NOT NULL)),
  PRIMARY KEY (stream_id, surface_id)
) STRICT;

CREATE TABLE proj_worktrees (
  worktree_id     TEXT PRIMARY KEY,
  record          TEXT NOT NULL CHECK (json_valid(record)),
  state           TEXT NOT NULL CHECK (state IN ('creating', 'active', 'released', 'removing', 'uncertain',
                                                 'removed', 'failed')),
  owner_thread_id TEXT,
  workspace_id    TEXT NOT NULL,
  updated_seq     INTEGER NOT NULL CHECK (updated_seq >= 1)
) STRICT;

CREATE TABLE proj_transcript_cursors (
  thread_id        TEXT PRIMARY KEY REFERENCES proj_threads (thread_id),
  generation       INTEGER NOT NULL CHECK (generation >= 1),
  path             TEXT NOT NULL,
  file_id          TEXT NOT NULL,
  event_offset     INTEGER NOT NULL CHECK (event_offset >= 0),
  event_continuity TEXT NOT NULL,
  read_offset      INTEGER NOT NULL,
  read_continuity  TEXT NOT NULL,
  CHECK (read_offset >= event_offset)
) STRICT;
"#;
