//! C1-8, continuity (D10): a rewritten prefix, truncation, a replaced file, and a file that
//! shrinks between the read and the commit each start a new generation; so do a same-inode
//! rewrite found by the final read's prefix check or by a modification time moving back,
//! and a break found by the check after a commit.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};

use super::{Flaky, Fx, MODE, TestBackend, assistant, shorts};
use crate::store::tests::support;
use crate::transcript::read::Step;

/// Overwrites `at..` of the file in place (same file, same length).
fn overwrite(fx: &Fx, at: u64, bytes: &[u8]) {
    let mut f = OpenOptions::new().write(true).open(&fx.path).unwrap();
    f.seek(SeekFrom::Start(at)).unwrap();
    f.write_all(bytes).unwrap();
}

fn truncate(fx: &Fx, len: u64) {
    OpenOptions::new()
        .write(true)
        .open(&fx.path)
        .unwrap()
        .set_len(len)
        .unwrap();
}

/// Same `file_id` and the same tail with a rewritten prefix: the next open finds it and
/// starts generation 2, which reads the file again from 0.
#[test]
fn a_rewritten_prefix_starts_a_generation_at_the_next_open() {
    let fx = Fx::new();
    fx.write([assistant("one"), MODE.to_owned()].concat().as_bytes());
    let mut tail = fx.tail();
    fx.settle(&mut tail);
    let id = fx.cursor().unwrap().file_id;
    // "one" → "uno": the same length, the same file.
    let at = assistant("").find("\"text\":\"").unwrap() + 8;
    overwrite(&fx, at as u64, b"uno");
    drop(tail);
    let mut next = fx.tail();
    assert_eq!(fx.settle(&mut next), Step::Wait);
    let evs = fx.events();
    assert_eq!(
        shorts(&evs),
        ["generation:1", "text:one", "generation:2", "text:uno"]
    );
    assert_eq!(evs[3].generation, Some(2));
    let c = fx.cursor().unwrap();
    assert_eq!((c.generation, c.file_id), (2, id));
}

/// After a cursor rebuild (the read position rewound to the last event), the open verifies
/// the rewound prefix: unchanged, the tailer continues the generation without storing a
/// line twice; rewritten, it starts a new generation.
#[test]
fn a_cursor_rebuild_verifies_the_rewound_prefix() {
    let fx = Fx::new();
    fx.write(
        [assistant("one"), MODE.to_owned(), MODE.to_owned()]
            .concat()
            .as_bytes(),
    );
    let mut tail = fx.tail();
    fx.settle(&mut tail);
    drop(tail);
    let rebuild = |fx: &Fx| {
        fx.store
            .run(|tx| Ok(tx.rebuild_projections().map(|_| ())?))
            .unwrap();
    };
    rebuild(&fx);
    let c = fx.cursor().unwrap();
    assert_eq!(c.read_offset, c.event_offset);
    let mut again = fx.tail();
    assert_eq!(fx.settle(&mut again), Step::Wait);
    assert_eq!(shorts(&fx.events()), ["generation:1", "text:one"]);
    drop(again);
    rebuild(&fx);
    let at = assistant("").find("\"text\":\"").unwrap() + 8;
    overwrite(&fx, at as u64, b"uno");
    let mut next = fx.tail();
    fx.settle(&mut next);
    assert_eq!(
        shorts(&fx.events()),
        ["generation:1", "text:one", "generation:2", "text:uno"]
    );
}

/// The file truncated below the cursor while a batch read past it: the batch is discarded,
/// a generation starts alone at offset 0, and the file is read again from 0. Also for a
/// batch without lines (only lines that produce no event).
#[test]
fn truncation_while_a_batch_is_read_discards_it() {
    for appended in [assistant("two"), MODE.to_owned()] {
        let fx = Fx::new();
        fx.write(assistant("one").as_bytes());
        let mut tail = fx.tail();
        fx.settle(&mut tail);
        fx.append(appended.as_bytes());
        let be = TestBackend {
            after_read: Box::new(|| truncate(&fx, 5)),
            ..fx.backend()
        };
        assert_eq!(tail.poll(&be), Step::Again);
        assert_eq!(tail.stats.discarded, 1);
        assert_eq!(
            shorts(&fx.events()),
            ["generation:1", "text:one", "generation:2"]
        );
        let c = fx.cursor().unwrap();
        assert_eq!((c.generation, c.read_offset), (2, 0));
        fx.write(assistant("three").as_bytes());
        fx.settle(&mut tail);
        assert_eq!(
            shorts(&fx.events()),
            ["generation:1", "text:one", "generation:2", "text:three"]
        );
    }
}

/// A file that shrinks between the read and the commit to below the batch's advance (but
/// not below the cursor) is discarded too.
#[test]
fn a_file_shrinking_below_the_advance_is_discarded() {
    let fx = Fx::new();
    let one = assistant("one");
    fx.write(one.as_bytes());
    let mut tail = fx.tail();
    fx.settle(&mut tail);
    let two = assistant("two");
    fx.append([two.as_str(), &assistant("three")].concat().as_bytes());
    let keep = (one.len() + two.len()) as u64;
    let be = TestBackend {
        after_read: Box::new(|| truncate(&fx, keep)),
        ..fx.backend()
    };
    assert_eq!(tail.poll(&be), Step::Again);
    assert_eq!(
        shorts(&fx.events()),
        ["generation:1", "text:one", "generation:2"]
    );
}

