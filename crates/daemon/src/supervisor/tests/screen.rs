//! `GET /api/threads/{thread_id}/screen` (C1 D1; C1-11, CL-15): the token matrix and the
//! thread read check, the typed snapshot outcomes, and the body.

use std::time::Duration;

use chaldea_core::ids::ThreadId;
use chaldea_core::sync::ScreenResponse;
use serde_json::json;

use super::support::http::Reply;
use super::{Pause, Rig, json as body, support};
use crate::store::TokenKind;

fn path(t: impl std::fmt::Display) -> String {
    format!("/api/threads/{t}/screen")
}

async fn get(rig: &Rig, path: &str, token: Option<&str>) -> Reply {
    let mut req = rig.srv.req("GET", path);
    if let Some(tok) = token {
        req = req.bearer(tok);
    }
    rig.srv.send(&req).await
}

async fn screen_of(rig: &Rig, t: ThreadId) -> ScreenResponse {
    let r = get(rig, &path(t), Some(&rig.owner)).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    serde_json::from_slice(&r.body).unwrap()
}

/// Polls the screen (10 s at most) until `cond` holds of it.
async fn screen_until(
    rig: &Rig,
    t: ThreadId,
    cond: impl Fn(&ScreenResponse) -> bool,
) -> ScreenResponse {
    let end = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let s = screen_of(rig, t).await;
        if cond(&s) {
            return s;
        }
        assert!(
            std::time::Instant::now() < end,
            "the screen never showed it"
        );
        tokio::task::yield_now().await;
    }
}

/// A case of [`token_matrix`]: its name, token, path, status, and code.
type Case<'a> = (&'a str, Option<&'a str>, String, u16, Option<&'a str>);

/// CL-15: owner and rescue read every thread, an agent its self+ only (another thread is
/// the same 404 as a missing one), hook and extension tokens are refused, and no token is
/// 401. Unknown and malformed ids are 404, a query is 400 `bad_query`, and every answer
/// carries `Cache-Control: no-store`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn token_matrix() {
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    let (other, other_l) = rig.spawn_codex(None).await;
    let child = ThreadId::from_u128(0xC1);
    rig.srv
        .append(vec![support::registered_under(child, Some(t))])
        .await;
    let agent = rig.srv.token(TokenKind::Agent, Some(t), Some(l)).await;
    let other_agent = rig
        .srv
        .token(TokenKind::Agent, Some(other), Some(other_l))
        .await;
    let rescue = rig.srv.token(TokenKind::Rescue, None, None).await;
    let hook = rig.srv.token(TokenKind::Hook, Some(t), Some(l)).await;
    let ext = rig.srv.token(TokenKind::Extension, None, None).await;
    let cases: [Case<'_>; 13] = [
        ("owner", Some(&rig.owner), path(t), 200, None),
        ("rescue", Some(&rescue), path(t), 200, None),
        ("agent self", Some(&agent), path(t), 200, None),
        // A descendant passes the read check; it has no launch.
        (
            "agent child",
            Some(&agent),
            path(child),
            409,
            Some("no_live_launch"),
        ),
        (
            "agent other",
            Some(&other_agent),
            path(t),
            404,
            Some("not_found"),
        ),
        ("hook", Some(&hook), path(t), 403, Some("forbidden")),
        ("extension", Some(&ext), path(t), 403, Some("forbidden")),
        ("none", None, path(t), 401, Some("unauthorized")),
        (
            "unknown",
            Some(&rig.owner),
            path(ThreadId::from_u128(0xDEAD)),
            404,
            Some("not_found"),
        ),
        (
            "malformed",
            Some(&rig.owner),
            path("nope"),
            404,
            Some("not_found"),
        ),
        (
            "not canonical",
            Some(&rig.owner),
            path(t.to_string().to_uppercase()),
            404,
            Some("not_found"),
        ),
        (
            "query",
            Some(&rig.owner),
            format!("{}?x=1", path(t)),
            400,
            Some("bad_query"),
        ),
        (
            "empty query",
            Some(&rig.owner),
            format!("{}?", path(t)),
            200,
            None,
        ),
    ];
    for (name, token, p, status, code) in cases {
        let r = get(&rig, &p, token).await;
        assert_eq!(
            (r.status, r.code().as_deref()),
            (status, code),
            "{name}: {}",
            String::from_utf8_lossy(&r.body)
        );
        assert_eq!(r.header("cache-control"), Some("no-store"), "{name}");
    }
    rig.srv.stop().await;
}

