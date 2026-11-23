//! Pair-code expiry decided at the removal (design §2.4 "Pair codes" step 1; PR7-CL-15).

use std::time::{Duration, Instant};

use chaldea_core::auth::TokenKind;

use super::shared;
use super::support::http::TestServer;
use crate::http::Mounts;

/// The clock crosses the code's 60 s while the exchange waits for the codes' lock: the
/// removal reads the clock under the lock, so the code is expired and no token is issued.
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
