//! The VT responder (`docs/design/pty-runtime.md` §2.5), PR2-CL-9 to CL-11.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chaldea_pty_runtime::fake::{FakeScript, ProbePoint};
use chaldea_pty_runtime::{ChildExit, Limits, ReplyKind, SessionEvent, WriteOrigin, WriteOutcome};
use common::*;
use proptest::prelude::*;

/// Queries whose replies do not depend on the screen, with those replies at 80x24.
const FIXED: [(&[u8], &[u8], ReplyKind); 9] = [
    (b"\x1b[c", b"\x1b[?6c", ReplyKind::Da1),
    (b"\x1b[>c", b"\x1b[>0;2600;1c", ReplyKind::Da2),
    (b"\x1b[5n", b"\x1b[0n", ReplyKind::Dsr5),
    (b"\x1b[?u", b"\x1b[?0u", ReplyKind::KittyKeyboard),
    (b"\x1b[?2004$p", b"\x1b[?2004;2$y", ReplyKind::ModeReport),
    (b"\x1b[18t", b"\x1b[8;24;80t", ReplyKind::TextAreaChars),
    (b"\x1b[14t", b"\x1b[4;384;640t", ReplyKind::TextAreaPixels),
    (
        b"\x1b]11;?\x07",
        b"\x1b]11;rgb:1e1e/1e1e/1e1e\x07",
        ReplyKind::Color { index: 257 },
    ),
    (
        b"\x1b]10;?\x1b\\",
        b"\x1b]10;rgb:d0d0/d0d0/d0d0\x1b\\",
        ReplyKind::Color { index: 256 },
    ),
];

fn reply_records(r: &Rig) -> Vec<(ReplyKind, u64)> {
    r.session
        .writes_after(0)
        .unwrap()
        .into_iter()
        .filter_map(|x| match x.origin {
            WriteOrigin::VtReply { kind } => Some((kind, x.bytes_written)),
            _ => None,
        })
        .collect()
}

/// PR2-CL-9: each reply is its own write call, exactly that reply, in query order.
#[test]
fn each_reply_is_one_write() {
    let r = rig();
    r.child.emit(b"\x1b[c\x1b[5n\x1b[6nhello\x1b[>c");
    assert!(wait_until(|| r.child.writes().len() == 4));
    assert_eq!(
        r.child.writes(),
        vec![
            b"\x1b[?6c".to_vec(),
            b"\x1b[0n".to_vec(),
            b"\x1b[1;1R".to_vec(),
            b"\x1b[>0;2600;1c".to_vec(),
        ]
    );
    let recs = reply_records(&r);
    assert_eq!(
        recs,
        vec![
            (ReplyKind::Da1, 5),
            (ReplyKind::Dsr5, 4),
            (ReplyKind::Dsr6, 6),
            (ReplyKind::Da2, 12),
        ]
    );
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    /// PR2-CL-9: random output with embedded queries gets exactly the expected replies,
    /// one write call each, in query order, however the output is chunked.
    #[test]
    fn replies_are_single_writes(
        items in prop::collection::vec((prop::collection::vec(b'a'..=b'z', 0..20), 0usize..9), 1..10),
        split in 1usize..64,
    ) {
        let r = rig();
        let mut out = Vec::new();
        let mut expected = Vec::new();
        for (text, q) in &items {
            out.extend_from_slice(text);
            out.extend_from_slice(FIXED[*q].0);
            expected.push(FIXED[*q].1.to_vec());
        }
        for chunk in out.chunks(split) {
            r.child.emit(chunk);
        }
        prop_assert!(wait_until(|| r.child.writes().len() >= expected.len()));
        wait_parsed(&r.session, out.len() as u64);
        prop_assert_eq!(r.child.writes(), expected);
    }
}

/// PR2-CL-10: every supported query gets the §2.5 reply and kind.
#[test]
fn answers_each_query() {
    let r = rig();
    for (q, reply, kind) in FIXED {
        let n = r.child.writes().len();
        r.child.emit(q);
        assert!(wait_until(|| r.child.writes().len() == n + 1), "{q:?}");
        assert_eq!(r.child.writes()[n], reply, "{q:?}");
        assert_eq!(reply_records(&r)[n].0, kind);
    }
    // DSR 6 reports the cursor; ANSI DECRQM; OSC 12 cursor color; OSC 4 palette.
    let more: [(&[u8], &[u8], ReplyKind); 4] = [
        (b"ab\x1b[6n", b"\x1b[1;3R", ReplyKind::Dsr6),
        (b"\x1b[4$p", b"\x1b[4;2$y", ReplyKind::ModeReport),
        (
            b"\x1b]12;?\x07",
            b"\x1b]12;rgb:d0d0/d0d0/d0d0\x07",
            ReplyKind::Color { index: 258 },
        ),
        (
            b"\x1b]4;1;?\x07",
            b"\x1b]4;1;rgb:d0d0/d0d0/d0d0\x07",
            ReplyKind::Color { index: 1 },
        ),
    ];
    for (q, reply, kind) in more {
        let n = r.child.writes().len();
        r.child.emit(q);
        assert!(wait_until(|| r.child.writes().len() == n + 1), "{q:?}");
        assert_eq!(r.child.writes()[n], reply, "{q:?}");
        assert_eq!(reply_records(&r)[n].0, kind);
    }
    // The kitty query reports the flags pushed by the CLI; DECRQM reports a set mode.
    for (q, reply) in [
        (&b"\x1b[>1u\x1b[?u"[..], &b"\x1b[?1u"[..]),
        (b"\x1b[?2004h\x1b[?2004$p", b"\x1b[?2004;1$y"),
    ] {
        let n = r.child.writes().len();
        r.child.emit(q);
        assert!(wait_until(|| r.child.writes().len() == n + 1), "{q:?}");
        assert_eq!(r.child.writes()[n], reply);
    }
}

