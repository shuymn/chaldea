//! Secrets in logs (`docs/design/http-surface.md` PR7-CL-24): a capturing subscriber at
//! `TRACE` sees one record per request with its route template and status, and no secret,
//! query string, or `/hook` or `/api/pair` body.

mod support;

use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

use chaldea_core::auth::TokenKind;
use chaldea_daemon::http::Mounts;
use support::http::TestServer;

/// Everything every test of this binary logged.
fn captured() -> Arc<Mutex<Vec<u8>>> {
    static LOG: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOG.get_or_init(|| {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || Writer(sink.clone()))
            .finish();
        tracing::subscriber::set_global_default(subscriber).unwrap();
        log
    })
    .clone()
}

struct Writer(Arc<Mutex<Vec<u8>>>);

impl Write for Writer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The tests of this binary share one global subscriber; they run one at a time so each
/// reads only its own records.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// What was logged since `from` bytes.
fn since(log: &Arc<Mutex<Vec<u8>>>, from: usize) -> String {
    String::from_utf8_lossy(&log.lock().unwrap()[from..]).into_owned()
}

fn len(log: &Arc<Mutex<Vec<u8>>>) -> usize {
    log.lock().unwrap().len()
}

/// The completion records in `log`.
fn records(log: &str) -> usize {
    log.lines()
        .filter(|l| l.contains("request finished"))
        .count()
}

