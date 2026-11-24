//! Pair-code expiry decided at the removal (design §2.4 "Pair codes" step 1), and the
//! issuer checked in the insert job's transaction (PR7-CL-15).

use std::sync::Arc;
use std::time::{Duration, Instant};

use chaldea_core::auth::TokenKind;
use chaldea_core::http::PairResponse;

use super::shared;
use super::support::http::{TestServer, reached, settled};
use super::support::soon;
use crate::http::Mounts;
use crate::store::TokenSelector;

/// A hook just before the codes' lock moves the clock across the code's 60 s after the
/// request was admitted: the removal reads the clock under the lock, so the code is expired
/// and no token is issued.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn take_reads_the_clock_under_the_lock() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let f = Instant::now();
    srv.clock.freeze(f);
    let late = srv.pair_code(&owner).await.unwrap();
    let early = srv.pair_code(&owner).await.unwrap();
    let s = shared(&srv);
    let arm = |to: Duration| {
        let clock = srv.clock.clone();
        *s.pair_codes.before_take_lock.lock().unwrap() =
            Some(Box::new(move || clock.freeze(f + to)));
    };
    // Valid when the request arrives (59.999 s), expired at the removal (60 s).
    srv.clock.freeze(f + Duration::from_millis(59_999));
    arm(Duration::from_secs(60));
    let r = srv.pair(&late).await;
    assert_eq!(r.outcome(), (401, Some("unauthorized".to_owned())), "{r:?}");
    // Positive control: still valid at the removal.
    srv.clock.freeze(f + Duration::from_millis(59_998));
    arm(Duration::from_millis(59_999));
    assert_eq!(srv.pair(&early).await.status, 200);
    srv.stop().await;
}

/// The issuer's revocation queued after the exchange's insert job: the new token stays
/// valid. The revocation is submitted once the exchange's observer waits on its ticket, so
/// its job is in the held writer's queue first. The server's clock stands still, so neither
/// the request deadline nor the reply cutoff ends the exchange while the writer is held.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issuer_revoked_after_exchange() {
    let srv = Arc::new(TestServer::start(Mounts::mvp()).await);
    let issuer = srv.token(TokenKind::Owner, None, None).await;
    let c = srv.pair_code(&issuer).await.unwrap();
    srv.clock.stop();
    let hold = srv.hold_writer().await;
    let parked = shared(&srv).faults.arm_ticket_park();
    let exchange = {
        let srv = srv.clone();
        tokio::spawn(async move { srv.pair(&c).await })
    };
    reached(parked).await;
    let id = srv.token_row(&issuer).unwrap().token_id;
    let revocation = srv
        .store
        .store()
        .submit(soon(), move |tx| {
            Ok(tx.token_revoke(TokenSelector::Token(id))?)
        })
        .unwrap();
    drop(hold);
    let r = exchange.await.unwrap();
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.header("cache-control"), Some("no-store"));
    let token = serde_json::from_slice::<PairResponse>(&r.body)
        .unwrap()
        .token;
    assert_eq!(settled(revocation).await.unwrap(), 1);
    assert_eq!(reads(&srv, &issuer).await, 401);
    assert_eq!(reads(&srv, &token).await, 200, "the new token stays valid");
    Arc::into_inner(srv).unwrap().stop().await;
}

async fn reads(srv: &TestServer, token: &str) -> u16 {
    srv.send(&srv.req("GET", "/api/threads").bearer(token))
        .await
        .status
}
