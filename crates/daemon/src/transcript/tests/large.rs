//! C1-8, D14 producer side: lines over 64 MiB, one valid and one not JSON, are stored whole
//! as large blobs, download equal, the next line is read, and memory stays bounded.

use std::fs::File;
use std::io::{BufWriter, Write};

use chaldea_core::ids::StreamId;
use sha2::{Digest, Sha256};

use super::{Fx, TestBackend, assistant, shorts};
use crate::store::{BlobDownload, MAX_BLOB_BYTES};
use crate::transcript::acc::ReplayProbe;
use crate::transcript::read::Step;

/// Writes `n` bytes of `b` to `w` in pieces.
fn fill(w: &mut impl Write, b: u8, n: usize) {
    let piece = vec![b; 1 << 20];
    let mut left = n;
    while left > 0 {
        let k = left.min(piece.len());
        w.write_all(&piece[..k]).unwrap();
        left -= k;
    }
}

/// The SHA-256 of `n` bytes of `b`.
fn sha_of(b: u8, n: usize) -> String {
    let mut h = Sha256::new();
    let piece = vec![b; 1 << 20];
    let mut left = n;
    while left > 0 {
        let k = left.min(piece.len());
        h.update(&piece[..k]);
        left -= k;
    }
    hex(&h.finalize())
}

fn hex(d: &[u8]) -> String {
    use std::fmt::Write;
    d.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// The SHA-256 and size of a large blob, read part by part as `GET /api/blobs` serves it.
fn download(fx: &Fx, sha: &str) -> (String, u64) {
    let (t, sha) = (fx.t, sha.to_owned());
    let s2 = sha.clone();
    let parts = fx
        .store
        .read(move |r| r.blob_download(StreamId::Thread(t), &s2));
    let Some(BlobDownload::Parts(large)) = parts else {
        panic!("a large blob: {parts:?}")
    };
    let mut h = Sha256::new();
    let mut size = 0u64;
    for i in 0..large.part_count {
        let sha = sha.clone();
        let part = fx.store.read(move |r| r.large_blob_part(&sha, i)).unwrap();
        size += part.len() as u64;
        h.update(&part);
    }
    assert_eq!(size, large.size);
    (hex(&h.finalize()), size)
}

#[test]
fn lines_over_64_mib_are_stored_whole() {
    let fx = Fx::new();
    let n = MAX_BLOB_BYTES + 1;
    {
        let mut w = BufWriter::new(File::create(&fx.path).unwrap());
        // A valid assistant line whose text is n bytes.
        w.write_all(
            b"{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"",
        )
        .unwrap();
        fill(&mut w, b'a', n);
        w.write_all(b"\"}]}}\n").unwrap();
        // A line of n bytes that is not JSON.
        fill(&mut w, b'x', n);
        w.write_all(b"\n").unwrap();
        w.write_all(assistant("after").as_bytes()).unwrap();
        w.flush().unwrap();
    }
    let mut tail = fx.tail();
    assert_eq!(fx.settle(&mut tail), Step::Wait);
    let evs = fx.events();
    assert_eq!(
        shorts(&evs),
        [
            "generation:1".to_owned(),
            format!("text:<blob {n}>"),
            format!("raw:unparseable:<blob {n}>"),
            "text:after".to_owned(),
        ]
    );
    let text = &evs[1].payload["text"]["blob"];
    assert_eq!(text["media_type"], "text/plain; charset=utf-8");
    let sha = text["sha256"].as_str().unwrap();
    assert_eq!(sha, sha_of(b'a', n));
    assert_eq!(download(&fx, sha), (sha_of(b'a', n), n as u64));
    let raw = &evs[2].payload["raw"]["blob"];
    let sha = raw["sha256"].as_str().unwrap();
    assert_eq!(sha, sha_of(b'x', n));
    assert_eq!(download(&fx, sha), (sha_of(b'x', n), n as u64));
    // The valid line held its raw bytes and its text in memory together (each up to the
    // in-memory limit before its spool), and never more.
    let peak = tail.stats.peak_line_memory;
    assert!(
        peak > MAX_BLOB_BYTES && peak <= 2 * MAX_BLOB_BYTES,
        "{peak}"
    );
    no_spools(&fx);
}

/// No spool file is left in the state directory.
fn no_spools(fx: &Fx) {
    let spools: Vec<_> = std::fs::read_dir(fx.store.dir())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(crate::store::SPOOL_FILE_PREFIX)
        })
        .collect();
    assert!(spools.is_empty(), "{spools:?}");
}

