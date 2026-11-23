//! Pairing (`docs/design/http-surface.md` PR7-CL-15, §8 D12): a pair code exchanges once,
//! before its expiry, with a matching `Origin`, for one new owner token, only while its
//! issuing token is valid in the token-insert transaction.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use chaldea_core::auth::TokenKind;
use chaldea_core::http::PairResponse;
use chaldea_daemon::http::Mounts;
use chaldea_daemon::store::TokenSelector;
use support::http::{Reply, TestServer};
use support::soon;

fn code(s: &str) -> Option<String> {
    Some(s.to_owned())
}

fn token_of(r: &Reply) -> String {
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.header("cache-control"), Some("no-store"));
    let body: PairResponse = serde_json::from_slice(&r.body).unwrap();
    body.token
}

/// The owner tokens stored.
fn owners(srv: &TestServer) -> usize {
    let path = srv.store.path();
    support::http::dump(&path)
        .into_iter()
        .find(|(t, _)| t == "tokens")
        .map_or(0, |(_, rows)| {
            rows.iter()
                .filter(|r| {
                    r.iter()
                        .any(|v| *v == rusqlite::types::Value::Text("owner".into()))
                })
                .count()
        })
}

async fn reads(srv: &TestServer, token: &str) -> u16 {
    srv.send(&srv.req("GET", "/api/threads").bearer(token))
        .await
        .status
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_use() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let c = srv.pair_code(&owner).await.unwrap();
    let token = token_of(&srv.pair(&c).await);
    assert_eq!(srv.token_row(&token).unwrap().kind, TokenKind::Owner);
    assert_eq!(reads(&srv, &token).await, 200);
    assert_eq!(srv.pair(&c).await.outcome(), (401, code("unauthorized")));
    // A code of the right form that was never issued.
    let never = format!("chdp_{}", "n".repeat(43));
    assert_eq!(
        srv.pair(&never).await.outcome(),
        (401, code("unauthorized"))
    );
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expires() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let f = Instant::now();
    srv.clock.freeze(f);
    let early = srv.pair_code(&owner).await.unwrap();
    let late = srv.pair_code(&owner).await.unwrap();
    srv.clock.freeze(f + Duration::from_millis(59_999));
    token_of(&srv.pair(&early).await);
    srv.clock.freeze(f + Duration::from_secs(60));
    assert_eq!(srv.pair(&late).await.outcome(), (401, code("unauthorized")));
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn origin_required() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let c = srv.pair_code(&owner).await.unwrap();
    let body = format!(r#"{{"code":"{c}"}}"#);
    let p = srv.port;
    for origin in [
        None,
        Some("http://evil".to_owned()),
        Some(format!("http://localhost:{p}")),
    ] {
        let mut req = srv.req("POST", "/api/pair").body(body.as_bytes());
        if let Some(o) = &origin {
            req = req.header("Origin", o);
        }
        assert_eq!(
            srv.send(&req).await.outcome(),
            (403, code("origin_rejected")),
            "{origin:?}"
        );
    }
    // Refused before the body: the code is not spent.
    token_of(&srv.pair(&c).await);
    // The minting route refuses any `Origin`.
    let r = srv
        .send(
            &srv.req("POST", "/api/pair-codes")
                .bearer(&owner)
                .header("Origin", &format!("http://127.0.0.1:{p}"))
                .body(b"{}"),
        )
        .await;
    assert_eq!(r.outcome(), (403, code("origin_rejected")));
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_exchange_once() {
    let srv = Arc::new(TestServer::start(Mounts::mvp()).await);
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let c = srv.pair_code(&owner).await.unwrap();
    let before = owners(&srv);
    let barrier = Arc::new(tokio::sync::Barrier::new(16));
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let (srv, barrier, c) = (srv.clone(), barrier.clone(), c.clone());
        tasks.push(tokio::spawn(async move {
            let req = srv.pair_req(&c);
            barrier.wait().await;
            srv.send(&req).await.outcome()
        }));
    }
    let mut ok = 0;
    for t in tasks {
        match t.await.unwrap() {
            (200, None) => ok += 1,
            other => assert_eq!(other, (401, code("unauthorized"))),
        }
    }
    assert_eq!(ok, 1);
    assert_eq!(owners(&srv), before + 1, "one token issued");
    Arc::into_inner(srv).unwrap().stop().await;
}

/// Waits until the server has `n` registered tasks (the pair request's write observer).
async fn tasks_reach(srv: &TestServer, n: usize) {
    support::http::eventually(Duration::from_secs(10), "the pair request's tasks", || {
        srv.server.tasks() >= n
    })
    .await;
}

/// The issuer's revocation committed before the exchange's job, both queued behind a held
/// writer (the revocation first): 401, no token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issuer_revoked_before_exchange() {
    let srv = Arc::new(TestServer::start(Mounts::mvp()).await);
    let issuer = srv.token(TokenKind::Owner, None, None).await;
    let c = srv.pair_code(&issuer).await.unwrap();
    let before = owners(&srv);
    let hold = srv.hold_writer().await;
    let id = srv.token_row(&issuer).unwrap().token_id;
    let revocation = srv
        .store
        .store()
        .submit(soon(), move |tx| {
            Ok(tx.token_revoke(TokenSelector::Token(id))?)
        })
        .unwrap();
    let base = srv.server.tasks();
    let exchange = {
        let srv = srv.clone();
        tokio::spawn(async move { srv.pair(&c).await })
    };
    // The connection and the exchange's write observer.
    tasks_reach(&srv, base + 2).await;
    drop(hold);
    assert_eq!(revocation.await.unwrap(), 1);
    assert_eq!(
        exchange.await.unwrap().outcome(),
        (401, code("unauthorized"))
    );
    assert_eq!(owners(&srv), before, "no token inserted");
    Arc::into_inner(srv).unwrap().stop().await;
}

