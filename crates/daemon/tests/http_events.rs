//! `GET /api/events` (`docs/design/http-surface.md` PR7-CL-9, CL-10, CL-11): replay pages,
//! backward pages, the bounds, snapshots, and paging while events are appended.

mod support;

use chaldea_core::auth::TokenKind;
use chaldea_core::event::{EventEnvelope, TypedEvent};
use chaldea_core::ids::{
    Actor, Epoch, EventId, LaunchId, StreamId, SurfaceId, ThreadId, WorkspaceId, WorktreeId,
};
use chaldea_core::payload::{
    Component, Content, LaunchReserved, SurfaceScope, SurfaceUpdate, VtCursor, VtModes,
    VtSnapshotTaken, WorktreeChanged, WorktreeState,
};
use chaldea_core::state::AttentionCause;
use chaldea_core::sync::{EventsResponse, REPLAY_MAX_BYTES, SnapshotReason, StreamSnapshot};
use chaldea_daemon::http::Mounts;
use chaldea_daemon::store::NewEvent;
use support::http::{Reply, TestServer};
use support::{TestStore, event, insert_raw_event, raw_unknown, registered, started};

fn code(s: &str) -> Option<String> {
    Some(s.to_owned())
}

async fn get(srv: &TestServer, token: &str, query: &str) -> Reply {
    srv.send(
        &srv.req("GET", &format!("/api/events?{query}"))
            .bearer(token),
    )
    .await
}

fn decode(r: &Reply) -> EventsResponse {
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.header("content-type"), Some("application/json"));
    assert_eq!(r.header("cache-control"), Some("no-store"));
    serde_json::from_slice(&r.body).unwrap_or_else(|e| panic!("{e}: {r:?}"))
}

/// A page: `(stream_id, epoch, seqs, head_seq, has_more, has_more_before)`.
fn page(r: &Reply) -> (StreamId, Epoch, Vec<u64>, u64, bool, bool, Vec<EventId>) {
    match decode(r) {
        EventsResponse::Replay {
            stream_id,
            epoch,
            events,
            head_seq,
            has_more,
            has_more_before,
        } => (
            stream_id,
            epoch,
            events.iter().map(|e| e.seq).collect(),
            head_seq,
            has_more,
            has_more_before,
            events.iter().map(|e| e.event_id).collect(),
        ),
        other @ EventsResponse::Snapshot { .. } => panic!("expected a page, got {other:?}"),
    }
}

/// A snapshot's `(stream_id, epoch, as_of_seq, reason)` and the snapshot.
fn snapshot(r: &Reply) -> (StreamId, Epoch, u64, SnapshotReason, StreamSnapshot) {
    match decode(r) {
        EventsResponse::Snapshot {
            stream_id,
            epoch,
            snapshot,
            as_of_seq,
            reason,
        } => (stream_id, epoch, as_of_seq, reason, snapshot),
        other @ EventsResponse::Replay { .. } => panic!("expected a snapshot, got {other:?}"),
    }
}

/// `n` small events on `stream`, keys `<prefix>:<i>`.
fn smalls(stream: StreamId, prefix: &str, n: usize) -> Vec<NewEvent> {
    (0..n)
        .map(|i| {
            raw_unknown(
                stream,
                &format!("{prefix}:{i}"),
                Content::text(format!("e{i}")),
            )
        })
        .collect()
}

/// The stored event ids of `stream` with `seq` in `range`, as the store reads them.
fn ids(srv: &TestServer, stream: StreamId, range: std::ops::RangeInclusive<u64>) -> Vec<EventId> {
    let all = srv.store.read(move |tx| tx.events_after(stream, 0, 1000));
    all.events
        .into_iter()
        .filter(|e| range.contains(&e.seq))
        .map(|e| e.event_id)
        .collect()
}