/// C1-11: a thread whose launch was never started, one not yet published, and one that
/// exited are 409 `no_live_launch`; the op lock held is 503 `busy` with `retry_after`, at
/// once; a failed VT is 500 `internal`. No answer appends an event.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typed_outcomes() {
    let rig = Rig::new().await;
    let no_live = (409, Some("no_live_launch".to_owned()));
    // Never spawned.
    let bare = ThreadId::from_u128(0xB1);
    rig.srv.append(vec![support::registered(bare, false)]).await;
    assert_eq!(
        get(&rig, &path(bare), Some(&rig.owner)).await.outcome(),
        no_live
    );

    // Reserved, not yet published: the live entry has no session.
    let armed = rig.pauses.arm(Pause::AfterReserve);
    let cwd = rig.cwd.path().to_string_lossy().into_owned();
    let spawn = {
        let req = rig.srv.command(
            "spawn",
            &rig.owner,
            super::cid(),
            &json!({ "agent": "codex", "cwd": cwd }).to_string(),
        );
        let port = rig.srv.port;
        tokio::spawn(async move { support::http::send(port, &req.bytes()).await })
    };
    let reached = armed.reached;
    tokio::task::spawn_blocking(move || reached.recv_timeout(Duration::from_secs(20)))
        .await
        .unwrap()
        .unwrap();
    let pending: ThreadId = rig
        .srv
        .store
        .read(|r| Ok(r.thread_infos()?.last().map(|t| t.thread_id)))
        .unwrap();
    assert_eq!(
        get(&rig, &path(pending), Some(&rig.owner)).await.outcome(),
        no_live
    );
    armed.release.send(()).unwrap();
    assert_eq!(spawn.await.unwrap().status, 200);

    // Live: the op lock held by another operation.
    let (t, l) = rig.spawn_codex(None).await;
    let entry = rig.sup.entry(l).unwrap();
    let before = rig.events(t).len();
    let g = entry.ops.clone().lock_owned().await;
    let r = get(&rig, &path(t), Some(&rig.owner)).await;
    assert_eq!(r.outcome(), (503, Some("busy".to_owned())));
    assert_eq!(body(&r)["retry_after"], json!(1));
    drop(g);
    screen_of(&rig, t).await;

    // The VT failed.
    entry
        .with_session(&rig.sup, rig.sup.now() + Duration::from_secs(5), |s| {
            s.fail_vt_on_next_piece();
        })
        .await
        .unwrap();
    let child = rig.pty.take_child().unwrap();
    child.emit(b"x");
    let end = std::time::Instant::now() + Duration::from_secs(10);
    let mut failed = false;
    while !failed {
        assert!(std::time::Instant::now() < end, "the VT did not fail");
        tokio::task::yield_now().await;
        failed = entry
            .with_session(&rig.sup, rig.sup.now() + Duration::from_secs(5), |s| {
                s.status().vt_failed
            })
            .await
            .unwrap();
    }
    assert!(failed, "the VT failed");
    assert_eq!(
        get(&rig, &path(t), Some(&rig.owner)).await.outcome(),
        (500, Some("internal".to_owned()))
    );
    assert_eq!(
        rig.events(t).len(),
        before,
        "the screen route appends nothing"
    );

    // Exited.
    let r = rig.cmd("stop", &json!({ "thread_id": t })).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(
        get(&rig, &path(t), Some(&rig.owner)).await.outcome(),
        no_live
    );
    rig.srv.stop().await;
}

/// C1-11: one text line per row, wide tails dropped, combining marks kept, empty rows,
/// trailing spaces trimmed, and the cursor in cell columns after wide characters.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn screen_body() {
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    let child = rig.pty.take_child().unwrap();
    child.emit("e\u{301}\r\n\r\nab  \r\n日本語".as_bytes());
    let s = screen_until(&rig, t, |s| s.lines.get(3).is_some_and(|x| x == "日本語")).await;
    assert_eq!((s.thread_id, s.launch_id), (t, l));
    assert_eq!((s.cols, s.rows), (80, 24));
    assert_eq!(s.lines.len(), 24);
    assert_eq!(&s.lines[..4], ["e\u{301}", "", "ab", "日本語"]);
    assert!(s.lines[4..].iter().all(String::is_empty));
    assert_eq!((s.cursor.row, s.cursor.col), (3, 6));
    assert!(!s.truncated);
    rig.srv.stop().await;
}

/// C1-11: a launch whose exit is recorded has no screen, even while its session is still in
/// its slot (the exit job runs before the close releases it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exit_recorded() {
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    screen_of(&rig, t).await;
    rig.srv.append(vec![support::exited(t, l)]).await;
    assert!(rig.sup.entry(l).is_some(), "still live");
    assert_eq!(
        get(&rig, &path(t), Some(&rig.owner)).await.outcome(),
        (409, Some("no_live_launch".to_owned()))
    );
    rig.srv.stop().await;
}
