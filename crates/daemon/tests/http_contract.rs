//! Contract fixtures (`docs/design/http-surface.md` §7 "Contract fixtures"): responses
//! captured from a running server decode into the exported types and validate against the
//! generated JSON Schema; every daemon-generated response carries `Cache-Control:
//! no-store`, and hyper's own parser responses carry no `ErrorBody` and no daemon header.
//!
//! PR 7a captures the codes its guard and outer layer decide; the credential, list, and
//! command bodies join with the handlers that produce them (PR 7b, 7c).

mod support;

use std::path::Path;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Response};
use chaldea_core::auth::TokenKind;
use chaldea_core::http::{ErrorBody, ErrorCode};
use chaldea_core::ids::{LaunchId, ThreadId};
use chaldea_daemon::http::{ApiError, Mount, Mounts, ReadCtx, spec};
use serde_json::{Value, json};
use support::http::{Reply, TestServer};

fn validator(name: &str) -> jsonschema::Validator {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/contracts/schema")
        .join(format!("{name}.schema.json"));
    let schema: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    jsonschema::validator_for(&schema).expect("schema compiles")
}

fn check_error_body(r: &Reply, want: ErrorCode, what: &str) {
    let v: Value = serde_json::from_slice(&r.body)
        .unwrap_or_else(|e| panic!("{what}: body is not JSON ({e}): {r:?}"));
    assert!(
        validator("ErrorBody").is_valid(&v),
        "{what}: {v} fails the schema"
    );
    let body: ErrorBody = serde_json::from_value(v).unwrap();
    assert_eq!(body.code, want, "{what}");
    assert_eq!(r.header("content-type"), Some("application/json"), "{what}");
    assert_eq!(r.header("cache-control"), Some("no-store"), "{what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn error_bodies_match_the_contract() {
    let mounts = Mounts::mvp().with(Mount::read(
        spec(&Method::GET, "/").unwrap(),
        |_ctx: ReadCtx, _req| std::future::pending::<Result<Response<Body>, ApiError>>(),
    ));
    let srv = TestServer::start(mounts).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let hook = srv.token(TokenKind::Hook, None, None).await;
    let agent = srv
        .agent(ThreadId::from_u128(1), LaunchId::from_u128(2))
        .await;
    let ticket = format!("chdt_{}", "a".repeat(43));
    let cases = [
        (
            srv.req("GET", "/api/threads").host(Some("evil:1")),
            ErrorCode::MisdirectedRequest,
        ),
        (
            srv.req("GET", "/api/threads")
                .header("Origin", "http://evil"),
            ErrorCode::OriginRejected,
        ),
        (srv.req("GET", "/api/threads"), ErrorCode::Unauthorized),
        (
            srv.req("GET", "/api/doctor").bearer(&agent),
            ErrorCode::Forbidden,
        ),
        (srv.req("GET", "/nowhere"), ErrorCode::NotFound),
        (
            srv.req("DELETE", "/api/threads"),
            ErrorCode::MethodNotAllowed,
        ),
        (srv.req("GET", "/"), ErrorCode::RequestTimeout),
        (
            srv.req("POST", "/hook").bearer(&hook).no_length(),
            ErrorCode::LengthRequired,
        ),
        (
            srv.req("POST", "/hook")
                .bearer(&hook)
                .no_length()
                .header("Content-Length", "5000000"),
            ErrorCode::PayloadTooLarge,
        ),
        (
            srv.req("GET", "/api/threads")
                .bearer(&owner)
                .header("Content-Length", "1")
                .body(b"x"),
            ErrorCode::BadRequest,
        ),
        (
            srv.req("GET", &format!("/api/ws?ticket={ticket}&x=1")),
            ErrorCode::BadQuery,
        ),
        (
            srv.req("GET", "/api/threads").bearer(&owner),
            ErrorCode::NotImplemented,
        ),
    ];
    for (req, code) in cases {
        let r = srv.send(&req).await;
        check_error_body(&r, code, &format!("{code:?}"));
    }
    // 401 carries its challenge; 405 its `Allow`; 503 `shutting_down` once the store closed.
    let r = srv.send(&srv.req("GET", "/api/threads")).await;
    assert_eq!(r.header("www-authenticate"), Some("Bearer"));
    let mut srv = srv;
    srv.store.shutdown();
    let r = srv
        .send(&srv.req("GET", "/api/threads").bearer(&owner))
        .await;
    assert_eq!(r.status, 503);
    check_error_body(&r, ErrorCode::ShuttingDown, "store closed");
    srv.shutdown_http(std::time::Instant::now() + Duration::from_secs(10))
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_error_bodies() {
    let srv = TestServer::start(Mounts::mvp()).await;
    // hyper's own replies: no ErrorBody, no `Cache-Control`.
    for (what, req, status) in [
        (
            "malformed Content-Length",
            srv.req("POST", "/hook")
                .no_length()
                .header("Content-Length", "+5")
                .body(b"hello"),
            400,
        ),
        (
            "oversized head",
            srv.req("GET", "/").header("X-Big", &"a".repeat(9 * 1024)),
            431,
        ),
    ] {
        let r = srv.send(&req).await;
        assert_eq!(r.status, status, "{what}");
        assert!(
            serde_json::from_slice::<ErrorBody>(&r.body).is_err(),
            "{what}: {r:?}"
        );
        assert!(r.headers_named("cache-control").is_empty(), "{what}: {r:?}");
    }
    // The daemon's empty `HEAD` 405: no body, but the daemon's headers.
    let r = srv.send(&srv.req("HEAD", "/api/threads")).await;
    assert_eq!(r.status, 405);
    assert!(r.body.is_empty());
    assert_eq!(r.header("cache-control"), Some("no-store"));
    srv.stop().await;
}

#[test]
fn command_request_args_must_be_an_object() {
    let v = validator("CommandRequest");
    let id = "00000000-0000-0000-0000-000000000001";
    assert!(v.is_valid(&json!({ "command_id": id, "args": { "a": 1 } })));
    for args in [json!([1]), json!("x"), json!(null), json!(1)] {
        assert!(
            !v.is_valid(&json!({ "command_id": id, "args": args })),
            "{args} accepted"
        );
    }
}
