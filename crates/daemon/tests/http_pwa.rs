//! The PWA mount (docs/design/w1-pwa.md §2.1, W1-9): `pwa_mount` serves a fixture table
//! through the production mount function, `pwa_absent_is_501` passes it the empty table a
//! Rust-only build embeds. Both run in every build, whatever `CHALDEA_WEB_DIST` embedded.

mod support;

use axum::body::Body;
use axum::http::{Method, Response};
use chaldea_core::auth::TokenKind;
use chaldea_daemon::http::{Asset, Mount, Mounts, ReadCtx, mount_pwa, spec};
use support::http::{Reply, TestServer};

const INDEX: &[u8] = b"<!doctype html><script type=module src=/assets/app-1.js></script>";
const JS: &[u8] = b"export {};";

static FIXTURE: &[Asset] = &[
    Asset {
        path: "/",
        content_type: "text/html; charset=utf-8",
        bytes: INDEX,
    },
    Asset {
        path: "/assets/app-1.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: JS,
    },
    Asset {
        path: "/assets/app-1.css",
        content_type: "text/css; charset=utf-8",
        bytes: b"body{}",
    },
    Asset {
        path: "/assets/vt-1.wasm",
        content_type: "application/wasm",
        bytes: b"\0asm",
    },
];

fn csp(host: &str) -> String {
    format!(
        "default-src 'self'; connect-src 'self' ws://{host}; object-src 'none'; \
         base-uri 'none'; frame-ancestors 'none'; form-action 'none'; \
         require-trusted-types-for 'script'; trusted-types 'none'"
    )
}

/// The headers every response of the PWA routes carries.
fn assert_resource(r: &Reply, what: &str) {
    assert_eq!(
        r.header("x-content-type-options"),
        Some("nosniff"),
        "{what}"
    );
    assert_eq!(
        r.header("cross-origin-resource-policy"),
        Some("same-origin"),
        "{what}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pwa_mount() {
    // A non-PWA handler that sets its own caching: the guard overwrites it.
    let mounts = mount_pwa(Mounts::mvp(), FIXTURE).with(Mount::read(
        spec(&Method::GET, "/api/threads").unwrap(),
        |_ctx: ReadCtx, _req| async {
            Ok(Response::builder()
                .header("Cache-Control", "public, max-age=60")
                .body(Body::from("x"))
                .unwrap())
        },
    ));
    let srv = TestServer::start(mounts).await;
    let port = srv.port;

    // `/`: the page, its policy for the lowercased `Host`, and no caching.
    for host in [format!("127.0.0.1:{port}"), format!("LocalHost:{port}")] {
        let r = srv.send(&srv.req("GET", "/").host(Some(&host))).await;
        assert_eq!(r.status, 200, "{host}");
        assert_eq!(r.body, INDEX);
        assert_eq!(r.header("content-type"), Some("text/html; charset=utf-8"));
        assert_eq!(
            r.header("content-security-policy"),
            Some(csp(&host.to_ascii_lowercase()).as_str())
        );
        assert_eq!(r.header("x-frame-options"), Some("DENY"));
        assert_eq!(r.header("referrer-policy"), Some("no-referrer"));
        assert_eq!(r.header("cross-origin-opener-policy"), Some("same-origin"));
        assert_eq!(r.header("cache-control"), Some("no-store"));
        assert_resource(&r, "/");
    }
    // A query string does not change the lookup.
    let r = srv.send(&srv.req("GET", "/?pair=x")).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body, INDEX);

    // Hashed assets: exact bytes and type, immutable, no page policy.
    for a in &FIXTURE[1..] {
        let r = srv.send(&srv.req("GET", a.path)).await;
        assert_eq!(r.status, 200, "{}", a.path);
        assert_eq!(r.body, a.bytes, "{}", a.path);
        assert_eq!(r.header("content-type"), Some(a.content_type), "{}", a.path);
        assert_eq!(
            r.header("cache-control"),
            Some("max-age=31536000, immutable"),
            "{}",
            a.path
        );
        assert_eq!(r.header("content-security-policy"), None, "{}", a.path);
        assert_resource(&r, a.path);
    }

    // Anything else under the routes is the 404 `ErrorBody`, uncached.
    for path in [
        "/assets/missing.js",
        "/assets/app-1.js/",
        "/assets/APP-1.js",
        "/assets/app-1%2ejs",
        "/assets/./app-1.js",
        "/assets/index.html",
    ] {
        let r = srv.send(&srv.req("GET", path)).await;
        assert_eq!(r.status, 404, "{path}");
        assert_eq!(r.code().as_deref(), Some("not_found"), "{path}");
        assert_eq!(r.header("content-type"), Some("application/json"), "{path}");
        assert_eq!(r.header("cache-control"), Some("no-store"), "{path}");
        assert_resource(&r, path);
    }

    // A non-PWA route's own caching is overwritten.
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let r = srv
        .send(&srv.req("GET", "/api/threads").bearer(&owner))
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("cache-control"), Some("no-store"));
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pwa_absent_is_501() {
    let srv = TestServer::start(mount_pwa(Mounts::mvp(), &[])).await;
    for path in ["/", "/assets/app-1.js"] {
        let r = srv.send(&srv.req("GET", path)).await;
        assert_eq!(r.status, 501, "{path}");
        assert_eq!(r.code().as_deref(), Some("not_implemented"), "{path}");
        assert_eq!(r.header("cache-control"), Some("no-store"), "{path}");
    }
    srv.stop().await;
}
