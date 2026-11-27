//! C1-8, continuity (D10): a rewritten prefix, truncation, a replaced file, and a file that
//! shrinks between the read and the commit each start a new generation.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};

use super::{Fx, MODE, TestBackend, assistant, shorts};
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
            store: &fx.store,
            after_read: Box::new(|| truncate(&fx, 5)),
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
        store: &fx.store,
        after_read: Box::new(|| truncate(&fx, keep)),
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
