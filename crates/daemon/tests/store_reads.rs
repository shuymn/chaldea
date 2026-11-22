//! Reads: page flags, heads and byte counts, and the plain lookups (PR6-CL-18, PR6-P-6).

mod support;

use chaldea_core::event::TypedEvent;
use chaldea_core::ids::{
    Actor, Agent, IdempotencyKey, StreamId, SurfaceId, WorkspaceId, WorktreeId,
};
use chaldea_core::input::InputState;
use chaldea_core::payload::{
    ApprovalRequested, Component, Content, InputCreated, InputEncoding, InputInitialState,
    InputOrigin, LaunchReserved, SurfaceScope, SurfaceUpdate, WorktreeChanged, WorktreeState,
};
use chaldea_daemon::store::{BufferedHook, HookReply, NewHookDelivery, NewRouting};
use support::{
    TestStore, approval, event, input, launch, raw_unknown, registered, started, thread,
};

fn three(s: &TestStore) {
    let events = (1..=3)
        .map(|i| {
            raw_unknown(
                StreamId::Host,
                &format!("k{i}"),
                Content::text("x".repeat(i * 10)),
            )
        })
        .collect();
    s.append(events).unwrap();
}

/// `has_more` and `has_more_before` of forward and backward pages, and the clamped cursor.
#[test]
fn page_flags() {
    let s = TestStore::new();
    three(&s);
    let h = StreamId::Host;
    let seqs = |p: &chaldea_daemon::store::Page| p.events.iter().map(|e| e.seq).collect::<Vec<_>>();
    let p = s.read(move |r| r.events_after(h, 0, 1));
    assert_eq!(
        (seqs(&p), p.has_more, p.has_more_before),
        (vec![1], true, false)
    );
    let p = s.read(move |r| r.events_after(h, 1, 1));
    assert_eq!(
        (seqs(&p), p.has_more, p.has_more_before),
        (vec![2], true, true)
    );
    let p = s.read(move |r| r.events_after(h, 2, 1));
    assert_eq!(
        (seqs(&p), p.has_more, p.has_more_before),
        (vec![3], false, true)
    );
    let p = s.read(move |r| r.events_after(h, 3, 1));
    assert_eq!(
        (seqs(&p), p.has_more, p.has_more_before),
        (vec![], false, true)
    );
    let p = s.read(move |r| r.events_before(h, 4, 1));
    assert_eq!(
        (seqs(&p), p.has_more, p.has_more_before),
        (vec![3], false, true)
    );
    let p = s.read(move |r| r.events_before(h, 3, 1));
    assert_eq!(
        (seqs(&p), p.has_more, p.has_more_before),
        (vec![2], true, true)
    );
    let p = s.read(move |r| r.events_before(h, 2, 1));
    assert_eq!(
        (seqs(&p), p.has_more, p.has_more_before),
        (vec![1], true, false)
    );
    let p = s.read(move |r| r.events_before(h, 1, 1));
    assert_eq!(
        (seqs(&p), p.has_more, p.has_more_before),
        (vec![], true, false)
    );
    let p = s.read(move |r| r.events_before(h, u64::MAX, 1));
    assert_eq!(
        (seqs(&p), p.has_more, p.has_more_before),
        (vec![3], false, true)
    );
    let p = s.read(move |r| r.events_before(h, u64::MAX, 10));
    assert_eq!(
        (seqs(&p), p.has_more, p.has_more_before),
        (vec![1, 2, 3], false, false)
    );
    let p = s.read(move |r| r.events_after(h, u64::MAX, 10));
    assert_eq!(
        (seqs(&p), p.has_more, p.has_more_before),
        (vec![], false, true)
    );
}

/// `head`'s byte counts equal the sums of the paged payloads.
#[test]
fn head_and_cum_bytes_agree_with_the_page() {
    let s = TestStore::new();
    three(&s);
    let h = StreamId::Host;
    let (head, page) = s.read(move |r| Ok((r.head(h, 1)?, r.events_after(h, 0, 10)?)));
    let sizes: Vec<u64> = page
        .events
        .iter()
        .map(|e| e.payload.get().len() as u64)
        .collect();
    assert_eq!(head.head_seq, 3);
    assert_eq!(head.cum_bytes_at_head, sizes.iter().sum::<u64>());
    assert_eq!(head.cum_bytes_at_after, sizes[0]);
    let above = s.read(move |r| r.head(h, 99));
    assert_eq!(above.cum_bytes_at_after, 0);
    let empty = s.read(|r| r.head(StreamId::Thread(thread(9)), 0));
    assert_eq!((empty.head_seq, empty.cum_bytes_at_head), (0, 0));
}