/// Truncation with nothing new to read shows at the next poll.
#[test]
fn truncation_shows_at_the_next_poll() {
    let fx = Fx::new();
    fx.write(assistant("one").as_bytes());
    let mut tail = fx.tail();
    fx.settle(&mut tail);
    truncate(&fx, 0);
    fx.settle(&mut tail);
    assert_eq!(
        shorts(&fx.events()),
        ["generation:1", "text:one", "generation:2"]
    );
}

/// Another file at the path (a rename and a new file): a generation with the new
/// `file_id`, whose lines are read from 0.
#[test]
fn a_replaced_file_starts_a_generation() {
    let fx = Fx::new();
    fx.write(assistant("old").as_bytes());
    let mut tail = fx.tail();
    fx.settle(&mut tail);
    let old_id = fx.cursor().unwrap().file_id;
    std::fs::rename(&fx.path, fx.files.path().join("moved.jsonl")).unwrap();
    fx.write(assistant("new").as_bytes());
    fx.settle(&mut tail);
    let evs = fx.events();
    assert_eq!(
        shorts(&evs),
        ["generation:1", "text:old", "generation:2", "text:new"]
    );
    let c = fx.cursor().unwrap();
    assert_eq!(c.generation, 2);
    assert_ne!(c.file_id, old_id);
    assert_eq!(evs[2].payload["file_id"], c.file_id.as_str());
}

/// A truncate or replace after the checks before a commit and before the commit itself: the
/// batch commits and stays in history, the check after the commit finds the break, and the
/// next poll starts a new generation even when the file has grown past the old offset
/// again. For a batch with an event and one that only advances the cursor.
#[test]
fn a_break_during_a_commit_starts_a_generation_after_it() {
    for (appended, kept) in [
        (assistant("two"), Some("text:two")),
        (MODE.to_owned(), None),
    ] {
        for replace in [false, true] {
            let fx = Fx::new();
            fx.write(assistant("one").as_bytes());
            let mut tail = fx.tail();
            fx.settle(&mut tail);
            fx.append(appended.as_bytes());
            let be = TestBackend {
                before_commit: Box::new(|_| {
                    if replace {
                        let other = fx.files.path().join("other.jsonl");
                        std::fs::write(&other, b"").unwrap();
                        std::fs::rename(&other, &fx.path).unwrap();
                    } else {
                        truncate(&fx, 0);
                    }
                }),
                ..fx.backend()
            };
            assert_eq!(tail.poll(&be), Step::Again, "replace: {replace}");
            assert_eq!(tail.stats.late, 1);
            let mut want = vec!["generation:1", "text:one"];
            want.extend(kept);
            assert_eq!(shorts(&fx.events()), want);
            // Longer than the committed offset, written in place (or into the new file).
            let body = [assistant("three"), assistant("four"), assistant("five")].concat();
            assert!(body.len() > assistant("one").len() + appended.len());
            fx.write(body.as_bytes());
            assert_eq!(fx.settle(&mut tail), Step::Wait);
            want.extend(["generation:2", "text:three", "text:four", "text:five"]);
            assert_eq!(shorts(&fx.events()), want, "replace: {replace}");
        }
    }
}

/// A break found while the generation that answers it fails to commit is kept: the original
/// prefix written back in place does not let the next open continue the old generation.
#[test]
fn a_break_is_kept_when_its_generation_fails_to_commit() {
    let fx = Fx::new();
    let one = assistant("one");
    fx.write(one.as_bytes());
    let mut tail = fx.tail();
    fx.settle(&mut tail);
    fx.append(assistant("two").as_bytes());
    let be = TestBackend {
        after_read: Box::new(|| truncate(&fx, 5)),
        fail_generations: std::cell::Cell::new(1),
        ..fx.backend()
    };
    assert_eq!(tail.poll(&be), Step::Wait);
    assert_eq!(shorts(&fx.events()), ["generation:1", "text:one"]);
    fx.write(
        [one, assistant("two"), assistant("three")]
            .concat()
            .as_bytes(),
    );
    assert_eq!(fx.settle(&mut tail), Step::Wait);
    assert_eq!(
        shorts(&fx.events()),
        [
            "generation:1",
            "text:one",
            "generation:2",
            "text:one",
            "text:two",
            "text:three"
        ]
    );
}

