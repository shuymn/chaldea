//! `GET /api/blobs/{sha256}?stream_id=` for a value above `MAX_BLOB_BYTES`
//! (`docs/design/event-store.md` §2.5, design D14): served in order through the same
//! authorization, holding a blob slot and no reader connection across network waits, and
//! aborted, never ended short, when a part cannot be read.

mod support;

use std::time::Duration;

use chaldea_core::auth::TokenKind;
use chaldea_core::ids::{StreamId, ThreadId};
use chaldea_core::payload::Content;
use chaldea_daemon::http::Mounts;
use chaldea_daemon::store::MAX_BLOB_BYTES;
use support::http::{Reply, TestServer, connect, parse, read_to_end, write};
use support::waits;
use support::{raw_unknown, registered};
use tokio::io::AsyncReadExt as _;
use tokio::net::TcpStream;

const PART: usize = 4 * 1024 * 1024;
const WAIT: Duration = Duration::from_secs(30);

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from(i % 251).unwrap() ^ seed)
        .collect()
}

/// Spools `bytes` through the store and stores them referenced from `stream`; returns the
/// hash.
async fn put(srv: &TestServer, stream: StreamId, key: &str, bytes: &[u8], media: &str) -> String {
    let mut spool = srv.store.store().blob_spool().unwrap();
    for chunk in bytes.chunks(1 << 20) {
        spool.append(chunk).unwrap();
    }
    let p = spool.finish(media).unwrap();
    let Content::Blob { blob, .. } = &p.content else {
        panic!("a blob")
    };
    let sha = blob.sha256.clone();
    let mut e = raw_unknown(stream, key, p.content.clone());
    e.blobs = p.blob.into_iter().collect();
    srv.append(vec![e]).await;
    sha
}

async fn thread(srv: &TestServer, n: u128) -> StreamId {
    let t = ThreadId::from_u128(0xB10B + n);
    srv.append(vec![registered(t, false)]).await;
    StreamId::Thread(t)
}

fn target(sha: &str, stream: StreamId) -> String {
    format!("/api/blobs/{sha}?stream_id={stream}")
}

async fn get(srv: &TestServer, token: &str, sha: &str, stream: StreamId) -> Reply {
    srv.send(&srv.req("GET", &target(sha, stream)).bearer(token))
        .await
}

/// A download whose client reads the head and `n` body bytes, then stops reading.
struct Held {
    conn: TcpStream,
    got: Vec<u8>,
}

async fn hold(srv: &TestServer, token: &str, sha: &str, stream: StreamId, n: usize) -> Held {
    let mut conn = connect(srv.port).await;
    let req = srv.req("GET", &target(sha, stream)).bearer(token);
    write(&mut conn, &req.bytes()).await;
    let mut got = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let have = |got: &[u8]| {
        got.windows(4)
            .position(|w| w == b"\r\n\r\n")
            .is_some_and(|at| got.len() >= at + 4 + n)
    };
    while !have(&got) {
        let k = waits::bounded(WAIT, conn.read(&mut buf))
            .await
            .expect("bytes within the wait")
            .unwrap();
        assert!(k > 0, "the connection ended early");
        got.extend_from_slice(&buf[..k]);
    }
    Held { conn, got }
}

impl Held {
    /// Reads the rest to EOF and parses the whole reply.
    async fn finish(mut self) -> Reply {
        let (rest, end) = read_to_end(&mut self.conn).await;
        self.got.extend_from_slice(&rest);
        parse(&self.got, end)
    }
}

/// A multi-part value downloads equal to its input with the safe headers; a stream without
/// a reference (another thread, the host, an unknown one) gets the same 404 as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn download_equals_input_through_a_referencing_stream() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let a = thread(&srv, 1).await;
    let b = thread(&srv, 2).await;
    let bytes = pattern(MAX_BLOB_BYTES + PART + 12_345, 1);
    let sha = put(&srv, a, "k", &bytes, "application/json").await;
    let r = get(&srv, &owner, &sha, a).await;
    assert_eq!(r.status, 200);
    let len = bytes.len().to_string();
    assert_eq!(r.header("content-length"), Some(len.as_str()));
    assert_eq!(r.header("content-type"), Some("application/json"));
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(r.header("content-disposition"), Some("attachment"));
    assert!(r.body == bytes, "the downloaded bytes differ");
    let unknown = StreamId::Thread(ThreadId::from_u128(0xDEAD));
    let mut bodies = Vec::new();
    for s in [b, StreamId::Host, unknown] {
        let r = get(&srv, &owner, &sha, s).await;
        assert_eq!((r.status, r.code()), (404, Some("not_found".into())), "{s}");
        bodies.push(r.body);
    }
    bodies.dedup();
    assert_eq!(bodies.len(), 1);
    srv.stop().await;
}