/// A registered thread with `n` events on its stream.
async fn thread_with(srv: &TestServer, n: u128, events: usize) -> StreamId {
    let t = ThreadId::from_u128(0xE000 + n);
    srv.append(vec![registered(t, false)]).await;
    if events > 0 {
        srv.append(smalls(StreamId::Thread(t), "small", events))
            .await;
    }
    StreamId::Thread(t)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_cases() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let epoch = srv.store.store().epoch().clone();
    let empty = thread_with(&srv, 1, 0).await;
    let s = thread_with(&srv, 2, 5).await;
    let q = |stream: StreamId, rest: &str| format!("stream_id={stream}{rest}");

    // An empty stream: an empty replay at head 0, both flags false.
    let (sid, e, seqs, head, more, before, _) = page(&get(&srv, &owner, &q(empty, "")).await);
    assert_eq!(
        (sid, e, seqs, head, more, before),
        (empty, epoch.clone(), vec![], 0, false, false)
    );

    // The first page, a middle page, the last page, and the head.
    for (rest, want_seqs, more, before) in [
        ("&limit=2", vec![1, 2], true, false),
        ("&after_seq=2&limit=2", vec![3, 4], true, true),
        ("&after_seq=3&limit=2", vec![4, 5], false, true),
        ("&after_seq=0", vec![1, 2, 3, 4, 5], false, false),
        ("&after_seq=5&limit=1000", vec![], false, true),
        (
            &format!("&after_seq=1&limit=1&epoch={epoch}"),
            vec![2],
            true,
            true,
        ),
    ] {
        let r = get(&srv, &owner, &q(s, rest)).await;
        let (sid, e, seqs, head, m, b, got_ids) = page(&r);
        assert_eq!(sid, s, "{rest}");
        assert_eq!(e, epoch, "{rest}");
        assert_eq!(head, 5, "{rest}");
        assert_eq!((m, b), (more, before), "{rest}");
        let want_ids = match (want_seqs.first(), want_seqs.last()) {
            (Some(a), Some(z)) => ids(&srv, s, *a..=*z),
            _ => vec![],
        };
        assert_eq!(seqs, want_seqs, "{rest}");
        assert_eq!(got_ids, want_ids, "{rest}");
    }

    // A cursor above the head and another epoch are snapshots, with the server's epoch.
    for (rest, reason) in [
        ("&after_seq=6", SnapshotReason::CursorAhead),
        ("&after_seq=1&epoch=elsewhere", SnapshotReason::EpochChanged),
        ("&epoch=", SnapshotReason::EpochChanged),
    ] {
        let (sid, e, as_of, why, _) = snapshot(&get(&srv, &owner, &q(s, rest)).await);
        assert_eq!(
            (sid, e, as_of, why),
            (s, epoch.clone(), 5, reason),
            "{rest}"
        );
    }

    // Refusals before or after the read; none changes the stream.
    for (rest, want) in [
        ("&limit=0", (400, code("bad_query"))),
        ("&limit=1001", (400, code("bad_query"))),
        ("&after_seq=1&before_seq=3", (400, code("bad_query"))),
        ("&limit=1&limit=1", (400, code("bad_query"))),
        ("&after=1", (400, code("bad_query"))),
        ("&after_seq=x", (400, code("bad_query"))),
    ] {
        assert_eq!(
            get(&srv, &owner, &q(s, rest)).await.outcome(),
            want,
            "{rest}"
        );
    }
    assert_eq!(
        get(&srv, &owner, "after_seq=1").await.outcome(),
        (400, code("bad_query"))
    );
    assert_eq!(
        srv.store.read(move |tx| tx.head(s, 0)).head_seq,
        5,
        "the refusals changed nothing"
    );
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backward_cases() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let epoch = srv.store.store().epoch().clone();
    let s = thread_with(&srv, 1, 6).await;
    for (rest, want_seqs, more, before) in [
        // Nothing precedes seq 1.
        ("&before_seq=1", vec![], true, false),
        ("&before_seq=5&limit=2", vec![3, 4], true, true),
        ("&before_seq=3&limit=2", vec![1, 2], true, false),
        ("&before_seq=3&limit=5", vec![1, 2], true, false),
        // Above the head: the latest page.
        ("&before_seq=100&limit=2", vec![5, 6], false, true),
        ("&before_seq=7", vec![1, 2, 3, 4, 5, 6], false, false),
        ("&before_seq=9007199254740991&limit=1", vec![6], false, true),
        // Another epoch: still a page, with the server's epoch.
        ("&before_seq=4&limit=1&epoch=elsewhere", vec![3], true, true),
    ] {
        let r = get(&srv, &owner, &format!("stream_id={s}{rest}")).await;
        let (sid, e, seqs, head, m, b, got_ids) = page(&r);
        assert_eq!((sid, e, head), (s, epoch.clone(), 6), "{rest}");
        assert_eq!(seqs, want_seqs, "{rest}");
        assert_eq!((m, b), (more, before), "{rest}");
        let want_ids = match (want_seqs.first(), want_seqs.last()) {
            (Some(a), Some(z)) => ids(&srv, s, *a..=*z),
            _ => vec![],
        };
        assert_eq!(got_ids, want_ids, "{rest}");
    }
    for rest in [
        "&before_seq=9007199254740992",
        "&before_seq=0x1",
        "&before_seq=1&before_seq=2",
    ] {
        let r = get(&srv, &owner, &format!("stream_id={s}{rest}")).await;
        assert_eq!(r.outcome(), (400, code("bad_query")), "{rest}");
    }
    srv.stop().await;
}

