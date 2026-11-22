//! The single writer and its origin log (`docs/design/pty-runtime.md` §2.3), PR2-CL-1 to
//! CL-8, CL-21, CL-26, CL-27.

mod common;

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chaldea_core::ids::ApprovalId;
use chaldea_pty_runtime::fake::{FakeEvent, ProbePoint};
use chaldea_pty_runtime::{
    Basis, ClassSet, Guard, Limits, OriginClass, Part, Refusal, ReplyKind, SessionEvent,
    SubmitError, WriteOrigin, WriteOutcome, WriteRecord, WriteUnit, completion_channel,
};
use common::*;
use proptest::prelude::*;

const DSR5: &[u8] = b"\x1b[5n";
const DSR5_REPLY: &[u8] = b"\x1b[0n";

/// The written records in `seq` order partition `input` exactly.
fn assert_partition(records: &[WriteRecord], input: &[u8]) {
    let mut pos = 0u64;
    let mut seq = 0;
    for r in records {
        assert_eq!(r.seq, seq + 1, "seqs are gapless: {records:#?}");
        seq = r.seq;
        match &r.outcome {
            WriteOutcome::Written => {
                assert_eq!(r.byte_start, pos, "contiguous span for {r:?}");
                pos += r.bytes_written;
            }
            WriteOutcome::NotStarted(_) => assert_eq!(r.bytes_written, 0, "{r:?}"),
            other => panic!("unexpected outcome {other:?}"),
        }
    }
    assert_eq!(
        pos,
        input.len() as u64,
        "spans cover the whole input stream"
    );
}

/// PR2-CL-1: every dequeued unit gets the next `seq` and its origin; the dispatched
/// spans in `seq` order partition the input stream.
#[test]
fn records_partition_the_input_stream() {
    let r = rig();
    let a = submit(&r.session, key(b"k1"), open());
    let i = submit(
        &r.session,
        unit(input_origin(7), vec![b("text"), pause(5), b("\r")]),
        open(),
    );
    a.wait();
    i.wait();
    r.child.emit(DSR5);
    assert!(wait_until(|| r.child.input().ends_with(DSR5_REPLY)));
    let h = submit(
        &r.session,
        WriteUnit::bytes(WriteOrigin::Attach, "日本"),
        open(),
    );
    h.wait();
    let recs = r.session.writes_after(0).unwrap();
    assert_partition(&recs, &r.child.input());
    let origins: Vec<_> = recs.iter().map(|x| x.origin.clone()).collect();
    assert_eq!(
        origins,
        vec![
            WriteOrigin::Key,
            input_origin(7),
            WriteOrigin::VtReply {
                kind: ReplyKind::Dsr5
            },
            WriteOrigin::Attach,
        ]
    );
    for x in &recs {
        let span = &r.child.input()[usize::try_from(x.byte_start).unwrap()
            ..usize::try_from(x.byte_start + x.bytes_written).unwrap()];
        assert!(!span.is_empty());
        assert!(x.started_at.is_some() && x.finished_at.is_some());
    }
    assert_eq!(
        r.child.writes(),
        vec![
            b"k1".to_vec(),
            b"text".to_vec(),
            b"\r".to_vec(),
            DSR5_REPLY.to_vec(),
            "日本".as_bytes().to_vec()
        ]
    );
}

#[derive(Debug, Clone)]
enum Op {
    Unit(Vec<Vec<u8>>, u8),
    Query,
    Expired,
    StaleBasis,
    CancelLast,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (prop::collection::vec(prop::collection::vec(b'a'..=b'z', 1..4), 1..4), 0u8..3)
            .prop_map(|(p, o)| Op::Unit(p, o)),
        2 => Just(Op::Query),
        1 => Just(Op::Expired),
        1 => Just(Op::StaleBasis),
        1 => Just(Op::CancelLast),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    /// PR2-CL-1 under random units, replies, and refusals.
    #[test]
    fn records_partition_under_random_units(ops in prop::collection::vec(op(), 1..12)) {
        let r = rig();
        let mut pending = Vec::new();
        let mut queries = 0u64;
        for op in ops {
            match op {
                Op::Unit(parts, o) => {
                    let origin = match o {
                        0 => WriteOrigin::Key,
                        1 => WriteOrigin::Attach,
                        _ => input_origin(1),
                    };
                    let mut ps = Vec::new();
                    for (i, p) in parts.into_iter().enumerate() {
                        if i > 0 {
                            ps.push(Part::Pause(Duration::from_millis(1)));
                        }
                        ps.push(Part::Bytes(p));
                    }
                    pending.push(submit(&r.session, unit(origin, ps), open()));
                }
                Op::Query => {
                    queries += 1;
                    r.child.emit(DSR5);
                }
                Op::Expired => {
                    let past = Instant::now();
                    pending.push(submit(&r.session, key(b"x"), Guard::until(past)));
                }
                Op::StaleBasis => {
                    let g = Guard { basis: Some(Basis { seq: u64::MAX / 2, counted: ClassSet::ALL }), ..open() };
                    pending.push(submit(&r.session, key(b"y"), g));
                }
                Op::CancelLast => {
                    if let Some(p) = pending.last() {
                        let _ = p.ticket.cancel();
                    }
                }
            }
        }
        for p in &pending {
            p.wait();
        }
        prop_assert!(wait_until(|| r.session.status().responder.dispatched >= queries
            && r.session.writes_after(0).unwrap().iter().all(|x| x.outcome != WriteOutcome::InProgress)));
        let recs = r.session.writes_after(0).unwrap();
        prop_assert_eq!(recs.len() as u64, pending.len() as u64 + queries);
        assert_partition(&recs, &r.child.input());
    }
}

