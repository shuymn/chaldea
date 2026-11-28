//! `GET /api/events` answers from one read snapshot (design §2.7; PR7-CL-9–11): a snapshot
//! equals the values the test computes from the jobs committed before a cut the test fixes,
//! and a page is consistent with its head, while later jobs commit during the request.

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::time::Duration;

use chaldea_core::auth::TokenKind;
use chaldea_core::event::TypedEvent;
use chaldea_core::ids::{Actor, Agent, Epoch, Open, StreamId, ThreadId, WorkspaceId};
use chaldea_core::payload::{
    ApprovalRequested, Content, HumanWriteOrigin, InputEncoding, ReadinessEvidence, SurfaceUpdate,
    ThreadRole, VtCursor, VtModes, VtSnapshotTaken, WorktreeChanged, WorktreeState,
};
use chaldea_core::state::AttentionCause;
use chaldea_core::sync::{
    EventsResponse, GateEvidence, HostSnapshot, InputInfo, LaunchInfo, SessionInfo, StreamSnapshot,
    ThreadInfo, ThreadSnapshot,
};

use super::shared;
use super::support::http::{CLIENT_WAIT, Reply, TestServer};
use super::support::{Model, Op, event, raw_unknown, registered, soon, thread};
use crate::http::Mounts;
use crate::store::NewEvent;

/// The events of a committed job, kept per stream in order, with each stream's
/// `raw_unknown` count.
#[derive(Default)]
struct Log {
    keys: BTreeMap<StreamId, Vec<String>>,
    events: Vec<TypedEvent>,
    raw_unknowns: BTreeMap<StreamId, u64>,
}

impl Log {
    fn record(&mut self, events: &[NewEvent]) {
        for e in events {
            self.keys
                .entry(e.stream_id)
                .or_default()
                .push(e.idempotency_key.0.clone());
            self.events.push(e.event.clone());
            if matches!(e.event, TypedEvent::RawUnknown(_)) {
                *self.raw_unknowns.entry(e.stream_id).or_default() += 1;
            }
        }
    }
}

/// Commits the model's pending job and logs its events.
async fn commit(srv: &TestServer, m: &mut Model, log: &mut Log) {
    let job = m.take_job();
    assert!(job.commits);
    for op in job.ops {
        let Op::Append(events) = op else {
            panic!("unexpected op {op:?}")
        };
        log.record(&events);
        srv.append(events).await;
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
            bracketed_paste: true,
            alternate_screen: false,
        },
    }
}

/// The thread's `ThreadInfo` as the model and its registration give it.
fn thread_info(m: &Model, t: ThreadId) -> ThreadInfo {
    let state = &m.threads[&t].state;
    ThreadInfo {
        thread_id: t,
        agent: Open::Known(Agent::Claude),
        cwd: "/work/repo".into(),
        workspace_id: WorkspaceId("ws".into()),
        parent_thread_id: None,
        role: ThreadRole::Standard,
        closed: false,
        process_state: state.process,
        turn_state: state.turn,
        task_state: state.task,
        attention: state.attention.iter().copied().collect(),
    }
}

/// The latest record per surface id of `stream`'s scope among the logged events, the
/// removed ones left out, sorted by id.
fn surfaces(log: &Log, want: impl Fn(&SurfaceUpdate) -> bool) -> Vec<SurfaceUpdate> {
    let mut by_id = BTreeMap::new();
    for e in &log.events {
        if let TypedEvent::SurfaceUpdate(s) = e
            && want(s)
        {
            by_id.insert(s.surface_id.0.clone(), s.clone());
        }
    }
    by_id
        .into_values()
        .filter(|s| !s.components.is_empty())
        .collect()
}

