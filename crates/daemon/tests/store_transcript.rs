//! Transcript batches: binding, cursor, generation, and content checks (PR6-CL-9, CL-23).

mod support;

use chaldea_core::event::{Source, TranscriptPos, TypedEvent};
use chaldea_core::ids::{Agent, LaunchId, Open, StreamId, ThreadId};
use chaldea_core::payload::{
    Content, RawUnknown, RawUnknownReason, SourceKind, TranscriptGeneration,
};
use chaldea_daemon::store::{ContentError, ReadPos, StoreError, TranscriptBatch, WriteError};
use support::{
    TestStore, assistant, batch, bound, bound_thread, cont, event, exited, launch, line,
    new_generation, pos, reserved, started, thread,
};

const A: &str = "/work/t/a.jsonl";
const B: &str = "/work/t/b.jsonl";

fn head(s: &TestStore, t: ThreadId) -> u64 {
    s.read(move |r| r.head(StreamId::Thread(t), 0)).head_seq
}

fn cursor(s: &TestStore, t: ThreadId) -> Option<chaldea_daemon::store::TranscriptCursor> {
    s.read(move |r| r.transcript_cursor(t))
}

#[allow(clippy::result_large_err, reason = "the test inspects the WriteError")]
fn submit(s: &TestStore, b: TranscriptBatch) -> Result<(), WriteError> {
    s.run(move |tx| Ok(tx.append_transcript(b).map(|_| ())?))
}

/// Generation 1 of `file` on `path`, committed alone.
fn start_generation(s: &TestStore, t: ThreadId, l: LaunchId, path: &str, file: &str) {
    let mut b = batch(t, l, path, None);
    b.new_generation = Some(new_generation(file));
    submit(s, b).unwrap();
}

/// A thread bound to `A` with generation 1 of `f1` and lines up to offset 20.
fn with_lines(s: &TestStore) -> (ThreadId, LaunchId) {
    let (t, l) = (thread(1), launch(1));
    bound_thread(s, t, l, Some(A));
    start_generation(s, t, l, A, "f1");
    let mut b = batch(t, l, A, Some(pos(1, A, "f1", 0)));
    b.lines = vec![line(0, 10, "one"), line(10, 20, "two")];
    b.advance_to.read_offset = 20;
    submit(s, b).unwrap();
    (t, l)
}

fn advance(to: u64) -> ReadPos {
    ReadPos {
        read_offset: to,
        continuity: cont(&format!("r{to}")),
    }
}

/// A stored cursor differing from `expect` in any field (or `None` with a cursor) aborts the batch.
#[test]
fn stale_cursor_aborts_the_batch() {
    let s = TestStore::new();
    let (t, l) = with_lines(&s);
    let before = (head(&s, t), cursor(&s, t));
    let cases = [
        Some(pos(2, A, "f1", 20)),
        Some(pos(1, B, "f1", 20)),
        Some(pos(1, A, "f2", 20)),
        Some(pos(1, A, "f1", 10)),
        None,
    ];
    for expect in cases {
        let mut b = batch(t, l, A, expect.clone());
        b.lines = vec![line(20, 30, "three")];
        b.advance_to.read_offset = 30;
        let r = submit(&s, b);
        assert!(
            matches!(
                r,
                Err(WriteError::RolledBack(StoreError::StaleCursor { .. }))
            ),
            "{expect:?}: {r:?}"
        );
        assert_eq!((head(&s, t), cursor(&s, t)), before, "{expect:?}");
    }
}

/// After a resume to path B, a generation starts on B with `expect` naming A; the first B line follows.
#[test]
fn rebind_starts_a_generation_on_the_new_path() {
    let s = TestStore::new();
    let (t, l) = with_lines(&s);
    let l2 = launch(2);
    s.append(vec![
        exited(t, l),
        reserved(t, l2),
        started(t, l2),
        bound(t, l2, "s1", Some(B)),
    ])
    .unwrap();
    let mut g = batch(t, l2, B, Some(pos(1, A, "f1", 20)));
    g.new_generation = Some(new_generation("fb"));
    submit(&s, g).unwrap();
    let c = cursor(&s, t).unwrap();
    assert_eq!(
        (
            c.generation,
            c.path.as_str(),
            c.file_id.as_str(),
            c.read_offset
        ),
        (2, B, "fb", 0)
    );
    let mut b = batch(t, l2, B, Some(pos(2, B, "fb", 0)));
    b.lines = vec![line(0, 8, "b-one")];
    b.advance_to.read_offset = 8;
    submit(&s, b).unwrap();
    let c = cursor(&s, t).unwrap();
    assert_eq!((c.event_offset, c.read_offset), (8, 8));
}

