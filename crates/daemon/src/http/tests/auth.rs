//! Object checks on stored objects and the 404/403 choice (design §2.5; PR7-CL-27).

use chaldea_core::auth::{ThreadGraph, TokenKind};
use chaldea_core::event::TypedEvent;
use chaldea_core::ids::{StreamId, SurfaceId, ThreadId, TokenId, WorkspaceId};
use chaldea_core::payload::{Component, SurfaceScope, SurfaceUpdate};

use super::support::http::TestServer;
use super::support::{event, registered};
use crate::http::{ApiError, Authorized, Mounts, ObjectScope, deny_for};

fn auth(kind: TokenKind, thread: Option<ThreadId>) -> Authorized {
    Authorized {
        token_id: TokenId::from_u128(1),
        kind,
        thread,
        launch: None,
    }
}

fn surface(stream: StreamId, id: &str, scope: SurfaceScope) -> crate::store::NewEvent {
    event(
        stream,
        &format!("surface:{id}"),
        TypedEvent::SurfaceUpdate(SurfaceUpdate {
            surface_id: SurfaceId(id.to_owned()),
            scope,
            components: vec![Component::Text { text: "x".into() }],
        }),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stored_surface_scope() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let t = ThreadId::from_u128(1);
    let ws = WorkspaceId::from("ws");
    srv.append(vec![registered(t, false)]).await;
    srv.append(vec![
        surface(StreamId::Host, "host", SurfaceScope::Host),
        surface(
            StreamId::Host,
            "workspace",
            SurfaceScope::Workspace {
                workspace_id: ws.clone(),
            },
        ),
        surface(
            StreamId::Thread(t),
            "thread",
            SurfaceScope::Thread { thread_id: t },
        ),
    ])
    .await;
    let host = SurfaceScope::Host;
    let in_ws = SurfaceScope::Workspace {
        workspace_id: ws.clone(),
    };
    let other_ws = SurfaceScope::Workspace {
        workspace_id: WorkspaceId::from("other"),
    };
    let of_t = SurfaceScope::Thread { thread_id: t };
    let of_other = SurfaceScope::Thread {
        thread_id: ThreadId::from_u128(9),
    };
    let stored = [
        (StreamId::Host, "host"),
        (StreamId::Host, "workspace"),
        (StreamId::Thread(t), "thread"),
    ];
    // (principal, configured scope, allowed for [host, workspace, thread] surfaces)
    let cases: Vec<(Authorized, Option<SurfaceScope>, [bool; 3])> = vec![
        (auth(TokenKind::Owner, None), None, [true; 3]),
        (auth(TokenKind::Rescue, Some(t)), None, [true; 3]),
        (auth(TokenKind::Extension, None), None, [false; 3]),
        (auth(TokenKind::Extension, None), Some(host), [true; 3]),
        (
            auth(TokenKind::Extension, None),
            Some(in_ws),
            [false, true, true],
        ),
        (auth(TokenKind::Extension, None), Some(other_ws), [false; 3]),
        (
            auth(TokenKind::Extension, None),
            Some(of_t),
            [false, false, true],
        ),
        (auth(TokenKind::Extension, None), Some(of_other), [false; 3]),
        (auth(TokenKind::Agent, Some(t)), None, [false; 3]),
        (auth(TokenKind::Hook, None), None, [false; 3]),
    ];
    for (a, configured, want) in cases {
        let (a2, c2) = (a.clone(), configured.clone());
        let got = srv.store.read(move |tx| {
            let mut out = [false; 3];
            for (i, (stream, id)) in stored.iter().enumerate() {
                out[i] = ObjectScope::stored_surface(
                    tx,
                    *stream,
                    &SurfaceId((*id).to_owned()),
                    &a2,
                    c2.as_ref(),
                )?;
            }
            // A surface that does not exist is never in scope.
            assert!(!ObjectScope::stored_surface(
                tx,
                StreamId::Host,
                &SurfaceId("missing".into()),
                &a2,
                Some(&SurfaceScope::Host),
            )?);
            Ok(out)
        });
        assert_eq!(got, want, "{:?} with {:?}", a.kind, configured);
    }
    srv.stop().await;
}

#[test]
fn deny_hides_unreadable_objects() {
    let (p, c, s) = (
        ThreadId::from_u128(1),
        ThreadId::from_u128(2),
        ThreadId::from_u128(3),
    );
    let g = ThreadGraph::new([(p, None), (c, Some(p)), (s, None)]);
    let agent = auth(TokenKind::Agent, Some(p));
    assert_eq!(deny_for(&g, &agent, c), ApiError::forbidden());
    assert_eq!(deny_for(&g, &agent, p), ApiError::forbidden());
    assert_eq!(deny_for(&g, &agent, s), ApiError::not_found());
    assert_eq!(
        deny_for(&g, &auth(TokenKind::Owner, None), s),
        ApiError::forbidden()
    );
    assert_eq!(
        deny_for(&g, &auth(TokenKind::Hook, None), s),
        ApiError::not_found()
    );
    // A thread that does not exist is 404 for every kind, `Grant::All` included.
    let missing = ThreadId::from_u128(0xdead);
    for kind in [TokenKind::Owner, TokenKind::Rescue, TokenKind::Agent] {
        assert_eq!(
            deny_for(&g, &auth(kind, Some(p)), missing),
            ApiError::not_found(),
            "{kind:?}"
        );
    }
}