/// A replay at 10 000 remaining events and a snapshot at 10 001.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_bound_edges() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let s = thread_with(&srv, 1, 0).await;
    for chunk in 0..11 {
        let n = if chunk == 10 { 1 } else { 1000 };
        srv.append(smalls(s, &format!("c{chunk}"), n)).await;
    }
    assert_eq!(srv.store.read(move |tx| tx.head(s, 0)).head_seq, 10_001);
    let (_, _, seqs, head, more, before, _) =
        page(&get(&srv, &owner, &format!("stream_id={s}&after_seq=1&limit=1")).await);
    assert_eq!((seqs, head, more, before), (vec![2], 10_001, true, true));
    let (_, _, as_of, why, _) =
        snapshot(&get(&srv, &owner, &format!("stream_id={s}&limit=1")).await);
    assert_eq!((as_of, why), (10_001, SnapshotReason::TooManyEvents));
    srv.stop().await;
}

/// The payload size of a `raw_unknown` with inline text of `len` ASCII bytes, measured.
async fn payload_bytes(srv: &TestServer, stream: StreamId, len: usize) -> u64 {
    let before = srv
        .store
        .read(move |tx| tx.head(stream, 0))
        .cum_bytes_at_head;
    srv.append(vec![raw_unknown(
        stream,
        &format!("probe:{len}"),
        Content::text("x".repeat(len)),
    )])
    .await;
    srv.store
        .read(move |tx| tx.head(stream, 0))
        .cum_bytes_at_head
        - before
}

/// A replay at exactly 8 MiB of remaining payload and a snapshot one byte above.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn byte_bound_edges() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    // The payload overhead of a `raw_unknown` around its text, measured on a side stream.
    let side = thread_with(&srv, 9, 0).await;
    let overhead = payload_bytes(&srv, side, 10).await - 10;
    assert_eq!(
        payload_bytes(&srv, side, 20).await,
        overhead + 20,
        "linear in the text"
    );
    for (n, extra, want) in [(1, 0, None), (2, 1, Some(SnapshotReason::TooManyBytes))] {
        let s = thread_with(&srv, n, 0).await;
        // A first event the cursor stands on, then events summing to 8 MiB (+ extra).
        srv.append(smalls(s, "first", 1)).await;
        let target = REPLAY_MAX_BYTES + extra;
        let chunk = 60_000;
        let mut sum = 0;
        let mut i = 0;
        while target - sum > chunk + overhead + 1000 {
            srv.append(vec![raw_unknown(
                s,
                &format!("big:{i}"),
                Content::text("b".repeat(60_000)),
            )])
            .await;
            sum += chunk + overhead;
            i += 1;
        }
        let last = usize::try_from(target - sum - overhead).unwrap();
        srv.append(vec![raw_unknown(
            s,
            "last",
            Content::text("l".repeat(last)),
        )])
        .await;
        let h = srv.store.read(move |tx| tx.head(s, 1));
        assert_eq!(
            h.cum_bytes_at_head - h.cum_bytes_at_after,
            target,
            "the fixture's arithmetic"
        );
        let r = get(&srv, &owner, &format!("stream_id={s}&after_seq=1&limit=1")).await;
        match want {
            None => {
                let (_, _, seqs, head, more, _, _) = page(&r);
                assert_eq!((seqs, more), (vec![2], true));
                assert_eq!(head, h.head_seq);
            }
            Some(reason) => {
                let (_, _, as_of, why, _) = snapshot(&r);
                assert_eq!((as_of, why), (h.head_seq, reason));
            }
        }
    }
    srv.stop().await;
}