/// PR2-CL-2: a human write submitted during a unit's pause lands after the unit.
#[test]
fn human_write_waits_for_the_unit() {
    let r = rig();
    let probe = r.session.probe();
    let i = submit(
        &r.session,
        unit(input_origin(1), vec![b("text"), pause(30), b("\r")]),
        open(),
    );
    assert!(probe.wait(ProbePoint::PauseEntered, 1, soon()));
    let k = submit(&r.session, key(b"K"), open());
    i.wait();
    k.wait();
    assert_eq!(
        r.child.writes(),
        vec![b"text".to_vec(), b"\r".to_vec(), b"K".to_vec()]
    );
}

/// PR2-CL-2: a query emitted during a unit's pause is answered after its submit.
#[test]
fn reply_waits_for_the_unit_boundary() {
    let r = rig();
    let probe = r.session.probe();
    let i = submit(
        &r.session,
        unit(input_origin(1), vec![b("text"), pause(30), b("\r")]),
        open(),
    );
    assert!(probe.wait(ProbePoint::PauseEntered, 1, soon()));
    r.child.emit(DSR5);
    assert!(probe.wait(ProbePoint::ReplyEnqueued, 1, soon()));
    assert_eq!(i.wait().outcome, WriteOutcome::Written);
    assert!(wait_until(|| r.child.writes().len() == 3));
    assert_eq!(
        r.child.writes(),
        vec![b"text".to_vec(), b"\r".to_vec(), DSR5_REPLY.to_vec()]
    );
}

fn assert_refused(rec: &WriteRecord, reason: &Refusal) {
    assert_eq!(
        rec.outcome,
        WriteOutcome::NotStarted(reason.clone()),
        "{rec:?}"
    );
    assert_eq!(rec.bytes_written, 0);
    assert_eq!(rec.started_at, None);
}

/// PR2-CL-3: one case per refusal reason; each writes zero bytes.
#[test]
#[allow(clippy::too_many_lines, reason = "one case per reason")]
fn each_refusal_writes_zero_bytes() {
    // Cancelled: queued behind a held write.
    let r = rig();
    let hold = r.child.hold_writes();
    let a = submit(&r.session, key(b"a"), open());
    assert!(hold.wait_entered(soon()));
    let c = submit(&r.session, key(b"never"), open());
    assert!(c.ticket.cancel());
    assert_refused(&c.wait(), &Refusal::Cancelled);
    hold.release();
    a.wait();
    assert_eq!(r.child.input(), b"a");

    // Closed: queued when the session closes.
    let r = rig();
    let hold = r.child.hold_writes();
    let _a = submit(&r.session, key(b"a"), open());
    assert!(hold.wait_entered(soon()));
    let c = submit(&r.session, key(b"never"), open());
    let child = r.child.clone();
    let _ = r.session.close(&[], short_bounds());
    assert_refused(&c.wait(), &Refusal::Closed);
    drop(hold);
    assert!(wait_until(|| child
        .log()
        .contains(&FakeEvent::InputDropped)));
    assert_eq!(child.input(), b"a");

    // ChildExited.
    let r = rig();
    r.child.exit(chaldea_pty_runtime::ChildExit {
        code: Some(0),
        signal: None,
    });
    assert!(r.events.wait(|e| matches!(e, SessionEvent::Exited(_))));
    assert_refused(
        &submit(&r.session, key(b"x"), open()).wait(),
        &Refusal::ChildExited,
    );
    assert!(r.child.input().is_empty());

    // InputBroken: after a failed write.
    let r = rig();
    r.child.fail_writes_after(0, io::ErrorKind::BrokenPipe);
    let first = submit(&r.session, key(b"x"), open()).wait();
    assert!(matches!(first.outcome, WriteOutcome::Incomplete { .. }));
    assert_refused(
        &submit(&r.session, key(b"y"), open()).wait(),
        &Refusal::InputBroken,
    );
    assert!(r.child.input().is_empty());

    let r = rig();
    // Expired.
    let past = Instant::now();
    assert_refused(
        &submit(&r.session, key(b"x"), Guard::until(past)).wait(),
        &Refusal::Expired,
    );
    // WriteSinceBasis: a counted write after the basis.
    let basis = r.session.last_started(ClassSet::ALL);
    submit(&r.session, key(b"k"), open()).wait();
    let g = Guard {
        basis: Some(Basis {
            seq: basis,
            counted: ClassSet::ALL,
        }),
        ..open()
    };
    let rec = submit(&r.session, key(b"x"), g).wait();
    assert_refused(&rec, &Refusal::WriteSinceBasis { seq: 2 });
    // BasisUnknown: a basis above the head.
    let g = Guard {
        basis: Some(Basis {
            seq: 100,
            counted: ClassSet::ALL,
        }),
        ..open()
    };
    assert_refused(
        &submit(&r.session, key(b"x"), g).wait(),
        &Refusal::BasisUnknown {
            basis: 100,
            head: 3,
        },
    );
    // BracketedPasteChanged: 2004 off.
    let g = Guard {
        bracketed_paste: Some(0),
        ..open()
    };
    assert_refused(
        &submit(&r.session, key(b"x"), g).wait(),
        &Refusal::BracketedPasteChanged,
    );
    // BracketedPasteChanged: 2004 on with another epoch.
    emit_parsed(&r, b"\x1b[?2004h");
    assert_eq!(r.session.modes().bracketed_paste_epoch, 1);
    let g = Guard {
        bracketed_paste: Some(0),
        ..open()
    };
    assert_refused(
        &submit(&r.session, key(b"x"), g).wait(),
        &Refusal::BracketedPasteChanged,
    );
    // Recheck fails.
    let g = Guard {
        recheck: Some(Box::new(|_| Err("gate closed".into()))),
        ..open()
    };
    assert_refused(
        &submit(&r.session, key(b"x"), g).wait(),
        &Refusal::Recheck("gate closed".into()),
    );
    // Recheck panics.
    let g = Guard {
        recheck: Some(Box::new(|_| panic!("recheck bug"))),
        ..open()
    };
    assert_refused(
        &submit(&r.session, key(b"x"), g).wait(),
        &Refusal::RecheckPanicked,
    );
    assert_eq!(r.child.input(), b"k");
}

