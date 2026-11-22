//! Server lifecycle (`docs/design/http-surface.md` PR7-CL-28): shutdown closes the
//! registry, stops accepting, releases the listener, and returns by its deadline with every
//! task counted as exited or abort-requested.

mod support;

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Method, Response};
use chaldea_daemon::http::{ApiError, KindCounts, Mount, Mounts, WriteCtx, spec};
use support::http::{TestServer, connect};
use tokio::io::AsyncWriteExt;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listener_released_with_no_connections() {
    let mut srv = TestServer::start(Mounts::mvp()).await;
    let port = srv.port;
    assert_eq!(srv.send(&srv.req("GET", "/nowhere")).await.status, 404);
    let report = srv
        .shutdown_http(Instant::now() + Duration::from_secs(10))
        .await;
    assert_eq!(report.exited.accept, 1, "{report:?}");
    assert_eq!(report.abort_requested, KindCounts::default(), "{report:?}");
    // The accept task exited, so the listener is closed: the port can be bound again.
    let again = std::net::TcpListener::bind(("127.0.0.1", port));
    assert!(again.is_ok(), "port {port} still bound: {again:?}");
    srv.store.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_stops_accepting() {
    let mut srv = TestServer::start(Mounts::mvp()).await;
    let port = srv.port;
    srv.shutdown_http(Instant::now() + Duration::from_secs(10))
        .await;
    match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
        Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::ConnectionRefused, "{e}"),
        Ok(mut s) => {
            // Something else bound the port meanwhile: it must not be this server.
            let _ = s.write_all(&srv.req("GET", "/").bytes()).await;
            let (out, _) = support::http::read_to_end(&mut s).await;
            assert!(
                !String::from_utf8_lossy(&out).contains("not_found"),
                "a closed server answered"
            );
        }
    }
    srv.store.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_returns_by_its_deadline() {
    // A write handler that never finishes (write mounts are not dropped at the deadline).
    let (entered_tx, mut entered) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mounts = Mounts::mvp().with(Mount::write(
        spec(&Method::POST, "/hook").unwrap(),
        move |_ctx: WriteCtx, _req| {
            let _ = entered_tx.send(());
            std::future::pending::<Result<Response<Body>, ApiError>>()
        },
    ));
    let mut srv = TestServer::start(mounts).await;
    let hook = srv
        .token(chaldea_core::auth::TokenKind::Hook, None, None)
        .await;
    let mut held = connect(srv.port).await;
    held.write_all(&srv.req("POST", "/hook").bearer(&hook).body(b"{}").bytes())
        .await
        .unwrap();
    // An idle connection, with no request at all.
    let _idle = connect(srv.port).await;
    // Wait until the held request is in its handler and the idle connection is registered.
    entered.recv().await.unwrap();
    let t0 = Instant::now();
    while srv.server.tasks() < 3 {
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "connections not registered"
        );
        tokio::task::yield_now().await;
    }
    let deadline = Instant::now() + Duration::from_millis(500);
    let report = srv.shutdown_http(deadline).await;
    let late = Instant::now().saturating_duration_since(deadline);
    assert!(
        late < Duration::from_millis(500),
        "shutdown returned {late:?} after its deadline"
    );
    assert_eq!(report.exited.accept, 1, "{report:?}");
    // The idle connection ends at the signal; the held one gets an abort request.
    assert_eq!(report.abort_requested.connection, 1, "{report:?}");
    assert_eq!(report.exited.connection, 1, "{report:?}");
    srv.store.shutdown();
}