/// An unknown stream and one the token may not read answer the same 404.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreadable_equals_unknown() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let me = ThreadId::from_u128(0xA1);
    let agent = srv.agent(me, LaunchId::from_u128(0xB1)).await;
    let sibling = thread_with(&srv, 2, 3).await;
    let unknown = StreamId::Thread(ThreadId::from_u128(0xDEAD));
    let mut bodies = Vec::new();
    for s in [StreamId::Host, sibling, unknown] {
        for rest in ["", "&after_seq=99", "&before_seq=1", "&limit=0"] {
            let r = get(&srv, &agent, &format!("stream_id={s}{rest}")).await;
            assert_eq!(r.outcome(), (404, code("not_found")), "{s}{rest}");
            bodies.push(r.body);
        }
    }
    bodies.dedup();
    assert_eq!(bodies.len(), 1, "one body for every 404");
    // The agent's own stream is readable.
    let (sid, ..) = page(&get(&srv, &agent, &format!("stream_id={}", StreamId::Thread(me))).await);
    assert_eq!(sid, StreamId::Thread(me));
    srv.stop().await;
}

/// Rows of an unknown kind, a newer `schema_version`, or an invalid payload come back as
/// stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_kind_passes_unchanged() {
    let t = ThreadId::from_u128(0xE1);
    let s = StreamId::Thread(t);
    let rows = [
        (
            "k1",
            "future_kind",
            r#"{"anything":[1,2],"nested":{"x":null}}"#,
            1,
        ),
        (
            "k2",
            "assistant_message",
            r#"{"text":{"inline":"x","encoding":"utf8"},"turn_ref":null,"more":1}"#,
            2,
        ),
        ("k3", "user_message", r#"{"prompt":7}"#, 1),
    ];
    // The store is opened, closed, and reopened off the runtime's threads.
    let store = tokio::task::spawn_blocking(move || {
        let mut store = TestStore::new();
        store.append(vec![registered(t, false)]).unwrap();
        store.shutdown();
        for (key, kind, payload, v) in rows {
            insert_raw_event(&store.path(), s, key, kind, payload, v);
        }
        store.reopen();
        store
    })
    .await
    .unwrap();
    let srv = TestServer::start_on(store, Mounts::mvp(), |_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let r = get(&srv, &owner, &format!("stream_id={s}")).await;
    let EventsResponse::Replay { events, .. } = decode(&r) else {
        panic!("a page")
    };
    let got: Vec<(&str, &str, u32, &str)> = events
        .iter()
        .map(|e: &EventEnvelope| {
            (
                e.idempotency_key.0.as_str(),
                e.kind.as_str(),
                e.schema_version,
                e.payload.get(),
            )
        })
        .collect();
    let want: Vec<(&str, &str, u32, &str)> = rows
        .iter()
        .map(|(k, kind, p, v)| (*k, *kind, u32::try_from(*v).unwrap(), *p))
        .collect();
    assert_eq!(got, want);
    // The bytes on the wire hold each payload verbatim.
    let text = String::from_utf8(r.body.clone()).unwrap();
    for (_, _, p, _) in rows {
        assert!(text.contains(p), "{p} not verbatim in {text}");
    }
    srv.stop().await;
}

/// While events are appended, a client paging forward from 0 with `limit` 7 gets every
/// event once; each page is consistent with its own head and includes every append committed
/// before it was requested.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn append_between_pages() {
    let srv = std::sync::Arc::new(TestServer::start(Mounts::mvp()).await);
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let s = thread_with(&srv, 1, 3).await;
    const APPENDS: u64 = 60;
    // Appends committed so far; the writer publishes each count after its commit.
    let (done, mut committed) = tokio::sync::watch::channel(0u64);
    let writer = {
        let srv = srv.clone();
        tokio::spawn(async move {
            for i in 0..APPENDS {
                srv.append(vec![raw_unknown(
                    s,
                    &format!("live:{i}"),
                    Content::text("x"),
                )])
                .await;
                done.send_replace(i + 1);
                tokio::task::yield_now().await;
            }
        })
    };
    let target = 3 + APPENDS;
    let mut cursor = 0;
    let mut got: Vec<EventId> = Vec::new();
    while cursor < target {
        let floor = 3 + *committed.borrow();
        let r = get(
            &srv,
            &owner,
            &format!("stream_id={s}&after_seq={cursor}&limit=7"),
        )
        .await;
        let (_, _, seqs, head, more, before, page_ids) = page(&r);
        // One snapshot per answer: the page lies within its head, `has_more` says whether
        // the head is beyond it, and the page starts right after the cursor.
        assert!(seqs.len() <= 7);
        assert!(
            seqs.iter().all(|q| *q <= head),
            "{seqs:?} beyond head {head}"
        );
        assert!(head >= floor, "head {head} misses commits up to {floor}");
        let last = seqs.last().copied().unwrap_or(cursor);
        assert_eq!(
            more,
            last < head,
            "has_more {more} with last {last}, head {head}"
        );
        assert_eq!(before, cursor >= 1);
        assert_eq!(seqs, ((cursor + 1)..=last).collect::<Vec<_>>());
        got.extend(page_ids);
        if last == cursor {
            // Caught up with the writer: wait for its next commit. The bound only stops a
            // stalled writer; the pace of appends is the host's.
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                committed.wait_for(|n| 3 + n > cursor),
            )
            .await
            .unwrap_or_else(|_| panic!("no append committed past {cursor}"))
            .unwrap();
        }
        cursor = last;
    }
    writer.await.unwrap();
    assert_eq!(got, ids(&srv, s, 1..=target), "each event once, in order");
    std::sync::Arc::into_inner(srv).unwrap().stop().await;
}

