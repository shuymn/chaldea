//! Per-resource filtering (`docs/design/http-surface.md` PR7-CL-12): an agent reads self+
//! only, never the `host` stream; owner and rescue read everything, human-owned worktrees
//! included.

mod support;

use std::collections::BTreeSet;

use chaldea_core::auth::TokenKind;
use chaldea_core::event::TypedEvent;
use chaldea_core::http::{ThreadsResponse, WorktreesResponse};
use chaldea_core::ids::{Actor, LaunchId, StreamId, ThreadId, WorkspaceId, WorktreeId};
use chaldea_core::payload::{WorktreeChanged, WorktreeState};
use chaldea_core::sync::{EventsResponse, StreamSnapshot};
use chaldea_daemon::http::Mounts;
use chaldea_daemon::store::prepare_content;
use support::http::TestServer;
use support::{event, raw_unknown, registered_under, reserved, started};

/// The tree: parent ⟶ me ⟶ child ⟶ grandchild, and parent ⟶ sibling.
struct Tree {
    parent: ThreadId,
    me: ThreadId,
    child: ThreadId,
    grandchild: ThreadId,
    sibling: ThreadId,
}

impl Tree {
    fn all(&self) -> [ThreadId; 5] {
        [
            self.parent,
            self.me,
            self.child,
            self.grandchild,
            self.sibling,
        ]
    }

    fn self_plus(&self) -> [ThreadId; 3] {
        [self.me, self.child, self.grandchild]
    }
}

fn worktree(n: u8, owner: Option<ThreadId>) -> WorktreeChanged {
    WorktreeChanged {
        worktree_id: WorktreeId(format!("{n:08x}")),
        repo: "/work/repo".into(),
        workspace_id: WorkspaceId("ws".into()),
        path: format!("/work/wt{n}"),
        branch: format!("chaldea/{n}"),
        base_ref: "main".into(),
        base_commit: "0".repeat(40),
        owner_thread_id: owner,
        created_by: Actor::Human,
        state: WorktreeState::Active,
    }
}

/// The tree, a worktree owned by each thread and one by the human, a blob referenced by
/// each thread stream, and an agent token for `me`. Returns the blob hashes per thread.
async fn fixture(srv: &TestServer) -> (Tree, String, Vec<(ThreadId, String)>) {
    let id = |n: u128| ThreadId::from_u128(0xC000 + n);
    let tree = Tree {
        parent: id(1),
        me: id(2),
        child: id(3),
        grandchild: id(4),
        sibling: id(5),
    };
    srv.append(vec![
        registered_under(tree.parent, None),
        registered_under(tree.me, Some(tree.parent)),
        registered_under(tree.child, Some(tree.me)),
        registered_under(tree.grandchild, Some(tree.child)),
        registered_under(tree.sibling, Some(tree.parent)),
    ])
    .await;
    let l = LaunchId::from_u128(0xD002);
    srv.append(vec![reserved(tree.me, l), started(tree.me, l)])
        .await;
    let agent = srv.token(TokenKind::Agent, Some(tree.me), Some(l)).await;
    let mut wts = Vec::new();
    for (i, t) in tree.all().iter().enumerate() {
        wts.push(event(
            StreamId::Host,
            &format!("wt:{i}"),
            TypedEvent::WorktreeChanged(worktree(u8::try_from(i).unwrap(), Some(*t))),
        ));
    }
    wts.push(event(
        StreamId::Host,
        "wt:human",
        TypedEvent::WorktreeChanged(worktree(9, None)),
    ));
    srv.append(wts).await;
    let mut blobs = Vec::new();
    for t in tree.all() {
        let p = prepare_content(
            format!("{t}:{}", "b".repeat(70_000)).into_bytes(),
            "application/json",
        )
        .unwrap();
        let chaldea_core::payload::Content::Blob { blob, .. } = &p.content else {
            panic!("a blob")
        };
        let sha = blob.sha256.clone();
        let mut e = raw_unknown(StreamId::Thread(t), &format!("blob:{t}"), p.content.clone());
        e.blobs = p.blob.into_iter().collect();
        srv.append(vec![e]).await;
        blobs.push((t, sha));
    }
    (tree, agent, blobs)
}

async fn threads(srv: &TestServer, token: &str) -> BTreeSet<ThreadId> {
    let r = srv
        .send(&srv.req("GET", "/api/threads").bearer(token))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let body: ThreadsResponse = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(&body.epoch, srv.store.store().epoch());
    body.threads.into_iter().map(|t| t.thread_id).collect()
}

async fn worktree_owners(srv: &TestServer, token: &str) -> BTreeSet<Option<ThreadId>> {
    let r = srv
        .send(&srv.req("GET", "/api/worktrees").bearer(token))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let body: WorktreesResponse = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(&body.epoch, srv.store.store().epoch());
    body.worktrees
        .into_iter()
        .map(|w| w.owner_thread_id)
        .collect()
}

async fn events_status(srv: &TestServer, token: &str, s: StreamId) -> u16 {
    srv.send(
        &srv.req("GET", &format!("/api/events?stream_id={s}"))
            .bearer(token),
    )
    .await
    .status
}