/// The thread snapshot the model and the log determine at the cut.
fn expected_thread(m: &Model, log: &Log, t: ThreadId) -> (ThreadSnapshot, Vec<String>) {
    let th = &m.threads[&t];
    let l = th.current_launch.expect("a launch");
    let ml = &m.launches[&l];
    let open_approvals: Vec<ApprovalRequested> = log
        .events
        .iter()
        .filter_map(|e| match e {
            TypedEvent::ApprovalRequested(a) if m.open_approvals.contains_key(&a.approval_id) => {
                Some(a.clone())
            }
            _ => None,
        })
        .collect();
    let attention_or_card = !th.state.attention.is_empty() || !open_approvals.is_empty();
    let latest_vt = log.events.iter().rev().find_map(|e| match e {
        TypedEvent::VtSnapshotTaken(v) => Some(v.clone()),
        _ => None,
    });
    let keys = log.keys[&StreamId::Thread(t)].clone();
    let recent = keys[keys.len().saturating_sub(50)..].to_vec();
    let snapshot = ThreadSnapshot {
        thread: thread_info(m, t),
        launch: Some(LaunchInfo {
            launch_id: l,
            pid: Some(4242),
            manual: false,
            exit_reason: None,
            death_confirmed: ml.death_confirmed,
        }),
        session: th.session.clone().map(|session_id| SessionInfo {
            session_id,
            transcript_path: th.path.clone(),
        }),
        gate: GateEvidence {
            readiness_seq: ml.readiness.map(|r| r.0),
            readiness_evidence: ml.readiness.map(|r| r.1),
            last_write_seq: ml.last_write,
            last_human_write_seq: ml.last_human,
            last_turn_start_seq: ml.last_turn_start,
        },
        inputs: m
            .inputs
            .iter()
            .filter(|(_, i)| i.thread == t && !i.state.is_terminal())
            .map(|(id, i)| InputInfo {
                input_id: *id,
                launch_id: i.launch,
                state: i.state,
                text: Content::text("hello"),
                encoding: InputEncoding::BracketedPaste,
                created_by: Actor::Human,
                sent_after_seq: None,
            })
            .collect(),
        open_approvals,
        worktree: None,
        surfaces: surfaces(
            log,
            |s| matches!(s.scope, chaldea_core::payload::SurfaceScope::Thread { thread_id } if thread_id == t),
        ),
        vt_snapshot: if attention_or_card { latest_vt } else { None },
        recent_events: Vec::new(),
        raw_unknown_count: log
            .raw_unknowns
            .get(&StreamId::Thread(t))
            .copied()
            .unwrap_or(0),
    };
    (snapshot, recent)
}

fn expected_host(m: &Model, log: &Log) -> HostSnapshot {
    let mut worktrees: BTreeMap<String, WorktreeChanged> = BTreeMap::new();
    for e in &log.events {
        if let TypedEvent::WorktreeChanged(w) = e {
            worktrees.insert(w.worktree_id.0.clone(), w.clone());
        }
    }
    HostSnapshot {
        threads: m.threads.keys().map(|t| thread_info(m, *t)).collect(),
        worktrees: worktrees.into_values().collect(),
        surfaces: surfaces(log, |s| {
            !matches!(s.scope, chaldea_core::payload::SurfaceScope::Thread { .. })
        }),
        raw_unknown_count: log.raw_unknowns.get(&StreamId::Host).copied().unwrap_or(0),
    }
}

fn decode(r: &Reply) -> EventsResponse {
    assert_eq!(r.status, 200, "{r:?}");
    serde_json::from_slice(&r.body).unwrap()
}

/// How long [`with_commits_during`] waits for all its queued jobs together: half the client's
/// wait for the reply, so a commit that never comes fails here, with its own message, before
/// the client gives up on the request.
const COMMITS_WAIT: Duration = CLIENT_WAIT.checked_div(2).unwrap();

/// Arms the handler's read (after the guard's token read) to let the jobs queued behind
/// `hold` commit, within [`COMMITS_WAIT`] in total, before the handler goes on, then sends
/// `req`.
async fn with_commits_during(
    srv: &TestServer,
    req: &super::support::http::Req,
    hold: mpsc::Sender<()>,
    committed: mpsc::Receiver<()>,
    jobs: usize,
) -> Reply {
    shared(srv).faults.arm_after_read(1, move || {
        let until = std::time::Instant::now() + COMMITS_WAIT;
        drop(hold);
        for n in 0..jobs {
            committed
                .recv_timeout(until.saturating_duration_since(std::time::Instant::now()))
                .unwrap_or_else(|e| {
                    panic!("{n} of {jobs} queued jobs committed within {COMMITS_WAIT:?}: {e}")
                });
        }
    });
    srv.send(req).await
}

/// Queues `events` behind the held writer; the returned receiver fires once it commits.
fn queue(srv: &TestServer, events: Vec<NewEvent>, done: &mpsc::Sender<()>) {
    let done = done.clone();
    let ticket = srv
        .store
        .store()
        .submit(soon(), move |tx| Ok(tx.append(events)?))
        .unwrap();
    tokio::spawn(async move {
        ticket.await.unwrap();
        let _ = done.send(());
    });
}

