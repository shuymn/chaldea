//! The session: start, resize, subscribe, drain endings, close order, and callbacks
//! (`docs/design/pty-runtime.md` §2.2, §2.4, §5), PR2-CL-14 to CL-20, CL-24, CL-28.

mod common;

use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use chaldea_pty_runtime::fake::{FakeEvent, FakePty, FakeScript, KillBehavior, ProbePoint};
use chaldea_pty_runtime::{
    ChildExit, Cleanup, CloseBounds, Flow, Limits, OutputEnd, OutputEvent, OutputSink,
    ProcessIdentity, PtySize, Received, Refusal, Resized, Session, SessionConfig, SessionEvent,
    SpawnError, StartFault, SubscribeError, VtUnavailable, WriteOutcome, channel_sink,
};
use common::*;

fn extra() -> ProcessIdentity {
    ProcessIdentity {
        pid: 4242,
        start_time: "descendant".into(),
        group: "fake-d".into(),
    }
}

/// The fake log without write-path entries.
fn lifecycle(log: &[FakeEvent]) -> Vec<FakeEvent> {
    log.iter()
        .filter(|e| !matches!(e, FakeEvent::WriteEntered | FakeEvent::Write(_)))
        .cloned()
        .collect()
}

/// PR2-CL-14: the PTY is resized first, then the grid and the responder's size.
#[test]
fn resize_moves_pty_grid_and_responder() {
    let r = rig();
    assert_eq!(r.session.size(), PtySize::new(80, 24).unwrap());
    let to = PtySize::new(100, 30).unwrap();
    assert_eq!(r.session.resize(to).unwrap(), Resized::PtyAndVt);
    assert!(r.child.log().contains(&FakeEvent::Resize(to)));
    assert_eq!(r.session.size(), to);
    let s = r.session.snapshot(soon()).unwrap();
    assert_eq!((s.cols, s.rows, s.lines.len()), (100, 30, 30));
    r.child.emit(b"\x1b[18t");
    assert!(wait_until(
        || r.child.writes() == vec![b"\x1b[8;30;100t".to_vec()]
    ));
    // Output parsed after the resize is laid out at the new size.
    emit_parsed(&r, &[b'x'; 90]);
    assert_eq!(
        r.session.snapshot(soon()).unwrap().lines[0].text().len(),
        90
    );
}

/// PR2-CL-14: a failed PTY resize changes neither side.
#[test]
fn failed_resize_changes_nothing() {
    let r = rig();
    r.child.fail_next_resize();
    assert!(r.session.resize(PtySize::new(100, 30).unwrap()).is_err());
    assert!(r.child.log().contains(&FakeEvent::ResizeFailed));
    assert_eq!(r.session.size(), PtySize::new(80, 24).unwrap());
    let s = r.session.snapshot(soon()).unwrap();
    assert_eq!((s.cols, s.rows), (80, 24));
    r.child.emit(b"\x1b[18t");
    assert!(wait_until(
        || r.child.writes() == vec![b"\x1b[8;24;80t".to_vec()]
    ));
}

/// PR2-CL-14: sizes outside the valid range are refused before anything changes.
#[test]
fn invalid_size_is_refused() {
    for (c, rows) in [(0, 10), (1, 10), (10, 0), (4096, 10), (10, 4096)] {
        assert!(PtySize::new(c, rows).is_err(), "{c}x{rows}");
    }
    for (c, rows) in [(2, 1), (4095, 4095), (80, 24)] {
        let s = PtySize::new(c, rows).unwrap();
        assert_eq!((s.cols(), s.rows()), (c, rows));
    }
}