async fn blob_status(srv: &TestServer, token: &str, s: StreamId, sha: &str) -> u16 {
    srv.send(
        &srv.req("GET", &format!("/api/blobs/{sha}?stream_id={s}"))
            .bearer(token),
    )
    .await
    .status
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_sees_self_plus_only() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let (tree, agent, blobs) = fixture(&srv).await;
    let mine: BTreeSet<ThreadId> = tree.self_plus().into_iter().collect();
    assert_eq!(threads(&srv, &agent).await, mine);
    assert_eq!(
        worktree_owners(&srv, &agent).await,
        mine.iter().map(|t| Some(*t)).collect()
    );
    assert_eq!(events_status(&srv, &agent, StreamId::Host).await, 404);
    for t in tree.all() {
        let want = if mine.contains(&t) { 200 } else { 404 };
        assert_eq!(
            events_status(&srv, &agent, StreamId::Thread(t)).await,
            want,
            "{t}"
        );
    }
    for (t, sha) in &blobs {
        let want = if mine.contains(t) { 200 } else { 404 };
        assert_eq!(
            blob_status(&srv, &agent, StreamId::Thread(*t), sha).await,
            want,
            "{t}"
        );
        // Another readable stream that does not reference the blob: 404.
        if !mine.contains(t) {
            assert_eq!(
                blob_status(&srv, &agent, StreamId::Thread(tree.me), sha).await,
                404,
                "{t}"
            );
        }
    }
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescue_and_owner_see_all() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let (tree, _agent, blobs) = fixture(&srv).await;
    let all: BTreeSet<ThreadId> = tree.all().into_iter().collect();
    let mut owners: BTreeSet<Option<ThreadId>> = all.iter().map(|t| Some(*t)).collect();
    owners.insert(None);
    for kind in [TokenKind::Owner, TokenKind::Rescue] {
        let tok = srv.token(kind, None, None).await;
        assert_eq!(threads(&srv, &tok).await, all, "{kind:?}");
        assert_eq!(worktree_owners(&srv, &tok).await, owners, "{kind:?}");
        assert_eq!(events_status(&srv, &tok, StreamId::Host).await, 200);
        for (t, sha) in &blobs {
            assert_eq!(events_status(&srv, &tok, StreamId::Thread(*t)).await, 200);
            assert_eq!(
                blob_status(&srv, &tok, StreamId::Thread(*t), sha).await,
                200
            );
        }
    }
    srv.stop().await;
}

/// A thread snapshot's `worktree` follows `/api/worktrees`'s rule: an agent sees the
/// launch's worktree only while its current owner is in self+; owner and rescue always.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_worktree_follows_the_worktree_rule() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let id = |n: u128| ThreadId::from_u128(0xC100 + n);
    let (parent, me, child, sibling) = (id(1), id(2), id(3), id(4));
    srv.append(vec![
        registered_under(parent, None),
        registered_under(me, Some(parent)),
        registered_under(child, Some(me)),
        registered_under(sibling, Some(parent)),
    ])
    .await;
    // `me`'s launch runs in a worktree the sibling owns now; the child's in its own.
    let foreign = worktree(1, Some(sibling));
    let own = worktree(2, Some(child));
    srv.append(vec![
        event(
            StreamId::Host,
            "wt:foreign",
            TypedEvent::WorktreeChanged(foreign.clone()),
        ),
        event(
            StreamId::Host,
            "wt:own",
            TypedEvent::WorktreeChanged(own.clone()),
        ),
    ])
    .await;
    let (lm, lc) = (LaunchId::from_u128(0xD101), LaunchId::from_u128(0xD102));
    srv.append(vec![
        reserved_in(me, lm, &foreign),
        started(me, lm),
        reserved_in(child, lc, &own),
        started(child, lc),
    ])
    .await;
    let agent = srv.token(TokenKind::Agent, Some(me), Some(lm)).await;
    assert_eq!(snapshot_worktree(&srv, &agent, me).await, None);
    assert_eq!(
        snapshot_worktree(&srv, &agent, child).await,
        Some(own.clone())
    );
    for kind in [TokenKind::Owner, TokenKind::Rescue] {
        let tok = srv.token(kind, None, None).await;
        assert_eq!(
            snapshot_worktree(&srv, &tok, me).await,
            Some(foreign.clone())
        );
        assert_eq!(
            snapshot_worktree(&srv, &tok, child).await,
            Some(own.clone())
        );
    }
    srv.stop().await;
}

fn reserved_in(t: ThreadId, l: LaunchId, w: &WorktreeChanged) -> chaldea_daemon::store::NewEvent {
    let mut e = reserved(t, l);
    if let TypedEvent::LaunchReserved(r) = &mut e.event {
        r.worktree_id = Some(w.worktree_id.clone());
    }
    e
}

/// The `worktree` of `t`'s thread snapshot as `token` reads it.
async fn snapshot_worktree(srv: &TestServer, token: &str, t: ThreadId) -> Option<WorktreeChanged> {
    let r = srv
        .send(
            &srv.req(
                "GET",
                &format!(
                    "/api/events?stream_id={}&after_seq=999999",
                    StreamId::Thread(t)
                ),
            )
            .bearer(token),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    match serde_json::from_slice(&r.body).unwrap() {
        EventsResponse::Snapshot {
            snapshot: StreamSnapshot::Thread(s),
            ..
        } => s.worktree,
        other => panic!("a thread snapshot: {other:?}"),
    }
}
