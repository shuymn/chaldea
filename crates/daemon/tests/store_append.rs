//! `append`: order, idempotency, compare-and-set, the task rule, and the readiness basis
//! (PR6-CL-1, CL-4, CL-5, CL-8, CL-23).

mod support;

use std::collections::BTreeMap;

use chaldea_core::event::TypedEvent;
use chaldea_core::ids::{Actor, CommandId, StreamId, Timestamp};
use chaldea_core::input::{InputReason, InputState};
use chaldea_core::payload::{
    Content, InputCreated, InputEncoding, InputInitialState, InputOrigin, InputStateChanged,
    ReadinessEvidence, ReadinessObserved, SourceKind, TaskSet,
};
use chaldea_core::state::{StatusCause, StatusChanged, TaskState};
use chaldea_daemon::store::{
    Appended, CountedClasses, EventAnnex, NewEvent, StoreError, WriteBasis, WriteError,
};
use proptest::prelude::*;
use rusqlite::types::Value;
use support::{
    TestStore, event, input, launch, raw_unknown, registered, reserved, source, started, thread,
};

fn ro(s: &TestStore) -> rusqlite::Connection {
    let c = rusqlite::Connection::open_with_flags(
        s.path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap();
    c.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    c
}

fn rolled_back(r: Result<Vec<Appended>, WriteError>) -> StoreError {
    match r {
        Err(WriteError::RolledBack(e)) => e,
        other => panic!("expected RolledBack, got {other:?}"),
    }
}

fn text(stream: StreamId, key: &str, body: &str) -> NewEvent {
    raw_unknown(stream, key, Content::text(body))
}

/// Per stream: (seq, cum_bytes, payload_bytes) and, globally, global_seq in commit order.
fn rows(c: &rusqlite::Connection) -> Vec<(String, i64, i64, i64, i64)> {
    let mut st = c
        .prepare("SELECT stream_id, seq, cum_bytes, payload_bytes, global_seq FROM events ORDER BY global_seq")
        .unwrap();
    st.query_map([], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

proptest! {
    /// Gapless per-stream seq, running cum_bytes, and global order hold across failed batches.
    #[test]
    fn seq_is_gapless_under_random_batches(
        batches in prop::collection::vec(
            (prop::collection::vec((0u8..4, 0usize..300), 1..5), prop::bool::weighted(0.25)),
            1..12,
        )
    ) {
        let s = TestStore::new();
        let threads = [thread(1), thread(2), thread(3)];
        s.append(threads.iter().map(|t| registered(*t, false)).collect()).unwrap();
        let mut key = 0u32;
        // Expected per stream: (head, cum).
        let mut expect: BTreeMap<String, (i64, i64)> = BTreeMap::new();
        let mut last_global = 0i64;
        for (bi, (events, fail)) in batches.iter().enumerate() {
            let mut batch = Vec::new();
            for (st, len) in events {
                key += 1;
                let stream = if *st == 0 { StreamId::Host } else { StreamId::Thread(threads[*st as usize - 1]) };
                batch.push(text(stream, &format!("k{key}"), &"x".repeat(*len)));
            }
            if *fail {
                // An event whose apply fails: thread_closed for an unknown thread.
                key += 1;
                batch.push(event(StreamId::Host, &format!("k{key}"), TypedEvent::ThreadClosed(
                    chaldea_core::payload::ThreadClosed { thread_id: thread(99), by: Actor::Human })));
            }
            let r = s.append(batch);
            prop_assert_eq!(r.is_ok(), !fail, "batch {} {:?}", bi, r.as_ref().err());
            if let Ok(appended) = r {
                for a in appended {
                    let e = a.envelope();
                    let entry = expect.entry(e.stream_id.to_string()).or_insert((0, 0));
                    entry.0 += 1;
                    entry.1 += i64::try_from(e.payload.get().len()).unwrap();
                }
            }
            let c = ro(&s);
            let mut seen: BTreeMap<String, (i64, i64)> = BTreeMap::new();
            let mut prev_global = 0;
            for (stream, seq, cum, bytes, global) in rows(&c) {
                prop_assert!(global > prev_global);
                prev_global = global;
                let e = seen.entry(stream.clone()).or_insert((0, 0));
                prop_assert_eq!(seq, e.0 + 1, "gap in {}", stream);
                prop_assert_eq!(cum, e.1 + bytes, "cum_bytes in {}", stream);
                *e = (seq, cum);
            }
            prop_assert!(prev_global >= last_global);
            last_global = prev_global;
            for (stream, (head, cum)) in &expect {
                let got = seen[stream];
                let base = if stream == "host" { host_base(&c) } else { (0, 0) };
                prop_assert_eq!(got, (base.0 + head, base.1 + cum));
            }
        }
    }
}

/// The registrations' (head, cum) on the host stream, which `expect` does not count.
fn host_base(c: &rusqlite::Connection) -> (i64, i64) {
    c.query_row(
        "SELECT count(*), coalesce(sum(payload_bytes), 0) FROM events WHERE stream_id = 'host' AND kind = 'thread_registered'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

fn dump(c: &rusqlite::Connection) -> BTreeMap<String, Vec<Vec<Value>>> {
    let tables: Vec<String> = c
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite%' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut out = BTreeMap::new();
    for t in tables {
        let mut st = c
            .prepare(&format!("SELECT * FROM \"{t}\" ORDER BY 1"))
            .unwrap();
        let n = st.column_count();
        let rows = st
            .query_map([], |r| {
                (0..n)
                    .map(|i| r.get::<_, Value>(i))
                    .collect::<Result<Vec<_>, _>>()
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        out.insert(t, rows);
    }
    out
}

/// A job whose second event fails `apply` leaves every table as it was.
#[test]
fn failed_job_changes_nothing() {
    let s = TestStore::new();
    let t = thread(1);
    s.append(vec![registered(t, false)]).unwrap();
    let before = dump(&ro(&s));
    let r = s.append(vec![
        reserved(t, launch(1)),
        // launch_started for a launch that was never reserved: apply fails.
        started(t, launch(2)),
    ]);
    assert!(matches!(rolled_back(r), StoreError::Projection(_)));
    assert_eq!(dump(&ro(&s)), before);
}

/// A replayed key with equal fields returns the first insert's event.
#[test]
fn replayed_key_returns_the_existing_event() {
    let s = TestStore::new();
    let e = text(StreamId::Host, "k", "body");
    let first = s.append(vec![e.clone()]).unwrap().remove(0);
    let Appended::New(first) = first else {
        panic!("not new")
    };
    s.clock.set("2026-11-23T00:00:00.000Z");
    let again = s.append(vec![e]).unwrap().remove(0);
    let Appended::Existing(again) = again else {
        panic!("not existing: {again:?}")
    };
    assert_eq!(again.event_id, first.event_id);
    assert_eq!(again.seq, first.seq);
    assert_eq!(again.recorded_at, first.recorded_at);
    assert_eq!(again.occurred_at, first.recorded_at);
    assert_eq!(s.read(|r| r.head(StreamId::Host, 0)).head_seq, 1);
}

/// A key reused with any producer-supplied field changed is a KeyConflict naming it.
#[test]
fn each_changed_field_conflicts() {
    let s = TestStore::new();
    let (t, l) = (thread(1), launch(1));
    s.append(vec![registered(t, false)]).unwrap();
    s.append(vec![reserved(t, l), started(t, l)]).unwrap();
    let stream = StreamId::Thread(t);
    let mut base = text(stream, "k", "body");
    base.launch_id = Some(l);
    base.occurred_at = Some(Timestamp("2026-11-21T00:00:00.000Z".into()));
    s.append(vec![base.clone()]).unwrap();
    let mut cases: Vec<(&str, NewEvent)> = Vec::new();
    let mut e = event(stream, "k", support::assistant("body"));
    e.launch_id = base.launch_id;
    e.occurred_at = base.occurred_at.clone();
    cases.push(("kind", e));
    let mut e = text(StreamId::Host, "k", "body");
    e.thread_id = Some(thread(2));
    // Same stream, other thread: the membership CHECK would refuse a new row, but the key
    // lookup comes first.
    e.stream_id = stream;
    e.launch_id = base.launch_id;
    e.occurred_at = base.occurred_at.clone();
    cases.push(("thread_id", e));
    let mut e = base.clone();
    e.launch_id = None;
    cases.push(("launch_id", e));
    let mut e = base.clone();
    e.command_id = Some(CommandId::from_u128(5));
    cases.push(("command_id", e));
    let mut e = base.clone();
    e.source = source(SourceKind::Hook);
    cases.push(("source", e));
    let mut e = text(stream, "k", "other body");
    e.launch_id = base.launch_id;
    e.occurred_at = base.occurred_at.clone();
    cases.push(("payload", e));
    let mut e = base.clone();
    e.occurred_at = Some(Timestamp("2026-11-21T00:00:01.000Z".into()));
    cases.push(("occurred_at", e));
    let mut e = base.clone();
    e.occurred_at = None;
    cases.push(("occurred_at", e));
    // The annex: a readiness event stored with one basis, retried with another.
    let basis = |position| {
        Some(EventAnnex::Readiness(WriteBasis {
            launch_id: l,
            position,
            counted: CountedClasses::ALL,
        }))
    };
    s.append(vec![readiness(t, l, basis(1))]).unwrap();
    cases.push(("annex", readiness(t, l, basis(2))));
    for (field, e) in cases {
        match rolled_back(s.append(vec![e])) {
            StoreError::KeyConflict { field: f, .. } => assert_eq!(f, field),
            other => panic!("{field}: {other:?}"),
        }
    }
    // And the other way: stored without a time, retried with one.
    s.append(vec![text(stream, "k2", "x")]).unwrap();
    let mut e = text(stream, "k2", "x");
    e.occurred_at = Some(Timestamp(support::T0.into()));
    assert!(matches!(
        rolled_back(s.append(vec![e])),
        StoreError::KeyConflict {
            field: "occurred_at",
            ..
        }
    ));
}

/// A key repeated inside one batch is `Existing` for the repeat.
#[test]
fn repeated_key_within_a_batch() {
    let s = TestStore::new();
    let e = text(StreamId::Host, "k", "body");
    let out = s.append(vec![e.clone(), e]).unwrap();
    assert!(matches!(out[0], Appended::New(_)));
    assert!(matches!(out[1], Appended::Existing(_)));
    assert_eq!(out[0].envelope(), out[1].envelope());
    assert_eq!(s.read(|r| r.head(StreamId::Host, 0)).head_seq, 1);
}

/// Distinct emit ids with identical content are distinct events.
#[test]
fn distinct_emit_ids_create_distinct_events() {
    let s = TestStore::new();
    let a = text(
        StreamId::Host,
        "hook:00000000-0000-4000-8000-000000000001",
        "same",
    );
    let b = text(
        StreamId::Host,
        "hook:00000000-0000-4000-8000-000000000002",
        "same",
    );
    let out = s.append(vec![a, b]).unwrap();
    assert!(out.iter().all(|a| matches!(a, Appended::New(_))));
    assert_ne!(out[0].envelope().event_id, out[1].envelope().event_id);
    assert_eq!(s.read(|r| r.head(StreamId::Host, 0)).head_seq, 2);
}

fn created(
    t: chaldea_core::ids::ThreadId,
    i: chaldea_core::ids::InputId,
    l: Option<chaldea_core::ids::LaunchId>,
) -> NewEvent {
    event(
        StreamId::Thread(t),
        &format!("created:{i}"),
        TypedEvent::InputCreated(InputCreated {
            input_id: i,
            launch_id: l,
            initial_state: InputInitialState::Queued,
            text: Content::text("hi"),
            encoding: InputEncoding::BracketedPaste,
            origin: InputOrigin::Cli,
            created_by: Actor::Human,
        }),
    )
}

fn sending(t: chaldea_core::ids::ThreadId, i: chaldea_core::ids::InputId, key: &str) -> NewEvent {
    event(
        StreamId::Thread(t),
        key,
        TypedEvent::InputStateChanged(InputStateChanged {
            input_id: i,
            from: InputState::Queued,
            to: InputState::Sending,
            reason: InputReason::GateOpened,
            sent_after_seq: Some(1),
            by: None,
        }),
    )
}

/// Two jobs recording the same `queued → sending`: the second aborts.
#[test]
fn lost_update_aborts() {
    let s = TestStore::new();
    let (t, l, i) = (thread(1), launch(1), input(1));
    s.append(vec![registered(t, false)]).unwrap();
    s.append(vec![reserved(t, l), created(t, i, Some(l))])
        .unwrap();
    s.append(vec![sending(t, i, "send:1")]).unwrap();
    assert!(matches!(
        rolled_back(s.append(vec![sending(t, i, "send:2")])),
        StoreError::Projection(_)
    ));
    assert_eq!(
        s.read(move |r| r.input(i)).unwrap().state,
        InputState::Sending
    );
}

fn task_set(t: chaldea_core::ids::ThreadId, key: &str, to: TaskState) -> NewEvent {
    event(
        StreamId::Thread(t),
        key,
        TypedEvent::TaskSet(TaskSet {
            to,
            by: Actor::Human,
            summary: None,
        }),
    )
}

fn task_change(
    t: chaldea_core::ids::ThreadId,
    key: &str,
    from: TaskState,
    to: TaskState,
) -> NewEvent {
    event(
        StreamId::Thread(t),
        key,
        TypedEvent::StatusChanged(StatusChanged::TaskState {
            from,
            to,
            cause: StatusCause::TaskSet,
        }),
    )
}

/// An equal retry of a stored derived `status_changed(TaskState)`, alone, is `Existing`.
#[test]
fn stored_task_status_change_retry_is_existing() {
    let s = TestStore::new();
    let t = thread(1);
    s.append(vec![registered(t, true)]).unwrap();
    let change = task_change(t, "c:2", TaskState::Active, TaskState::Blocked);
    s.append(vec![task_set(t, "c:1", TaskState::Blocked), change.clone()])
        .unwrap();
    let out = s.append(vec![change]).unwrap();
    assert!(matches!(out[0], Appended::Existing(_)));
}

/// A new `status_changed(TaskState)` without an unused matching `task_set` in its job is refused.
#[test]
fn standalone_task_status_change_is_refused() {
    let s = TestStore::new();
    let t = thread(1);
    s.append(vec![registered(t, true)]).unwrap();
    let refused = |r| matches!(rolled_back(r), StoreError::TaskChangeWithoutCause);
    // No task_set in the job.
    assert!(refused(s.append(vec![task_change(
        t,
        "a",
        TaskState::Active,
        TaskState::Blocked
    )])));
    // A task_set whose to differs.
    assert!(refused(s.append(vec![
        task_set(t, "b1", TaskState::Completed),
        task_change(t, "b2", TaskState::Active, TaskState::Blocked),
    ])));
    // A task_set whose from (the state before it) differs.
    assert!(refused(s.append(vec![
        task_set(t, "c1", TaskState::Blocked),
        task_change(t, "c2", TaskState::None, TaskState::Blocked),
    ])));
    // One task_set justifies one change: a second new key with the same from/to is refused.
    assert!(refused(s.append(vec![
        task_set(t, "d1", TaskState::Blocked),
        task_change(t, "d2", TaskState::Active, TaskState::Blocked),
        task_change(t, "d3", TaskState::Active, TaskState::Blocked),
    ])));
    // The pair alone commits, and an equal retry of the stored change is still Existing.
    let change = task_change(t, "e2", TaskState::Active, TaskState::Blocked);
    s.append(vec![task_set(t, "e1", TaskState::Blocked), change.clone()])
        .unwrap();
    let out = s.append(vec![change]).unwrap();
    assert!(matches!(out[0], Appended::Existing(_)));
    assert_eq!(
        s.read(move |r| r.thread(t)).unwrap().state.task,
        TaskState::Blocked
    );
}

/// `sending` sets a launchless input's launch from the envelope, and that launch's last write.
#[test]
fn sending_sets_a_missing_input_launch() {
    let s = TestStore::new();
    let (t, l, i, j) = (thread(1), launch(1), input(1), input(2));
    s.append(vec![registered(t, false)]).unwrap();
    s.append(vec![
        reserved(t, l),
        created(t, i, None),
        created(t, j, None),
    ])
    .unwrap();
    let mut e = sending(t, i, "send:i");
    e.launch_id = Some(l);
    let seq = s.append(vec![e]).unwrap()[0].envelope().seq;
    let row = s.read(move |r| r.input(i)).unwrap();
    assert_eq!(row.launch_id, Some(l));
    assert_eq!(
        s.read(move |r| r.launch(l)).unwrap().last_write_seq,
        Some(seq)
    );
    // Neither the input nor the envelope names a launch.
    assert!(matches!(
        rolled_back(s.append(vec![sending(t, j, "send:j")])),
        StoreError::Projection(_)
    ));
    assert_eq!(s.read(move |r| r.input(j)).unwrap().launch_id, None);
}

fn readiness(
    t: chaldea_core::ids::ThreadId,
    l: chaldea_core::ids::LaunchId,
    annex: Option<EventAnnex>,
) -> NewEvent {
    let mut e = event(
        StreamId::Thread(t),
        "ready:1",
        TypedEvent::ReadinessObserved(ReadinessObserved {
            launch_id: l,
            evidence: ReadinessEvidence::SessionStart,
            by: Actor::Daemon,
        }),
    );
    e.annex = annex;
    e
}

/// The write basis is stored, retried as `Existing`, and restored by rebuild.
#[test]
fn readiness_basis_round_trip() {
    let s = TestStore::new();
    let (t, l) = (thread(1), launch(1));
    s.append(vec![registered(t, false)]).unwrap();
    s.append(vec![reserved(t, l)]).unwrap();
    let basis = WriteBasis {
        launch_id: l,
        position: 42,
        counted: CountedClasses::ALL,
    };
    let e = readiness(t, l, Some(EventAnnex::Readiness(basis.clone())));
    s.append(vec![e.clone()]).unwrap();
    assert_eq!(
        s.read(move |r| r.launch(l)).unwrap().write_basis,
        Some(basis.clone())
    );
    assert!(matches!(
        s.append(vec![e]).unwrap()[0],
        Appended::Existing(_)
    ));
    s.run(|tx| Ok(tx.rebuild_projections()?)).unwrap();
    assert_eq!(
        s.read(move |r| r.launch(l)).unwrap().write_basis,
        Some(basis)
    );
}

/// A readiness event without its basis, or with another launch's, is refused.
#[test]
fn readiness_basis_is_refused() {
    let s = TestStore::new();
    let (t, l) = (thread(1), launch(1));
    s.append(vec![registered(t, false)]).unwrap();
    s.append(vec![reserved(t, l)]).unwrap();
    assert!(matches!(
        rolled_back(s.append(vec![readiness(t, l, None)])),
        StoreError::Annex(_)
    ));
    let other = EventAnnex::Readiness(WriteBasis {
        launch_id: launch(2),
        position: 1,
        counted: CountedClasses::ALL,
    });
    assert!(matches!(
        rolled_back(s.append(vec![readiness(t, l, Some(other))])),
        StoreError::Annex(_)
    ));
    assert_eq!(s.read(move |r| r.launch(l)).unwrap().write_basis, None);
}

/// A `user_message` attributed to an input of another launch is refused.
#[test]
fn attributed_prompt_must_match_the_inputs_launch() {
    use chaldea_core::payload::UserMessage;

    let s = TestStore::new();
    let (t, l1, l2, i) = (thread(1), launch(1), launch(2), input(1));
    let mut next = reserved(t, l2);
    if let TypedEvent::LaunchReserved(r) = &mut next.event {
        r.expected_previous = Some(l1);
    }
    s.append(vec![
        registered(t, false),
        reserved(t, l1),
        started(t, l1),
        created(t, i, Some(l1)),
        sending(t, i, "send"),
        support::exited(t, l1),
        next,
        started(t, l2),
    ])
    .unwrap();

    let mut e = event(
        StreamId::Thread(t),
        "prompt-l2",
        TypedEvent::UserMessage(UserMessage {
            prompt: Content::text("hi"),
            input_id: Some(i),
        }),
    );
    e.launch_id = Some(l2);
    e.source = source(SourceKind::Hook);
    let result = s.append(vec![e]);
    let ack = s.read(move |r| r.input(i)).unwrap().ack_seq;
    assert!(result.is_err() && ack.is_none(), "{result:?}, ack {ack:?}");
}