// ---- snapshots ----------------------------------------------------------------------------

fn worktree(n: u8, owner: Option<ThreadId>, state: WorktreeState) -> WorktreeChanged {
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
        state,
    }
}

fn surface(id: &str, scope: SurfaceScope, text: &str) -> SurfaceUpdate {
    SurfaceUpdate {
        surface_id: SurfaceId(id.into()),
        scope,
        components: if text.is_empty() {
            vec![]
        } else {
            vec![Component::Text { text: text.into() }]
        },
    }
}

fn vt(n: u16) -> VtSnapshotTaken {
    VtSnapshotTaken {
        grid: Content::text(format!(r#"[["{n}"]]"#)),
        cols: 80,
        rows: 1,
        cursor: VtCursor {
            row: 0,
            col: 1,
            visible: true,
        },
        modes: VtModes {
            bracketed_paste: false,
            alternate_screen: false,
        },
    }
}

async fn thread_snapshot(
    srv: &TestServer,
    token: &str,
    t: ThreadId,
) -> (u64, chaldea_core::sync::ThreadSnapshot) {
    let r = get(
        srv,
        token,
        &format!("stream_id={}&after_seq=999999", StreamId::Thread(t)),
    )
    .await;
    let (_, _, as_of, why, snap) = snapshot(&r);
    assert_eq!(why, SnapshotReason::CursorAhead);
    match snap {
        StreamSnapshot::Thread(s) => (as_of, *s),
        StreamSnapshot::Host(_) => panic!("a thread snapshot"),
    }
}

async fn host_snapshot(srv: &TestServer, token: &str) -> (u64, chaldea_core::sync::HostSnapshot) {
    let r = get(srv, token, "stream_id=host&epoch=elsewhere").await;
    let (_, _, as_of, why, snap) = snapshot(&r);
    assert_eq!(why, SnapshotReason::EpochChanged);
    match snap {
        StreamSnapshot::Host(s) => (as_of, s),
        StreamSnapshot::Thread(_) => panic!("a host snapshot"),
    }
}

/// `as_of_seq` is the requested stream's head at the cut, whatever the other streams hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn as_of_is_the_stream_head() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let s = thread_with(&srv, 1, 4).await;
    let StreamId::Thread(t) = s else {
        unreachable!()
    };
    srv.append(smalls(StreamId::Host, "host", 9)).await;
    assert_eq!(thread_snapshot(&srv, &owner, t).await.0, 4);
    let host_head = srv.store.read(|tx| tx.head(StreamId::Host, 0)).head_seq;
    assert_eq!(host_snapshot(&srv, &owner).await.0, host_head);
    srv.stop().await;
}

/// A `worktree_changed` on the host stream between two thread snapshots: the same
/// `as_of_seq`, a different `worktree`, each the value of its cut.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_cursor_changed_projection() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let t = ThreadId::from_u128(0xE7);
    let l = LaunchId::from_u128(0xF7);
    let w1 = worktree(1, Some(t), WorktreeState::Active);
    srv.append(vec![registered(t, false)]).await;
    srv.append(vec![event(
        StreamId::Host,
        "wt:1",
        TypedEvent::WorktreeChanged(w1.clone()),
    )])
    .await;
    srv.append(vec![
        event(
            StreamId::Thread(t),
            &format!("launch:{l}:reserved"),
            TypedEvent::LaunchReserved(LaunchReserved {
                launch_id: l,
                expected_previous: None,
                agent: chaldea_core::ids::Agent::Claude,
                argv_digest: "0".repeat(64),
                manual: false,
                worktree_id: Some(w1.worktree_id.clone()),
            }),
        ),
        started(t, l),
    ])
    .await;
    let (as_of_1, snap_1) = thread_snapshot(&srv, &owner, t).await;
    assert_eq!(snap_1.worktree, Some(w1.clone()));
    let w2 = WorktreeChanged {
        state: WorktreeState::Released,
        ..w1.clone()
    };
    srv.append(vec![event(
        StreamId::Host,
        "wt:2",
        TypedEvent::WorktreeChanged(w2.clone()),
    )])
    .await;
    let (as_of_2, snap_2) = thread_snapshot(&srv, &owner, t).await;
    assert_eq!(as_of_1, as_of_2, "the thread stream did not advance");
    assert_eq!(snap_2.worktree, Some(w2));
    assert_ne!(snap_1.worktree, snap_2.worktree);
    srv.stop().await;
}

