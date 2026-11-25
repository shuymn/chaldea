//! The resume reservation's checks (design §2.3 step 3; PR10a1-CL-3).

use chaldea_core::event::TypedEvent;
use chaldea_core::ids::{Actor, LaunchId, StreamId, ThreadId, WorkspaceId, WorktreeId};
use chaldea_core::payload::{LaunchReserved, WorktreeChanged, WorktreeState};
use chaldea_pty_runtime::fake::{FakeScript, KillBehavior};
use serde_json::json;

use super::support::{bound, event, exited, registered, started};
use super::{Rig, json};

fn code(r: &super::support::http::Reply) -> String {
    json(r)["code"].as_str().unwrap_or_default().to_owned()
}

/// A thread whose exited, confirmed launch used worktree `w` in `state`, with a session.
async fn with_worktree(rig: &Rig, w: &WorktreeId, state: WorktreeState) -> (ThreadId, LaunchId) {
    let t = ThreadId(uuid::Uuid::new_v4());
    let l = LaunchId(uuid::Uuid::new_v4());
    let reserved = event(
        StreamId::Thread(t),
        &format!("launch:{l}:reserved"),
        TypedEvent::LaunchReserved(LaunchReserved {
            launch_id: l,
            expected_previous: None,
            agent: chaldea_core::ids::Agent::Claude,
            argv_digest: "0".repeat(64),
            manual: false,
            worktree_id: Some(w.clone()),
        }),
    );
    let worktree = event(
        StreamId::Host,
        &format!("worktree:{w}"),
        TypedEvent::WorktreeChanged(WorktreeChanged {
            worktree_id: w.clone(),
            repo: "/work/repo".into(),
            workspace_id: WorkspaceId("ws".into()),
            path: "/work/wt".into(),
            branch: "chaldea/x".into(),
            base_ref: "main".into(),
            base_commit: "0".repeat(40),
            owner_thread_id: None,
            created_by: Actor::Human,
            state,
        }),
    );
    rig.srv.append(vec![registered(t, false), worktree]).await;
    rig.srv.append(vec![reserved, started(t, l)]).await;
    rig.srv.append(vec![bound(t, l, "s", None)]).await;
    rig.srv.append(vec![exited(t, l)]).await;
    (t, l)
}

fn resume_blocked(rig: &Rig, t: ThreadId) -> usize {
    rig.events(t)
        .iter()
        .filter(|(_, p)| p.to_string().contains("resume_blocked"))
        .count()
}

/// PR10a1-CL-3: each refusal alone, with nothing written but `death_unconfirmed`'s one
/// `resume_blocked`; the worktree states; the previous `worktree_id` carried over.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refusals_and_carry_over() {
    let rig = Rig::new().await;
    let refused = |t: ThreadId, l: LaunchId, want: &'static str| {
        let rig = &rig;
        async move {
            let before = rig.kinds(t).len();
            let r = rig
                .cmd(
                    "resume",
                    &json!({ "thread_id": t, "expected_launch_id": l }),
                )
                .await;
            assert_eq!((r.status, code(&r).as_str()), (409, want));
            assert_eq!(rig.kinds(t).len(), before, "{want}: nothing written");
        }
    };
    // Worktree states.
    for (n, state, ok) in [
        (1, WorktreeState::Creating, false),
        (2, WorktreeState::Removing, false),
        (3, WorktreeState::Uncertain, false),
        (4, WorktreeState::Removed, false),
        (5, WorktreeState::Failed, false),
        (6, WorktreeState::Active, true),
        (7, WorktreeState::Released, true),
    ] {
        let w = WorktreeId(format!("{n:08x}"));
        let (t, l) = with_worktree(&rig, &w, state).await;
        if ok {
            let r = rig
                .cmd(
                    "resume",
                    &json!({ "thread_id": t, "expected_launch_id": l }),
                )
                .await;
            assert_eq!(
                r.status,
                200,
                "{state:?}: {}",
                String::from_utf8_lossy(&r.body)
            );
            let new: LaunchId = serde_json::from_value(json(&r)["launch_id"].clone()).unwrap();
            let w2 = w.clone();
            let using = rig.srv.store.read(move |r| r.launches_using_worktree(&w2));
            assert!(using.iter().any(|row| row.launch_id == new), "{state:?}");
            assert_eq!(rig.launch(new).worktree_id, Some(w));
        } else {
            refused(t, l, "worktree_unavailable").await;
        }
    }
    // The expected launch.
    let (t, l) = rig.stopped_bound().await;
    refused(t, LaunchId(uuid::Uuid::new_v4()), "launch_changed").await;
    // Live.
    let (live, live_l) = rig.spawn_codex(None).await;
    rig.bind(live, live_l, "live").await;
    refused(live, live_l, "launch_live").await;
    // No session.
    let (ns, ns_l) = rig.spawn_codex(None).await;
    rig.cmd("stop", &json!({ "thread_id": ns })).await;
    refused(ns, ns_l, "no_session").await;
    // Death unconfirmed: `resume_blocked` raised once, not again.
    rig.pty.script_next(FakeScript {
        kill: KillBehavior::Unconfirmed,
        ..FakeScript::default()
    });
    let (u, ul) = rig.spawn_codex(None).await;
    rig.bind(u, ul, "u").await;
    rig.cmd("stop", &json!({ "thread_id": u })).await;
    for _ in 0..2 {
        let r = rig
            .cmd(
                "resume",
                &json!({ "thread_id": u, "expected_launch_id": ul }),
            )
            .await;
        assert_eq!((r.status, code(&r).as_str()), (409, "death_unconfirmed"));
        assert_eq!(resume_blocked(&rig, u), 1);
    }
    assert!(
        rig.thread(u)
            .state
            .attention
            .iter()
            .any(|a| format!("{a:?}").contains("ResumeBlocked"))
    );
    // Closed.
    let r = rig
        .cmd("close", &json!({ "thread_id": t, "expected_launch_id": l }))
        .await;
    assert_eq!(r.status, 200);
    refused(t, l, "thread_closed").await;
    // Success: `expected_previous` is the expected launch.
    let (s, sl) = rig.stopped_bound().await;
    let r = rig
        .cmd(
            "resume",
            &json!({ "thread_id": s, "expected_launch_id": sl }),
        )
        .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let reserved = rig
        .events(s)
        .into_iter()
        .rfind(|(k, _)| k == "launch_reserved")
        .unwrap()
        .1;
    assert_eq!(reserved["expected_previous"], json!(sl));
    rig.srv.stop().await;
}
