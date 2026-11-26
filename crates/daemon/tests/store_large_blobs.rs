//! Values above `MAX_BLOB_BYTES` (`docs/design/event-store.md` §2.5): spooled by the
//! producer, stored as ordered parts under one `BlobRef`, verified before commit, and read
//! back whole or part by part.

mod support;

use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;

use chaldea_core::ids::StreamId;
use chaldea_core::payload::Content;
use chaldea_daemon::store::{
    BlobDownload, BlobInput, ContentError, LargeBlob, MAX_BLOB_BYTES, Prepared, SPOOL_FILE_PREFIX,
    StoreError, WriteError,
};
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
    assert!(s.read(move |r| r.blob(StreamId::Host, &digest)).is_none());
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
    let ticket = s
        .store()
        .submit(
            std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(1))
                .unwrap(),
            {
                let e = carrying(StreamId::Host, "late", p);
                move |tx| Ok(tx.append(vec![e])?)
            },
        )
        .unwrap();
    assert!(matches!(ticket.blocking_wait(), Err(WriteError::Expired)));
    assert!(spool_files(&s).is_empty());
}
