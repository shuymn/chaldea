//! The schema's triggers and constraints, through raw SQL on production-configured
//! connections (PR6-CL-1, CL-2, CL-3, CL-6, CL-10, CL-19, CL-23).

use crate::store::rows::sql_len;
use rusqlite::Connection;
use rusqlite::config::DbConfig;

use super::helpers::{
    DAEMON_SOURCE, RawEvent, dump, is_constraint, message, raw, raw_without_recursive_triggers,
    transcript_source,
};
use super::support::{
    TestStore, batch, bound, bound_thread, launch, line, new_generation, pos, reserved, started,
    thread,
};

const A: &str = "/work/a.jsonl";

/// A store with a thread bound to path A, generation 1 (`f1`) and one line (0..10), shut
/// down so raw connections can run alone.
fn fixture() -> TestStore {
    let mut s = TestStore::new();
    let (t, l) = (thread(1), launch(1));
    bound_thread(&s, t, l, Some(A));
    let mut b = batch(t, l, A, None);
    b.new_generation = Some(new_generation("f1"));
    s.run(move |tx| Ok(tx.append_transcript(b)?)).unwrap();
    let mut b = batch(t, l, A, Some(pos(1, A, "f1", 0)));
    b.lines = vec![line(0, 10, "one")];
    b.advance_to.read_offset = 10;
    s.run(move |tx| Ok(tx.append_transcript(b)?)).unwrap();
    s.shutdown();
    s
}

fn t1() -> String {
    format!("thread:{}", thread(1))
}

fn l1() -> String {
    launch(1).to_string()
}

/// Runs `stmt` after an earlier write in the same transaction; asserts that it failed and
/// that the transaction ended with the earlier write gone (`RAISE(ROLLBACK)`).
fn assert_rolls_back(
    c: &Connection,
    what: &str,
    stmt: impl FnOnce(&Connection) -> rusqlite::Result<usize>,
) -> String {
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    c.execute("INSERT INTO meta (key, value) VALUES ('probe', 'x')", [])
        .unwrap();
    let err = match stmt(c) {
        Ok(n) => panic!("{what}: the statement changed {n} rows"),
        Err(e) => e,
    };
    assert!(
        c.is_autocommit(),
        "{what}: the transaction is still open after {err}"
    );
    let probe: i64 = c
        .query_row("SELECT count(*) FROM meta WHERE key = 'probe'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        probe, 0,
        "{what}: the earlier write of the transaction survived"
    );
    message(&err)
}