/// PR2-CL-14: before any resize, startup size queries (`CSI 18 t` characters, `CSI 14 t`
/// pixels) are answered at the spawn size.
#[test]
fn startup_size_answers_csi_18t() {
    // (cols, rows, startup output, expected replies)
    type Case = (u16, u16, &'static [u8], &'static [&'static [u8]]);
    let cases: [Case; 2] = [
        (132, 43, b"\x1b[18t", &[b"\x1b[8;43;132t"]),
        (
            120,
            40,
            b"\x1b[18t\x1b[14t",
            &[b"\x1b[8;40;120t", b"\x1b[4;640;960t"],
        ),
    ];
    for (cols, rows, startup, replies) in cases {
        let r = rig_with(
            FakeScript {
                startup_output: startup.to_vec(),
                ..FakeScript::default()
            },
            Limits::default(),
            cols,
            rows,
        );
        let want: Vec<Vec<u8>> = replies.iter().map(|b| b.to_vec()).collect();
        assert!(
            wait_until(|| r.child.writes().len() >= want.len()),
            "{cols}x{rows}"
        );
        assert_eq!(r.child.writes(), want, "{cols}x{rows}");
        assert_eq!(r.fake.spawns()[0].size, PtySize::new(cols, rows).unwrap());
    }
}

/// Bytes and ends a sink got.
#[derive(Default)]
struct Got {
    bytes: Vec<u8>,
    first: Option<u64>,
    ends: usize,
}

fn recorder() -> (OutputSink, Arc<Mutex<Got>>) {
    let got = Arc::new(Mutex::new(Got::default()));
    let g = got.clone();
    let sink: OutputSink = Box::new(move |ev| {
        let mut g = g.lock().unwrap();
        match ev {
            OutputEvent::Bytes { offset, bytes } => {
                let start = *g.first.get_or_insert(offset);
                assert_eq!(offset, start + g.bytes.len() as u64, "no gap, no duplicate");
                g.bytes.extend_from_slice(bytes);
            }
            OutputEvent::End(_) => g.ends += 1,
        }
        Flow::Continue
    });
    (sink, got)
}

/// PR2-CL-15: a subscriber gets exactly the bytes after its snapshot's offset: one that
/// subscribes between two pieces of one chunk (from inside a dispatch), and many that
/// subscribe from another thread while output streams.
#[test]
fn subscribe_is_gapless() {
    let r = rig();
    let session = Arc::new(r.session);
    let all = Arc::new(Mutex::new(Vec::<u8>::new()));
    type Inner = Arc<Mutex<Option<(u64, Arc<Mutex<Got>>)>>>;
    let inner: Inner = Arc::default();
    let weak: Weak<Session> = Arc::downgrade(&session);
    let (a, i2) = (all.clone(), inner.clone());
    let outer: OutputSink = Box::new(move |ev| {
        if let OutputEvent::Bytes { bytes, .. } = ev {
            a.lock().unwrap().extend_from_slice(bytes);
            let mut slot = i2.lock().unwrap();
            if slot.is_none()
                && let Some(s) = weak.upgrade()
            {
                let (sink, got) = recorder();
                let (snap, _) = s.subscribe(sink, soon()).unwrap();
                *slot = Some((snap.output_offset, got));
            }
        }
        Flow::Continue
    });
    session.subscribe(outer, soon()).unwrap();
    // One chunk, three pieces: "abc", "\x1b[1mdef", "\x1b[0mghi".
    let chunk = b"abc\x1b[1mdef\x1b[0mghi";
    r.child.emit(chunk);
    wait_parsed(&session, chunk.len() as u64);
    // Parsing runs ahead of dispatch; wait for the sinks before checking bytes.
    assert!(wait_until(|| all.lock().unwrap().len() >= chunk.len()));
    let (offset, got) = inner.lock().unwrap().clone().unwrap();
    assert_eq!(offset, 3, "subscribed after the first piece");
    assert!(wait_until(
        || got.lock().unwrap().bytes.len() >= chunk.len() - 3
    ));
    assert_eq!(got.lock().unwrap().bytes, &chunk[3..]);
    assert_eq!(*all.lock().unwrap(), chunk);

    // Racing subscriptions from another thread.
    let stream: Vec<u8> = (0..4000u32)
        .flat_map(|i| format!("\x1b[{}m{}", i % 8, i % 10).into_bytes())
        .collect();
    let base = chunk.len() as u64;
    let s2 = session.clone();
    let subscriber = std::thread::spawn(move || {
        let mut subs = Vec::new();
        for _ in 0..6 {
            let (sink, got) = recorder();
            let (snap, _) = s2.subscribe(sink, soon()).unwrap();
            subs.push((snap.output_offset, got));
            std::thread::yield_now();
        }
        subs
    });
    r.child.emit(&stream);
    let subs = subscriber.join().unwrap();
    wait_parsed(&session, base + stream.len() as u64);
    let full: Vec<u8> = chunk.iter().chain(stream.iter()).copied().collect();
    for (offset, got) in subs {
        let from = usize::try_from(offset).unwrap();
        assert!(wait_until(
            || got.lock().unwrap().bytes.len() >= full.len() - from
        ));
        let g = got.lock().unwrap();
        assert_eq!(g.bytes, &full[from..], "subscriber at {offset}");
        if let Some(first) = g.first {
            assert_eq!(first, offset);
        }
    }
}

/// PR2-CL-15: a subscriber racing EOF gets exactly one `End`, also when it subscribes
/// after the output ended.
#[test]
fn subscribe_racing_eof_gets_one_end() {
    for _ in 0..30 {
        let r = rig();
        r.child.emit(b"bytes");
        let child = r.child.clone();
        let closer = std::thread::spawn(move || child.close_output());
        let mut gots = Vec::new();
        for _ in 0..4 {
            let (sink, got) = recorder();
            r.session.subscribe(sink, soon()).unwrap();
            gots.push(got);
        }
        closer.join().unwrap();
        assert!(
            r.events
                .wait(|e| matches!(e, SessionEvent::OutputEnded(OutputEnd::Eof)))
        );
        let (sink, late) = recorder();
        r.session.subscribe(sink, soon()).unwrap();
        gots.push(late);
        for g in gots {
            assert_eq!(g.lock().unwrap().ends, 1);
        }
    }
}

/// PR2-CL-15: a full `channel_sink` still records the end.
#[test]
fn full_channel_sink_at_eof_ends() {
    let r = rig();
    let (sink, rx) = channel_sink(4);
    r.session.subscribe(sink, soon()).unwrap();
    emit_parsed(&r, b"abcd");
    r.child.close_output();
    assert_eq!(
        rx.recv_timeout(WAIT).unwrap(),
        Received::Bytes {
            offset: 0,
            bytes: b"abcd".to_vec()
        }
    );
    assert_eq!(
        rx.recv_timeout(WAIT).unwrap(),
        Received::End(OutputEnd::Eof)
    );
    assert!(rx.recv_timeout(ms(10)).is_err());
}

/// PR2-CL-15: bytes that do not fit are dropped, reported as `Lagged` at the first
/// dropped offset, and the sink stops.
#[test]
fn full_channel_sink_on_bytes_lags() {
    let r = rig();
    emit_parsed(&r, b"0123");
    let (sink, rx) = channel_sink(4);
    let (snap, _) = r.session.subscribe(sink, soon()).unwrap();
    assert_eq!(snap.output_offset, 4);
    emit_parsed(&r, b"abcd");
    emit_parsed(&r, b"ef");
    r.child.close_output();
    assert!(r.events.wait(|e| matches!(e, SessionEvent::OutputEnded(_))));
    assert_eq!(
        rx.recv_timeout(WAIT).unwrap(),
        Received::Bytes {
            offset: 4,
            bytes: b"abcd".to_vec()
        }
    );
    assert_eq!(rx.recv_timeout(WAIT).unwrap(), Received::Lagged { at: 8 });
    assert!(rx.recv_timeout(ms(10)).is_err());
}

/// PR2-CL-16: a startup query is parsed while `spawn` runs; its reply waits in the lane
/// until the writer has the input, then is the first write.
#[test]
fn startup_reply_waits_for_the_writer() {
    let r = rig_with(
        FakeScript {
            startup_output: b"banner\x1b[5n".to_vec(),
            ..FakeScript::default()
        },
        Limits::default(),
        80,
        24,
    );
    assert_eq!(
        r.session.snapshot(soon()).unwrap().output_offset,
        10,
        "parsed before spawn returned"
    );
    assert!(wait_until(|| r.child.writes() == vec![b"\x1b[0n".to_vec()]));
    let recs = r.session.writes_after(0).unwrap();
    assert_eq!(recs[0].seq, 1);
    assert_eq!(recs[0].byte_start, 0);
}

/// PR2-CL-16: a panicking or stopping sink is removed; draining continues.
#[test]
fn sink_panic_keeps_draining() {
    let r = rig();
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    r.session
        .subscribe(
            Box::new(move |_| {
                c.fetch_add(1, Ordering::SeqCst);
                panic!("sink bug")
            }),
            soon(),
        )
        .unwrap();
    let stops = Arc::new(AtomicUsize::new(0));
    let s = stops.clone();
    r.session
        .subscribe(
            Box::new(move |_| {
                s.fetch_add(1, Ordering::SeqCst);
                Flow::Stop
            }),
            soon(),
        )
        .unwrap();
    let (sink, got) = recorder();
    r.session.subscribe(sink, soon()).unwrap();
    emit_parsed(&r, b"one");
    emit_parsed(&r, b" two");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    assert_eq!(got.lock().unwrap().bytes, b"one two");
    assert_eq!(
        r.session.snapshot(soon()).unwrap().lines[0].text(),
        "one two"
    );
}

/// PR2-CL-16: a panicking `on_event` handler does not stop the drain.
#[test]
fn reply_dropped_panic_keeps_draining() {
    let fake = FakePty::new();
    let dropped = Arc::new(AtomicUsize::new(0));
    let d = dropped.clone();
    let mut config = SessionConfig::new(move |e| {
        if e == SessionEvent::ReplyDropped {
            d.fetch_add(1, Ordering::SeqCst);
            panic!("handler bug");
        }
    });
    config.limits.reply_queue = 1;
    let session = Session::spawn(&fake, &spec(80, 24), config).unwrap();
    let child = fake.take_child().unwrap();
    let hold = child.hold_writes();
    let (done, _rx) = chaldea_pty_runtime::completion_channel();
    session.submit(key(b"a"), open(), done).unwrap();
    assert!(hold.wait_entered(soon()));
    child.emit(b"\x1b[5n\x1b[5n\x1b[5n");
    assert!(wait_until(|| dropped.load(Ordering::SeqCst) == 2));
    child.emit(b"still here");
    assert!(wait_until(|| session
        .snapshot(soon())
        .is_ok_and(|s| s.lines[0].text() == "still here")));
    drop(hold);
}

/// PR2-CL-16: a VT panic marks the VT failed; snapshots are unavailable, the responder
/// stops, and bytes still reach subscribers.
#[test]
fn vt_panic_marks_vt_failed() {
    let r = rig();
    let (sink, got) = recorder();
    r.session.subscribe(sink, soon()).unwrap();
    r.session.fail_vt_on_next_piece();
    r.child.emit(b"boom");
    assert!(r.events.wait(|e| *e == SessionEvent::VtFailed));
    assert_eq!(r.session.snapshot(soon()), Err(VtUnavailable));
    assert!(matches!(
        r.session.subscribe(recorder().0, soon()),
        Err(SubscribeError::VtUnavailable)
    ));
    r.child.emit(b"\x1b[5n");
    assert!(wait_until(|| got.lock().unwrap().bytes == b"boom\x1b[5n"));
    let st = r.session.status();
    assert!(st.vt_failed && st.responder.stopped);
    assert!(r.child.writes().is_empty(), "no reply after the VT failed");
    assert_eq!(r.events.count(|e| *e == SessionEvent::VtFailed), 1);
}

/// PR2-CL-16: a read error on a blocked read reaches the status as an error.
#[test]
fn read_error_reaches_status() {
    let r = rig();
    emit_parsed(&r, b"before");
    r.child.fail_output(io::ErrorKind::ConnectionReset);
    assert!(
        r.events
            .wait(|e| matches!(e, SessionEvent::OutputEnded(OutputEnd::Error(_))))
    );
    assert!(matches!(
        r.session.status().output,
        Some(OutputEnd::Error(_))
    ));
    assert_eq!(
        r.session.snapshot(soon()).unwrap().output_offset,
        6,
        "the wake byte is swallowed"
    );
}

/// PR2-CL-17: settle, kill with the extra identities, release the input, close the PTY,
/// finish the drain, join, in that order.
#[test]
fn close_order() {
    let r = rig();
    emit_parsed(&r, b"last words");
    let child = r.child.clone();
    let out = r.session.close(&[extra()], CloseBounds::default());
    assert_eq!(
        lifecycle(&child.log()),
        vec![
            FakeEvent::Spawned,
            FakeEvent::Kill {
                extra: vec![extra()]
            },
            FakeEvent::Exited,
            FakeEvent::InputDropped,
            FakeEvent::ControlClosed,
            FakeEvent::OutputClosed,
        ]
    );
    assert_eq!(
        out.released,
        chaldea_pty_runtime::Released {
            input: true,
            control: true,
            output: true,
            tree_dead: true
        }
    );
    assert!(out.abandoned.is_empty(), "{out:?}");
    assert_eq!(out.output, Some(OutputEnd::Eof));
    assert!(out.final_after_end);
    assert_eq!(out.final_snapshot.unwrap().lines[0].text(), "last words");
}

/// PR2-CL-17: close settles queued units itself while the writer is blocked, giving
/// never-dequeued units their seqs; nothing new dispatches.
#[test]
fn close_settles_queued_units_with_a_blocked_writer() {
    let r = rig();
    let hold = r.child.hold_writes();
    let a = submit(&r.session, key(b"a"), open());
    assert!(hold.wait_entered(soon()));
    let b2 = submit(&r.session, key(b"b"), open());
    let c = submit(&r.session, key(b"c"), open());
    assert_eq!(
        r.session.last_started(chaldea_pty_runtime::ClassSet::ALL),
        1
    );
    let child = r.child.clone();
    let out = r.session.close(&[], short_bounds());
    let (rb, rc) = (b2.wait(), c.wait());
    assert_eq!((rb.seq, rc.seq), (2, 3));
    for x in [&rb, &rc] {
        assert_eq!(x.outcome, WriteOutcome::NotStarted(Refusal::Closed));
        assert_eq!(x.started_at, None);
    }
    let ra = a.wait();
    assert_eq!(ra.seq, 1);
    assert_eq!(
        ra.outcome,
        WriteOutcome::Incomplete {
            error: "unknown_after_close".into()
        }
    );
    assert!(out.abandoned.contains(&"pty-writer"));
    hold.release();
    assert!(wait_until(|| child
        .log()
        .contains(&FakeEvent::InputDropped)));
    assert_eq!(child.input(), b"a");
}

/// PR2-CL-17: a unit cancelled while queued gets its seq at cancel; seqs stay gapless.
#[test]
fn cancel_while_blocked_gets_a_seq() {
    let r = rig();
    let hold = r.child.hold_writes();
    let a = submit(&r.session, key(b"a"), open());
    assert!(hold.wait_entered(soon()));
    let b2 = submit(&r.session, key(b"b"), open());
    let c = submit(&r.session, key(b"c"), open());
    assert!(c.ticket.cancel());
    assert!(!c.ticket.cancel(), "cancel is once");
    let rc = c.wait();
    assert_eq!(
        (rc.seq, rc.outcome),
        (2, WriteOutcome::NotStarted(Refusal::Cancelled))
    );
    hold.release();
    assert_eq!(a.wait().seq, 1);
    let rb = b2.wait();
    assert_eq!((rb.seq, rb.outcome), (3, WriteOutcome::Written));
    assert!(!b2.ticket.cancel(), "a written unit cannot be cancelled");
    let seqs: Vec<u64> = r
        .session
        .writes_after(0)
        .unwrap()
        .iter()
        .map(|x| x.seq)
        .collect();
    assert_eq!(seqs, vec![1, 2, 3]);
}

/// PR2-CL-17: after a kill reported the tree dead, close kills again only for new extra
/// identities.
#[test]
fn close_kills_again_for_new_extra() {
    let r = rig();
    assert!(r.session.kill(&[], soon()).tree_dead);
    let child = r.child.clone();
    let _ = r.session.close(&[extra()], CloseBounds::default());
    let kills: Vec<_> = child
        .log()
        .into_iter()
        .filter(|e| matches!(e, FakeEvent::Kill { .. }))
        .collect();
    assert_eq!(
        kills,
        vec![
            FakeEvent::Kill { extra: vec![] },
            FakeEvent::Kill {
                extra: vec![extra()]
            }
        ]
    );
    let r = rig();
    assert!(r.session.kill(&[], soon()).tree_dead);
    let child = r.child.clone();
    let out = r.session.close(&[], CloseBounds::default());
    assert!(out.released.tree_dead);
    assert_eq!(
        child
            .log()
            .iter()
            .filter(|e| matches!(e, FakeEvent::Kill { .. }))
            .count(),
        1
    );
}

/// PR2-CL-18: with the writer blocked, close returns within its budgets, fires the
/// in-progress completion once as `unknown_after_close`, names the writer, and reports
/// the input as not released.
#[test]
fn close_is_bounded_with_a_blocked_write() {
    let r = rig_with(
        FakeScript {
            kill: KillBehavior::Ignore,
            ..FakeScript::default()
        },
        Limits::default(),
        80,
        24,
    );
    let hold = r.child.hold_writes();
    let (done, seen) = counting();
    r.session.submit(key(b"blocked"), open(), done).unwrap();
    assert!(hold.wait_entered(soon()));
    let bounds = short_bounds();
    let t = Instant::now();
    let out = r.session.close(&[], bounds);
    let took = t.elapsed();
    assert!(took <= bounds.total() + Duration::from_secs(1), "{took:?}");
    assert!(out.abandoned.contains(&"pty-writer"), "{out:?}");
    assert!(!out.released.input);
    assert!(!out.released.tree_dead);
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].outcome,
            WriteOutcome::Incomplete {
                error: "unknown_after_close".into()
            }
        );
    }
    let child = r.child.clone();
    hold.release();
    assert!(wait_until(|| child
        .log()
        .contains(&FakeEvent::InputDropped)));
    assert_eq!(seen.lock().unwrap().len(), 1, "fired once");
}