/// PR2-CL-3: after exit, `ChildExited` wins over every other check except `Closed`.
#[test]
fn child_exited_precedes_other_checks() {
    let r = rig();
    r.child.exit(chaldea_pty_runtime::ChildExit {
        code: Some(1),
        signal: None,
    });
    assert!(r.events.wait(|e| matches!(e, SessionEvent::Exited(_))));
    let ran = Arc::new(AtomicBool::new(false));
    let ran2 = ran.clone();
    let g = Guard {
        not_after: Instant::now(),
        basis: Some(Basis {
            seq: 99,
            counted: ClassSet::ALL,
        }),
        bracketed_paste: Some(7),
        recheck: Some(Box::new(move |_| {
            ran2.store(true, Ordering::SeqCst);
            Err("no".into())
        })),
    };
    assert_refused(
        &submit(&r.session, key(b"x"), g).wait(),
        &Refusal::ChildExited,
    );
    assert!(!ran.load(Ordering::SeqCst));
}

/// PR2-CL-3: `Expired` is decided before the recheck runs.
#[test]
fn expired_precedes_recheck() {
    let r = rig();
    let ran = Arc::new(AtomicBool::new(false));
    let ran2 = ran.clone();
    let g = Guard {
        recheck: Some(Box::new(move |_| {
            ran2.store(true, Ordering::SeqCst);
            Err("no".into())
        })),
        ..Guard::until(Instant::now())
    };
    assert_refused(&submit(&r.session, key(b"x"), g).wait(), &Refusal::Expired);
    assert!(!ran.load(Ordering::SeqCst));
}

/// PR2-CL-4: `last_started` advances before the first write call; the in-progress span
/// is visible while the write blocks; a failed write leaves a partial span.
#[test]
fn last_started_advances_on_dispatch() {
    let r = rig();
    let hold = r.child.hold_writes();
    let a = submit(&r.session, key(b"abc"), open());
    assert!(hold.wait_entered(soon()));
    assert_eq!(
        r.session.last_started(ClassSet::of(&[OriginClass::Human])),
        1
    );
    assert_eq!(
        r.session.last_started(ClassSet::of(&[OriginClass::Input])),
        0
    );
    let live = r.session.writes_after(0).unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].outcome, WriteOutcome::InProgress);
    assert!(live[0].started_at.is_some());
    assert_eq!((live[0].byte_start, live[0].bytes_written), (0, 0));
    hold.release();
    assert_eq!(a.wait().outcome, WriteOutcome::Written);

    r.child.fail_writes_after(5, io::ErrorKind::BrokenPipe);
    let rec = submit(&r.session, unit(input_origin(2), vec![b("defgh")]), open()).wait();
    assert_eq!(
        r.session.last_started(ClassSet::of(&[OriginClass::Input])),
        2
    );
    assert!(
        matches!(rec.outcome, WriteOutcome::Incomplete { .. }),
        "{rec:?}"
    );
    assert_eq!(
        (rec.byte_start, rec.bytes_written, rec.split_writes),
        (3, 2, 1)
    );
    assert_eq!(r.child.input(), b"abcde");
}

fn basis(seq: u64, counted: ClassSet) -> Guard {
    Guard {
        basis: Some(Basis { seq, counted }),
        ..open()
    }
}