/// Four downloads stalled by their clients (every blob slot) hold no reader connection:
/// the whole reader pool can be taken and other reads answer meanwhile; each download then
/// completes equal to its input.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_downloads_hold_no_reader() {
    let srv = TestServer::start_with(Mounts::mvp(), |c| {
        c.write_stall_timeout = Duration::from_secs(120);
    })
    .await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let a = thread(&srv, 1).await;
    let bytes = pattern(MAX_BLOB_BYTES + 1, 2);
    let sha = put(&srv, a, "k", &bytes, "application/octet-stream").await;
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(hold(&srv, &owner, &sha, a, 1024).await);
    }
    // Every slot is taken: a fifth download is refused.
    assert_eq!(get(&srv, &owner, &sha, a).await.status, 503);
    // Other reads proceed while the four stream.
    for _ in 0..8 {
        let r = srv
            .send(&srv.req("GET", "/api/threads").bearer(&owner))
            .await;
        assert_eq!(r.status, 200);
    }
    // All four reader connections are free: taking them all succeeds at once.
    let release = srv.hold_readers(4);
    drop(release);
    for h in held {
        let r = h.finish().await;
        assert_eq!(r.status, 200);
        assert!(r.body == bytes, "a slow download's bytes differ");
    }
    srv.stop().await;
}

/// A part that cannot be read mid-stream (deleted under the test, past the immutability
/// trigger) aborts the response after the parts before it: the client gets a prefix shorter
/// than `Content-Length`, never a complete-looking reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_part_aborts_the_response() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let a = thread(&srv, 1).await;
    let bytes = pattern(MAX_BLOB_BYTES + 1, 3);
    let sha = put(&srv, a, "k", &bytes, "application/octet-stream").await;
    let c = rusqlite::Connection::open(srv.store.path()).unwrap();
    c.execute_batch(&format!(
        "DROP TRIGGER large_blob_parts_no_delete;
         DELETE FROM large_blob_parts WHERE sha256 = '{sha}' AND part_index = 1;"
    ))
    .unwrap();
    drop(c);
    let r = get(&srv, &owner, &sha, a).await;
    assert_eq!(r.status, 200);
    let len = bytes.len().to_string();
    assert_eq!(r.header("content-length"), Some(len.as_str()));
    // At most part 0 arrives (an abort may discard bytes not yet flushed), as a prefix.
    assert!(r.body.len() <= PART, "{} bytes past part 0", r.body.len());
    assert!(r.body == bytes[..r.body.len()], "the bytes sent differ");
    srv.stop().await;
}

/// A client that disconnects mid-download releases its blob slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnect_releases_the_slot() {
    let srv = TestServer::start_with(Mounts::mvp(), |c| c.blob_slots = 1).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let a = thread(&srv, 1).await;
    let bytes = pattern(MAX_BLOB_BYTES + 1, 4);
    let sha = put(&srv, a, "k", &bytes, "application/octet-stream").await;
    let h = hold(&srv, &owner, &sha, a, 1024).await;
    assert_eq!(get(&srv, &owner, &sha, a).await.status, 503);
    drop(h);
    let mut ok = false;
    for _ in 0..100 {
        let r = get(&srv, &owner, &sha, a).await;
        if r.status == 200 {
            assert!(r.body == bytes);
            ok = true;
            break;
        }
        assert_eq!(r.status, 503);
        waits::pause(Duration::from_millis(100)).await;
    }
    assert!(ok, "the slot was not released within 10 s");
    srv.stop().await;
}
