//! Values above `MAX_BLOB_BYTES` (`docs/design/event-store.md` §2.5): spooled by the
//! producer, stored as ordered parts under one `BlobRef`, verified before commit, and read
//! back whole or part by part.

mod support;

use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use chaldea_core::ids::StreamId;
use chaldea_core::payload::{Content, INLINE_LIMIT_BYTES};
use chaldea_daemon::store::{
    BlobDownload, BlobInput, ContentError, LargeBlob, MAX_BLOB_BYTES, Prepared, SPOOL_FILE_PREFIX,
    StoreError, WriteError, prepare_content,
};
use futures_util::FutureExt as _;
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};
use support::{TestStore, raw_unknown, registered, thread};

const PART: usize = 4 * 1024 * 1024;
const TEXT: &str = "text/plain; charset=utf-8";
const JSON: &str = "application/json";

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from(i % 251).unwrap() ^ seed)
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `bytes` through the store's spool, in 1 MiB writes.
fn spooled(s: &TestStore, bytes: &[u8], media: &str) -> Prepared {
    let mut spool = s.store().blob_spool().unwrap();
    for chunk in bytes.chunks(1 << 20) {
        spool.append(chunk).unwrap();
    }
    spool.finish(media).unwrap()
}

/// The spool files in the state directory.
fn spool_files(s: &TestStore) -> Vec<PathBuf> {
    std::fs::read_dir(s.dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(SPOOL_FILE_PREFIX))
        })
        .collect()
}

/// Rows of `large_blobs`, `large_blob_parts`, and `large_blob_refs`.
fn large_rows(s: &TestStore) -> (i64, i64, i64) {
    let c = Connection::open_with_flags(s.path(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let n = |t: &str| -> i64 {
        c.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0))
            .unwrap()
    };
    (
        n("large_blobs"),
        n("large_blob_parts"),
        n("large_blob_refs"),
    )
}

fn sha(p: &Prepared) -> String {
    match &p.content {
        Content::Blob { blob, .. } => blob.sha256.clone(),
        c @ Content::Inline { .. } => panic!("not a blob: {c:?}"),
    }
}

fn carrying(stream: StreamId, key: &str, p: Prepared) -> chaldea_daemon::store::NewEvent {
    let mut e = raw_unknown(stream, key, p.content);
    e.blobs = p.blob.into_iter().collect();
    e
}

fn head(s: &TestStore, stream: StreamId) -> u64 {
    s.read(move |r| r.head(stream, 0)).head_seq
}

/// A value of `MAX_BLOB_BYTES + 1` bytes is spooled, stored in 17 parts under its whole
/// SHA-256 and size, read back equal whole and part by part, and its spool file is gone
/// after the commit.
#[test]
fn cap_plus_one_round_trips() {
    let s = TestStore::new();
    let bytes = pattern(MAX_BLOB_BYTES + 1, 7);
    let p = spooled(&s, &bytes, TEXT);
    let Content::Blob { blob, preview, .. } = &p.content else {
        panic!("a blob")
    };
    assert_eq!(blob.sha256, hex(&bytes));
    assert_eq!(blob.size, bytes.len() as u64);
    assert!(!preview.is_empty());
    assert!(matches!(&p.blob, Some(BlobInput::Spooled(_))));
    assert_eq!(spool_files(&s).len(), 1);
    let digest = blob.sha256.clone();
    s.append(vec![carrying(StreamId::Host, "big", p)]).unwrap();
    assert!(
        spool_files(&s).is_empty(),
        "the spool file outlived the commit"
    );
    assert_eq!(large_rows(&s), (1, 17, 1));
    let (whole, download, last) = {
        let d = digest.clone();
        s.read(move |r| {
            Ok((
                r.blob(StreamId::Host, &d)?,
                r.blob_download(StreamId::Host, &d)?,
                r.large_blob_part(&d, 16)?,
            ))
        })
    };
    assert_eq!(
        download,
        Some(BlobDownload::Parts(LargeBlob {
            media_type: TEXT.into(),
            size: bytes.len() as u64,
            part_count: 17,
        }))
    );
    assert_eq!(last, Some(vec![*bytes.last().unwrap()]));
    let whole = whole.unwrap();
    assert_eq!(whole.media_type, TEXT);
    assert!(whole.data == bytes, "the stored bytes differ");
}

/// Spooled values at or below the cap stay as today: inline, or one in-memory blob.
#[test]
fn spool_at_or_below_the_cap_is_prepared_in_memory() {
    let s = TestStore::new();
    let small = spooled(&s, b"hello", TEXT);
    assert_eq!(small.content, Content::text("hello"));
    assert!(small.blob.is_none());
    let at_cap = spooled(&s, &pattern(MAX_BLOB_BYTES, 1), JSON);
    assert!(matches!(&at_cap.blob, Some(BlobInput::Bytes(b)) if b.len() == MAX_BLOB_BYTES));
    assert!(
        spool_files(&s).is_empty(),
        "an in-memory result keeps no file"
    );
}