/// A snapshot equals, field by field, the values computed from the jobs committed before
/// the cut, with `as_of_seq` the stream's head at the cut; jobs committed while the
/// request runs (after its read) are not in it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_equals_model_at_cut() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let mut m = Model::default();
    let mut log = Log::default();
    // The jobs before the cut, across the host and thread streams.
    let t = m.register(true);
    commit(&srv, &mut m, &mut log).await;
    assert!(m.reserve(t).is_some());
    assert!(m.start(t));
    assert!(m.bind(t, Some("/work/t.jsonl".into())));
    commit(&srv, &mut m, &mut log).await;
    assert!(m.readiness(t, ReadinessEvidence::SessionStart, 0, 0));
    assert!(m.human_write(t, HumanWriteOrigin::Key));
    assert!(m.prompt(t, false));
    commit(&srv, &mut m, &mut log).await;
    let i = m.create_input(t, true);
    assert!(m.request_approval(t));
    assert!(m.attention(t, AttentionCause::InputGateTimeout { input_id: i }, true));
    m.surface(Some(t), 1, false);
    m.surface(None, 2, false);
    m.worktree(1, WorktreeState::Active);
    commit(&srv, &mut m, &mut log).await;
    let vt_events = vec![
        event(
            StreamId::Thread(t),
            "vt:1",
            TypedEvent::VtSnapshotTaken(vt(1)),
        ),
        event(
            StreamId::Thread(t),
            "vt:2",
            TypedEvent::VtSnapshotTaken(vt(2)),
        ),
    ];
    log.record(&vt_events);
    srv.append(vt_events).await;
    for stream in [StreamId::Thread(t), StreamId::Host] {
        let thread_head = log.keys[&StreamId::Thread(t)].len() as u64;
        let host_head = log.keys[&StreamId::Host].len() as u64;
        // The cut: the writer held, jobs after it queued, and released only after the
        // handler's read.
        let hold = srv.hold_writer().await;
        let (done_tx, done_rx) = mpsc::channel();
        let later_thread = vec![
            raw_unknown(
                StreamId::Thread(t),
                &format!("after:{stream}:t"),
                Content::text("later"),
            ),
            event(
                StreamId::Thread(t),
                &format!("after:{stream}:vt"),
                TypedEvent::VtSnapshotTaken(vt(9)),
            ),
        ];
        let later_host = vec![raw_unknown(
            StreamId::Host,
            &format!("after:{stream}:h"),
            Content::text("later"),
        )];
        queue(&srv, later_thread.clone(), &done_tx);
        queue(&srv, later_host.clone(), &done_tx);
        let req = srv
            .req(
                "GET",
                &format!("/api/events?stream_id={stream}&epoch=elsewhere"),
            )
            .bearer(&owner);
        let r = with_commits_during(&srv, &req, hold, done_rx, 2).await;
        let EventsResponse::Snapshot {
            stream_id,
            snapshot,
            as_of_seq,
            ..
        } = decode(&r)
        else {
            panic!("a snapshot")
        };
        assert_eq!(stream_id, stream);
        match snapshot {
            StreamSnapshot::Thread(s) => {
                assert_eq!(as_of_seq, thread_head, "as_of_seq is the head at the cut");
                let ThreadSnapshot {
                    thread,
                    launch,
                    session,
                    gate,
                    inputs,
                    open_approvals,
                    worktree,
                    surfaces,
                    vt_snapshot,
                    recent_events,
                    raw_unknown_count,
                } = *s;
                let (want, recent) = expected_thread(&m, &log, t);
                assert_eq!(thread, want.thread);
                assert_eq!(launch, want.launch);
                assert_eq!(session, want.session);
                assert_eq!(gate, want.gate);
                assert_eq!(inputs, want.inputs);
                assert_eq!(open_approvals, want.open_approvals);
                assert_eq!(worktree, want.worktree);
                assert_eq!(surfaces, want.surfaces);
                assert_eq!(vt_snapshot, want.vt_snapshot);
                assert!(vt_snapshot.is_some(), "attention is active");
                let got: Vec<String> = recent_events
                    .iter()
                    .map(|e| e.idempotency_key.0.clone())
                    .collect();
                assert_eq!(got, recent);
                assert_eq!(raw_unknown_count, want.raw_unknown_count);
            }
            StreamSnapshot::Host(s) => {
                assert_eq!(as_of_seq, host_head, "as_of_seq is the head at the cut");
                let HostSnapshot {
                    threads,
                    worktrees,
                    surfaces,
                    raw_unknown_count,
                } = s;
                let want = expected_host(&m, &log);
                assert_eq!(threads, want.threads);
                assert_eq!(worktrees, want.worktrees);
                assert_eq!(surfaces, want.surfaces);
                assert_eq!(raw_unknown_count, want.raw_unknown_count);
            }
        }
        // Both queued jobs committed after the read; the later state is the next cut's.
        log.record(&later_thread);
        log.record(&later_host);
    }
    srv.stop().await;
}