/// A new `file_id` on the same path starts a generation; its first line is measured from 0.
#[test]
fn same_path_swapped_file() {
    let s = TestStore::new();
    let (t, l) = with_lines(&s);
    let mut g = batch(t, l, A, Some(pos(1, A, "f1", 20)));
    g.new_generation = Some(new_generation("f2"));
    submit(&s, g).unwrap();
    let mut b = batch(t, l, A, Some(pos(2, A, "f2", 0)));
    b.lines = vec![line(0, 5, "new file")];
    b.advance_to.read_offset = 5;
    submit(&s, b).unwrap();
    let c = cursor(&s, t).unwrap();
    assert_eq!(
        (c.generation, c.file_id.as_str(), c.event_offset),
        (2, "f2", 5)
    );
}

/// A batch read before the launch rebound to another path is refused (StaleBinding).
#[test]
fn old_batch_after_rebind_is_refused() {
    let s = TestStore::new();
    let (t, l) = with_lines(&s);
    s.append(vec![bound(t, l, "s2", Some(B))]).unwrap();
    let n = head(&s, t);
    let mut b = batch(t, l, A, Some(pos(1, A, "f1", 20)));
    b.lines = vec![line(20, 30, "late")];
    b.advance_to.read_offset = 30;
    let r = submit(&s, b);
    assert!(
        matches!(
            r,
            Err(WriteError::RolledBack(StoreError::StaleBinding { .. }))
        ),
        "{r:?}"
    );
    assert_eq!(head(&s, t), n);
}

/// A batch of the old launch after a resume is refused even though its cursor matches.
#[test]
fn old_batch_after_resume_is_refused() {
    let s = TestStore::new();
    let (t, l) = with_lines(&s);
    let l2 = launch(2);
    s.append(vec![
        exited(t, l),
        reserved(t, l2),
        started(t, l2),
        bound(t, l2, "s1", Some(A)),
    ])
    .unwrap();
    let n = head(&s, t);
    let mut b = batch(t, l, A, Some(pos(1, A, "f1", 20)));
    b.lines = vec![line(20, 30, "late")];
    b.advance_to.read_offset = 30;
    let r = submit(&s, b);
    assert!(
        matches!(
            r,
            Err(WriteError::RolledBack(StoreError::StaleBinding { .. }))
        ),
        "{r:?}"
    );
    assert_eq!(head(&s, t), n);
}

/// One batch with a new generation and lines is refused.
#[test]
fn generation_with_lines_is_refused() {
    let s = TestStore::new();
    let (t, l) = (thread(1), launch(1));
    bound_thread(&s, t, l, Some(A));
    let n = head(&s, t);
    let mut b = batch(t, l, A, None);
    b.new_generation = Some(new_generation("f1"));
    b.lines = vec![line(0, 10, "one")];
    let r = submit(&s, b);
    assert!(
        matches!(
            r,
            Err(WriteError::RolledBack(StoreError::GenerationWithLines))
        ),
        "{r:?}"
    );
    assert_eq!(head(&s, t), n);
    assert_eq!(cursor(&s, t), None);
}

