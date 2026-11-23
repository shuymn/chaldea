//! `GET /api/blobs/{sha256}?stream_id=` (`docs/design/http-surface.md` PR7-CL-13): served
//! only through a readable stream that references the blob, with the safe type and the
//! download headers.

mod support;

use chaldea_core::auth::TokenKind;
use chaldea_core::ids::{StreamId, ThreadId};
use chaldea_core::payload::Content;
use chaldea_daemon::http::Mounts;
use chaldea_daemon::store::prepare_content;
use support::http::{Reply, TestServer};
use support::{raw_unknown, registered};

fn code(s: &str) -> Option<String> {
    Some(s.to_owned())
}

/// Stores `bytes` as a blob of `media` referenced from `stream`; returns its hash.
async fn put(srv: &TestServer, stream: StreamId, key: &str, bytes: &[u8], media: &str) -> String {
    let p = prepare_content(bytes.to_vec(), media).unwrap();
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

async fn get(srv: &TestServer, token: &str, sha: &str, stream: StreamId) -> Reply {
    srv.send(
        &srv.req("GET", &format!("/api/blobs/{sha}?stream_id={stream}"))
            .bearer(token),
    )
    .await
}

fn big(seed: u8) -> Vec<u8> {
    (0..100_000u32).map(|i| (i % 251) as u8 ^ seed).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn served_only_through_a_referencing_stream() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let a = thread(&srv, 1).await;
    let b = thread(&srv, 2).await;
    let sha = put(&srv, a, "k", &big(1), "application/octet-stream").await;
    assert_eq!(get(&srv, &owner, &sha, a).await.status, 200);
    // The same blob through a stream with no reference, the host stream, an unknown
    // stream: one 404.
    let unknown = StreamId::Thread(ThreadId::from_u128(0xDEAD));
    let mut bodies = Vec::new();
    for s in [b, StreamId::Host, unknown] {
        let r = get(&srv, &owner, &sha, s).await;
        assert_eq!(r.outcome(), (404, code("not_found")), "{s}");
        bodies.push(r.body);
    }
    // An unknown hash on the referencing stream.
    let r = get(&srv, &owner, &"0".repeat(64), a).await;
    assert_eq!(r.outcome(), (404, code("not_found")));
    bodies.push(r.body);
    bodies.dedup();
    assert_eq!(bodies.len(), 1);
    srv.stop().await;
}

/// With every blob slot taken, a missing reference is still 404; a present one is 503.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_ref_is_404_with_slots_full() {
    let srv = TestServer::start_with(Mounts::mvp(), |c| c.blob_slots = 0).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let a = thread(&srv, 1).await;
    let b = thread(&srv, 2).await;
    let sha = put(&srv, a, "k", &big(1), "application/octet-stream").await;
    assert_eq!(
        get(&srv, &owner, &sha, b).await.outcome(),
        (404, code("not_found"))
    );
    let r = get(&srv, &owner, &sha, a).await;
    assert_eq!(r.outcome(), (503, code("busy")));
    assert!(
        r.body.windows(14).any(|w| w == b"\"retry_after\":"),
        "{r:?}"
    );
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn type_rule() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let a = thread(&srv, 1).await;
    for (n, stored, served) in [
        (1, "text/plain; charset=utf-8", "text/plain; charset=utf-8"),
        (2, "application/json", "application/json"),
        (3, "text/html", "application/octet-stream"),
        (4, "image/svg+xml", "application/octet-stream"),
    ] {
        let sha = put(&srv, a, &format!("k{n}"), &big(n), stored).await;
        let r = get(&srv, &owner, &sha, a).await;
        assert_eq!(r.status, 200);
        assert_eq!(r.header("content-type"), Some(served), "{stored}");
    }
    // One stream referencing one blob with two types: they disagree.
    let bytes = big(9);
    let sha = put(&srv, a, "j1", &bytes, "application/json").await;
    let again = put(&srv, a, "j2", &bytes, "text/plain; charset=utf-8").await;
    assert_eq!(sha, again);
    let r = get(&srv, &owner, &sha, a).await;
    assert_eq!(r.header("content-type"), Some("application/octet-stream"));
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_query_is_400() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let sha = "a".repeat(64);
    for target in [
        format!("/api/blobs/{sha}"),
        format!("/api/blobs/{sha}?stream_id=nope"),
        format!("/api/blobs/{sha}?stream_id=host&stream_id=host"),
        format!("/api/blobs/{sha}?stream_id=host&x=1"),
        format!("/api/blobs/{}?stream_id=host", "A".repeat(64)),
        format!("/api/blobs/{}?stream_id=host", "a".repeat(63)),
        "/api/blobs/xyz?stream_id=host".to_owned(),
    ] {
        let r = srv.send(&srv.req("GET", &target).bearer(&owner)).await;
        assert_eq!(r.outcome(), (400, code("bad_query")), "{target}");
    }
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bytes_equal() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let a = thread(&srv, 1).await;
    // Larger than one 64 KiB body frame, not a multiple of it.
    let bytes: Vec<u8> = (0..300_001u32).map(|i| (i % 253) as u8).collect();
    let sha = put(&srv, a, "k", &bytes, "application/json").await;
    let r = get(&srv, &owner, &sha, a).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body, bytes);
    assert_eq!(r.header("content-length"), Some("300001"));
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(r.header("content-disposition"), Some("attachment"));
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert_eq!(r.header("connection"), Some("close"));
    for h in ["etag", "accept-ranges"] {
        assert_eq!(r.header(h), None, "{h}");
    }
    srv.stop().await;
}
