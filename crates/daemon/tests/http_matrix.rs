//! Route and action matrices against an independent literal inventory
//! (`docs/design/http-surface.md` PR7-CL-2, CL-4, CL-22's route cases).
//!
//! The inventory below is written from mvp §2.5 and §2.4, not from `ROUTES`; one test
//! asserts it equals `ROUTES`, the others probe the production router (`Mounts::mvp()`).

mod support;

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;

use axum::body::Body;
use axum::http::{Method, Response};
use chaldea_core::auth::{Action, MVP_COMMANDS, TokenKind, grant};
use chaldea_core::http::WsTicketResponse;
use chaldea_core::ids::ThreadId;
use chaldea_daemon::http::{
    ActionRule, Credential, HttpConfig, HttpServer, Mount, MountKind, Mounts, OriginRule, ROUTES,
    ReadCtx, RouteSpec, TicketPurposeKind, spec,
};
use futures_util::StreamExt;
use support::http::{Req, TestServer};

use MountKind::{Read, Upgrade, Write};
use OriginRule::{AbsentOrMatching, MustBeAbsent, NotChecked, PresentMatching};
use TokenKind::{Agent, Extension, Hook, Owner, Rescue};
use support::waits;

/// How a route is authenticated, as the inventory states it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cred {
    None,
    Bearer(&'static [TokenKind]),
    TicketEvents,
    TicketAttach,
    PairCode,
}