/// A generation start and lines, or a cursor advance without lines, of one thread in one job
/// are refused in either call order.
#[test]
fn generation_and_lines_in_one_job_are_refused() {
    let s = TestStore::new();
    let (t, l) = with_lines(&s);
    let before = (head(&s, t), cursor(&s, t));
    for with_lines in [true, false] {
        for generation_first in [true, false] {
            let r = s.run(move |tx| {
                let generation = || {
                    let mut g = batch(t, l, A, Some(pos(1, A, "f1", 20)));
                    g.new_generation = Some(new_generation("f2"));
                    g
                };
                let mut c = batch(t, l, A, Some(pos(1, A, "f1", 20)));
                if with_lines {
                    c.lines = vec![line(20, 30, "x")];
                }
                c.advance_to = advance(40);
                if generation_first {
                    tx.append_transcript(generation())?;
                    // The cursor is now generation 2 at 0.
                    let mut c2 = batch(t, l, A, Some(pos(2, A, "f2", 0)));
                    if with_lines {
                        c2.lines = vec![line(0, 10, "x")];
                    }
                    c2.advance_to = advance(40);
                    tx.append_transcript(c2)?;
                } else {
                    tx.append_transcript(c)?;
                    let mut g = generation();
                    g.expect = Some(pos(1, A, "f1", 40));
                    tx.append_transcript(g)?;
                }
                Ok(())
            });
            assert!(
                matches!(
                    r,
                    Err(WriteError::RolledBack(StoreError::GenerationWithLines))
                ),
                "lines {with_lines} generation first {generation_first}: {r:?}"
            );
            assert_eq!((head(&s, t), cursor(&s, t)), before);
        }
    }
}

/// The first line of a new generation commits in the next job.
#[test]
fn first_line_in_the_next_job() {
    let s = TestStore::new();
    let (t, l) = (thread(1), launch(1));
    bound_thread(&s, t, l, Some(A));
    start_generation(&s, t, l, A, "f1");
    let c = cursor(&s, t).unwrap();
    assert_eq!((c.generation, c.event_offset, c.read_offset), (1, 0, 0));
    let mut b = batch(t, l, A, Some(pos(1, A, "f1", 0)));
    b.lines = vec![line(0, 10, "one")];
    b.advance_to.read_offset = 10;
    let out = s.run(move |tx| Ok(tx.append_transcript(b)?)).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].envelope().idempotency_key.0, "transcript:1:0");
}

/// A continuation's first line starts at or after the expected read offset.
#[test]
fn continuation_starts_at_the_read_offset() {
    let s = TestStore::new();
    let (t, l) = with_lines(&s);
    let mut b = batch(t, l, A, Some(pos(1, A, "f1", 20)));
    b.lines = vec![line(15, 30, "overlap")];
    b.advance_to.read_offset = 30;
    let r = submit(&s, b);
    assert!(
        matches!(r, Err(WriteError::RolledBack(StoreError::BadBatch(_)))),
        "{r:?}"
    );
    let mut b = batch(t, l, A, Some(pos(1, A, "f1", 20)));
    b.lines = vec![line(20, 30, "three")];
    b.advance_to.read_offset = 30;
    submit(&s, b).unwrap();
    assert_eq!(cursor(&s, t).unwrap().event_offset, 30);
}

/// Events and the cursor advance commit together: a failing second line leaves neither.
#[test]
fn batch_and_cursor_commit_together() {
    let s = TestStore::new();
    let (t, l) = with_lines(&s);
    let before = (head(&s, t), cursor(&s, t));
    let mut b = batch(t, l, A, Some(pos(1, A, "f1", 20)));
    let mut bad = line(30, 40, "");
    bad.event = TypedEvent::AssistantMessage(chaldea_core::payload::AssistantMessage {
        text: Content::text("x".repeat(65_537)),
        turn_ref: None,
    });
    b.lines = vec![line(20, 30, "fine"), bad];
    b.advance_to.read_offset = 50;
    let r = submit(&s, b);
    assert!(
        matches!(r, Err(WriteError::RolledBack(StoreError::Content(_)))),
        "{r:?}"
    );
    assert_eq!((head(&s, t), cursor(&s, t)), before);
}

/// A batch without lines advances the read offset past lines that produced no event.
#[test]
fn advance_without_events() {
    let s = TestStore::new();
    let (t, l) = with_lines(&s);
    let n = head(&s, t);
    let mut b = batch(t, l, A, Some(pos(1, A, "f1", 20)));
    b.advance_to = advance(55);
    submit(&s, b).unwrap();
    let c = cursor(&s, t).unwrap();
    assert_eq!(
        (c.event_offset, c.read_offset, c.read_continuity.0.as_str()),
        (20, 55, "r55")
    );
    assert_eq!(head(&s, t), n);
}