fn spawn_fault(fault: StartFault, script: FakeScript) -> (SpawnError, Vec<FakeEvent>) {
    let fake = FakePty::new();
    fake.script_next(script);
    let mut config = SessionConfig::new(|_| {});
    config.start_fault = Some(fault);
    let e = Session::spawn(&fake, &spec(80, 24), config).unwrap_err();
    let child = fake.take_child().unwrap();
    let log = child.log();
    // Ends a drain abandoned by a hanging close: the reader keeps the write end.
    child.close_output();
    (e, log)
}

/// PR2-CL-19: a writer-creation failure kills before dropping the input and closing the
/// PTY; no writer ever owned the input, and dropping it wrote nothing.
#[test]
fn writer_creation_failure_tears_down() {
    let (e, log) = spawn_fault(StartFault::Writer, FakeScript::default());
    assert!(
        matches!(
            e,
            SpawnError::StartedThenFailed {
                stage: "writer_thread",
                identity: Some(_),
                ..
            }
        ),
        "{e:?}"
    );
    assert_eq!(*e.cleanup(), Cleanup::Confirmed);
    assert_eq!(
        log,
        vec![
            FakeEvent::Spawned,
            FakeEvent::Kill { extra: vec![] },
            FakeEvent::Exited,
            FakeEvent::InputDropped,
            FakeEvent::ControlClosed,
            FakeEvent::OutputClosed,
        ]
    );
}