/// Each plain lookup returns what one known history stored.
#[test]
fn lookups_smoke() {
    let s = TestStore::new();
    let (t, l, i, a) = (thread(1), launch(1), input(1), approval(1));
    let w = WorktreeId("0000abcd".into());
    let st = StreamId::Thread(t);
    s.append(vec![
        registered(t, false),
        event(
            StreamId::Host,
            "wt",
            TypedEvent::WorktreeChanged(WorktreeChanged {
                worktree_id: w.clone(),
                repo: "/work/repo".into(),
                workspace_id: WorkspaceId("ws".into()),
                path: "/work/wt".into(),
                branch: "chaldea/0000abcd".into(),
                base_ref: "main".into(),
                base_commit: "0".repeat(40),
                owner_thread_id: Some(t),
                created_by: Actor::Human,
                state: WorktreeState::Active,
            }),
        ),
    ])
    .unwrap();
    let requested = ApprovalRequested {
        approval_id: a,
        launch_id: l,
        turn_ref: None,
        tool_name: "Bash".into(),
        tool_input: Content::text("ls"),
        correlation: None,
    };
    let surface = SurfaceUpdate {
        surface_id: SurfaceId("panel".into()),
        scope: SurfaceScope::Thread { thread_id: t },
        components: vec![Component::Text { text: "hi".into() }],
    };
    s.append(vec![
        event(
            st,
            "reserve",
            TypedEvent::LaunchReserved(LaunchReserved {
                launch_id: l,
                expected_previous: None,
                agent: Agent::Codex,
                argv_digest: "1".repeat(64),
                manual: true,
                worktree_id: Some(w.clone()),
            }),
        ),
        started(t, l),
        event(
            st,
            "input",
            TypedEvent::InputCreated(InputCreated {
                input_id: i,
                launch_id: Some(l),
                initial_state: InputInitialState::Queued,
                text: Content::text("hello"),
                encoding: InputEncoding::WinKeys,
                origin: InputOrigin::Pwa,
                created_by: Actor::Human,
            }),
        ),
        event(
            st,
            "approval",
            TypedEvent::ApprovalRequested(requested.clone()),
        ),
        event(st, "surface", TypedEvent::SurfaceUpdate(surface.clone())),
    ])
    .unwrap();
    let emit = uuid::Uuid::from_u128(1);
    s.run(move |tx| {
        Ok(tx.hook_delivery_record(NewHookDelivery {
            launch_id: l,
            emit_id: emit,
            receipt_ordinal: 1,
            first_reply: HookReply {
                http_status: 200,
                body: "{}".into(),
            },
            routing: NewRouting::Buffered(BufferedHook {
                body: b"raw".to_vec(),
                meta: "{}".into(),
            }),
        })?)
    })
    .unwrap();
    let w2 = w.clone();
    s.read(move |r| {
        assert!(r.stream_exists(StreamId::Host)?);
        assert!(r.stream_exists(st)?);
        assert!(!r.stream_exists(StreamId::Thread(thread(2)))?);
        let heads = r.heads(&[StreamId::Host, st, StreamId::Thread(thread(2))])?;
        assert_eq!(
            heads.iter().map(|h| h.head_seq).collect::<Vec<_>>(),
            vec![2, 5, 0]
        );
        let e = r
            .event_by_key(st, &IdempotencyKey("input".into()))?
            .unwrap();
        assert_eq!(e.seq, 3);
        assert_eq!(r.event_by_key(st, &IdempotencyKey("nope".into()))?, None);
        let p = r.events_of_kind(st, "launch_started", 0, 10)?;
        assert_eq!(p.events.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2]);
        assert_eq!(r.latest_of_kind(st, "surface_update")?.unwrap().seq, 5);
        assert_eq!(r.latest_of_kind(st, "task_set")?, None);
        let th = r.thread(t)?.unwrap();
        assert_eq!(
            (th.current_launch_id, th.cwd.as_str()),
            (Some(l), "/work/repo")
        );
        assert_eq!(
            r.threads()?.iter().map(|x| x.thread_id).collect::<Vec<_>>(),
            vec![t]
        );
        let lr = r.launch(l)?.unwrap();
        assert_eq!(
            (lr.agent, lr.manual, lr.pid),
            (Agent::Codex, true, Some(4242))
        );
        assert_eq!(
            r.open_launches()?
                .iter()
                .map(|x| x.launch_id)
                .collect::<Vec<_>>(),
            vec![l]
        );
        assert_eq!(r.launches_using_worktree(&w2)?.len(), 1);
        assert_eq!(
            r.launches_using_worktree(&WorktreeId("ffffffff".into()))?
                .len(),
            0
        );
        let ir = r.input(i)?.unwrap();
        assert_eq!(
            (ir.state, ir.launch_id, ir.encoding),
            (InputState::Queued, Some(l), InputEncoding::WinKeys)
        );
        assert_eq!(r.inputs(t, &[InputState::Queued])?.len(), 1);
        assert_eq!(r.inputs(t, &[InputState::Sent])?.len(), 0);
        assert_eq!(r.approval(a)?.unwrap().requested, requested);
        assert_eq!(r.open_approvals(t)?.len(), 1);
        let sr = r.surface(st, &SurfaceId("panel".into()))?.unwrap();
        assert_eq!((sr.stream_id, &sr.update), (st, &surface));
        assert_eq!(r.worktree(&w2)?.unwrap().record.owner_thread_id, Some(t));
        assert_eq!(r.worktrees()?.len(), 1);
        assert_eq!(r.buffered_launches()?, vec![l]);
        let info = r.schema_info()?;
        assert_eq!((info.application_id, info.user_version), (0x4348_4C44, 1));
        assert_eq!(info.projection_version, "1");
        assert!(r.integrity_check(10)?.is_empty());
        Ok(())
    });
}