/// A line that records a finished request for `route` with `status`.
fn has_record(log: &str, route: &str, status: u16) -> bool {
    log.lines().any(|l| {
        l.contains("request finished")
            && l.contains(&format!("route={route}"))
            && l.contains(&format!("status={status}"))
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secrets_absent() {
    let _serial = SERIAL.lock().await;
    let log = captured();
    let from = len(&log);
    let srv = TestServer::start(Mounts::mvp()).await;
    let p = srv.port;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let hook = srv.token(TokenKind::Hook, None, None).await;
    let origin = format!("http://127.0.0.1:{p}");

    // Positive control first: one record per request, with its route and status.
    srv.send(&srv.req("GET", "/api/threads").bearer(&owner))
        .await;
    srv.send(&srv.req("GET", "/api/ws")).await;
    srv.send(
        &srv.req("POST", "/api/pair")
            .header("Origin", &origin)
            .body(b"{}"),
    )
    .await;
    srv.send(&srv.req("POST", "/hook").bearer(&hook).body(b"{}"))
        .await;
    srv.send(&srv.req("GET", "/nowhere")).await;
    let before = since(&log, from);
    for (route, status) in [
        ("/api/threads", 200),
        ("/api/ws", 401),
        ("/api/pair", 401),
        ("/hook", 501),
        ("unmatched", 404),
    ] {
        assert!(
            has_record(&before, route, status),
            "no record of {route} {status}:\n{before}"
        );
    }
    assert!(before.contains("token_id="), "the token id is recorded");
    assert_eq!(records(&before), 5, "one record per request:\n{before}");

    // Secrets and markers, in accepted and rejected requests alike.
    let ticket = format!("chdt_{}", "T".repeat(43));
    let code = format!("chdp_{}", "P".repeat(43));
    let unknown = format!("chd_{}", "U".repeat(43));
    let method = format!("chd_{}", "M".repeat(43));
    let query_marker = "QUERYMARKER7a";
    let body_marker = "BODYMARKER7a";
    let reqs = [
        srv.req("GET", "/api/threads").bearer(&owner),
        srv.req("GET", "/api/threads").bearer(&unknown),
        srv.req("GET", &format!("/api/threads?{query_marker}=1"))
            .bearer(&owner),
        srv.req("GET", &format!("/api/ws?ticket={ticket}")),
        srv.req("GET", &format!("/api/ws?ticket={ticket}&{query_marker}=1")),
        srv.req("POST", "/api/pair")
            .header("Origin", &origin)
            .body(format!(r#"{{"code":"{code}","m":"{body_marker}"}}"#).as_bytes()),
        srv.req("POST", "/hook")
            .bearer(&hook)
            .body(format!(r#"{{"m":"{body_marker}"}}"#).as_bytes()),
        srv.req("POST", "/hook")
            .bearer(&owner)
            .body(format!(r#"{{"m":"{body_marker}"}}"#).as_bytes()),
        srv.req("GET", &format!("/nowhere/{query_marker}?t={ticket}"))
            .bearer(&owner),
        srv.req("GET", "/api/threads")
            .host(Some("evil:1"))
            .bearer(&owner),
    ];
    for r in &reqs {
        srv.send(r).await;
    }
    // An extension method shaped like a token: 405, and the method is not recorded.
    let r = srv.send(&srv.req(&method, "/api/threads")).await;
    assert_eq!(r.status, 405);
    srv.stop().await;
    let all = since(&log, from);
    assert_eq!(
        records(&all),
        5 + reqs.len() + 1,
        "one record per request:\n{all}"
    );
    for secret in [
        owner.as_str(),
        hook.as_str(),
        unknown.as_str(),
        method.as_str(),
        ticket.as_str(),
        code.as_str(),
        query_marker,
        body_marker,
    ] {
        assert!(!all.contains(secret), "{secret} appears in the log:\n{all}");
    }
    for prefix in ["chd_", "chdt_", "chdp_"] {
        assert!(
            !all.contains(prefix),
            "a {prefix} secret appears in the log:\n{all}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn span_is_outermost() {
    let _serial = SERIAL.lock().await;
    let log = captured();
    let from = len(&log);
    let srv = TestServer::start(Mounts::mvp()).await;
    srv.send(&srv.req("GET", "/api/threads").host(Some("evil:1")))
        .await;
    srv.send(&srv.req("GET", "/span-outermost-404")).await;
    srv.stop().await;
    let all = since(&log, from);
    assert!(
        has_record(&all, "unmatched", 421),
        "no record of the 421:\n{all}"
    );
    assert!(
        has_record(&all, "unmatched", 404),
        "no record of the 404:\n{all}"
    );
    assert_eq!(records(&all), 2, "one record per request:\n{all}");
}

/// Credential headers reach every layer already marked sensitive, so a `Debug` of the
/// headers prints `Sensitive` instead of the value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credential_headers_are_marked_sensitive() {
    let _serial = SERIAL.lock().await;
    use axum::body::Body;
    use axum::http::{Method, Response, header};
    use chaldea_daemon::http::{Mount, ReadCtx, spec};

    let seen = Arc::new(Mutex::new(None));
    let s = seen.clone();
    let mounts = Mounts::mvp().with(Mount::read(
        spec(&Method::GET, "/").unwrap(),
        move |_ctx: ReadCtx, req: axum::http::Request<Body>| {
            let h = req.headers();
            let marked = [
                header::AUTHORIZATION,
                header::COOKIE,
                header::SEC_WEBSOCKET_KEY,
            ]
            .map(|n| h.get(n).is_some_and(axum::http::HeaderValue::is_sensitive));
            *s.lock().unwrap() = Some((marked, format!("{h:?}")));
            async { Ok(Response::new(Body::empty())) }
        },
    ));
    let srv = TestServer::start(mounts).await;
    let secret = format!("chd_{}", "S".repeat(43));
    let r = srv
        .send(
            &srv.req("GET", "/")
                .bearer(&secret)
                .header("Cookie", "c=COOKIEMARK")
                .header("Sec-WebSocket-Key", "KEYMARK"),
        )
        .await;
    assert_eq!(r.status, 200);
    let (marked, debug) = seen.lock().unwrap().take().unwrap();
    assert_eq!(marked, [true; 3]);
    for m in [secret.as_str(), "COOKIEMARK", "KEYMARK"] {
        assert!(!debug.contains(m), "{m} in {debug}");
    }
    srv.stop().await;
}