/// PR2-CL-4: a stale basis refuses while the input is healthy, and a fresh one admits.
#[test]
fn stale_basis_refuses_with_healthy_input() {
    let r = rig();
    let stale = r.session.last_started(ClassSet::ALL);
    submit(&r.session, key(b"k"), open()).wait();
    let rec = submit(&r.session, key(b"x"), basis(stale, ClassSet::ALL)).wait();
    assert_refused(&rec, &Refusal::WriteSinceBasis { seq: 1 });
    assert!(!r.session.status().input_broken);
    let fresh = r.session.last_started(ClassSet::ALL);
    let rec = submit(&r.session, key(b"y"), basis(fresh, ClassSet::ALL)).wait();
    assert_eq!(rec.outcome, WriteOutcome::Written);
    assert_eq!(r.child.input(), b"ky");
}

/// PR2-CL-4: a startup reply before readiness is inside the basis; a reply after it
/// refuses the send.
#[test]
fn reply_before_basis_admits_after_refuses() {
    let r = rig();
    r.child.emit(DSR5);
    assert!(wait_until(|| r.session.last_started(ClassSet::ALL) == 1));
    let ready = r.session.last_started(ClassSet::ALL);
    let rec = submit(
        &r.session,
        unit(input_origin(1), vec![b("a")]),
        basis(ready, ClassSet::ALL),
    )
    .wait();
    assert_eq!(rec.outcome, WriteOutcome::Written);
    let ready = r.session.last_started(ClassSet::ALL);
    r.child.emit(DSR5);
    assert!(wait_until(|| r.session.last_started(ClassSet::ALL) == 3));
    let rec = submit(
        &r.session,
        unit(input_origin(2), vec![b("b")]),
        basis(ready, ClassSet::ALL),
    )
    .wait();
    assert_refused(&rec, &Refusal::WriteSinceBasis { seq: 3 });
}

/// PR2-CL-4: the counters decide, not the ring: a human write evicted from the ring by
/// replies still refuses a basis counting `Human`; replies outside `counted` do not.
#[test]
fn counted_human_after_reply_only_eviction() {
    let r = rig_limits(Limits {
        log_records: 8,
        ..Limits::default()
    });
    let before = r.session.last_started(ClassSet::ALL);
    submit(&r.session, key(b"k"), open()).wait();
    let after_human = r.session.last_started(ClassSet::ALL);
    for _ in 0..10 {
        r.child.emit(DSR5);
    }
    assert!(wait_until(|| r.session.last_started(ClassSet::ALL) == 11));
    assert!(
        r.session.writes_after(0).is_err(),
        "the human record was evicted"
    );
    let human = ClassSet::of(&[OriginClass::Human]);
    let rec = submit(
        &r.session,
        unit(input_origin(1), vec![b("a")]),
        basis(before, human),
    )
    .wait();
    assert_refused(&rec, &Refusal::WriteSinceBasis { seq: 1 });
    let rec = submit(
        &r.session,
        unit(input_origin(1), vec![b("b")]),
        basis(after_human, human),
    )
    .wait();
    assert_eq!(rec.outcome, WriteOutcome::Written);
}

/// PR2-CL-5: a recheck runs after every earlier unit finished, with their counters.
#[test]
fn recheck_sees_every_earlier_write() {
    let r = rig();
    let child = r.child.clone();
    let a = submit(&r.session, key(b"A"), open());
    let b2 = submit(
        &r.session,
        WriteUnit::bytes(WriteOrigin::Attach, "B"),
        open(),
    );
    let g = Guard {
        recheck: Some(Box::new(move |ctx| {
            if child.input() != b"AB" {
                return Err(format!("saw {:?}", child.input()));
            }
            if ctx.seq != 3 || ctx.last_started[OriginClass::Human.index()] != 2 {
                return Err(format!("context {ctx:?}"));
            }
            Ok(())
        })),
        ..open()
    };
    let c = submit(&r.session, unit(input_origin(1), vec![b("C")]), g);
    a.wait();
    b2.wait();
    assert_eq!(c.wait().outcome, WriteOutcome::Written);
    assert_eq!(r.child.input(), b"ABC");
}

/// PR2-CL-5: DECRST 2004 while the recheck runs refuses at the dispatch boundary.
#[test]
fn decrst_during_recheck_refuses() {
    let r = rig();
    emit_parsed(&r, b"\x1b[?2004h");
    let epoch = r.session.modes().bracketed_paste_epoch;
    let probe = r.session.probe();
    let park = probe.park(ProbePoint::RecheckEntered);
    let g = Guard {
        bracketed_paste: Some(epoch),
        recheck: Some(Box::new(|_| Ok(()))),
        ..open()
    };
    let p = submit(&r.session, unit(input_origin(1), vec![b("text")]), g);
    assert!(probe.wait(ProbePoint::RecheckEntered, 1, soon()));
    emit_parsed(&r, b"\x1b[?2004l");
    park.release();
    assert_refused(&p.wait(), &Refusal::BracketedPasteChanged);
    assert!(r.child.input().is_empty());
}