/// A continuation that would move the read offset backwards fails the conditional cursor update.
#[test]
fn advance_backwards_is_stale() {
    let s = TestStore::new();
    let (t, l) = with_lines(&s);
    let mut b = batch(t, l, A, Some(pos(1, A, "f1", 20)));
    b.advance_to = advance(5);
    let r = submit(&s, b);
    assert!(
        matches!(
            r,
            Err(WriteError::RolledBack(StoreError::StaleCursor { .. }))
        ),
        "{r:?}"
    );
    assert_eq!(cursor(&s, t).unwrap().read_offset, 20);
}

/// After `launch_exited` the launch stays current, so a trailing `partial_line` commits.
#[test]
fn partial_line_after_exit_commits() {
    let s = TestStore::new();
    let (t, l) = with_lines(&s);
    s.append(vec![exited(t, l)]).unwrap();
    let mut b = batch(t, l, A, Some(pos(1, A, "f1", 20)));
    let mut partial = line(20, 27, "");
    partial.event = TypedEvent::RawUnknown(RawUnknown {
        source: SourceKind::Transcript,
        reason: RawUnknownReason::PartialLine,
        agent: Some(Agent::Claude),
        cli_version: None,
        content_type: "application/jsonl".into(),
        raw: Content::text("{\"trunc"),
    });
    b.lines = vec![partial];
    b.advance_to.read_offset = 27;
    let out = s.run(move |tx| Ok(tx.append_transcript(b)?)).unwrap();
    assert_eq!(out[0].envelope().kind.as_str(), "raw_unknown");
    assert_eq!(cursor(&s, t).unwrap().event_offset, 27);
}

/// Transcript lines pass the same content validation as `append` (inline above 64 KiB).
#[test]
fn invalid_content_line_is_refused() {
    let s = TestStore::new();
    let (t, l) = with_lines(&s);
    let n = head(&s, t);
    let mut b = batch(t, l, A, Some(pos(1, A, "f1", 20)));
    let mut big = line(20, 30, "");
    big.event = assistant(&"y".repeat(65_537));
    b.lines = vec![big];
    b.advance_to.read_offset = 30;
    let r = submit(&s, b);
    assert!(
        matches!(
            r,
            Err(WriteError::RolledBack(StoreError::Content(
                ContentError::InlineTooLarge(65_537)
            )))
        ),
        "{r:?}"
    );
    assert_eq!(head(&s, t), n);
}

/// `append` refuses transcript-sourced events and `transcript_generation`.
#[test]
fn transcript_source_via_append_is_refused() {
    let s = TestStore::new();
    let (t, l) = with_lines(&s);
    let n = head(&s, t);
    let mut line_event = event(StreamId::Thread(t), "transcript:1:20", assistant("sneak"));
    line_event.launch_id = Some(l);
    line_event.source = Source {
        kind: Open::Known(SourceKind::Transcript),
        agent: Some(Open::Known(Agent::Claude)),
        cli_version: None,
        transcript: Some(TranscriptPos {
            generation: 1,
            path: A.into(),
            file_id: "f1".into(),
            offset_end: 30,
        }),
    };
    line_event.annex = Some(chaldea_daemon::store::EventAnnex::Transcript(cont("x")));
    let generation = event(
        StreamId::Thread(t),
        "transcript_generation:2",
        TypedEvent::TranscriptGeneration(TranscriptGeneration {
            generation: 2,
            path: A.into(),
            file_id: "f2".into(),
        }),
    );
    for e in [line_event, generation] {
        let r = s.append(vec![e]);
        assert!(
            matches!(
                r,
                Err(WriteError::RolledBack(StoreError::TranscriptViaAppend))
            ),
            "{r:?}"
        );
    }
    assert_eq!(head(&s, t), n);
}

/// The read position and its evidence survive a reopen.
#[test]
fn read_continuity_survives_reopen() {
    let mut s = TestStore::new();
    let (t, l) = with_lines(&s);
    let mut b = batch(t, l, A, Some(pos(1, A, "f1", 20)));
    b.advance_to = advance(33);
    submit(&s, b).unwrap();
    s.reopen();
    let c = cursor(&s, t).unwrap();
    assert_eq!((c.read_offset, c.read_continuity.0.as_str()), (33, "r33"));
    assert_eq!((c.event_offset, c.event_continuity.0.as_str()), (20, "c20"));
}
