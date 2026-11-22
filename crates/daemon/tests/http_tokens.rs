//! Stored credentials (`docs/design/http-surface.md` PR7-CL-25, the token cases): the
//! store holds each token's SHA-256, never its plaintext, in the database or its WAL.

mod support;

use std::time::{Duration, Instant};

use chaldea_core::auth::TokenKind;
use chaldea_daemon::http::Mounts;
use sha2::{Digest, Sha256};
use support::http::TestServer;

fn sha(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// One token of each kind, plus one issued through `issue_owner`.
async fn issue_all(srv: &TestServer) -> Vec<(TokenKind, String)> {
    let mut out = Vec::new();
    for (i, k) in TokenKind::ALL.into_iter().enumerate() {
        out.push((k, srv.token_of(k, i as u128).await));
    }
    let issuer = sha(&out[0].1);
    let issued = srv
        .server
        .tokens()
        .issue_owner(Some(issuer), Instant::now() + Duration::from_secs(5))
        .await
        .unwrap();
    out.push((TokenKind::Owner, issued.expose().to_owned()));
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stored_credential_is_the_digest() {
    let srv = TestServer::start(Mounts::mvp()).await;
    for (kind, secret) in issue_all(&srv).await {
        let digest = sha(&secret);
        let row = srv
            .store
            .read(move |tx| tx.token_by_hash(&digest))
            .unwrap_or_else(|| panic!("no {kind:?} token stored under its digest"));
        assert_eq!(row.sha256, digest);
        assert_eq!(row.kind, kind);
    }
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issue_owner_refuses_a_revoked_issuer() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let issuer = srv.token(TokenKind::Owner, None, None).await;
    srv.revoke(&issuer).await;
    let before = support::http::dump(&srv.store.path());
    let r = srv
        .server
        .tokens()
        .issue_owner(Some(sha(&issuer)), Instant::now() + Duration::from_secs(5))
        .await;
    assert_eq!(r.unwrap_err().status(), 401);
    assert!(
        support::http::dump(&srv.store.path()) == before,
        "a token was inserted"
    );
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_plaintext_at_rest() {
    let mut srv = TestServer::start(Mounts::mvp()).await;
    let tokens = issue_all(&srv).await;
    let paths = srv.store.store().paths();
    // The WAL while the store is open, before any checkpoint.
    let wal = std::fs::read(&paths.wal).unwrap();
    assert!(
        contains(&wal, &sha(&tokens[0].1)),
        "positive control: the digest is in the WAL"
    );
    for (kind, secret) in &tokens {
        assert!(
            !contains(&wal, secret.as_bytes()),
            "a {kind:?} token is in the WAL"
        );
    }
    srv.shutdown_http(Instant::now() + Duration::from_secs(10))
        .await;
    srv.store.shutdown();
    let db = std::fs::read(&paths.db).unwrap();
    assert!(
        contains(&db, &sha(&tokens[0].1)),
        "positive control: the digest is in the db"
    );
    for (kind, secret) in &tokens {
        assert!(
            !contains(&db, secret.as_bytes()),
            "a {kind:?} token is in the db"
        );
    }
}