/// PR2-CL-5: close during the recheck settles the unit once, `Closed`, with zero bytes.
#[test]
fn close_during_recheck_settles_once() {
    let r = rig();
    let probe = r.session.probe();
    let park = probe.park(ProbePoint::RecheckEntered);
    let (done, seen) = counting();
    let g = Guard {
        recheck: Some(Box::new(|_| Ok(()))),
        ..open()
    };
    r.session.submit(key(b"x"), g, done).unwrap();
    assert!(probe.wait(ProbePoint::RecheckEntered, 1, soon()));
    let child = r.child.clone();
    let closer = std::thread::spawn(move || r.session.close(&[], short_bounds()));
    assert!(wait_until(|| seen.lock().unwrap().len() == 1));
    park.release();
    let out = closer.join().unwrap();
    assert!(out.abandoned.is_empty(), "{out:?}");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_refused(&seen[0], &Refusal::Closed);
    assert!(child.input().is_empty());
}

/// PR2-CL-6: a dispatched unit writes its remaining parts after its deadline.
#[test]
fn started_unit_ignores_its_deadline() {
    let r = rig();
    let g = Guard::until(Instant::now() + ms(2000));
    let probe = r.session.probe();
    let p = submit(
        &r.session,
        unit(input_origin(1), vec![b("a"), pause(50), b("b")]),
        g,
    );
    assert!(probe.wait(ProbePoint::PauseEntered, 1, soon()));
    let rec = p.wait();
    assert_eq!(rec.outcome, WriteOutcome::Written);
    // A deadline that passes during the pause does not stop the submit either.
    let p = submit(
        &r.session,
        unit(input_origin(2), vec![b("c"), pause(200), b("d")]),
        Guard::until(Instant::now() + ms(100)),
    );
    assert_eq!(p.wait().outcome, WriteOutcome::Written);
    assert_eq!(r.child.input(), b"abcd");
}

/// PR2-CL-6: close during the pause stops the unit before its submit.
#[test]
fn close_in_pause_skips_the_submit() {
    let r = rig();
    let probe = r.session.probe();
    let p = submit(
        &r.session,
        unit(input_origin(1), vec![b("text"), pause(4000), b("\r")]),
        open(),
    );
    assert!(probe.wait(ProbePoint::PauseEntered, 1, soon()));
    let child = r.child.clone();
    let out = r.session.close(&[], short_bounds());
    let rec = p.wait();
    assert_eq!(
        rec.outcome,
        WriteOutcome::Incomplete {
            error: "closed".into()
        }
    );
    assert_eq!(rec.bytes_written, 4);
    assert_eq!(child.input(), b"text");
    assert!(out.released.input, "{out:?}");
}

/// PR2-CL-6: child exit during the pause stops the unit before its submit.
#[test]
fn exit_in_pause_skips_the_submit() {
    let r = rig();
    let probe = r.session.probe();
    let p = submit(
        &r.session,
        unit(input_origin(1), vec![b("text"), pause(4000), b("\r")]),
        open(),
    );
    assert!(probe.wait(ProbePoint::PauseEntered, 1, soon()));
    r.child.exit(chaldea_pty_runtime::ChildExit {
        code: Some(0),
        signal: None,
    });
    let rec = p.wait();
    assert_eq!(
        rec.outcome,
        WriteOutcome::Incomplete {
            error: "child_exited".into()
        }
    );
    assert_eq!(r.child.input(), b"text");
}

/// PR2-CL-6: a call permitted before close still reaches the backend after `close`
/// returns; the record holds the prefix before it and the unit is `unknown_after_close`.
#[test]
fn close_after_permission_reports_unknown() {
    let r = rig();
    let probe = r.session.probe();
    submit(&r.session, key(b"pre"), open()).wait();
    let park = probe.park(ProbePoint::Permitted);
    let p = submit(
        &r.session,
        unit(input_origin(1), vec![b("abc"), pause(1), b("def")]),
        open(),
    );
    assert!(probe.wait(ProbePoint::Permitted, 2, soon()));
    let child = r.child.clone();
    let out = r.session.close(&[], short_bounds());
    let rec = p.wait();
    assert_eq!(
        rec.outcome,
        WriteOutcome::Incomplete {
            error: "unknown_after_close".into()
        }
    );
    assert_eq!((rec.byte_start, rec.bytes_written), (3, 0));
    assert_eq!(out.abandoned, vec!["pty-writer"]);
    assert_eq!(child.input(), b"pre");
    park.release();
    assert!(wait_until(|| child
        .log()
        .contains(&FakeEvent::InputDropped)));
    assert_eq!(child.writes(), vec![b"pre".to_vec(), b"abc".to_vec()]);
    assert!(p.try_now().is_none(), "the completion fired once");
}

/// PR2-CL-7: a write error ends the unit `Incomplete`, breaks the input once, and later
/// units are refused.
#[test]
fn write_error_breaks_input() {
    let r = rig();
    r.child.fail_writes_after(0, io::ErrorKind::BrokenPipe);
    let rec = submit(&r.session, key(b"abc"), open()).wait();
    assert!(
        matches!(rec.outcome, WriteOutcome::Incomplete { .. }),
        "{rec:?}"
    );
    assert_eq!(rec.bytes_written, 0);
    assert_refused(
        &submit(&r.session, key(b"d"), open()).wait(),
        &Refusal::InputBroken,
    );
    assert_refused(
        &submit(&r.session, key(b"e"), open()).wait(),
        &Refusal::InputBroken,
    );
    assert_eq!(
        r.events
            .count(|e| matches!(e, SessionEvent::InputBroken(_))),
        1
    );
    assert!(r.session.status().input_broken);
}