/// The issuer's revocation queued after the exchange's job: the new token stays valid.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issuer_revoked_after_exchange() {
    let srv = Arc::new(TestServer::start(Mounts::mvp()).await);
    let issuer = srv.token(TokenKind::Owner, None, None).await;
    let c = srv.pair_code(&issuer).await.unwrap();
    let hold = srv.hold_writer().await;
    let base = srv.server.tasks();
    let exchange = {
        let srv = srv.clone();
        tokio::spawn(async move { srv.pair(&c).await })
    };
    tasks_reach(&srv, base + 2).await;
    let id = srv.token_row(&issuer).unwrap().token_id;
    let revocation = srv
        .store
        .store()
        .submit(soon(), move |tx| {
            Ok(tx.token_revoke(TokenSelector::Token(id))?)
        })
        .unwrap();
    drop(hold);
    let token = token_of(&exchange.await.unwrap());
    assert_eq!(revocation.await.unwrap(), 1);
    assert_eq!(reads(&srv, &issuer).await, 401);
    assert_eq!(reads(&srv, &token).await, 200, "the new token stays valid");
    Arc::into_inner(srv).unwrap().stop().await;
}

/// A restart drops every pair code; the owner mints a new one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_after_restart() {
    let mut srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let c = srv.pair_code(&owner).await.unwrap();
    srv.shutdown_http(Instant::now() + Duration::from_secs(10))
        .await;
    let srv = TestServer::start_on(srv.store, Mounts::mvp(), |_| {}).await;
    assert_eq!(srv.pair(&c).await.outcome(), (401, code("unauthorized")));
    let fresh = srv.pair_code(&owner).await.unwrap();
    token_of(&srv.pair(&fresh).await);
    srv.stop().await;
}

/// `POST /api/pair` takes exactly `{ "code" }`: another body shape is 400, a missing or
/// malformed code 401, and neither spends the code. `POST /api/pair-codes` takes an empty
/// body or `{}`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn body_rules() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let c = srv.pair_code(&owner).await.unwrap();
    let origin = format!("http://127.0.0.1:{}", srv.port);
    for (body, want) in [
        ("[]".to_owned(), (400, code("bad_request"))),
        ("not json".to_owned(), (400, code("bad_request"))),
        (
            format!(r#"{{"code":"{c}","x":1}}"#),
            (400, code("bad_request")),
        ),
        ("{}".to_owned(), (401, code("unauthorized"))),
        (String::new(), (401, code("unauthorized"))),
        (r#"{"code":5}"#.to_owned(), (401, code("unauthorized"))),
        (
            r#"{"code":"chdp_short"}"#.to_owned(),
            (401, code("unauthorized")),
        ),
    ] {
        let req = srv
            .req("POST", "/api/pair")
            .header("Origin", &origin)
            .body(body.as_bytes());
        assert_eq!(srv.send(&req).await.outcome(), want, "{body}");
    }
    token_of(&srv.pair(&c).await);
    for (body, want) in [("", 200), ("{}", 200), (r#"{"x":1}"#, 400), ("[]", 400)] {
        let r = srv
            .send(
                &srv.req("POST", "/api/pair-codes")
                    .bearer(&owner)
                    .body(body.as_bytes()),
            )
            .await;
        assert_eq!(r.status, want, "{body}: {r:?}");
    }
    srv.stop().await;
}
