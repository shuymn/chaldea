//! C1-8, D14 producer side: lines over 64 MiB, one valid and one not JSON, are stored whole
//! as large blobs, download equal, the next line is read, and memory stays bounded.

use std::fs::File;
use std::io::{BufWriter, Write};

use chaldea_core::ids::StreamId;
use sha2::{Digest, Sha256};

use super::{Fx, assistant, shorts};
use crate::store::{BlobDownload, MAX_BLOB_BYTES};
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
    // One line never held more than the in-memory limit.
    assert!(
        tail.stats.peak_line_memory <= MAX_BLOB_BYTES,
        "{}",
        tail.stats.peak_line_memory
    );
    // No spool file is left in the state directory.
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