/// PR2-CL-7: `Ok(0)` for a non-empty part breaks the input like an error.
#[test]
fn write_zero_breaks_input() {
    let r = rig();
    r.child.short_writes(0);
    let rec = submit(&r.session, key(b"abc"), open()).wait();
    assert_eq!(
        rec.outcome,
        WriteOutcome::Incomplete {
            error: "write_zero".into()
        }
    );
    assert_refused(
        &submit(&r.session, key(b"d"), open()).wait(),
        &Refusal::InputBroken,
    );
    assert_eq!(
        r.events
            .count(|e| matches!(e, SessionEvent::InputBroken(_))),
        1
    );
}

/// PR2-CL-8: a full lane fails at once with no record, while the writer is blocked.
#[test]
fn full_queue_fails_without_blocking() {
    let r = rig_limits(Limits {
        queue_units: 2,
        queue_bytes: 10,
        ..Limits::default()
    });
    let hold = r.child.hold_writes();
    let a = submit(&r.session, key(b"a"), open());
    assert!(hold.wait_entered(soon()));
    let _b = submit(&r.session, key(b"bb"), open());
    let t = Instant::now();
    let (done, _rx) = completion_channel();
    let e = r
        .session
        .submit(key(b"ccccccccc"), open(), done)
        .unwrap_err();
    assert_eq!(e, SubmitError::QueueFull, "bytes over queue_bytes");
    let _c = submit(&r.session, key(b"c"), open());
    let (done, _rx) = completion_channel();
    assert_eq!(
        r.session.submit(key(b"d"), open(), done).unwrap_err(),
        SubmitError::QueueFull
    );
    assert!(t.elapsed() < Duration::from_secs(2));
    assert_eq!(
        r.session.writes_after(0).unwrap().len(),
        1,
        "only the in-progress record"
    );
    hold.release();
    a.wait();
}

/// PR2-CL-8: every bad shape fails at submit.
#[test]
fn bad_shapes_are_refused() {
    let r = rig();
    let cases = [
        (vec![], "1 to 8 parts"),
        (vec![b("a"); 9], "1 to 8 parts"),
        (vec![pause(1), b("a")], "first and last part are bytes"),
        (vec![b("a"), pause(1)], "first and last part are bytes"),
        (vec![b("a"), pause(1), pause(1), b("b")], "adjacent pauses"),
        (vec![b("a"), b("")], "empty bytes part"),
        (
            vec![b("a"), pause(6000), b("b")],
            "total pause over the limit",
        ),
        (
            vec![b("a"), pause(3000), b("b"), pause(3000), b("c")],
            "total pause over the limit",
        ),
        (
            vec![b("a"), Part::Pause(Duration::MAX), b("b"), pause(1), b("c")],
            "total pause over the limit",
        ),
    ];
    for (parts, why) in cases {
        let (done, _rx) = completion_channel();
        let e = r
            .session
            .submit(unit(WriteOrigin::Key, parts.clone()), open(), done)
            .unwrap_err();
        assert_eq!(e, SubmitError::BadShape(why), "{parts:?}");
    }
    assert!(r.session.writes_after(0).unwrap().is_empty());
    assert!(r.child.input().is_empty());
}

/// PR2-CL-8: the configured `max_unit_bytes` is inclusive.
#[test]
fn unit_at_limit_accepted_limit_plus_one_refused() {
    let r = rig_limits(Limits {
        max_unit_bytes: 16,
        ..Limits::default()
    });
    let (done, _rx) = completion_channel();
    let e = r
        .session
        .submit(
            unit(WriteOrigin::Key, vec![b([b'x'; 9]), pause(1), b([b'y'; 8])]),
            open(),
            done,
        )
        .unwrap_err();
    assert_eq!(e, SubmitError::UnitTooLarge);
    let rec = submit(
        &r.session,
        unit(WriteOrigin::Key, vec![b([b'x'; 8]), pause(1), b([b'y'; 8])]),
        open(),
    )
    .wait();
    assert_eq!(rec.outcome, WriteOutcome::Written);
    assert_eq!(r.child.input().len(), 16);
    assert_eq!(r.session.writes_after(0).unwrap().len(), 1);
}

/// PR2-CL-8: callers cannot submit replies.
#[test]
fn reply_origin_is_refused() {
    let r = rig();
    let (done, _rx) = completion_channel();
    let e = r
        .session
        .submit(
            WriteUnit::bytes(
                WriteOrigin::VtReply {
                    kind: ReplyKind::Dsr5,
                },
                DSR5_REPLY,
            ),
            open(),
            done,
        )
        .unwrap_err();
    assert_eq!(e, SubmitError::ReplyOrigin);
    assert!(r.session.writes_after(0).unwrap().is_empty());
}