/// Runs `stmt` after an earlier write; asserts that the statement failed alone and the
/// transaction is still open with the earlier write (a `CHECK` or foreign key).
fn assert_statement_fails(
    c: &Connection,
    what: &str,
    stmt: impl FnOnce(&Connection) -> rusqlite::Result<usize>,
) {
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    c.execute("INSERT INTO meta (key, value) VALUES ('probe', 'x')", [])
        .unwrap();
    let err = match stmt(c) {
        Ok(n) => panic!("{what}: the statement changed {n} rows"),
        Err(e) => e,
    };
    assert!(is_constraint(&err), "{what}: {err}");
    assert!(!c.is_autocommit(), "{what}: the transaction ended");
    let probe: i64 = c
        .query_row("SELECT count(*) FROM meta WHERE key = 'probe'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(probe, 1, "{what}: the earlier write is gone");
    c.execute_batch("ROLLBACK").unwrap();
}

fn host_payload_len(c: &Connection) -> (i64, i64) {
    c.query_row(
        "SELECT max(seq), (SELECT cum_bytes FROM events WHERE stream_id = 'host' ORDER BY seq DESC LIMIT 1)
         FROM events WHERE stream_id = 'host'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

const SURFACE: &str = r#"{"surface_id":"s","scope":{"kind":"host"},"components":[]}"#;

/// `seq` must be the stream's next: a gap, a repeat, and 0 are refused.
#[test]
fn out_of_order_seq_is_refused() {
    let s = fixture();
    let c = raw(&s.path());
    let (head, _) = host_payload_len(&c);
    for (what, seq) in [("gap", head + 2), ("repeat", head), ("zero", 0)] {
        let mut e = RawEvent::new("host", &format!("k-{what}"), "surface_update", SURFACE);
        e.seq = Some(seq);
        let m = assert_rolls_back(&c, what, |c| e.insert(c));
        assert!(m.contains("events_order"), "{what}: {m}");
    }
    // The control: the next seq passes.
    RawEvent::new("host", "k-next", "surface_update", SURFACE)
        .insert(&c)
        .unwrap();
}

/// `cum_bytes` must be the running sum.
#[test]
fn wrong_cum_bytes_is_refused() {
    let s = fixture();
    let c = raw(&s.path());
    let (_, cum) = host_payload_len(&c);
    let mut e = RawEvent::new("host", "k", "surface_update", SURFACE);
    e.cum_bytes = Some(cum + sql_len(SURFACE.len()) + 1);
    let m = assert_rolls_back(&c, "cum_bytes", |c| e.insert(c));
    assert!(m.contains("events_order"), "{m}");
}

/// `payload_bytes` must be the payload's byte length, even when `cum_bytes` agrees with it.
#[test]
fn wrong_payload_bytes_is_refused() {
    let s = fixture();
    let c = raw(&s.path());
    let (_, cum) = host_payload_len(&c);
    let mut e = RawEvent::new("host", "k", "surface_update", SURFACE);
    e.payload_bytes = Some(sql_len(SURFACE.len()) + 1);
    e.cum_bytes = Some(cum + sql_len(SURFACE.len()) + 1);
    assert_statement_fails(&c, "payload_bytes", |c| e.insert(c));
}

/// An explicit `global_seq` below an existing one is refused.
#[test]
fn explicit_lower_global_seq_is_refused() {
    let s = fixture();
    let c = raw(&s.path());
    let max: i64 = c
        .query_row("SELECT max(global_seq) FROM events", [], |r| r.get(0))
        .unwrap();
    let mut high = RawEvent::new("host", "k-high", "surface_update", SURFACE);
    high.global_seq = Some(max + 10);
    high.insert(&c).unwrap();
    let mut low = RawEvent::new("host", "k-low", "surface_update", SURFACE);
    low.global_seq = Some(max + 5);
    let m = assert_rolls_back(&c, "lower global_seq", |c| low.insert(c));
    assert!(m.contains("events_global_order"), "{m}");
}

/// The row with host seq 1, copied as a raw insert at the stream's next seq.
fn copy_of_first(c: &Connection, key: Option<&str>, event_id: Option<&str>) -> RawEvent {
    let (kind, payload, id, k): (String, String, String, String) = c
        .query_row(
            "SELECT kind, payload, event_id, idempotency_key FROM events WHERE stream_id = 'host' AND seq = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    let mut e = RawEvent::new("host", key.unwrap_or(&k), &kind, &payload);
    e.thread = Some(thread(1).to_string());
    e.event_id = Some(event_id.map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_owned));
    if event_id.is_some() {
        e.event_id = Some(id);
    }
    e
}

/// A statement run against the raw connection.
type Statement = Box<dyn Fn(&Connection) -> rusqlite::Result<usize>>;

fn forbidden_cases(c: &Connection) -> Vec<(&'static str, Statement)> {
    let same_id = copy_of_first(c, Some("fresh-key"), Some("same"));
    let same_key = copy_of_first(c, None, None);
    let same_key_upsert = copy_of_first(c, None, None);
    let replace = |e: RawEvent, verb: &'static str| -> Statement {
        Box::new(move |c| {
            c.execute(
                &format!(
                    "{verb} events (stream_id, seq, event_id, schema_version, idempotency_key, kind, thread_id,
                       source, occurred_at, occurred_explicit, recorded_at, payload, payload_bytes, cum_bytes)
                     SELECT 'host', max(seq) + 1, ?1, 1, ?2, ?3, ?4, ?5, 't', 0, 't', ?6, octet_length(?6),
                       (SELECT cum_bytes FROM events WHERE stream_id = 'host' ORDER BY seq DESC LIMIT 1) + octet_length(?6)
                     FROM events WHERE stream_id = 'host'"
                ),
                rusqlite::params![e.event_id, e.key, e.kind, e.thread, DAEMON_SOURCE, e.payload],
            )
        })
    };
    vec![
        (
            "update",
            Box::new(|c: &Connection| {
                c.execute(
                    "UPDATE events SET kind = 'x' WHERE stream_id = 'host' AND seq = 1",
                    [],
                )
            }),
        ),
        (
            "delete with where",
            Box::new(|c: &Connection| {
                c.execute(
                    "DELETE FROM events WHERE stream_id = 'host' AND seq = 1",
                    [],
                )
            }),
        ),
        (
            "delete without where",
            Box::new(|c: &Connection| c.execute("DELETE FROM events", [])),
        ),
        (
            "insert or replace on event_id",
            replace(same_id.clone(), "INSERT OR REPLACE INTO"),
        ),
        (
            "replace into on the key",
            replace(same_key.clone(), "REPLACE INTO"),
        ),
        (
            "upsert do update on the key",
            Box::new(move |c: &Connection| {
                let e = &same_key_upsert;
                c.execute(
                    "INSERT INTO events (stream_id, seq, event_id, schema_version, idempotency_key, kind, thread_id,
                       source, occurred_at, occurred_explicit, recorded_at, payload, payload_bytes, cum_bytes)
                     SELECT 'host', max(seq) + 1, ?1, 1, ?2, ?3, ?4, ?5, 't', 0, 't', ?6, octet_length(?6),
                       (SELECT cum_bytes FROM events WHERE stream_id = 'host' ORDER BY seq DESC LIMIT 1) + octet_length(?6)
                     FROM events WHERE stream_id = 'host'
                     ON CONFLICT (stream_id, idempotency_key) DO UPDATE SET kind = excluded.kind",
                    rusqlite::params![e.event_id, e.key, e.kind, e.thread, DAEMON_SOURCE, e.payload],
                )
            }),
        ),
        (
            "update or replace",
            Box::new(|c: &Connection| {
                c.execute(
                    "UPDATE OR REPLACE events SET seq = 1 WHERE stream_id = ?1 AND seq = 2",
                    [t1()],
                )
            }),
        ),
    ]
}