/// A block's `text` over 64 MiB read before its `type` is held aside in a spool and moved
/// to the line's text by replay, whole.
#[test]
fn a_text_over_64_mib_before_its_type_is_kept_whole() {
    let fx = Fx::new();
    let n = MAX_BLOB_BYTES + 1;
    {
        let mut w = BufWriter::new(File::create(&fx.path).unwrap());
        w.write_all(b"{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"b\"},{\"text\":\"")
            .unwrap();
        fill(&mut w, b'a', n);
        w.write_all(b"\",\"type\":\"text\"}]}}\n").unwrap();
        w.flush().unwrap();
    }
    let mut tail = fx.tail();
    assert_eq!(fx.settle(&mut tail), Step::Wait);
    let evs = fx.events();
    assert_eq!(
        shorts(&evs),
        ["generation:1".to_owned(), format!("text:<blob {}>", n + 1)]
    );
    let sha = evs[1].payload["text"]["blob"]["sha256"].as_str().unwrap();
    let mut h = Sha256::new();
    h.update(b"b");
    h.update(vec![b'a'; n]);
    let want = hex(&h.finalize());
    assert_eq!(download(&fx, sha), (want, n as u64 + 1));
    no_spools(&fx);
}

/// With room for one spool at a time, lines over 64 MiB in a row are still stored: a batch
/// closes after a spooled line, so its spool is gone before the next line needs one, and a
/// failed spool keeps the lines before it.
#[test]
fn consecutive_lines_over_64_mib_need_one_spool_at_a_time() {
    let fx = Fx::new();
    let n = MAX_BLOB_BYTES + 1;
    {
        let mut w = BufWriter::new(File::create(&fx.path).unwrap());
        w.write_all(assistant("before").as_bytes()).unwrap();
        for _ in 0..2 {
            fill(&mut w, b'x', n);
            w.write_all(b"\n").unwrap();
        }
        w.write_all(assistant("after").as_bytes()).unwrap();
        w.flush().unwrap();
    }
    let mut tail = fx.tail_with_spool_limit(1);
    assert_eq!(fx.settle(&mut tail), Step::Wait);
    assert_eq!(
        shorts(&fx.events()),
        [
            "generation:1".to_owned(),
            "text:before".to_owned(),
            format!("raw:unparseable:<blob {n}>"),
            format!("raw:unparseable:<blob {n}>"),
            "text:after".to_owned(),
        ]
    );
    no_spools(&fx);
}

/// A batch closes after a line whose blob is spooled: the line before it and the spooled
/// line commit together, the next line in another batch.
#[test]
fn a_batch_closes_after_a_spooled_line() {
    let fx = Fx::new();
    let n = MAX_BLOB_BYTES + 1;
    {
        let mut w = BufWriter::new(File::create(&fx.path).unwrap());
        w.write_all(assistant("before").as_bytes()).unwrap();
        fill(&mut w, b'x', n);
        w.write_all(b"\n").unwrap();
        w.write_all(assistant("after").as_bytes()).unwrap();
        w.flush().unwrap();
    }
    let sizes = std::cell::RefCell::new(Vec::new());
    let be = TestBackend {
        before_commit: Box::new(|b| sizes.borrow_mut().push(b.lines.len())),
        ..fx.backend()
    };
    let mut tail = fx.tail();
    assert_eq!(fx.settle_with(&mut tail, &be), Step::Wait);
    assert_eq!(*sizes.borrow(), [2, 1]);
    no_spools(&fx);
}

/// A spool that cannot be created (no descriptor left): the lines before it commit, then
/// every poll fails on the line and waits for the next period; nothing loops.
#[test]
fn a_spool_that_cannot_be_created_waits_for_the_next_poll() {
    let fx = Fx::new();
    let n = MAX_BLOB_BYTES + 1;
    {
        let mut w = BufWriter::new(File::create(&fx.path).unwrap());
        w.write_all(assistant("before").as_bytes()).unwrap();
        fill(&mut w, b'x', n);
        w.write_all(b"\n").unwrap();
        w.flush().unwrap();
    }
    let mut tail = fx.tail_with_spool_limit(0);
    assert_eq!(tail.poll(&fx.backend()), Step::Again);
    assert_eq!(tail.poll(&fx.backend()), Step::Again);
    assert_eq!(tail.poll(&fx.backend()), Step::Wait);
    assert_eq!(tail.poll(&fx.backend()), Step::Wait);
    assert_eq!(shorts(&fx.events()), ["generation:1", "text:before"]);
}

/// The stop set while a spooled `text` read before its `type` is replayed: the replay ends
/// at its next chunk, and nothing is stored.
#[test]
fn a_stop_during_a_replay_ends_it_at_the_next_chunk() {
    let fx = Fx::new();
    let n = MAX_BLOB_BYTES + 1;
    {
        let mut w = BufWriter::new(File::create(&fx.path).unwrap());
        w.write_all(b"{\"type\":\"assistant\",\"message\":{\"content\":[{\"text\":\"")
            .unwrap();
        fill(&mut w, b'a', n);
        w.write_all(b"\",\"type\":\"text\"}]}}\n").unwrap();
        w.flush().unwrap();
    }
    let probe = ReplayProbe::default();
    let p = probe.clone();
    let mut tail = fx.tail_with(|d| d.replay = Some(p.clone()), probe.stop.clone());
    // The generation first; the stop is set only by the replay.
    assert_eq!(tail.poll(&fx.backend()), Step::Again);
    assert_eq!(tail.poll(&fx.backend()), Step::End("stopped".into()));
    assert_eq!(probe.chunks.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(shorts(&fx.events()), ["generation:1"]);
    no_spools(&fx);
}