/// PR2-CL-21: per-class counters only move forward and do not depend on the ring.
#[test]
fn counters_are_monotonic() {
    let r = rig_limits(Limits {
        log_records: 2,
        ..Limits::default()
    });
    let mut last = [0u64; 3];
    for i in 0..12u8 {
        match i % 4 {
            0 => drop(submit(&r.session, key(b"k"), open()).wait()),
            1 => drop(submit(&r.session, unit(input_origin(1), vec![b("i")]), open()).wait()),
            2 => {
                let n = r.session.last_started(ClassSet::of(&[OriginClass::Reply]));
                r.child.emit(DSR5);
                assert!(wait_until(|| r
                    .session
                    .last_started(ClassSet::of(&[OriginClass::Reply]))
                    > n));
            }
            _ => drop(submit(&r.session, key(b"k"), Guard::until(Instant::now())).wait()),
        }
        for c in OriginClass::ALL {
            let now = r.session.last_started(ClassSet::of(&[c]));
            assert!(now >= last[c.index()], "{c:?} went back");
            last[c.index()] = now;
        }
        let all = r.session.last_started(ClassSet::ALL);
        assert_eq!(all, *last.iter().max().unwrap());
    }
    assert!(last.iter().all(|&n| n > 0));
}

/// PR2-CL-21: `writes_after` reports eviction instead of a silent gap.
#[test]
fn writes_after_reports_truncation() {
    let r = rig_limits(Limits {
        log_records: 4,
        ..Limits::default()
    });
    for _ in 0..6 {
        submit(&r.session, key(b"k"), open()).wait();
    }
    assert!(r.session.writes_after(0).is_err());
    assert!(r.session.writes_after(1).is_err());
    let recs = r.session.writes_after(2).unwrap();
    assert_eq!(
        recs.iter().map(|x| x.seq).collect::<Vec<_>>(),
        vec![3, 4, 5, 6]
    );
    assert!(r.session.writes_after(6).unwrap().is_empty());
}

/// PR2-CL-21: the blocked, in-progress record is in `writes_after`.
#[test]
fn writes_after_shows_the_blocked_write() {
    let r = rig();
    submit(&r.session, key(b"a"), open()).wait();
    let hold = r.child.hold_writes();
    let p = submit(&r.session, key(b"b"), open());
    assert!(hold.wait_entered(soon()));
    let recs = r.session.writes_after(1).unwrap();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].seq, 2);
    assert_eq!(recs[0].outcome, WriteOutcome::InProgress);
    hold.release();
    p.wait();
    assert_eq!(
        r.session.writes_after(1).unwrap()[0].outcome,
        WriteOutcome::Written
    );
}

/// PR2-CL-26: a short accept is completed in further calls and counted.
#[test]
fn short_writes_are_counted() {
    let r = rig();
    r.child.short_writes(3);
    let rec = submit(&r.session, key(b"0123456789"), open()).wait();
    assert_eq!(rec.outcome, WriteOutcome::Written);
    assert_eq!((rec.bytes_written, rec.split_writes), (10, 3));
    assert_eq!(
        r.child.writes(),
        vec![
            b"012".to_vec(),
            b"345".to_vec(),
            b"678".to_vec(),
            b"9".to_vec()
        ]
    );
}

/// PR2-CL-27: an approval card closed after enqueue refuses its key with zero bytes.
#[test]
fn card_key_refused_after_enqueue() {
    let r = rig();
    let card_open = Arc::new(AtomicBool::new(true));
    let hold = r.child.hold_writes();
    let a = submit(&r.session, key(b"a"), open());
    assert!(hold.wait_entered(soon()));
    let flag = card_open.clone();
    let g = Guard {
        recheck: Some(Box::new(move |_| {
            if flag.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err("card_closed".into())
            }
        })),
        ..open()
    };
    let origin = WriteOrigin::CardKey {
        approval_id: ApprovalId::from_u128(9),
    };
    let p = submit(&r.session, WriteUnit::bytes(origin, "1"), g);
    card_open.store(false, Ordering::SeqCst);
    hold.release();
    a.wait();
    assert_refused(&p.wait(), &Refusal::Recheck("card_closed".into()));
    assert_eq!(r.child.input(), b"a");
}

/// A panic in the backend `write` settles every unit and breaks the input (§5 endings).
#[test]
fn writer_panic_settles_everything() {
    let r = rig();
    let hold = r.child.hold_writes();
    let a = submit(&r.session, key(b"a"), open());
    assert!(hold.wait_entered(soon()));
    let queued = submit(&r.session, key(b"b"), open());
    r.child.panic_on_next_write();
    hold.release();
    a.wait();
    // The second unit's write panics.
    let rec = queued.wait();
    assert_eq!(
        rec.outcome,
        WriteOutcome::Incomplete {
            error: "unknown_after_panic".into()
        }
    );
    assert!(r.session.status().input_broken);
    let (done, _rx) = completion_channel();
    assert_eq!(
        r.session.submit(key(b"c"), open(), done).unwrap_err(),
        SubmitError::Closed
    );
    assert!(wait_until(|| r
        .child
        .log()
        .contains(&FakeEvent::InputDropped)));
}