#[derive(Clone, Copy)]
struct Row {
    method: &'static str,
    path: &'static str,
    /// A concrete path to probe.
    probe: &'static str,
    mount: MountKind,
    origin: OriginRule,
    cred: Cred,
    action: Option<Action>,
    commands: bool,
    /// The outcome of the probe request (no query, a `{}` body for `POST`) once the guard
    /// admits it: the handler's answer, or 501 until the route's PR mounts one.
    reached: (u16, Option<&'static str>),
}

const fn row(
    method: &'static str,
    path: &'static str,
    probe: &'static str,
    mount: MountKind,
    origin: OriginRule,
    cred: Cred,
    action: Option<Action>,
) -> Row {
    Row {
        method,
        path,
        probe,
        mount,
        origin,
        cred,
        action,
        commands: false,
        reached: UNIMPLEMENTED,
    }
}

const UNIMPLEMENTED: (u16, Option<&str>) = (501, Some("not_implemented"));

/// `r` with the outcome its mounted handler gives the probe.
const fn reaches(r: Row, status: u16, code: Option<&'static str>) -> Row {
    Row {
        reached: (status, code),
        ..r
    }
}

const READ_KINDS: &[TokenKind] = &[Owner, Agent, Rescue];

/// mvp §2.5 "Routes", written out again.
const INVENTORY: &[Row] = &[
    row("GET", "/", "/", Read, NotChecked, Cred::None, None),
    row(
        "GET",
        "/assets/{*path}",
        "/assets/app.js",
        Read,
        NotChecked,
        Cred::None,
        None,
    ),
    reaches(
        row(
            "POST",
            "/api/pair",
            "/api/pair",
            Write,
            PresentMatching,
            Cred::PairCode,
            None,
        ),
        401,
        Some("unauthorized"),
    ),
    reaches(
        row(
            "POST",
            "/api/pair-codes",
            "/api/pair-codes",
            Read,
            MustBeAbsent,
            Cred::Bearer(&[Owner]),
            None,
        ),
        200,
        None,
    ),
    reaches(
        row(
            "POST",
            "/api/ws-ticket",
            "/api/ws-ticket",
            Read,
            AbsentOrMatching,
            Cred::Bearer(READ_KINDS),
            Some(Action::ReadStream),
        ),
        400,
        Some("bad_request"),
    ),
    row(
        "GET",
        "/api/ws",
        "/api/ws",
        Upgrade,
        AbsentOrMatching,
        Cred::TicketEvents,
        None,
    ),
    row(
        "GET",
        "/api/attach/{thread}",
        "/api/attach/00000000-0000-0000-0000-000000000001",
        Upgrade,
        AbsentOrMatching,
        Cred::TicketAttach,
        Some(Action::Attach),
    ),
    reaches(
        row(
            "GET",
            "/api/events",
            "/api/events",
            Read,
            AbsentOrMatching,
            Cred::Bearer(READ_KINDS),
            Some(Action::ReadStream),
        ),
        400,
        Some("bad_query"),
    ),
    reaches(
        row(
            "GET",
            "/api/blobs/{sha256}",
            "/api/blobs/00",
            Read,
            AbsentOrMatching,
            Cred::Bearer(READ_KINDS),
            Some(Action::ReadBlob),
        ),
        400,
        Some("bad_query"),
    ),
    reaches(
        row(
            "GET",
            "/api/threads",
            "/api/threads",
            Read,
            AbsentOrMatching,
            Cred::Bearer(READ_KINDS),
            Some(Action::ReadThreadList),
        ),
        200,
        None,
    ),
    // A missing thread: the handler's 404.
    reaches(
        row(
            "GET",
            "/api/threads/{thread_id}",
            "/api/threads/00000000-0000-0000-0000-000000000001",
            Read,
            AbsentOrMatching,
            Cred::Bearer(READ_KINDS),
            Some(Action::ReadStream),
        ),
        404,
        Some("not_found"),
    ),
    row(
        "GET",
        "/api/threads/{thread_id}/screen",
        "/api/threads/00000000-0000-0000-0000-000000000001/screen",
        Read,
        AbsentOrMatching,
        Cred::Bearer(READ_KINDS),
        Some(Action::ReadStream),
    ),
    reaches(
        row(
            "GET",
            "/api/worktrees",
            "/api/worktrees",
            Read,
            AbsentOrMatching,
            Cred::Bearer(READ_KINDS),
            Some(Action::ReadWorktrees),
        ),
        200,
        None,
    ),
    row(
        "GET",
        "/api/adapters",
        "/api/adapters",
        Read,
        AbsentOrMatching,
        Cred::Bearer(READ_KINDS),
        Some(Action::ReadAdapters),
    ),
    row(
        "GET",
        "/api/doctor",
        "/api/doctor",
        Read,
        AbsentOrMatching,
        Cred::Bearer(&[Owner, Rescue]),
        Some(Action::ReadDoctor),
    ),
    Row {
        commands: true,
        ..row(
            "POST",
            "/api/commands/{name}",
            "/api/commands/spawn",
            Write,
            AbsentOrMatching,
            Cred::Bearer(&[Owner, Agent]),
            None,
        )
    },
    // Mounted (PR 9): the probe's `{}` without `X-Chaldea-Emit-Id` reaches the handler,
    // which answers 400 before the body.
    reaches(
        row(
            "POST",
            "/hook",
            "/hook",
            Write,
            MustBeAbsent,
            Cred::Bearer(&[Hook]),
            None,
        ),
        400,
        Some("bad_request"),
    ),
    row(
        "POST",
        "/events",
        "/events",
        Write,
        MustBeAbsent,
        Cred::Bearer(&[Owner, Rescue, Extension]),
        Some(Action::PostEvents),
    ),
];

/// mvp §2.4's command names, written out again, with the kinds the action table refuses
/// for agents.
const COMMANDS: &[&str] = &[
    "spawn",
    "resume",
    "send",
    "ready",
    "key",
    "stop",
    "close",
    "input_resolve",
    "input_cancel",
    "task_set",
    "mailbox_send",
    "worktree_remove",
    "doctor_run",
    "surface_action",
];
const AGENT_REFUSED: &[&str] = &[
    "key",
    "ready",
    "input_resolve",
    "doctor_run",
    "surface_action",
];

fn spec_cred(c: Credential) -> Cred {
    match c {
        Credential::None => Cred::None,
        Credential::Bearer(k) => Cred::Bearer(k),
        Credential::Ticket(TicketPurposeKind::Events) => Cred::TicketEvents,
        Credential::Ticket(TicketPurposeKind::Attach) => Cred::TicketAttach,
        Credential::PairCode => Cred::PairCode,
    }
}

fn same(r: &Row, s: &RouteSpec) -> bool {
    let (action, commands) = match s.action {
        ActionRule::None => (None, false),
        ActionRule::Fixed(a) => (Some(a), false),
        ActionRule::ByCommandName => (None, true),
    };
    s.method.as_str() == r.method
        && s.path == r.path
        && s.mount == r.mount
        && s.origin == r.origin
        && spec_cred(s.credential) == r.cred
        && action == r.action
        && commands == r.commands
}

#[test]
fn inventory_equals_routes() {
    for r in INVENTORY {
        let n = ROUTES.iter().filter(|s| same(r, s)).count();
        assert_eq!(n, 1, "{} {}: {n} matching ROUTES entries", r.method, r.path);
    }
    for s in ROUTES {
        assert!(
            INVENTORY.iter().any(|r| same(r, s)),
            "ROUTES has {} {} ({s:?}), which the inventory does not",
            s.method,
            s.path
        );
    }
    assert_eq!(ROUTES.len(), INVENTORY.len());
    let names: Vec<&str> = MVP_COMMANDS.iter().map(|c| c.name).collect();
    assert_eq!(names, COMMANDS);
}

fn code(s: &str) -> Option<String> {
    Some(s.to_owned())
}

/// A request to `r`'s probe path that passes its `Origin` rule.
fn base(srv: &TestServer, r: &Row) -> Req {
    let mut req = srv.req(r.method, r.probe);
    if r.origin == PresentMatching {
        req = req.header("Origin", &format!("http://127.0.0.1:{}", srv.port));
    }
    if r.method == "POST" {
        req = req.body(b"{}");
    }
    req
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inventory_matches_router() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token_of(Owner, 1).await;
    let hook = srv.token_of(Hook, 2).await;
    for r in INVENTORY {
        let req = base(&srv, r);
        let req = match r.cred {
            Cred::Bearer(k) if k.contains(&Hook) => req.bearer(&hook),
            Cred::Bearer(_) => req.bearer(&owner),
            _ => req,
        };
        let got = srv.send(&req).await.outcome();
        let want = match r.cred {
            // No ticket: the credential step refuses (`ticket_kind_matrix` mints them).
            Cred::TicketEvents | Cred::TicketAttach => (401, code("unauthorized")),
            _ => (r.reached.0, r.reached.1.map(str::to_owned)),
        };
        assert_eq!(got, want, "{} {}", r.method, r.probe);
    }
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn methods_are_exact() {
    let srv = TestServer::start(Mounts::mvp()).await;
    for r in INVENTORY {
        let mut others = vec!["PUT", "DELETE", "PATCH", "OPTIONS"];
        others.push(if r.method == "GET" { "POST" } else { "GET" });
        for m in others {
            let mut req = srv.req(m, r.probe);
            if m == "POST" {
                req = req.body(b"{}");
            }
            let rep = srv.send(&req).await;
            assert_eq!(
                rep.outcome(),
                (405, code("method_not_allowed")),
                "{m} {}",
                r.probe
            );
            assert_eq!(rep.header("allow"), Some(r.method), "{m} {}", r.probe);
            assert_eq!(rep.header("cache-control"), Some("no-store"));
        }
        let rep = srv.send(&srv.req("HEAD", r.probe)).await;
        assert_eq!(rep.status, 405, "HEAD {}", r.probe);
        assert_eq!(rep.header("allow"), Some(r.method), "HEAD {}", r.probe);
        assert_eq!(
            rep.header("cache-control"),
            Some("no-store"),
            "HEAD {}",
            r.probe
        );
        assert!(
            rep.body.is_empty() && rep.rest.is_empty(),
            "HEAD {} has a body",
            r.probe
        );
    }
    srv.stop().await;
}

const KINDS: [Option<TokenKind>; 6] = [
    Some(Owner),
    Some(Agent),
    Some(Rescue),
    Some(Hook),
    Some(Extension),
    None,
];

async fn tokens(srv: &TestServer) -> BTreeMap<TokenKind, String> {
    let mut out = BTreeMap::new();
    for (i, k) in TokenKind::ALL.into_iter().enumerate() {
        out.insert(k, srv.token_of(k, i as u128).await);
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_kind_matrix() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let toks = tokens(&srv).await;
    for r in INVENTORY.iter().filter(|r| !r.commands) {
        let Cred::Bearer(kinds) = r.cred else {
            continue;
        };
        for kind in KINDS {
            let mut req = base(&srv, r);
            if let Some(k) = kind {
                req = req.bearer(&toks[&k]);
            }
            let want = match kind {
                None => (401, code("unauthorized")),
                Some(k) if kinds.contains(&k) && r.action.is_none_or(|a| grant(k, a).is_some()) => {
                    (r.reached.0, r.reached.1.map(str::to_owned))
                }
                Some(_) => (403, code("forbidden")),
            };
            assert_eq!(
                srv.send(&req).await.outcome(),
                want,
                "{kind:?} on {} {}",
                r.method,
                r.path
            );
        }
    }
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_matrix() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let toks = tokens(&srv).await;
    for name in COMMANDS
        .iter()
        .copied()
        .chain(["bogus", "Spawn", "spawn%20"])
    {
        let known = COMMANDS.contains(&name);
        for kind in KINDS {
            let mut req = srv
                .req("POST", &format!("/api/commands/{name}"))
                .body(b"{}");
            if let Some(k) = kind {
                req = req.bearer(&toks[&k]);
            }
            let want = match kind {
                None => (401, code("unauthorized")),
                Some(Rescue | Hook | Extension) => (403, code("forbidden")),
                Some(_) if !known => (404, code("not_found")),
                Some(Agent) if AGENT_REFUSED.contains(&name) => (403, code("forbidden")),
                Some(_) => (501, code("not_implemented")),
            };
            assert_eq!(srv.send(&req).await.outcome(), want, "{kind:?} on {name}");
        }
    }
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credential_paths() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let toks = tokens(&srv).await;
    for r in INVENTORY
        .iter()
        .filter(|r| matches!(r.cred, Cred::TicketEvents | Cred::TicketAttach))
    {
        for tok in toks.values() {
            // A Bearer header is no ticket.
            let rep = srv.send(&srv.req("GET", r.probe).bearer(tok)).await;
            assert_eq!(rep.outcome(), (401, code("unauthorized")), "{}", r.probe);
        }
        // A ticket of the wrong form, a repeated ticket, and other parameters.
        let t = format!("chdt_{}", "a".repeat(43));
        for (q, want) in [
            (format!("?ticket={t}"), (401, code("unauthorized"))),
            (
                format!("?ticket={t}&ticket={t}"),
                (401, code("unauthorized")),
            ),
            ("?ticket=x".to_owned(), (401, code("unauthorized"))),
            (format!("?ticket={t}&other=1"), (400, code("bad_query"))),
        ] {
            let rep = srv.send(&srv.req("GET", &format!("{}{q}", r.probe))).await;
            assert_eq!(rep.outcome(), want, "{}{q}", r.probe);
        }
    }
    srv.stop().await;
}

/// The ticket a `POST /api/ws-ticket` with `purpose` returns, or its outcome.
async fn mint(
    srv: &TestServer,
    token: Option<&str>,
    purpose: &str,
) -> Result<String, (u16, Option<String>)> {
    let mut req = srv
        .req("POST", "/api/ws-ticket")
        .body(format!(r#"{{"purpose":"{purpose}"}}"#).as_bytes());
    if let Some(t) = token {
        req = req.bearer(t);
    }
    let r = srv.send(&req).await;
    if r.status != 200 {
        return Err(r.outcome());
    }
    let body: WsTicketResponse = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(body.expires_in, 30);
    Ok(body.ticket)
}

/// A WebSocket handshake for `path` with `ticket`.
fn handshake(srv: &TestServer, path: &str, ticket: &str) -> Req {
    srv.ws_handshake(&format!("{path}?ticket={ticket}"))
}

/// PR 7b's credential cells of PR7-CL-4: a ticket minted with each kind (and none) and
/// redeemed on each ticket route, and `/api/pair` with a Bearer header and no code.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn minted_ticket_matrix() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let toks = tokens(&srv).await;
    let agent_index = TokenKind::ALL.iter().position(|k| *k == Agent).unwrap();
    let thread = ThreadId::from_u128(0xA000 + agent_index as u128);
    let attach = format!("/api/attach/{thread}");
    let other = "/api/attach/00000000-0000-0000-0000-00000000beef";
    for kind in KINDS {
        let tok = kind.map(|k| toks[&k].clone());
        let events = mint(&srv, tok.as_deref(), "events").await;
        let att = mint(&srv, tok.as_deref(), &format!("attach:{thread}")).await;
        match kind {
            None => {
                assert_eq!(events, Err((401, code("unauthorized"))));
                assert_eq!(att, Err((401, code("unauthorized"))));
                continue;
            }
            Some(Hook | Extension) => {
                assert_eq!(events, Err((403, code("forbidden"))), "{kind:?}");
                assert_eq!(att, Err((403, code("forbidden"))), "{kind:?}");
                continue;
            }
            Some(Owner) => assert!(att.is_ok(), "{att:?}"),
            Some(_) => assert_eq!(att, Err((403, code("forbidden"))), "{kind:?}"),
        }
        let events = events.unwrap_or_else(|e| panic!("{kind:?}: {e:?}"));
        // A ticket opens only its own route (and thread).
        let r = srv.send(&handshake(&srv, &attach, &events)).await;
        assert_eq!(r.outcome(), (401, code("unauthorized")), "{kind:?}");
        if let Ok(att) = att {
            let r = srv.send(&handshake(&srv, "/api/ws", &att)).await;
            assert_eq!(r.outcome(), (401, code("unauthorized")));
            let r = srv.send(&handshake(&srv, other, &att)).await;
            assert_eq!(r.outcome(), (401, code("unauthorized")));
            // PR 11 mounts the attach handler; until then the guard admits and answers 501.
            let r = srv.send(&handshake(&srv, &attach, &att)).await;
            assert_eq!(r.outcome(), (501, code("not_implemented")), "attach");
        }
        let url = format!("ws://127.0.0.1:{}/api/ws?ticket={events}", srv.port);
        let (mut ws, resp) = tokio_tungstenite::connect_async(url)
            .await
            .unwrap_or_else(|e| panic!("{kind:?}: {e}"));
        assert_eq!(resp.status(), 101, "{kind:?}");
        let hello = waits::bounded(std::time::Duration::from_secs(10), ws.next())
            .await
            .expect("hello within 10 s")
            .unwrap()
            .unwrap();
        assert!(
            hello.to_text().unwrap().contains(r#""type":"hello""#),
            "{hello:?}"
        );
        ws.close(None).await.unwrap();
    }
    // `/api/pair` takes no Bearer token: without a pair code it is 401.
    let origin = format!("http://127.0.0.1:{}", srv.port);
    for body in ["{}".to_owned(), format!(r#"{{"code":"{}"}}"#, toks[&Owner])] {
        let r = srv
            .send(
                &srv.req("POST", "/api/pair")
                    .header("Origin", &origin)
                    .bearer(&toks[&Owner])
                    .body(body.as_bytes()),
            )
            .await;
        assert_eq!(r.outcome(), (401, code("unauthorized")), "{body}");
    }
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn origin_matrix() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let p = srv.port;
    let ip = format!("127.0.0.1:{p}");
    let lh = format!("localhost:{p}");
    // A port other than the server's, only ever sent in an `Origin` header (nothing binds or
    // connects to it).
    let other = if p == 1 { 2 } else { 1 };
    // (name, Host, Origin values, accepted when the rule is AbsentOrMatching / PresentMatching)
    let variants: Vec<(&str, String, Vec<String>, bool, bool)> = vec![
        ("absent", ip.clone(), vec![], true, false),
        (
            "matching",
            ip.clone(),
            vec![format!("http://{ip}")],
            true,
            true,
        ),
        (
            "matching localhost",
            lh.clone(),
            vec![format!("http://{lh}")],
            true,
            true,
        ),
        (
            "localhost for 127.0.0.1",
            ip.clone(),
            vec![format!("http://{lh}")],
            false,
            false,
        ),
        (
            "other port",
            ip.clone(),
            vec![format!("http://127.0.0.1:{other}")],
            false,
            false,
        ),
        ("null", ip.clone(), vec!["null".to_owned()], false, false),
        (
            "https",
            ip.clone(),
            vec![format!("https://{ip}")],
            false,
            false,
        ),
        (
            "trailing slash",
            ip.clone(),
            vec![format!("http://{ip}/")],
            false,
            false,
        ),
        (
            "duplicate",
            ip.clone(),
            vec![format!("http://{ip}"), format!("http://{ip}")],
            false,
            false,
        ),
    ];
    for r in INVENTORY {
        for (name, host, origins, absent_or_matching, present_matching) in &variants {
            let mut req = srv.req(r.method, r.probe).host(Some(host));
            for o in origins {
                req = req.header("Origin", o);
            }
            if r.method == "POST" {
                req = req.body(b"{}");
            }
            let accepted = match r.origin {
                NotChecked => true,
                AbsentOrMatching => *absent_or_matching,
                PresentMatching => *present_matching,
                MustBeAbsent => origins.is_empty(),
            };
            let rep = srv.send(&req).await;
            let rejected = rep.outcome() == (403, code("origin_rejected"));
            assert_eq!(
                !rejected,
                accepted,
                "{name} on {} {}: {:?}",
                r.method,
                r.probe,
                rep.outcome()
            );
        }
    }
    srv.stop().await;
}

fn ok_handler() -> impl Fn(
    ReadCtx,
    axum::http::Request<Body>,
) -> std::future::Ready<Result<Response<Body>, chaldea_daemon::http::ApiError>>
+ Send
+ Sync
+ 'static {
    |_ctx, _req| std::future::ready(Ok(Response::new(Body::empty())))
}

#[test]
fn build_router_rejects_inconsistent_mounts() {
    let store = support::TestStore::new();
    let new = |m: Mounts| {
        let s = store.store().clone();
        std::panic::catch_unwind(AssertUnwindSafe(move || {
            let sink: std::sync::Arc<dyn chaldea_daemon::http::LifecycleSink> =
                std::sync::Arc::new(support::http::RecordingSink::default());
            drop(HttpServer::new(
                HttpConfig::new(1),
                s,
                chaldea_daemon::http::CommandRegistry::mvp(),
                sink,
                m,
            ));
        }))
    };
    let hook = spec(&Method::POST, "/hook").unwrap();
    let root = spec(&Method::GET, "/").unwrap();
    // A write route mounted as a read route.
    assert!(std::panic::catch_unwind(|| Mount::read(hook, ok_handler())).is_err());
    // A read route mounted as a write route, and an upgrade route as a read route.
    assert!(
        std::panic::catch_unwind(|| Mount::write(root, |_c, _r| std::future::ready(Ok(
            Response::new(Body::empty())
        ))))
        .is_err()
    );
    let ws = spec(&Method::GET, "/api/ws").unwrap();
    assert!(std::panic::catch_unwind(|| Mount::read(ws, ok_handler())).is_err());
    // A route without a mount.
    let mut missing = Mounts::empty();
    for s in ROUTES.iter().filter(|s| !std::ptr::eq(*s, root)) {
        missing = missing.with(Mount::unimplemented(s));
    }
    assert!(new(missing).is_err());
    // A route mounted twice.
    assert!(new(Mounts::mvp().push(Mount::read(root, ok_handler()))).is_err());
    // A mount whose spec is not in ROUTES (an equal copy is not the table's entry).
    let stray: &'static RouteSpec = Box::leak(Box::new(RouteSpec {
        method: Method::GET,
        path: "/",
        mount: Read,
        origin: NotChecked,
        credential: Credential::None,
        action: ActionRule::None,
        body: chaldea_daemon::http::BodyRule::NoBody,
        owner_pr: 18,
    }));
    assert!(new(Mounts::mvp().push(Mount::read(stray, ok_handler()))).is_err());
    // The consistent set builds.
    assert!(new(Mounts::mvp().with(Mount::read(root, ok_handler()))).is_ok());
}