/// The latest `vt_snapshot_taken` exactly while an attention cause is active or a card is
/// open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vt_snapshot_branches() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let mut m = support::Model::default();
    let t = m.register(false);
    m.reserve(t);
    m.start(t);
    run(&srv, &mut m).await;
    let vt_event = |n: u16| {
        event(
            StreamId::Thread(t),
            &format!("vt:{n}"),
            TypedEvent::VtSnapshotTaken(vt(n)),
        )
    };
    srv.append(vec![vt_event(1), vt_event(2)]).await;
    // Nothing active: no snapshot.
    assert_eq!(thread_snapshot(&srv, &owner, t).await.1.vt_snapshot, None);
    // An attention cause: the latest one.
    let input = m.create_input(t, true);
    let cause = AttentionCause::InputGateTimeout { input_id: input };
    assert!(m.attention(t, cause, true));
    run(&srv, &mut m).await;
    assert_eq!(
        thread_snapshot(&srv, &owner, t).await.1.vt_snapshot,
        Some(vt(2))
    );
    // Cleared: none again.
    assert!(m.attention(t, cause, false));
    run(&srv, &mut m).await;
    assert_eq!(thread_snapshot(&srv, &owner, t).await.1.vt_snapshot, None);
    // An open card: the latest one, a newer one included.
    m.bind(t, None);
    assert!(m.request_approval(t));
    run(&srv, &mut m).await;
    srv.append(vec![vt_event(3)]).await;
    assert_eq!(
        thread_snapshot(&srv, &owner, t).await.1.vt_snapshot,
        Some(vt(3))
    );
    // The card resolved: none.
    assert!(m.resolve_approval(0, chaldea_core::payload::ApprovalOutcome::Ran));
    run(&srv, &mut m).await;
    assert_eq!(thread_snapshot(&srv, &owner, t).await.1.vt_snapshot, None);
    srv.stop().await;
}

/// Commits the model's pending job.
async fn run(srv: &TestServer, m: &mut support::Model) {
    let job = m.take_job();
    assert!(job.commits);
    for op in job.ops {
        match op {
            support::Op::Append(events) => srv.append(events).await,
            other => panic!("unexpected op {other:?}"),
        }
    }
}

/// A host snapshot lists the host- and workspace-scoped surfaces of its cut; a removed one
/// is absent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_snapshot_surfaces() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let host = surface("a-host", SurfaceScope::Host, "h");
    let ws = surface(
        "b-ws",
        SurfaceScope::Workspace {
            workspace_id: WorkspaceId("ws".into()),
        },
        "w",
    );
    let gone = surface("c-gone", SurfaceScope::Host, "g");
    let ev =
        |k: &str, s: &SurfaceUpdate| event(StreamId::Host, k, TypedEvent::SurfaceUpdate(s.clone()));
    srv.append(vec![ev("s1", &host), ev("s2", &ws), ev("s3", &gone)])
        .await;
    srv.append(vec![ev("s4", &surface("c-gone", SurfaceScope::Host, ""))])
        .await;
    let (_, snap) = host_snapshot(&srv, &owner).await;
    assert_eq!(snap.surfaces, vec![host, ws]);
    srv.stop().await;
}