/// PR2-CL-3 / CL-20: an exit that is visible before a unit's first write permission
/// refuses the unit; a unit permitted first may write.
#[test]
fn exit_visible_before_dispatch_refuses() {
    let r = rig();
    let probe = r.session.probe();
    let park = probe.park(ProbePoint::Dispatching);
    let p = submit(&r.session, key(b"X"), open());
    assert!(probe.wait(ProbePoint::Dispatching, 1, soon()));
    r.child.exit(chaldea_pty_runtime::ChildExit {
        code: Some(0),
        signal: None,
    });
    let visible = wait_for(ms(500), || r.session.status().exit.is_some());
    park.release();
    let rec = p.wait();
    assert!(
        !(visible && rec.outcome == WriteOutcome::Written),
        "exit was visible before the permission, yet {rec:?}"
    );
    assert!(r.events.wait(|e| matches!(e, SessionEvent::Exited(_))));
}

/// PR2-CL-3 / CL-5: DECRST 2004 visible in `modes()` before a guarded paste's first write
/// permission refuses it with zero bytes.
#[test]
fn decrst_visible_before_dispatch_refuses() {
    let r = rig();
    emit_parsed(&r, b"\x1b[?2004h");
    let epoch = r.session.modes().bracketed_paste_epoch;
    let probe = r.session.probe();
    let park = probe.park(ProbePoint::Dispatching);
    let g = Guard {
        bracketed_paste: Some(epoch),
        recheck: Some(Box::new(|_| Ok(()))),
        ..open()
    };
    let p = submit(
        &r.session,
        unit(input_origin(1), vec![b("\x1b[200~text\x1b[201~")]),
        g,
    );
    assert!(probe.wait(ProbePoint::Dispatching, 1, soon()));
    r.child.emit(b"\x1b[?2004l");
    let visible = wait_for(ms(500), || !r.session.modes().modes.bracketed_paste);
    park.release();
    let rec = p.wait();
    assert!(
        !(visible && rec.outcome == WriteOutcome::Written),
        "2004 was off before the permission, yet {rec:?}"
    );
    wait_parsed(&r.session, 16);
    assert!(!r.session.modes().modes.bracketed_paste);
}

/// PR2-CL-28: a recheck capture is dropped after the queue lock is released, so its
/// destructor may call the session: cancelling the queued unit does not deadlock.
#[test]
fn cancelled_recheck_capture_is_dropped_unlocked() {
    let Rig { session, child, .. } = rig();
    let session = Arc::new(session);
    let hold = child.hold_writes();
    let a = submit(&session, key(b"a"), open());
    assert!(hold.wait_entered(soon()));
    let (tx, rx) = std::sync::mpsc::channel();
    let w = Arc::downgrade(&session);
    let capture = OnDrop(move || {
        let seen = w.upgrade().map(|s| s.last_started(ClassSet::ALL));
        let _ = tx.send(seen);
    });
    let g = Guard {
        recheck: Some(Box::new(move |_| {
            let _keep = &capture;
            Ok(())
        })),
        ..open()
    };
    let b2 = submit(&session, key(b"b"), g);
    let s2 = session.clone();
    let ticket = b2.ticket;
    std::thread::spawn(move || {
        let _ = ticket.cancel();
        drop(s2);
    });
    let Ok(seen) = rx.recv_timeout(Duration::from_secs(5)) else {
        // Deadlocked under the queue lock: nothing can be torn down.
        std::mem::forget(session);
        std::mem::forget(hold);
        panic!("the recheck capture was dropped under the queue lock");
    };
    assert_eq!(seen, Some(1));
    hold.release();
    a.wait();
}

/// PR2-CL-1: a write call that unwinds after the backend accepted its bytes leaves an
/// uncertain span, as `unknown_after_close` does: the record keeps its confirmed prefix
/// and says the last permitted call may have written more.
#[test]
fn panic_after_acceptance_marks_the_span_uncertain() {
    let r = rig();
    submit(&r.session, key(b"pre"), open()).wait();
    r.child.on_input(|bytes| {
        assert!(bytes != b"abc", "script bug after acceptance");
        Vec::new()
    });
    let rec = submit(&r.session, key(b"abc"), open()).wait();
    assert_eq!(
        rec.outcome,
        WriteOutcome::Incomplete {
            error: "unknown_after_panic".into()
        }
    );
    assert_eq!(
        (rec.byte_start, rec.bytes_written),
        (3, 0),
        "the confirmed prefix"
    );
    assert_eq!(
        r.child.input(),
        b"preabc",
        "the call wrote more than the prefix"
    );
}

/// PR2-CL-3 / CL-5: `not_after` is part of the first-permission decision: a unit whose
/// deadline passes after its recheck and before that permission is refused `Expired`.
#[test]
fn deadline_passed_before_first_permission_expires() {
    for recheck in [false, true] {
        let r = rig();
        let probe = r.session.probe();
        let park = probe.park(ProbePoint::Dispatching);
        let not_after = Instant::now() + ms(200);
        let g = Guard {
            recheck: recheck.then(|| -> chaldea_pty_runtime::Recheck { Box::new(|_| Ok(())) }),
            ..Guard::until(not_after)
        };
        let p = submit(&r.session, key(b"late"), g);
        assert!(probe.wait(ProbePoint::Dispatching, 1, soon()));
        // Waiting out the deadline, not ordering threads.
        std::thread::sleep(not_after.saturating_duration_since(Instant::now()) + ms(20));
        park.release();
        assert_refused(&p.wait(), &Refusal::Expired);
        assert!(r.child.input().is_empty(), "recheck: {recheck}");
    }
}