/// PR2-CL-19: a waiter-creation failure stops the writer that never got the input, then
/// tears down; an unconfirmed close and an abandoned drain are reported apart.
#[test]
fn waiter_creation_failure_tears_down() {
    let (e, log) = spawn_fault(
        StartFault::Waiter,
        FakeScript {
            startup_output: b"\x1b[5n".to_vec(),
            ..FakeScript::default()
        },
    );
    assert!(
        matches!(
            e,
            SpawnError::StartedThenFailed {
                stage: "waiter_thread",
                ..
            }
        ),
        "{e:?}"
    );
    assert_eq!(*e.cleanup(), Cleanup::Confirmed);
    assert_eq!(
        log,
        vec![
            FakeEvent::Spawned,
            FakeEvent::Kill { extra: vec![] },
            FakeEvent::Exited,
            FakeEvent::InputDropped,
            FakeEvent::ControlClosed,
            FakeEvent::OutputClosed,
        ],
        "the startup reply was never written"
    );

    let (e, log) = spawn_fault(
        StartFault::Waiter,
        FakeScript {
            hang_control_close: true,
            ..FakeScript::default()
        },
    );
    match e.cleanup() {
        Cleanup::Unresolved {
            kill: Some(k),
            close: Some(c),
            drain_abandoned,
        } => {
            assert!(k.tree_dead, "death was confirmed");
            assert_eq!(c.elapsed, None, "the close was not");
            assert!(*drain_abandoned, "no EOF without the close");
        }
        other => panic!("{other:?}"),
    }
    assert!(!log.iter().any(|e| matches!(e, FakeEvent::Write(_))));
}