/// A value of several parts and a short last part round-trips; the same bytes spooled again
/// for another stream and type are reused by hash (refs only), and a later event references
/// the stored blob without an input.
#[test]
fn multi_part_value_round_trips_and_is_reused() {
    let s = TestStore::new();
    let (a, b) = (thread(1), thread(2));
    s.append(vec![registered(a, false), registered(b, false)])
        .unwrap();
    let bytes = pattern(MAX_BLOB_BYTES + 3 * PART + 12_345, 3);
    let p = spooled(&s, &bytes, TEXT);
    let digest = sha(&p);
    let content = p.content.clone();
    s.append(vec![carrying(StreamId::Thread(a), "one", p)])
        .unwrap();
    assert_eq!(large_rows(&s), (1, 20, 1));
    let again = spooled(&s, &bytes, JSON);
    assert_eq!(sha(&again), digest);
    s.append(vec![carrying(StreamId::Thread(b), "two", again)])
        .unwrap();
    // A reference to the stored blob, without an input.
    s.append(vec![raw_unknown(StreamId::Thread(a), "three", content)])
        .unwrap();
    assert_eq!(large_rows(&s), (1, 20, 2));
    assert!(spool_files(&s).is_empty());
    for (stream, media) in [(a, TEXT), (b, JSON)] {
        let d = digest.clone();
        let body = s
            .read(move |r| r.blob(StreamId::Thread(stream), &d))
            .unwrap();
        assert_eq!(body.media_type, media);
        assert!(body.data == bytes, "{media}: the stored bytes differ");
    }
    // Readable only through a referencing stream.
    let d = digest.clone();
    assert!(!s.read(move |r| r.blob_ref_exists(StreamId::Host, &d)));
    let d = digest.clone();
    assert!(s.read(move |r| r.blob(StreamId::Host, &d)).is_none());
    // Rebuilding and verifying the projections leave the large-blob tables alone.
    let before = large_snapshot(&s);
    s.run(|tx| Ok(tx.rebuild_projections()?)).unwrap();
    let diff = s.run(|tx| Ok(tx.verify_projections()?)).unwrap();
    assert!(diff.is_empty(), "{diff:?}");
    assert_eq!(large_snapshot(&s), before);
    let body = s
        .read(move |r| r.blob(StreamId::Thread(a), &digest))
        .unwrap();
    assert!(body.data == bytes, "the bytes differ after the rebuild");
}

/// Every row of the large-blob tables, each part as its index, length, and SHA-256.
fn large_snapshot(s: &TestStore) -> Vec<String> {
    let c = Connection::open_with_flags(s.path(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut out = Vec::new();
    for sql in [
        "SELECT sha256 || '|' || size || '|' || part_count FROM large_blobs ORDER BY 1",
        "SELECT stream_id || '|' || sha256 || '|' || media_type FROM large_blob_refs ORDER BY 1",
    ] {
        let mut stmt = c.prepare(sql).unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        out.extend(rows.map(Result::unwrap));
    }
    let mut stmt = c
        .prepare("SELECT sha256, part_index, data FROM large_blob_parts ORDER BY 1, 2")
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            let data: Vec<u8> = r.get(2)?;
            Ok(format!(
                "{}|{}|{}|{}",
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                data.len(),
                hex(&data)
            ))
        })
        .unwrap();
    out.extend(rows.map(Result::unwrap));
    out
}

/// A spooled input for `bytes` whose file then has its first byte changed (its hash still
/// claims `bytes`), and the content of the changed bytes, whose preview the changed head
/// matches (a preview reads only the head, so a blob of the changed head's first bytes has
/// the same one).
fn changed_head(s: &TestStore, bytes: &[u8]) -> (Prepared, Content) {
    let p = spooled(s, bytes, TEXT);
    let Some(BlobInput::Spooled(input)) = &p.blob else {
        panic!("spooled")
    };
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(input.path())
        .unwrap();
    f.write_all(b"Z").unwrap();
    drop(f);
    let mut changed = bytes[..=INLINE_LIMIT_BYTES].to_vec();
    changed[0] = b'Z';
    let Content::Blob {
        preview,
        preview_lossy,
        ..
    } = prepare_content(changed, TEXT).unwrap().content
    else {
        panic!("a blob")
    };
    let Content::Blob { blob, .. } = p.content.clone() else {
        panic!("a blob")
    };
    (
        p,
        Content::Blob {
            blob,
            preview,
            preview_lossy,
        },
    )
}

fn preview_refused(r: Result<Vec<chaldea_daemon::store::Appended>, WriteError>) {
    match r {
        Err(WriteError::RolledBack(StoreError::Content(ContentError::BlobMismatch {
            field: "preview",
            ..
        }))) => {}
        other => panic!("expected a preview mismatch, got {other:?}"),
    }
}