/// PR2-CL-10: replies queued while a unit is blocked are written, in query order,
/// before the next user unit.
#[test]
fn batch_precedes_the_next_unit() {
    let r = rig();
    let probe = r.session.probe();
    let hold = r.child.hold_writes();
    let a = submit(&r.session, key(b"A"), open());
    assert!(hold.wait_entered(soon()));
    let b2 = submit(&r.session, key(b"B"), open());
    r.child.emit(b"\x1b[5n\x1b[c");
    assert!(probe.wait(ProbePoint::ReplyEnqueued, 2, soon()));
    hold.release();
    a.wait();
    b2.wait();
    assert_eq!(
        r.child.writes(),
        vec![
            b"A".to_vec(),
            b"\x1b[0n".to_vec(),
            b"\x1b[?6c".to_vec(),
            b"B".to_vec()
        ]
    );
}

/// PR2-CL-10: an OSC 52 clipboard read gets no reply.
#[test]
fn clipboard_read_is_not_answered() {
    let r = rig();
    r.child.emit(b"\x1b]52;c;?\x07\x1b[5n");
    assert!(wait_until(|| !r.child.writes().is_empty()));
    wait_parsed(&r.session, 13);
    assert_eq!(r.child.writes(), vec![b"\x1b[0n".to_vec()]);
    assert_eq!(reply_records(&r), vec![(ReplyKind::Dsr5, 4)]);
}

/// PR2-CL-11: a CLI that answers every reply with a new query cannot starve a queued
/// unit: it is dequeued after at most one batch.
#[test]
fn replies_cannot_starve_units() {
    let r = rig();
    let probe = r.session.probe();
    let echoes = Arc::new(AtomicUsize::new(0));
    let e = echoes.clone();
    r.child.on_input(move |bytes| {
        if bytes.starts_with(b"\x1b[") && e.fetch_add(1, Ordering::SeqCst) < 200 {
            b"\x1b[5n\x1b[5n".to_vec()
        } else {
            Vec::new()
        }
    });
    let hold = r.child.hold_writes();
    let a = submit(&r.session, key(b"A"), open());
    assert!(hold.wait_entered(soon()));
    let b2 = submit(&r.session, key(b"B"), open());
    r.child.emit(b"\x1b[5n");
    assert!(probe.wait(ProbePoint::ReplyEnqueued, 1, soon()));
    hold.release();
    a.wait();
    b2.wait();
    let writes = r.child.writes();
    let at = writes.iter().position(|w| w == b"B").unwrap();
    assert_eq!(at, 2, "A, one batch of one reply, then B: {writes:?}");
    // Replies keep flowing after the unit.
    assert!(wait_until(|| echoes.load(Ordering::SeqCst) > 10));
}

/// PR2-CL-11: a reply the lane cannot hold is discarded, counted, and reported.
#[test]
fn overflow_is_reported() {
    let r = rig();
    let probe = r.session.probe();
    let hold = r.child.hold_writes();
    let a = submit(&r.session, key(b"A"), open());
    assert!(hold.wait_entered(soon()));
    r.child.emit(&b"\x1b[5n".repeat(65));
    assert!(probe.wait(ProbePoint::ReplyEnqueued, 64, soon()));
    assert!(r.events.wait(|e| *e == SessionEvent::ReplyDropped));
    wait_parsed(&r.session, 4 * 65);
    assert_eq!(r.events.count(|e| *e == SessionEvent::ReplyDropped), 1);
    assert_eq!(r.session.status().responder.dropped_overflow, 1);
    hold.release();
    a.wait();
    assert!(wait_until(|| r.child.writes().len() == 65));
    assert_eq!(r.session.status().responder.written, 64);
}

/// PR2-CL-11: replies queued when the child exits, and new ones after, are discarded and
/// counted, with no record.
#[test]
fn replies_after_exit_are_discarded() {
    let r = rig();
    let probe = r.session.probe();
    let hold = r.child.hold_writes();
    let a = submit(&r.session, key(b"A"), open());
    assert!(hold.wait_entered(soon()));
    r.child.emit(b"\x1b[5n\x1b[5n\x1b[c");
    assert!(probe.wait(ProbePoint::ReplyEnqueued, 3, soon()));
    r.child.exit(ChildExit {
        code: Some(0),
        signal: None,
    });
    assert!(r.events.wait(|e| matches!(e, SessionEvent::Exited(_))));
    emit_parsed(&r, b"\x1b[5n");
    hold.release();
    assert_eq!(a.wait().outcome, WriteOutcome::Written);
    let st = r.session.status();
    assert_eq!(st.responder.dropped_after_end, 4);
    assert_eq!(st.responder.written, 0);
    assert_eq!(r.child.writes(), vec![b"A".to_vec()]);
    assert!(reply_records(&r).is_empty());
}

/// Replies to startup queries are answered at the spawn size (also PR2-CL-14).
#[test]
fn startup_queries_use_the_spawn_size() {
    let r = rig_with(
        FakeScript {
            startup_output: b"\x1b[18t\x1b[14t".to_vec(),
            ..FakeScript::default()
        },
        Limits::default(),
        120,
        40,
    );
    assert!(wait_until(|| r.child.writes().len() == 2));
    assert_eq!(
        r.child.writes(),
        vec![b"\x1b[8;40;120t".to_vec(), b"\x1b[4;640;960t".to_vec()]
    );
}