/// A backend spawn failure passes through unchanged.
#[test]
fn backend_spawn_failure_is_returned() {
    let fake = FakePty::new();
    fake.script_next(FakeScript {
        fail: Some(SpawnError::NeverStarted {
            kind: chaldea_pty_runtime::NeverStartedKind::NotFound,
            error: io::Error::from(io::ErrorKind::NotFound),
            cleanup: Cleanup::Confirmed,
        }),
        ..FakeScript::default()
    });
    let e = Session::spawn(&fake, &spec(80, 24), SessionConfig::new(|_| {})).unwrap_err();
    assert!(matches!(e, SpawnError::NeverStarted { .. }));
    assert!(fake.take_child().is_none());
}

/// PR2-CL-20: the exit is published once; units not yet dispatched are refused.
#[test]
fn exit_is_reported_once() {
    let r = rig();
    let e = ChildExit {
        code: Some(3),
        signal: None,
    };
    r.child.exit(e);
    r.child.exit(ChildExit {
        code: Some(4),
        signal: None,
    });
    assert!(r.events.wait(|x| *x == SessionEvent::Exited(e)));
    assert_eq!(r.session.status().exit, Some(e));
    let rec = submit(&r.session, key(b"x"), open()).wait();
    assert_eq!(rec.outcome, WriteOutcome::NotStarted(Refusal::ChildExited));
    let _ = r.session.kill(&[], soon());
    assert_eq!(r.events.count(|x| matches!(x, SessionEvent::Exited(_))), 1);
}

/// PR2-CL-20: a submit made from the `Exited` callback already sees the exit.
#[test]
fn submit_from_exited_callback_is_refused() {
    let fake = FakePty::new();
    let me: Arc<OnceLock<Weak<Session>>> = Arc::default();
    let result: Arc<Mutex<Option<chaldea_pty_runtime::CompletionReceiver>>> = Arc::default();
    let (m, res) = (me.clone(), result.clone());
    let config = SessionConfig::new(move |e| {
        if let SessionEvent::Exited(_) = e
            && let Some(s) = m.get().and_then(Weak::upgrade)
        {
            let (done, rx) = chaldea_pty_runtime::completion_channel();
            s.submit(key(b"late"), open(), done).unwrap();
            *res.lock().unwrap() = Some(rx);
        }
    });
    let session = Arc::new(Session::spawn(&fake, &spec(80, 24), config).unwrap());
    me.set(Arc::downgrade(&session)).unwrap();
    let child = fake.take_child().unwrap();
    child.exit(ChildExit {
        code: Some(0),
        signal: None,
    });
    assert!(wait_until(|| result.lock().unwrap().is_some()));
    let rec = result
        .lock()
        .unwrap()
        .take()
        .unwrap()
        .recv_deadline(soon())
        .unwrap();
    assert_eq!(rec.outcome, WriteOutcome::NotStarted(Refusal::ChildExited));
    assert!(child.input().is_empty());
}

