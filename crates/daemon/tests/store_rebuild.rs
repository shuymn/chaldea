//! Rebuild and the model (PR6-CL-7, CL-8, CL-23).

mod support;

use chaldea_core::event::TypedEvent;
use chaldea_core::ids::{Actor, StreamId};
use chaldea_core::payload::TaskSet;
use chaldea_core::state::{StateInput, TaskState, ThreadState, reduce};
use proptest::prelude::*;
use support::{
    Model, TestStore, batch, bound_thread, check_model, event, jobs, launch, line, new_generation,
    pos, registered, run_ops, steps, thread,
};

proptest! {
    /// The model computes the axes, current launch, input state and launch, gate evidence and
    /// write basis, and the cursor itself; the store must match it after live appends and after
    /// `rebuild_projections`, so a bug shared by both paths of `apply` fails.
    #[test]
    fn model_matches_projection(steps in steps()) {
        let s = TestStore::new();
        let mut model = Model::default();
        for (job, expected) in jobs(&mut model, &steps, false, true) {
            let commits = job.commits;
            let ops = job.ops.clone();
            let r = s.run(move |tx| run_ops(tx, ops, &|_, _, _| Ok(())));
            prop_assert_eq!(r.is_ok(), commits, "job {:?}: {:?}", job.ops, r.err());
            let m = expected.clone();
            s.read(move |r| Ok(check_model(r, &m))).map_err(TestCaseError::fail)?;
        }
        let r = s.run(|tx| Ok(tx.rebuild_projections()?));
        prop_assert!(r.is_ok(), "{:?}", r.err());
        model.rebuild();
        let _ = model.take_job();
        let m = model.clone();
        s.read(move |r| Ok(check_model(r, &m))).map_err(TestCaseError::fail)?;
    }
}

/// A maintained read offset past the event offset passes verification; any other changed
/// column is reported.
#[test]
fn verify_allows_only_the_rewind() {
    let s = TestStore::new();
    let (t, l) = (thread(1), launch(1));
    bound_thread(&s, t, l, Some("/work/a.jsonl"));
    let mut b = batch(t, l, "/work/a.jsonl", None);
    b.new_generation = Some(new_generation("f1"));
    s.run(move |tx| Ok(tx.append_transcript(b)?)).unwrap();
    let mut b = batch(
        t,
        l,
        "/work/a.jsonl",
        Some(pos(1, "/work/a.jsonl", "f1", 0)),
    );
    b.lines = vec![line(0, 10, "one")];
    b.advance_to.read_offset = 30;
    s.run(move |tx| Ok(tx.append_transcript(b)?)).unwrap();
    let diff = s.run(|tx| Ok(tx.verify_projections()?)).unwrap();
    assert!(diff.is_empty(), "{diff:?}");
    // The cursor stays where the maintained projection had it.
    let c = s.read(move |r| r.transcript_cursor(t)).unwrap();
    assert_eq!((c.event_offset, c.read_offset), (10, 30));
    // A projection column changed by another connection is reported, the cursor's event
    // evidence included.
    let raw = rusqlite::Connection::open(s.path()).unwrap();
    raw.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    raw.execute("UPDATE proj_threads SET cwd = '/elsewhere'", [])
        .unwrap();
    raw.execute(
        "UPDATE proj_transcript_cursors SET event_continuity = 'forged'",
        [],
    )
    .unwrap();
    drop(raw);
    let diff = s.run(|tx| Ok(tx.verify_projections()?)).unwrap();
    assert_eq!(diff.differences.len(), 2, "{diff:?}");
    assert!(
        diff.differences.iter().any(|d| d.contains("cwd")),
        "{diff:?}"
    );
    assert!(
        diff.differences
            .iter()
            .any(|d| d.contains("event_continuity")),
        "{diff:?}"
    );
    // verify rolled its rebuild back: the forged values are still there.
    let row = s.read(move |r| r.thread(t)).unwrap();
    assert_eq!(row.cwd, "/elsewhere");
}

/// `task_set` and the `status_changed` that `core::state::reduce` returns for `TaskSet`:
/// `completed` after commit and after rebuild.
#[test]
fn task_set_with_reducer_output() {
    let s = TestStore::new();
    let t = thread(1);
    s.append(vec![registered(t, true)]).unwrap();
    let (_, changes) = reduce(
        &ThreadState::registered(true),
        &StateInput::TaskSet {
            to: TaskState::Completed,
            by: Actor::Human,
        },
    )
    .unwrap();
    assert_eq!(changes.len(), 1);
    let mut job = vec![event(
        StreamId::Thread(t),
        "command:task:1",
        TypedEvent::TaskSet(TaskSet {
            to: TaskState::Completed,
            by: Actor::Human,
            summary: None,
        }),
    )];
    job.push(event(
        StreamId::Thread(t),
        "command:task:2",
        TypedEvent::StatusChanged(changes[0].clone()),
    ));
    s.append(job).unwrap();
    let task = |s: &TestStore| s.read(move |r| r.thread(t)).unwrap().state.task;
    assert_eq!(task(&s), TaskState::Completed);
    s.run(|tx| Ok(tx.rebuild_projections()?)).unwrap();
    assert_eq!(task(&s), TaskState::Completed);
}

/// The event position's continuity is restored by rebuild; the read position's evidence is
/// replaced by it (the permitted rewind).
#[test]
fn event_continuity_restored_by_rebuild() {
    let s = TestStore::new();
    let (t, l) = (thread(1), launch(1));
    bound_thread(&s, t, l, Some("/work/a.jsonl"));
    let mut b = batch(t, l, "/work/a.jsonl", None);
    b.new_generation = Some(new_generation("f1"));
    s.run(move |tx| Ok(tx.append_transcript(b)?)).unwrap();
    let mut b = batch(
        t,
        l,
        "/work/a.jsonl",
        Some(pos(1, "/work/a.jsonl", "f1", 0)),
    );
    b.lines = vec![line(0, 10, "one"), line(10, 25, "two")];
    b.advance_to = chaldea_daemon::store::ReadPos {
        read_offset: 40,
        continuity: support::cont("read-40"),
    };
    s.run(move |tx| Ok(tx.append_transcript(b)?)).unwrap();
    let before = s.read(move |r| r.transcript_cursor(t)).unwrap();
    assert_eq!(before.event_continuity.0, "c25");
    assert_eq!(before.read_continuity.0, "read-40");
    s.run(|tx| Ok(tx.rebuild_projections()?)).unwrap();
    let after = s.read(move |r| r.transcript_cursor(t)).unwrap();
    assert_eq!(after.event_offset, 25);
    assert_eq!(after.event_continuity.0, "c25");
    assert_eq!(after.read_offset, 25);
    assert_eq!(after.read_continuity.0, "c25");
}