/// An update on a thread stream never changes a host surface of the same id: the host
/// snapshot is unchanged or the host stream records the change.
#[test]
fn host_surface_removal_is_visible_in_host_replay() {
    let s = TestStore::new();
    let t = thread(1);
    let mut panel = SurfaceUpdate {
        surface_id: SurfaceId("panel".into()),
        scope: SurfaceScope::Host,
        components: vec![Component::Text { text: "hi".into() }],
    };
    s.append(vec![
        registered(t, false),
        event(
            StreamId::Host,
            "panel",
            TypedEvent::SurfaceUpdate(panel.clone()),
        ),
    ])
    .unwrap();
    #[expect(
        clippy::redundant_closure_for_method_calls,
        reason = "the method path is not general over the ReadTx lifetime"
    )]
    let (before, head) = s.read(|r| r.host_snapshot());
    assert_eq!(before.surfaces.len(), 1);

    panel.scope = SurfaceScope::Thread { thread_id: t };
    panel.components.clear();
    let change = s.append(vec![event(
        StreamId::Thread(t),
        "remove-panel",
        TypedEvent::SurfaceUpdate(panel),
    )]);
    if change.is_ok() {
        let (after, page) = s.read(|r| {
            Ok((
                r.host_snapshot()?.0,
                r.events_after(StreamId::Host, head, 10)?,
            ))
        });
        assert!(after.surfaces == before.surfaces || !page.events.is_empty());
    }
}

/// The thread snapshot and `inputs(t, states)` select by state in SQL: a terminal input's
/// row is never read (here its stored text no longer decodes, so reading it would fail).
#[test]
fn terminal_inputs_are_not_loaded() {
    let mut s = TestStore::new();
    let t = thread(1);
    let created = |n: u128| {
        event(
            StreamId::Thread(t),
            &format!("input-{n}"),
            TypedEvent::InputCreated(InputCreated {
                input_id: input(n),
                launch_id: None,
                initial_state: InputInitialState::Queued,
                text: Content::text("hello"),
                encoding: InputEncoding::WinKeys,
                origin: InputOrigin::Pwa,
                created_by: Actor::Human,
            }),
        )
    };
    s.append(vec![registered(t, false), created(1), created(2)])
        .unwrap();
    s.shutdown();
    {
        let c = rusqlite::Connection::open(s.path()).unwrap();
        let changed = c
            .execute(
                "UPDATE proj_inputs SET state = 'cancelled', text = '1' WHERE input_id = ?1",
                [input(2).to_string()],
            )
            .unwrap();
        assert_eq!(changed, 1);
    }
    // The projection version is unchanged, so reopening does not rebuild over the edit.
    s.reopen();
    let (snapshot, queued) =
        s.read(move |r| Ok((r.thread_snapshot(t)?, r.inputs(t, &[InputState::Queued])?)));
    let ids: Vec<_> = snapshot
        .unwrap()
        .0
        .inputs
        .iter()
        .map(|i| i.input_id)
        .collect();
    assert_eq!(ids, [input(1)]);
    assert_eq!(queued.len(), 1);
}