/// A page and its head come from one snapshot: events committed after the read are not in
/// it, and `has_more` and `head_seq` agree with the page.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn page_and_head_from_one_snapshot() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let hold = srv.hold_writer().await;
    let (done_tx, done_rx) = mpsc::channel();
    queue(
        &srv,
        vec![raw_unknown(StreamId::Host, "later", Content::text("later"))],
        &done_tx,
    );
    let req = srv
        .req("GET", "/api/events?stream_id=host&after_seq=0")
        .bearer(&owner);
    let r = with_commits_during(&srv, &req, hold, done_rx, 1).await;
    let EventsResponse::Replay {
        events,
        head_seq,
        has_more,
        ..
    } = decode(&r)
    else {
        panic!("a page")
    };
    assert_eq!((events.len(), head_seq, has_more), (0, 0, false));
    srv.stop().await;
}

/// A forced snapshot (`epoch` mismatch) of `stream`: its epoch, `as_of_seq`, and
/// `raw_unknown_count`.
async fn forced_snapshot(srv: &TestServer, owner: &str, stream: StreamId) -> (Epoch, u64, u64) {
    let req = srv
        .req(
            "GET",
            &format!("/api/events?stream_id={stream}&epoch=elsewhere"),
        )
        .bearer(owner);
    let EventsResponse::Snapshot {
        epoch,
        snapshot,
        as_of_seq,
        ..
    } = decode(&srv.send(&req).await)
    else {
        panic!("a snapshot")
    };
    let count = match snapshot {
        StreamSnapshot::Thread(s) => s.raw_unknown_count,
        StreamSnapshot::Host(s) => s.raw_unknown_count,
    };
    (epoch, as_of_seq, count)
}

/// A snapshot's `raw_unknown_count` is the stream's `raw_unknown` events at or below
/// `as_of_seq`, on a thread and on the host; a later snapshot's count equals that base plus
/// the `raw_unknown` events replayed after `as_of_seq`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_raw_unknown_count() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    srv.append(vec![registered(thread(1), true)]).await;
    let thread = StreamId::Thread(thread(1));
    // Another kind between the `raw_unknown` events, so the count is not the head.
    let mut before = vec![
        raw_unknown(thread, "t:1", Content::text("a")),
        event(thread, "vt:1", TypedEvent::VtSnapshotTaken(vt(1))),
        raw_unknown(thread, "t:2", Content::text("b")),
    ];
    before.extend(
        (1..=3).map(|n| raw_unknown(StreamId::Host, &format!("h:{n}"), Content::text("h"))),
    );
    srv.append(before).await;
    for (stream, want) in [(thread, 2), (StreamId::Host, 3)] {
        let (epoch, as_of_seq, count) = forced_snapshot(&srv, &owner, stream).await;
        assert_eq!(
            count, want,
            "{stream}: the raw_unknown events at or below as_of_seq"
        );
        assert!(as_of_seq > count, "{stream}: other kinds are not counted");
        srv.append(vec![raw_unknown(
            stream,
            &format!("later:{stream}"),
            Content::text("later"),
        )])
        .await;
        let req = srv
            .req(
                "GET",
                &format!("/api/events?stream_id={stream}&after_seq={as_of_seq}&epoch={epoch}"),
            )
            .bearer(&owner);
        let EventsResponse::Replay { events, .. } = decode(&srv.send(&req).await) else {
            panic!("a page")
        };
        let replayed = events
            .iter()
            .filter(|e| e.kind.as_str() == "raw_unknown")
            .count() as u64;
        assert_eq!(replayed, 1);
        let (_, next_seq, fresh) = forced_snapshot(&srv, &owner, stream).await;
        assert!(next_seq > as_of_seq);
        assert_eq!(fresh, count + replayed, "{stream}: base plus replayed");
    }
    srv.stop().await;
}