/// Every statement that would update or delete an event ends the transaction with every
/// earlier write gone and the row unchanged.
#[test]
fn each_forbidden_statement_aborts() {
    let s = fixture();
    let c = raw(&s.path());
    let before = dump(&c, "events");
    for (what, stmt) in forbidden_cases(&c) {
        let m = assert_rolls_back(&c, what, |c| stmt(c));
        assert!(m.contains("events_no_"), "{what}: {m}");
        assert_eq!(dump(&c, "events"), before, "{what}: events changed");
    }
}

/// The control: on a connection without `recursive_triggers`, the `REPLACE` cases pass
/// and silently remove the old event.
#[test]
fn replace_passes_without_recursive_triggers() {
    let s = fixture();
    let c = raw_without_recursive_triggers(&s.path());
    let cases = forbidden_cases(&c);
    for (what, stmt) in cases
        .into_iter()
        .filter(|(w, _)| w.contains("replace into") || w.contains("insert or replace"))
    {
        c.execute_batch("BEGIN IMMEDIATE").unwrap();
        let first = c
            .query_row(
                "SELECT event_id FROM events WHERE stream_id = 'host' AND seq = 1",
                [],
                |r| r.get::<_, String>(0),
            )
            .unwrap();
        stmt(&c).unwrap_or_else(|e| panic!("{what}: {e}"));
        let still: i64 = c
            .query_row(
                "SELECT count(*) FROM events WHERE event_id = ?1 AND seq = 1",
                [first],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(still, 0, "{what}: the old event was not removed");
        c.execute_batch("ROLLBACK").unwrap();
    }
}

/// Every immutable table refuses its forbidden updates and deletes with `RAISE(ROLLBACK)`.
#[test]
fn immutable_tables_refuse_changes() {
    let s = fixture();
    let c = raw(&s.path());
    // A receipt, a delivery, a token, and a blob to change.
    c.execute_batch(&format!(
        "INSERT INTO command_receipts (command_id, token_id, request_sha256, status, http_status, body, created_at, finished_at)
           VALUES ('c1', 't1', zeroblob(32), 'completed', 200, '{{}}', 't', 't');
         INSERT INTO command_receipts (command_id, token_id, request_sha256, status, created_at)
           VALUES ('c2', 't1', zeroblob(32), 'in_progress', 't');
         INSERT INTO hook_deliveries (launch_id, emit_id, receipt_ordinal, commit_ordinal, reply_status, reply_body,
           routing, stream_id, seq, received_at)
           VALUES ('{l}', 'e1', 1, 1, 200, '{{}}', 'appended', '{t}', 1, 't');
         INSERT INTO hook_deliveries (launch_id, emit_id, receipt_ordinal, commit_ordinal, reply_status, reply_body,
           routing, pending_body, pending_meta, received_at)
           VALUES ('{l}', 'e2', 2, 2, 200, '{{}}', 'buffered', x'00', '{{}}', 't');
         INSERT INTO tokens (token_id, kind, sha256, created_at) VALUES ('k1', 'owner', zeroblob(32), 't');
         INSERT INTO tokens (token_id, kind, sha256, created_at, revoked_at) VALUES ('k2', 'owner', randomblob(32), 't', 't');
         INSERT INTO blobs (sha256, size, data) VALUES ('{sha}', 65537, zeroblob(65537));
         INSERT INTO blob_refs (stream_id, sha256, media_type) VALUES ('host', '{sha}', 'text/plain');",
        l = l1(),
        t = t1(),
        sha = "a".repeat(64),
    ))
    .unwrap();
    let cases: &[(&str, &str, &str)] = &[
        (
            "meta epoch",
            "UPDATE meta SET value = 'x' WHERE key = 'epoch'",
            "meta_no_update",
        ),
        (
            "meta created_at",
            "UPDATE meta SET value = 'x' WHERE key = 'created_at'",
            "meta_no_update",
        ),
        (
            "meta delete",
            "DELETE FROM meta WHERE key = 'projection_version'",
            "meta_no_delete",
        ),
        (
            "meta replace",
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('epoch', 'x')",
            "meta_no_delete",
        ),
        (
            "blob update",
            "UPDATE blobs SET data = zeroblob(65538), size = 65538",
            "blobs_no_update",
        ),
        ("blob delete", "DELETE FROM blobs", "blobs_no_delete"),
        (
            "blob ref update",
            "UPDATE blob_refs SET media_type = 'x'",
            "blob_refs_no_update",
        ),
        (
            "blob ref delete",
            "DELETE FROM blob_refs",
            "blob_refs_no_delete",
        ),
        (
            "finished receipt",
            "UPDATE command_receipts SET body = 'x' WHERE command_id = 'c1'",
            "command_receipts_finish",
        ),
        (
            "receipt back to in_progress",
            "UPDATE command_receipts SET status = 'in_progress', http_status = NULL, body = NULL, finished_at = NULL WHERE command_id = 'c1'",
            "command_receipts_finish",
        ),
        (
            "receipt token",
            "UPDATE command_receipts SET token_id = 'x', status = 'unknown_after_crash', finished_at = 't' WHERE command_id = 'c2'",
            "command_receipts_finish",
        ),
        (
            "receipt delete",
            "DELETE FROM command_receipts",
            "command_receipts_no_delete",
        ),
        (
            "delivery reply",
            "UPDATE hook_deliveries SET reply_body = 'x' WHERE emit_id = 'e1'",
            "hook_deliveries_promote",
        ),
        (
            "delivery ordinal",
            "UPDATE hook_deliveries SET receipt_ordinal = 9 WHERE emit_id = 'e2'",
            "hook_deliveries_promote",
        ),
        (
            "delivery key",
            "UPDATE hook_deliveries SET emit_id = 'e9' WHERE emit_id = 'e2'",
            "hook_deliveries_promote",
        ),
        (
            "delivery back to buffered",
            "UPDATE hook_deliveries SET routing = 'buffered', stream_id = NULL, seq = NULL, pending_body = x'00', pending_meta = '{}' WHERE emit_id = 'e1'",
            "hook_deliveries_promote",
        ),
        (
            "delivery delete",
            "DELETE FROM hook_deliveries",
            "hook_deliveries_no_delete",
        ),
        (
            "delivery commit ordinal",
            "INSERT INTO hook_deliveries (launch_id, emit_id, receipt_ordinal, commit_ordinal, reply_status, reply_body, routing, pending_body, pending_meta, received_at) SELECT launch_id, 'e3', 3, 5, 200, '{}', 'buffered', x'00', '{}', 't' FROM hook_deliveries LIMIT 1",
            "hook_deliveries_order",
        ),
        (
            "token kind",
            "UPDATE tokens SET kind = 'agent' WHERE token_id = 'k1'",
            "tokens_revoke",
        ),
        (
            "token revoked twice",
            "UPDATE tokens SET revoked_at = 'u' WHERE token_id = 'k2'",
            "tokens_revoke",
        ),
        (
            "token unrevoked",
            "UPDATE tokens SET revoked_at = NULL WHERE token_id = 'k2'",
            "tokens_revoke",
        ),
        ("token delete", "DELETE FROM tokens", "tokens_no_delete"),
    ];
    for (what, sql, trigger) in cases {
        let m = assert_rolls_back(&c, what, |c| c.execute(sql, []));
        assert!(m.contains(trigger), "{what}: {m}");
    }
    // The allowed transitions pass.
    c.execute(
        "UPDATE meta SET value = '1' WHERE key = 'projection_version'",
        [],
    )
    .unwrap();
    c.execute("UPDATE command_receipts SET status = 'unknown_after_crash', finished_at = 'u' WHERE command_id = 'c2'", [])
        .unwrap();
    c.execute(
        &format!("UPDATE hook_deliveries SET routing = 'appended', stream_id = '{}', seq = 2, pending_body = NULL, pending_meta = NULL WHERE emit_id = 'e2'", t1()),
        [],
    )
    .unwrap();
    c.execute(
        "UPDATE tokens SET revoked_at = 'u' WHERE token_id = 'k1'",
        [],
    )
    .unwrap();
}

/// A `CHECK` whose operand is `NULL` fails the statement (the transaction stays open).
#[test]
fn null_operands_fail_checks() {
    let s = fixture();
    let c = raw(&s.path());
    // A thread stream with a null thread_id.
    let mut e = RawEvent::new(
        &t1(),
        "k1",
        "assistant_message",
        r#"{"text":{"inline":"x","encoding":"utf8"},"turn_ref":null}"#,
    );
    e.thread = None;
    assert_statement_fails(&c, "null thread_id", |c| e.insert(c));
    // Half-null routing: appended with a stream but no seq.
    assert_statement_fails(&c, "half-null routing", |c| {
        c.execute(
            &format!("INSERT INTO hook_deliveries (launch_id, emit_id, receipt_ordinal, commit_ordinal, reply_status, reply_body, routing, stream_id, seq, received_at) VALUES ('{}', 'e', 1, 1, 200, '', 'appended', '{}', NULL, 't')", l1(), t1()),
            [],
        )
    });
    // Partial receipt fields: completed without a body.
    assert_statement_fails(&c, "partial receipt", |c| {
        c.execute(
            "INSERT INTO command_receipts (command_id, token_id, request_sha256, status, http_status, created_at, finished_at) VALUES ('c', 't', zeroblob(32), 'completed', 200, 't', 't')",
            [],
        )
    });
    // A transcript source with null positions. The transcript triggers fire before the
    // CHECK (and end the transaction); with triggers off on this connection the CHECK
    // alone refuses the row.
    c.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER, false)
        .unwrap();
    let e = RawEvent::new(&t1(), "k2", "assistant_message", r#"{"text":{"inline":"x","encoding":"utf8"},"turn_ref":null}"#)
        .launch(&l1())
        .source(r#"{"kind":"transcript","agent":null,"cli_version":null,"transcript":{"generation":null,"path":"/work/a.jsonl","file_id":"f1","offset_end":20}}"#.to_owned())
        .annex(r#"{"transcript":"c"}"#);
    assert_statement_fails(&c, "transcript without a generation", |c| e.insert(c));
    c.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER, true)
        .unwrap();
    // A readiness annex without `counted`.
    let e = RawEvent::new(
        &t1(),
        "k3",
        "readiness_observed",
        &format!(
            r#"{{"launch_id":"{}","evidence":"session_start","by":"daemon"}}"#,
            l1()
        ),
    )
    .launch(&l1())
    .annex(&format!(
        r#"{{"readiness":{{"launch_id":"{}","position":1}}}}"#,
        l1()
    ));
    assert_statement_fails(&c, "readiness without counted", |c| e.insert(c));
}

/// A terminal routing must name an existing event (foreign key).
#[test]
fn terminal_routing_needs_an_event() {
    let s = fixture();
    let c = raw(&s.path());
    assert_statement_fails(&c, "routing to a missing event", |c| {
        c.execute(
            &format!("INSERT INTO hook_deliveries (launch_id, emit_id, receipt_ordinal, commit_ordinal, reply_status, reply_body, routing, stream_id, seq, received_at) VALUES ('{}', 'e', 1, 1, 200, '', 'appended', '{}', 999, 't')", l1(), t1()),
            [],
        )
    });
    // The control: an existing event passes.
    c.execute(
        &format!("INSERT INTO hook_deliveries (launch_id, emit_id, receipt_ordinal, commit_ordinal, reply_status, reply_body, routing, stream_id, seq, received_at) VALUES ('{}', 'e', 1, 1, 200, '', 'appended', '{}', 1, 't')", l1(), t1()),
        [],
    )
    .unwrap();
}

/// A raw transcript line (assistant text) at `offset_end` of `generation`/`file_id`/`path`.
fn raw_line(
    key: &str,
    generation: u64,
    path: &str,
    file_id: &str,
    offset_end: u64,
    launch: &str,
) -> RawEvent {
    RawEvent::new(
        &t1(),
        key,
        "assistant_message",
        r#"{"text":{"inline":"x","encoding":"utf8"},"turn_ref":null}"#,
    )
    .launch(launch)
    .source(transcript_source(generation, path, file_id, offset_end))
    .annex(r#"{"transcript":"c"}"#)
}

fn raw_generation(key: &str, generation: u64, path: &str, file_id: &str, launch: &str) -> RawEvent {
    RawEvent::new(
        &t1(),
        key,
        "transcript_generation",
        &format!(r#"{{"generation":{generation},"path":"{path}","file_id":"{file_id}"}}"#),
    )
    .launch(launch)
    .source(transcript_source(generation, path, file_id, 0))
    .annex(r#"{"transcript":"g"}"#)
}

fn assert_line_refused(c: &Connection, what: &str, e: &RawEvent) {
    let m = assert_rolls_back(c, what, |c| e.insert(c));
    assert!(m.contains("events_transcript_line"), "{what}: {m}");
}

fn assert_generation_refused(c: &Connection, what: &str, e: &RawEvent) {
    let m = assert_rolls_back(c, what, |c| e.insert(c));
    assert!(m.contains("events_generation"), "{what}: {m}");
}

/// A line of a generation that was never recorded is refused.
#[test]
fn line_before_its_generation_is_refused() {
    let s = fixture();
    let c = raw(&s.path());
    assert_line_refused(
        &c,
        "generation 2 line",
        &raw_line("k", 2, A, "f1", 20, &l1()),
    );
    // The control: the next line of generation 1 passes.
    raw_line("k", 1, A, "f1", 20, &l1()).insert(&c).unwrap();
}

/// A line of an older generation, or of another file or path, is refused.
#[test]
fn older_generation_line_is_refused() {
    let s = fixture();
    let c = raw(&s.path());
    raw_generation("g2", 2, A, "f2", &l1()).insert(&c).unwrap();
    assert_line_refused(
        &c,
        "generation 1 after 2",
        &raw_line("k", 1, A, "f1", 20, &l1()),
    );
    assert_line_refused(&c, "another file_id", &raw_line("k", 2, A, "f1", 20, &l1()));
    assert_line_refused(
        &c,
        "another path",
        &raw_line("k", 2, "/work/b.jsonl", "f2", 20, &l1()),
    );
    raw_line("k", 2, A, "f2", 20, &l1()).insert(&c).unwrap();
}

/// Offsets must increase within a generation.
#[test]
fn offsets_must_increase() {
    let s = fixture();
    let c = raw(&s.path());
    assert_line_refused(&c, "equal offset", &raw_line("k", 1, A, "f1", 10, &l1()));
    assert_line_refused(&c, "lower offset", &raw_line("k", 1, A, "f1", 5, &l1()));
    raw_line("k", 1, A, "f1", 11, &l1()).insert(&c).unwrap();
}

/// The first line of a generation ends past offset 0 (the generation marker's offset).
#[test]
fn first_line_of_a_generation_ends_past_zero() {
    let s = fixture();
    let c = raw(&s.path());
    raw_generation("g2", 2, A, "f2", &l1()).insert(&c).unwrap();
    assert_line_refused(&c, "zero offset", &raw_line("zero", 2, A, "f2", 0, &l1()));
    raw_line("one", 2, A, "f2", 1, &l1()).insert(&c).unwrap();
}

/// A generation must be the next number.
#[test]
fn generation_must_be_next() {
    let s = fixture();
    let c = raw(&s.path());
    assert_generation_refused(&c, "skip", &raw_generation("g", 3, A, "f2", &l1()));
    assert_generation_refused(&c, "repeat", &raw_generation("g", 1, A, "f2", &l1()));
    raw_generation("g", 2, A, "f2", &l1()).insert(&c).unwrap();
}

/// A generation must name the current launch and its bound path.
#[test]
fn generation_path_must_match_the_session() {
    let s = fixture();
    let c = raw(&s.path());
    assert_generation_refused(
        &c,
        "another path",
        &raw_generation("g", 2, "/work/b.jsonl", "f2", &l1()),
    );
    assert_generation_refused(
        &c,
        "another launch",
        &raw_generation("g", 2, A, "f2", &launch(9).to_string()),
    );
}

/// Lines of path A are refused once the thread rebound to B.
#[test]
fn raw_line_after_rebind_is_refused() {
    let mut s = fixture();
    s.reopen();
    let (t, l2) = (thread(1), launch(2));
    s.append(vec![super::support::exited(t, launch(1))])
        .unwrap();
    s.append(vec![
        reserved(t, l2),
        started(t, l2),
        bound(t, l2, "s2", Some("/work/b.jsonl")),
    ])
    .unwrap();
    s.shutdown();
    let c = raw(&s.path());
    assert_line_refused(
        &c,
        "old launch, path A",
        &raw_line("k", 1, A, "f1", 20, &l1()),
    );
    assert_line_refused(
        &c,
        "new launch, path A",
        &raw_line("k", 1, A, "f1", 20, &l2.to_string()),
    );
}

/// L1 bound to A, L2 reserved and not bound: a raw L1 line is refused.
#[test]
fn raw_line_after_a_new_reservation_is_refused() {
    let mut s = fixture();
    s.reopen();
    let (t, l2) = (thread(1), launch(2));
    s.append(vec![super::support::exited(t, launch(1))])
        .unwrap();
    s.append(vec![reserved(t, l2)]).unwrap();
    s.shutdown();
    let c = raw(&s.path());
    assert_line_refused(
        &c,
        "L1 line after L2 reserved",
        &raw_line("k", 1, A, "f1", 20, &l1()),
    );
}

/// After L1 exited and before the next reservation, its trailing `partial_line` passes.
#[test]
fn partial_line_before_the_next_reservation_passes() {
    let mut s = fixture();
    s.reopen();
    s.append(vec![super::support::exited(thread(1), launch(1))])
        .unwrap();
    s.shutdown();
    let c = raw(&s.path());
    RawEvent::new(
        &t1(),
        "k",
        "raw_unknown",
        r#"{"source":"transcript","reason":"partial_line","agent":"claude","cli_version":null,"content_type":"application/jsonl","raw":{"inline":"{\"par","encoding":"utf8"}}"#,
    )
    .launch(&l1())
    .source(transcript_source(1, A, "f1", 20))
    .annex(r#"{"transcript":"c"}"#)
    .insert(&c)
    .unwrap();
}

/// A transcript source must carry every position.
#[test]
fn transcript_positions_are_required() {
    let s = fixture();
    let c = raw(&s.path());
    for (what, source) in [
        (
            "no transcript",
            r#"{"kind":"transcript","agent":null,"cli_version":null,"transcript":null}"#,
        ),
        (
            "no offset",
            r#"{"kind":"transcript","agent":null,"cli_version":null,"transcript":{"generation":1,"path":"/work/a.jsonl","file_id":"f1"}}"#,
        ),
        (
            "no file_id",
            r#"{"kind":"transcript","agent":null,"cli_version":null,"transcript":{"generation":1,"path":"/work/a.jsonl","offset_end":30}}"#,
        ),
    ] {
        let e = RawEvent::new(
            &t1(),
            "k",
            "assistant_message",
            r#"{"text":{"inline":"x","encoding":"utf8"},"turn_ref":null}"#,
        )
        .launch(&l1())
        .source(source.to_owned())
        .annex(r#"{"transcript":"c"}"#);
        c.execute_batch("BEGIN IMMEDIATE").unwrap();
        let err = e.insert(&c).expect_err(what);
        assert!(
            message(&err).contains("events_transcript_line") || is_constraint(&err),
            "{what}: {err}"
        );
        if !c.is_autocommit() {
            c.execute_batch("ROLLBACK").unwrap();
        }
    }
}

/// A revoked token stays revoked.
#[test]
fn revocation_is_final() {
    let s = fixture();
    let c = raw(&s.path());
    c.execute("INSERT INTO tokens (token_id, kind, sha256, created_at) VALUES ('k', 'hook', zeroblob(32), 't')", [])
        .unwrap();
    c.execute(
        "UPDATE tokens SET revoked_at = 'r1' WHERE token_id = 'k'",
        [],
    )
    .unwrap();
    for (what, sql) in [
        (
            "revoke again",
            "UPDATE tokens SET revoked_at = 'r2' WHERE token_id = 'k'",
        ),
        (
            "unrevoke",
            "UPDATE tokens SET revoked_at = NULL WHERE token_id = 'k'",
        ),
    ] {
        let m = assert_rolls_back(&c, what, |c| c.execute(sql, []));
        assert!(m.contains("tokens_revoke"), "{what}: {m}");
    }
    let r: String = c
        .query_row(
            "SELECT revoked_at FROM tokens WHERE token_id = 'k'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(r, "r1");
}

/// The readiness annex has one canonical form: an array of known classes in order, without
/// repeats, a non-negative integer position, for the event's own launch.
#[test]
fn readiness_annex_check() {
    let s = fixture();
    let c = raw(&s.path());
    let payload = format!(
        r#"{{"launch_id":"{}","evidence":"session_start","by":"daemon"}}"#,
        l1()
    );
    let annex = |position: &str, counted: &str| {
        format!(
            r#"{{"readiness":{{"launch_id":"{}","position":{position},"counted":{counted}}}}}"#,
            l1()
        )
    };
    for (what, a) in [
        ("counted as a string", annex("1", r#""[\"input\"]""#)),
        ("negative position", annex("-1", r#"["input"]"#)),
        ("position as text", annex(r#""1""#, r#"["input"]"#)),
        ("unknown class", annex("1", r#"["input","other"]"#)),
        ("non-canonical order", annex("1", r#"["human","input"]"#)),
        ("repeated class", annex("1", r#"["input","input"]"#)),
        (
            "missing counted",
            format!(r#"{{"readiness":{{"launch_id":"{}","position":1}}}}"#, l1()),
        ),
        (
            "another launch",
            format!(
                r#"{{"readiness":{{"launch_id":"{}","position":1,"counted":[]}}}}"#,
                launch(9)
            ),
        ),
    ] {
        let e = RawEvent::new(&t1(), "k", "readiness_observed", &payload)
            .launch(&l1())
            .annex(&a);
        assert_statement_fails(&c, what, |c| e.insert(c));
    }
    for counted in [
        r#"[]"#,
        r#"["input","human","reply"]"#,
        r#"["human","reply"]"#,
    ] {
        RawEvent::new(
            &t1(),
            &format!("ok{counted}"),
            "readiness_observed",
            &payload,
        )
        .launch(&l1())
        .annex(&annex("0", counted))
        .insert(&c)
        .unwrap();
    }
}

/// `events_session_bound_once`: a second binding of a launch, a binding without a launch,
/// and a binding of a launch that is not the stream's latest reservation are refused; the
/// next launch's first binding passes (PR9-CL-6, hook-ingest §2.12).
#[test]
fn session_bound_once() {
    use crate::store::{StoreError, WriteError};
    let refused_by_trigger =
        |r: Result<Vec<crate::store::Appended>, WriteError>, what: &str| match r {
            Err(WriteError::RolledBack(StoreError::Constraint(m))) => assert!(
                m.contains("events_session_bound_once"),
                "{what}: refused by another check: {m}"
            ),
            other => panic!("{what}: not refused by the trigger: {other:?}"),
        };
    let s = TestStore::new();
    let (t, l1, l2) = (thread(1), launch(1), launch(2));
    bound_thread(&s, t, l1, Some(A));
    refused_by_trigger(s.append(vec![bound(t, l1, "s2", None)]), "second binding");
    let mut no_launch = bound(t, l1, "s3", None);
    no_launch.launch_id = None;
    refused_by_trigger(s.append(vec![no_launch]), "no launch");
    s.append(vec![super::support::exited(t, l1)]).unwrap();
    s.append(vec![reserved(t, l2), started(t, l2)]).unwrap();
    // L1 has its binding already; a fresh launch id that was never reserved names another
    // launch than the stream's latest reservation.
    let l9 = launch(9);
    refused_by_trigger(
        s.append(vec![bound(t, l9, "s9", None)]),
        "a launch that is not the latest reservation",
    );
    s.append(vec![bound(t, l2, "s2", Some("/work/b.jsonl"))])
        .expect("the next launch's first binding passes");
    // A reserved launch that was never bound, after the next reservation: only the
    // latest-reservation clause refuses it.
    let (t2, l3, l4) = (thread(2), launch(3), launch(4));
    s.append(vec![super::support::registered(t2, false)])
        .unwrap();
    s.append(vec![reserved(t2, l3), started(t2, l3)]).unwrap();
    s.append(vec![super::support::exited(t2, l3)]).unwrap();
    s.append(vec![reserved(t2, l4)]).unwrap();
    refused_by_trigger(
        s.append(vec![bound(t2, l3, "s3", None)]),
        "an unbound launch after the next reservation",
    );
}