/// PR2-CL-24: with every step stuck, each ends by its budget and the call by their sum.
#[test]
fn close_steps_end_by_their_budgets() {
    let r = rig_with(
        FakeScript {
            kill: KillBehavior::Ignore,
            hang_control_close: true,
            ..FakeScript::default()
        },
        Limits::default(),
        80,
        24,
    );
    let hold = r.child.hold_writes();
    let _p = submit(&r.session, key(b"x"), open());
    assert!(hold.wait_entered(soon()));
    let bounds = CloseBounds {
        kill: ms(200),
        input: ms(200),
        close: ms(200),
        drain: ms(200),
        join: ms(200),
    };
    let t = Instant::now();
    let out = r.session.close(&[], bounds);
    let took = t.elapsed();
    // Every step waits out its whole budget, and no step runs past it.
    assert!(took >= ms(950), "{took:?}");
    assert!(took <= bounds.total() + Duration::from_secs(1), "{took:?}");
    assert!(out.elapsed <= took);
    assert_eq!(out.close.elapsed, None);
    assert_eq!(out.output, None);
    assert!(
        out.abandoned.contains(&"pty-writer") && out.abandoned.contains(&"pty-drain"),
        "{out:?}"
    );
    assert_eq!(
        out.released,
        chaldea_pty_runtime::Released {
            input: false,
            control: false,
            output: false,
            tree_dead: false
        }
    );
    // Ends the abandoned drain and writer: the reader keeps the write end.
    r.child.close_output();
    drop(hold);
}

/// PR2-CL-24: the final snapshot is skipped when the VT lock is held.
#[test]
fn final_snapshot_skipped_with_a_held_vt_lock() {
    let r = rig();
    let probe = r.session.probe();
    let park = probe.park(ProbePoint::PieceParsing);
    r.child.emit(b"x");
    assert!(probe.wait(ProbePoint::PieceParsing, 1, soon()));
    let t = Instant::now();
    let out = r.session.close(&[], short_bounds());
    assert!(t.elapsed() <= short_bounds().total() + Duration::from_secs(1));
    assert_eq!(out.final_snapshot, None);
    assert!(out.abandoned.contains(&"pty-drain"), "{out:?}");
    park.release();
}

/// PR2-CL-24: a snapshot or subscribe with the VT lock held returns by its deadline.
#[test]
fn snapshot_with_a_held_vt_lock_is_unavailable() {
    let r = rig();
    let probe = r.session.probe();
    let park = probe.park(ProbePoint::PieceParsing);
    r.child.emit(b"x");
    assert!(probe.wait(ProbePoint::PieceParsing, 1, soon()));
    let t = Instant::now();
    assert_eq!(
        r.session.snapshot(Instant::now() + ms(200)),
        Err(VtUnavailable)
    );
    let took = t.elapsed();
    assert!(took >= ms(200) && took < Duration::from_secs(3), "{took:?}");
    assert!(matches!(
        r.session.subscribe(recorder().0, Instant::now()),
        Err(SubscribeError::VtUnavailable)
    ));
    park.release();
    wait_parsed(&r.session, 1);
}

/// PR2-CL-24: receivers and `kill` return by their deadlines.
#[test]
fn receivers_end_by_their_deadlines() {
    let r = rig_with(
        FakeScript {
            kill: KillBehavior::Ignore,
            ..FakeScript::default()
        },
        Limits::default(),
        80,
        24,
    );
    let hold = r.child.hold_writes();
    let (done, rx) = chaldea_pty_runtime::completion_channel();
    r.session.submit(key(b"x"), open(), done).unwrap();
    assert!(hold.wait_entered(soon()));
    let t = Instant::now();
    assert!(rx.recv_deadline(Instant::now() + ms(100)).is_err());
    let (sink, orx) = channel_sink(64);
    r.session.subscribe(sink, soon()).unwrap();
    assert!(orx.recv_timeout(ms(100)).is_err());
    let k = r.session.kill(&[], Instant::now() + ms(100));
    assert!(!k.tree_dead && k.survivors.len() == 1);
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    hold.release();
    assert!(rx.recv_deadline(soon()).is_ok());
}