/// A reference to a stored large blob is checked against the stored bytes, not against a
/// spooled input of the same hash whose head differs: the stored blob is what it names.
#[test]
fn reuse_checks_the_stored_blob() {
    let s = TestStore::new();
    let bytes = pattern(MAX_BLOB_BYTES + 1, 6);
    s.append(vec![carrying(
        StreamId::Host,
        "first",
        spooled(&s, &bytes, TEXT),
    )])
    .unwrap();
    let (p, content) = changed_head(&s, &bytes);
    let mut e = raw_unknown(StreamId::Host, "second", content);
    e.blobs = p.blob.into_iter().collect();
    preview_refused(s.append(vec![e]));
    assert_eq!(head(&s, StreamId::Host), 1);
    assert_eq!(large_rows(&s), (1, 17, 1));
}

/// Several inputs of one hash: the reference is checked against the first, the one
/// inserted, so a later input with another head cannot vouch for the preview.
#[test]
fn one_input_per_hash_is_checked_and_inserted() {
    let s = TestStore::new();
    let bytes = pattern(MAX_BLOB_BYTES + 1, 7);
    let good = spooled(&s, &bytes, TEXT);
    let (bad, content) = changed_head(&s, &bytes);
    let mut e = raw_unknown(StreamId::Host, "both", content);
    e.blobs = vec![good.blob.unwrap(), bad.blob.unwrap()];
    preview_refused(s.append(vec![e]));
    assert_eq!(head(&s, StreamId::Host), 0);
    assert_eq!(large_rows(&s), (0, 0, 0));
    assert!(spool_files(&s).is_empty());
}

/// A spool file changed after `finish`: the bytes no longer hash to the reference (the
/// head unchanged, so the preview still matches), or the file is longer or shorter than
/// the reference. Each job rolls back with nothing stored, and the file is removed.
#[test]
fn hash_or_size_mismatch_rolls_back() {
    let s = TestStore::new();
    let bytes = pattern(MAX_BLOB_BYTES + PART + 9, 5);
    type Edit = fn(&mut std::fs::File, usize);
    let cases: [(&str, Edit, &str); 3] = [
        (
            "a byte changed",
            |f, _| {
                f.seek(SeekFrom::Start(5 << 20)).unwrap();
                f.write_all(&[0xAA]).unwrap();
            },
            "sha256",
        ),
        (
            "a byte appended",
            |f, _| {
                f.seek(SeekFrom::End(0)).unwrap();
                f.write_all(&[0]).unwrap();
            },
            "size",
        ),
        (
            "truncated",
            |f, len| f.set_len(len as u64 - 1).unwrap(),
            "size",
        ),
    ];
    for (i, (what, edit, field)) in cases.into_iter().enumerate() {
        let p = spooled(&s, &bytes, TEXT);
        let Some(BlobInput::Spooled(input)) = &p.blob else {
            panic!("spooled")
        };
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(input.path())
                .unwrap();
            edit(&mut f, bytes.len());
        }
        let r = s.append(vec![carrying(StreamId::Host, &format!("bad:{i}"), p)]);
        match r {
            Err(WriteError::RolledBack(StoreError::Content(ContentError::BlobMismatch {
                field: f,
                ..
            }))) if f == field => {}
            other => panic!("{what}: {other:?}"),
        }
        assert_eq!(large_rows(&s), (0, 0, 0), "{what}");
        assert_eq!(head(&s, StreamId::Host), 0, "{what}");
        assert!(
            spool_files(&s).is_empty(),
            "{what}: the spool file outlived the rollback"
        );
    }
}

/// A spool dropped without `finish`, and a prepared value whose job never runs, leave no
/// file.
#[test]
fn unused_spools_leave_no_file() {
    let s = TestStore::new();
    let mut spool = s.store().blob_spool().unwrap();
    spool.append(b"partial").unwrap();
    assert_eq!(spool_files(&s).len(), 1);
    drop(spool);
    assert!(spool_files(&s).is_empty());
    let p = spooled(&s, &pattern(MAX_BLOB_BYTES + 1, 2), TEXT);
    assert_eq!(spool_files(&s).len(), 1);
    // The writer's clock runs ahead, so the job expires in the queue.
    s.clock.lead(Duration::from_secs(60));
    let (entered_tx, entered) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let mut ticket = s
        .store()
        .submit(Instant::now(), {
            let e = carrying(StreamId::Host, "late", p);
            let blocked = BlockedDrop {
                entered: entered_tx,
                release: release_rx,
            };
            move |tx| {
                let _blocked = blocked;
                Ok(tx.append(vec![e])?)
            }
        })
        .unwrap();
    entered
        .recv_timeout(Duration::from_secs(30))
        .expect("the expired job was not dropped");
    // The job's drop is under way: the reply waits for it.
    assert!(
        (&mut ticket).now_or_never().is_none(),
        "the reply came before the drop"
    );
    release.send(()).unwrap();
    assert!(matches!(ticket.blocking_wait(), Err(WriteError::Expired)));
    assert!(spool_files(&s).is_empty());
    s.clock.lead(Duration::ZERO);
}

/// A value whose drop reports that it began, then blocks until released (or until the
/// releasing side is gone).
struct BlockedDrop {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

impl Drop for BlockedDrop {
    fn drop(&mut self) {
        let _ = self.entered.send(());
        let _ = self.release.recv();
    }
}