/// A commit refused as `StaleCursor` (another tailer advanced the cursor meanwhile): the
/// tailer reads the stored cursor and verifies the prefix from 0 again, here finding it
/// rewritten.
#[test]
fn a_stale_cursor_verifies_the_prefix_again() {
    let fx = Fx::new();
    fx.write(assistant("one").as_bytes());
    let mut first = fx.tail();
    fx.settle(&mut first);
    fx.append(assistant("two").as_bytes());
    let other = std::cell::RefCell::new(fx.tail());
    let be = TestBackend {
        after_read: Box::new(|| {
            assert_eq!(fx.settle(&mut other.borrow_mut()), Step::Wait);
            overwrite(&fx, 0, assistant("One").as_bytes());
        }),
        ..fx.backend()
    };
    assert_eq!(first.poll(&be), Step::Again);
    assert_eq!(
        shorts(&fx.events()),
        ["generation:1", "text:one", "text:two"]
    );
    assert_eq!(fx.settle(&mut first), Step::Wait);
    assert_eq!(
        shorts(&fx.events()),
        [
            "generation:1",
            "text:one",
            "text:two",
            "generation:2",
            "text:One",
            "text:two"
        ]
    );
}

/// A same-inode rewrite of the read bytes with the same length, then an append: the light
/// checks of each poll pass, and the final read checks the prefix once and starts a new
/// generation from 0.
#[test]
fn the_final_read_finds_a_same_inode_rewrite() {
    let fx = Fx::new();
    fx.write([assistant("one"), assistant("two")].concat().as_bytes());
    let mut tail = fx.tail();
    fx.settle(&mut tail);
    overwrite(
        &fx,
        0,
        [assistant("uno"), assistant("dos")].concat().as_bytes(),
    );
    fx.append(assistant("new").as_bytes());
    assert_eq!(fx.settle(&mut tail), Step::Wait);
    assert_eq!(
        shorts(&fx.events()),
        ["generation:1", "text:one", "text:two", "text:new"]
    );
    fx.add(vec![support::exited(fx.t, fx.l)]);
    assert!(matches!(fx.settle(&mut tail), Step::End(_)));
    assert_eq!(
        shorts(&fx.events()),
        [
            "generation:1",
            "text:one",
            "text:two",
            "text:new",
            "generation:2",
            "text:uno",
            "text:dos",
            "text:new"
        ]
    );
}

/// A modification time that moves back makes the next poll check the prefix: a same-length
/// rewrite is found before the final read.
#[test]
fn a_modification_time_moving_back_checks_the_prefix() {
    let fx = Fx::new();
    fx.write(assistant("one").as_bytes());
    let mut tail = fx.tail();
    fx.settle(&mut tail);
    overwrite(&fx, 0, assistant("One").as_bytes());
    fx.append(assistant("two").as_bytes());
    let past = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
    OpenOptions::new()
        .write(true)
        .open(&fx.path)
        .unwrap()
        .set_modified(past)
        .unwrap();
    assert_eq!(fx.settle(&mut tail), Step::Wait);
    assert_eq!(
        shorts(&fx.events()),
        [
            "generation:1",
            "text:one",
            "generation:2",
            "text:One",
            "text:two"
        ]
    );
}

/// The check after a commit fails to open the path (another file there, not readable yet):
/// the tailer does not end on it but drops the handle; the poll after it fails the same
/// way and waits (no immediate retry), and once the path opens the new file is verified and
/// read as a new generation.
#[test]
fn a_failed_check_after_a_commit_opens_the_path_again() {
    let fx = Fx::new();
    fx.write(assistant("one").as_bytes());
    let mut tail = fx.tail();
    fx.settle(&mut tail);
    fx.append(assistant("two").as_bytes());
    fx.add(vec![support::exited(fx.t, fx.l)]);
    let flaky = std::cell::RefCell::new(None);
    let be = TestBackend {
        before_commit: Box::new(|_| {
            let other = fx.files.path().join("other.jsonl");
            std::fs::write(&other, assistant("new")).unwrap();
            std::fs::rename(&other, &fx.path).unwrap();
            *flaky.borrow_mut() = Some(Flaky::new(&fx.path));
        }),
        ..fx.backend()
    };
    assert_eq!(tail.poll(&be), Step::Again);
    assert_eq!(
        shorts(&fx.events()),
        ["generation:1", "text:one", "text:two"]
    );
    #[cfg(unix)]
    assert_eq!(tail.poll(&fx.backend()), Step::Wait);
    drop(flaky.borrow_mut().take());
    assert!(matches!(fx.settle(&mut tail), Step::End(_)));
    assert_eq!(
        shorts(&fx.events()),
        [
            "generation:1",
            "text:one",
            "text:two",
            "generation:2",
            "text:new"
        ]
    );
}

/// The stop set after a poll's read and before its commit: nothing is written.
#[test]
fn a_stop_before_a_commit_writes_nothing() {
    let fx = Fx::new();
    fx.write(assistant("one").as_bytes());
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut tail = fx.tail_with(|_| {}, stop.clone());
    fx.settle(&mut tail);
    fx.append(assistant("two").as_bytes());
    let be = TestBackend {
        after_read: Box::new(|| stop.store(true, std::sync::atomic::Ordering::Release)),
        ..fx.backend()
    };
    assert_eq!(tail.poll(&be), Step::End("stopped".into()));
    assert_eq!(shorts(&fx.events()), ["generation:1", "text:one"]);
}