/// PR2-CL-28: callbacks may call the non-blocking methods: a completion cancels another
/// ticket, a `ReplyDropped` handler takes a snapshot, and a sink subscribes.
#[test]
fn callbacks_may_reenter() {
    let fake = FakePty::new();
    let me: Arc<OnceLock<Weak<Session>>> = Arc::default();
    let seen_text: Arc<Mutex<Option<String>>> = Arc::default();
    let (m, st) = (me.clone(), seen_text.clone());
    let mut config = SessionConfig::new(move |e| {
        if e == SessionEvent::ReplyDropped
            && let Some(s) = m.get().and_then(Weak::upgrade)
        {
            let text = s.snapshot(soon()).map(|x| x.lines[0].text());
            *st.lock().unwrap() = Some(text.unwrap_or_else(|_| "unavailable".into()));
        }
    });
    config.limits.reply_queue = 1;
    let session = Arc::new(Session::spawn(&fake, &spec(80, 24), config).unwrap());
    me.set(Arc::downgrade(&session)).unwrap();
    let child = fake.take_child().unwrap();

    // A completion that cancels the next unit.
    let hold = child.hold_writes();
    let next: Arc<OnceLock<chaldea_pty_runtime::WriteTicket>> = Arc::default();
    let cancelled = Arc::new(AtomicBool::new(false));
    let (n, c) = (next.clone(), cancelled.clone());
    session
        .submit(
            key(b"a"),
            open(),
            Box::new(move |_| {
                c.store(
                    n.get()
                        .is_some_and(chaldea_pty_runtime::WriteTicket::cancel),
                    Ordering::SeqCst,
                );
            }),
        )
        .unwrap();
    assert!(hold.wait_entered(soon()));
    let (done, rx) = chaldea_pty_runtime::completion_channel();
    let _ = next.set(session.submit(key(b"b"), open(), done).unwrap());

    // An overflow while the write is held: the handler's snapshot shows earlier text.
    child.emit(b"hello\x1b[5n\x1b[5n");
    assert!(wait_until(|| seen_text.lock().unwrap().is_some()));
    assert_eq!(seen_text.lock().unwrap().as_deref(), Some("hello"));
    hold.release();
    assert_eq!(
        rx.recv_deadline(soon()).unwrap().outcome,
        WriteOutcome::NotStarted(Refusal::Cancelled)
    );
    assert!(cancelled.load(Ordering::SeqCst));

    // A sink that subscribes another sink.
    let inner: Arc<Mutex<Option<Arc<Mutex<Got>>>>> = Arc::default();
    let (w, i2) = (Arc::downgrade(&session), inner.clone());
    session
        .subscribe(
            Box::new(move |_| {
                let mut slot = i2.lock().unwrap();
                if slot.is_none()
                    && let Some(s) = w.upgrade()
                {
                    let (sink, got) = recorder();
                    s.subscribe(sink, soon()).unwrap();
                    *slot = Some(got);
                }
                Flow::Continue
            }),
            soon(),
        )
        .unwrap();
    child.emit(b"x");
    assert!(wait_until(|| inner.lock().unwrap().is_some()));
    child.emit(b"yz");
    let got = inner.lock().unwrap().clone().unwrap();
    assert!(wait_until(|| got.lock().unwrap().bytes == b"yz"));
    assert!(wait_until(|| child.input() == b"a\x1b[0n"));
}

/// The subscriber limit counts the subscribers of a dispatch in progress.
#[test]
fn subscriber_limit_holds_during_a_dispatch() {
    let fake = FakePty::new();
    let mut config = SessionConfig::new(|_| {});
    config.limits.subscribers = 2;
    let session = Arc::new(Session::spawn(&fake, &spec(80, 24), config).unwrap());
    let child = fake.take_child().unwrap();
    let results: Arc<Mutex<Vec<bool>>> = Arc::default();
    let (w, res) = (Arc::downgrade(&session), results.clone());
    session
        .subscribe(
            Box::new(move |_| {
                let mut res = res.lock().unwrap();
                if res.is_empty()
                    && let Some(s) = w.upgrade()
                {
                    for _ in 0..2 {
                        res.push(s.subscribe(recorder().0, soon()).is_ok());
                    }
                }
                Flow::Continue
            }),
            soon(),
        )
        .unwrap();
    child.emit(b"x");
    assert!(wait_until(|| results.lock().unwrap().len() == 2));
    assert_eq!(*results.lock().unwrap(), vec![true, false]);
    assert!(matches!(
        session.subscribe(recorder().0, soon()),
        Err(SubscribeError::TooManySubscribers)
    ));
}

/// A sink whose destructor takes a snapshot, and the result it got.
fn dropping_sink(w: Weak<Session>) -> (OutputSink, Arc<Mutex<Option<bool>>>) {
    let got: Arc<Mutex<Option<bool>>> = Arc::default();
    let g = got.clone();
    let capture = OnDrop(move || {
        let ok = w
            .upgrade()
            .map(|s| s.snapshot(Instant::now() + Duration::from_secs(2)).is_ok());
        *g.lock().unwrap() = ok;
    });
    let sink: OutputSink = Box::new(move |_| {
        let _keep = &capture;
        Flow::Continue
    });
    (sink, got)
}

/// PR2-CL-28: a removed subscriber is dropped after the VT lock is released, both on
/// `unsubscribe` and when it unsubscribes itself during a dispatch.
#[test]
fn removed_sink_is_dropped_unlocked() {
    let Rig { session, child, .. } = rig();
    let session = Arc::new(session);
    let (sink, got) = dropping_sink(Arc::downgrade(&session));
    let (_, id) = session.subscribe(sink, soon()).unwrap();
    session.unsubscribe(id);
    assert_eq!(*got.lock().unwrap(), Some(true), "unsubscribe");

    // A sink that unsubscribes itself from inside its callback.
    let (inner, got) = dropping_sink(Arc::downgrade(&session));
    let me: Arc<OnceLock<chaldea_pty_runtime::SubscriptionId>> = Arc::default();
    let (m, w) = (me.clone(), Arc::downgrade(&session));
    let mut inner = Some(inner);
    let sink: OutputSink = Box::new(move |ev| {
        if let (Some(s), Some(id)) = (w.upgrade(), m.get()) {
            s.unsubscribe(*id);
        }
        inner.as_mut().map_or(Flow::Continue, |f| f(ev))
    });
    let (_, id) = session.subscribe(sink, soon()).unwrap();
    me.set(id).unwrap();
    child.emit(b"x");
    assert!(wait_until(|| got.lock().unwrap().is_some()));
    assert_eq!(*got.lock().unwrap(), Some(true), "self-unsubscribe");
}

