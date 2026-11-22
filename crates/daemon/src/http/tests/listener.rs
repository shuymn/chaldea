//! The accept loop's error rule (design §2.2, H13; PR7-CL-28 `accept_error_continues`).

use std::io::ErrorKind;

use super::support::http::TestServer;
use super::{eventually, shared};
use crate::http::Mounts;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accept_error_continues() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let s = shared(&srv);
    // The loop is already waiting in `accept`: the injected errors are taken from the next
    // iteration on, after the first connection.
    s.faults.inner.arm_accept_errors(&[
        ErrorKind::ConnectionReset,
        ErrorKind::ConnectionAborted,
        ErrorKind::Other,
    ]);
    for _ in 0..3 {
        let r = srv.send(&srv.req("GET", "/nowhere")).await;
        assert_eq!(r.status, 404);
    }
    assert!(
        s.faults.inner.accept_errors.lock().unwrap().is_empty(),
        "the injected errors were not all taken"
    );
    // The accept task is still registered once the connections have ended.
    let server = &srv.server;
    eventually(
        std::time::Duration::from_secs(10),
        "connections end",
        || server.tasks() == 1,
    )
    .await;
    srv.stop().await;
}