/// PR2-CL-24: the snapshot copy is bounded by its deadline: a past deadline, or one that
/// passes during the copy of a large grid, gives `VtUnavailable`.
#[test]
fn snapshot_copy_ends_by_its_deadline() {
    let r = rig_with(FakeScript::default(), Limits::default(), 1000, 1000);
    let past = Instant::now();
    std::thread::sleep(ms(1));
    assert!(r.session.snapshot(past).is_err(), "past deadline");
    let t = Instant::now();
    assert!(
        r.session.snapshot(Instant::now() + ms(1)).is_err(),
        "deadline during the copy"
    );
    assert!(t.elapsed() < ms(500), "{:?}", t.elapsed());
    assert!(r.session.snapshot(soon()).is_ok());
}

/// PR2-CL-17 / CL-18 / CL-24: once the join budget has run out, close skips the final
/// snapshot instead of copying past its bound.
#[test]
fn final_snapshot_skipped_after_the_join_budget() {
    let r = rig_with(FakeScript::default(), Limits::default(), 1000, 1000);
    let hold = r.child.hold_writes();
    let _p = submit(&r.session, key(b"x"), open());
    assert!(hold.wait_entered(soon()));
    let bounds = CloseBounds {
        kill: ms(20),
        input: ms(20),
        close: ms(20),
        drain: ms(20),
        join: ms(20),
    };
    let out = r.session.close(&[], bounds);
    assert!(
        out.final_snapshot.is_none(),
        "the writer used the whole join budget"
    );
    assert!(out.abandoned.contains(&"pty-writer"));
    drop(hold);
}

/// PR2-CL-14: after `VtFailed`, a resize reaches the PTY and says that the VT did not
/// follow.
#[test]
fn resize_after_vt_failure_is_pty_only() {
    let r = rig();
    r.session.fail_vt_on_next_piece();
    r.child.emit(b"x");
    assert!(r.events.wait(|e| *e == SessionEvent::VtFailed));
    let to = PtySize::new(100, 30).unwrap();
    assert_eq!(r.session.resize(to).unwrap(), Resized::PtyOnly);
    assert!(r.child.log().contains(&FakeEvent::Resize(to)));
    assert_eq!(r.session.size(), to);
}

/// PR2-CL-16 / CL-24: a `channel_sink` never makes the drain wait for its receiver:
/// with the receiver's side held, the drain still reaches the next query.
#[test]
fn channel_sink_never_stalls_the_drain() {
    let r = rig();
    let (sink, rx) = channel_sink(1024);
    r.session.subscribe(sink, soon()).unwrap();
    let held = rx.hold();
    r.child.emit(b"x\x1b[5n");
    let reached = wait_for(Duration::from_secs(5), || {
        r.child.writes() == vec![b"\x1b[0n".to_vec()]
    });
    drop(held);
    assert!(reached, "the drain waited for the receiver");
    assert_eq!(
        rx.recv_timeout(WAIT).unwrap(),
        Received::Bytes {
            offset: 0,
            bytes: b"x".to_vec()
        }
    );
}

/// PR2-CL-19: when the failure teardown cannot confirm the tree dead, the error says so,
/// while the input, the PTY, and the drain it did release are not reported abandoned.
#[test]
fn failed_spawn_reports_unconfirmed_kill_with_ends_released() {
    let (e, log) = spawn_fault(
        StartFault::Writer,
        FakeScript {
            kill: KillBehavior::Ignore,
            ..FakeScript::default()
        },
    );
    match e.cleanup() {
        Cleanup::Unresolved {
            kill: Some(k),
            close: Some(c),
            drain_abandoned,
        } => {
            assert!(!k.tree_dead && k.survivors.len() == 1, "{k:?}");
            assert!(c.elapsed.is_some(), "the PTY was released");
            assert!(!*drain_abandoned, "the drain ended");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        log,
        vec![
            FakeEvent::Spawned,
            FakeEvent::Kill { extra: vec![] },
            FakeEvent::InputDropped,
            FakeEvent::ControlClosed,
            FakeEvent::OutputClosed,
        ]
    );
}

/// PR2-CL-24: one deadline bounds both waiting for the receiver's side and the receive,
/// so a receiver held elsewhere (another `recv_timeout`) cannot extend this call.
#[test]
fn recv_timeout_holds_its_bound_against_another_receiver() {
    let (_sink, rx) = channel_sink(64);
    let rx = Arc::new(rx);
    let held = rx.hold();
    let r2 = rx.clone();
    let (tx, done) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let t = Instant::now();
        let r = r2.recv_timeout(ms(10));
        let _ = tx.send((r, t.elapsed()));
    });
    let got = done.recv_timeout(Duration::from_secs(2));
    drop(held);
    let (r, took) = got.expect("recv_timeout did not return while the receiver was held");
    assert_eq!(r, Err(std::sync::mpsc::RecvTimeoutError::Timeout));
    assert!(took < Duration::from_secs(1), "{took:?}");
}

/// PR2-CL-24: a copy that outlasts the deadline inside its last row still returns
/// `VtUnavailable`, not a late snapshot.
#[test]
fn snapshot_checks_its_deadline_before_returning() {
    let r = rig_with(FakeScript::default(), Limits::default(), 80, 1);
    let mut out = b"a".to_vec();
    out.extend("\u{301}".repeat(2_000_000).as_bytes());
    r.child.emit(&out);
    wait_parsed(&r.session, out.len() as u64);
    let t = Instant::now();
    let got = r.session.snapshot(Instant::now() + ms(1));
    assert!(
        got.is_err(),
        "a snapshot returned {:?} after a 1 ms deadline",
        t.elapsed()
    );
    assert!(r.session.snapshot(soon()).is_ok());
}
