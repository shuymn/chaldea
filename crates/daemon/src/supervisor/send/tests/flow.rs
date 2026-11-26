//! The send path end to end: admission, the unit, the first prompt, `ready`, the input
//! commands, and the receipts.

use chaldea_core::adapter::HostOs;
use chaldea_core::ids::{Actor, Agent, InputId};
use chaldea_core::input::encode::{Purpose, encode};
use chaldea_core::input::{InputState, MAX_PROMPT_BYTES};
use chaldea_core::payload::InputEncoding;
use serde_json::{Value, json};

use super::{Live, SRig};
use crate::store::TokenKind;
use crate::supervisor::tests::{cid, json as body};

/// Smoke: a Claude launch bound with its `SessionStart` writes a sent text once and its
/// `UserPromptSubmit` acks it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_then_ack() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let id = rig.send(&live, "hello").await;
    rig.until_input(id, InputState::Sent).await;
    assert_eq!(live.child.input(), b"\x1b[200~hello\x1b[201~\r".to_vec());
    rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "hello" }))
        .await;
    assert_eq!(rig.input(id).state, InputState::Acked);
    let msgs = rig.of_kind(live.t, "user_message");
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["input_id"], json!(id));
    rig.stop().await;
}

const ALL_STATES: [InputState; 8] = [
    InputState::Queued,
    InputState::Sending,
    InputState::Sent,
    InputState::Acked,
    InputState::Unconfirmed,
    InputState::Rejected,
    InputState::Cancelled,
    InputState::Dismissed,
];

fn inputs(rig: &SRig, live: &Live) -> usize {
    let t = live.t;
    rig.srv.store.read(move |r| r.inputs(t, &ALL_STATES)).len()
}

/// PR10b-CL-1: `send` refuses an oversized text before any receipt or input, rejects a text
/// `check` refuses in its own job, and otherwise queues it with the row's encoding and the
/// token's actor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admission() {
    for host in [HostOs::Macos, HostOs::Windows] {
        let rig = SRig::on(host, |_| {}).await;
        for agent in [Agent::Claude, Agent::Codex] {
            // Unbound: nothing opens, so every accepted input stays `queued`.
            let (live, _) = rig.spawn(agent, None).await;
            let enc = chaldea_core::adapter::capabilities(agent, host).pty_encoding();
            let n = inputs(&rig, &live);
            let id = cid();
            let big = "a".repeat(MAX_PROMPT_BYTES + 1);
            let r = rig
                .cmd_as(
                    &rig.owner,
                    "send",
                    id,
                    &json!({ "thread_id": live.t, "text": big }),
                )
                .await;
            assert_eq!(
                (r.status, body(&r)["code"].clone()),
                (400, json!("prompt_too_large"))
            );
            assert!(rig.srv.receipt(id).is_none());
            assert_eq!(inputs(&rig, &live), n);
            // The largest text is accepted.
            let max = "a".repeat(MAX_PROMPT_BYTES);
            let v = rig
                .cmd_ok("send", &json!({ "thread_id": live.t, "text": max }), 200)
                .await;
            assert_eq!(
                (v["state"].clone(), v["reject_reason"].clone()),
                (json!("queued"), Value::Null)
            );
            let row = rig.input(serde_json::from_value(v["input_id"].clone()).unwrap());
            assert_eq!(
                (row.state, row.encoding, row.created_by, row.launch_id),
                (InputState::Queued, enc, Actor::Human, Some(live.l))
            );
            // Rejected in one job: created and rejected with the reason.
            let mut rejected = vec!["a\u{1b}b", "   "];
            if host == HostOs::Windows && agent == Agent::Codex {
                rejected.push("trailing ");
            }
            for text in rejected {
                let v = rig
                    .cmd_ok("send", &json!({ "thread_id": live.t, "text": text }), 200)
                    .await;
                assert_eq!(v["state"], json!("rejected"), "{host:?} {agent:?} {text:?}");
                assert!(v["reject_reason"].is_string(), "{v}");
                let id: InputId = serde_json::from_value(v["input_id"].clone()).unwrap();
                assert_eq!(rig.input(id).state, InputState::Rejected);
                let moves: Vec<_> = rig
                    .of_kind(live.t, "input_state_changed")
                    .into_iter()
                    .filter(|p| p["input_id"] == json!(id))
                    .collect();
                assert_eq!(moves.len(), 1);
                assert_eq!(moves[0]["by"], json!("human"));
            }
            // An agent token's input is created by its thread.
            let agent_token = rig
                .srv
                .token(TokenKind::Agent, Some(live.t), Some(live.l))
                .await;
            let r = rig
                .cmd_as(
                    &agent_token,
                    "send",
                    cid(),
                    &json!({ "thread_id": live.t, "text": "from an agent" }),
                )
                .await;
            assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
            let id: InputId = serde_json::from_value(body(&r)["input_id"].clone()).unwrap();
            assert_eq!(rig.input(id).created_by, Actor::Thread(live.t));
        }
        rig.stop().await;
    }
}

/// The bytes `encode` gives `text` on `agent`'s row: the text part and the submit part.
fn parts(agent: Agent, host: HostOs, text: &str) -> Vec<Vec<u8>> {
    let e = encode(agent, host, text, Purpose::Send).unwrap();
    vec![e.text, e.submit.unwrap().bytes.to_vec()]
}

/// PR10b-CL-2: each row's unit reaches the child as one write call per `Bytes` part, the
/// text from `encode` at write time over the stored text (a blob for a long one).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unit_shape() {
    let texts = [
        "two lines\nとエモジ 😀".to_owned(),
        // About 70 KiB: stored as a blob.
        "日本語のテキスト".repeat(70 * 1024 / 24 + 1),
    ];
    for (agent, host) in [
        (Agent::Claude, HostOs::Macos),
        (Agent::Codex, HostOs::Macos),
        (Agent::Codex, HostOs::Windows),
    ] {
        for text in &texts {
            let rig = SRig::on(host, |_| {}).await;
            let live = rig.bound(agent).await;
            let id = rig.send(&live, text).await;
            rig.until_input(id, InputState::Sent).await;
            assert_eq!(
                live.child.writes(),
                parts(agent, host, text),
                "{agent:?} {host:?} {}",
                text.len()
            );
            if text.len() > chaldea_core::payload::INLINE_LIMIT_BYTES {
                assert!(matches!(
                    rig.input(id).text,
                    chaldea_core::payload::Content::Blob { .. }
                ));
            }
            rig.stop().await;
        }
    }
}

/// PR10b-CL-19: a Claude `spawn --prompt` or `resume --prompt` creates a `queued` input in the
/// reservation job, after `launch_reserved` and before `launch_started`, written once the gate
/// first opens.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claude_first_prompt() {
    let long = "日本語".repeat(70 * 1024 / 9 + 1);
    for text in ["first", long.as_str()] {
        let rig = SRig::new().await;
        let (live, v) = rig.spawn(Agent::Claude, Some(text)).await;
        let id: InputId = serde_json::from_value(v["input_id"].clone()).unwrap();
        let row = rig.input(id);
        assert_eq!(
            (row.state, row.encoding, row.launch_id),
            (
                InputState::Queued,
                InputEncoding::BracketedPaste,
                Some(live.l)
            )
        );
        assert!(
            !rig.pty
                .spawns()
                .iter()
                .any(|s| s.argv.iter().any(|a| a.to_string_lossy().contains("first")))
        );
        let kinds = rig.kinds(live.t);
        let at = |k: &str| kinds.iter().position(|x| x == k).unwrap();
        assert!(at("launch_reserved") < at("input_created"), "{kinds:?}");
        assert!(at("input_created") < at("launch_started"), "{kinds:?}");
        rig.paste(&live, true).await;
        rig.hook(&live, "SessionStart", &json!({})).await;
        rig.until_input(id, InputState::Sent).await;
        assert_eq!(
            live.child.writes(),
            parts(Agent::Claude, HostOs::Macos, text)
        );
        // Resume with a prompt.
        let r = rig.cmd("stop", &json!({ "thread_id": live.t })).await;
        assert_eq!(r.status, 200);
        let v = rig
            .cmd_ok(
                "resume",
                &json!({ "thread_id": live.t, "expected_launch_id": live.l, "prompt": text }),
                200,
            )
            .await;
        let l2 = serde_json::from_value(v["launch_id"].clone()).unwrap();
        let live2 = rig.adopt(live.t, l2, Agent::Claude);
        let id2: InputId = serde_json::from_value(v["input_id"].clone()).unwrap();
        assert_eq!(rig.input(id2).state, InputState::Queued);
        rig.paste(&live2, true).await;
        let mut h = rig.hook_of(&live2, "SessionStart", &json!({}));
        // A resumed Claude keeps the thread's session.
        h.body = serde_json::to_vec(&json!({
            "hook_event_name": "SessionStart",
            "session_id": live.session,
            "transcript_path": null,
        }))
        .unwrap();
        assert_eq!(rig.post(&h).await.status, 200);
        rig.until_input(id2, InputState::Sent).await;
        assert_eq!(
            live2.child.writes(),
            parts(Agent::Claude, HostOs::Macos, text)
        );
        rig.stop().await;
    }
}

/// PR10b-CL-14: `ready` records `owner_asserted` with the basis sampled in its effect only for
/// the current, bound, `running` launch of an open thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ready() {
    let rig = SRig::new().await;
    let (live, _) = rig.spawn(Agent::Codex, None).await;
    let refuse = |args: Value, code: &'static str| {
        let rig = &rig;
        async move {
            let v = rig.cmd_ok("ready", &args, 409).await;
            assert_eq!(v["code"], json!(code), "{args}");
        }
    };
    let n = rig.events(live.t).len();
    refuse(json!({ "thread_id": live.t }), "not_bound").await;
    rig.paste(&live, true).await;
    rig.hook(&live, "SessionStart", &json!({})).await;
    let n2 = rig.events(live.t).len();
    assert!(n2 > n);
    refuse(
        json!({ "thread_id": live.t, "expected_launch_id": chaldea_core::ids::LaunchId::from_u128(7) }),
        "launch_changed",
    )
    .await;
    assert_eq!(rig.events(live.t).len(), n2, "nothing written");
    // A VT reply answered before `ready` is inside its basis.
    live.child.emit(b"\x1b[6n");
    rig.until("the reply was written", || !live.child.input().is_empty())
        .await;
    let v = rig
        .cmd_ok(
            "ready",
            &json!({ "thread_id": live.t, "expected_launch_id": live.l }),
            200,
        )
        .await;
    assert_eq!(v["launch_id"], json!(live.l));
    let row = rig.launch(live.l);
    let basis = row.write_basis.unwrap();
    assert!(basis.position >= 1, "the reply's position: {basis:?}");
    assert_eq!(
        rig.thread(live.t).state.turn,
        chaldea_core::state::TurnState::Idle
    );
    let ev = rig.of_kind(live.t, "readiness_observed");
    assert_eq!(
        (ev[0]["evidence"].clone(), ev[0]["by"].clone()),
        (json!("owner_asserted"), json!("human"))
    );
    let id = rig.send(&live, "go").await;
    rig.until_input(id, InputState::Sent).await;
    // Stopped: no live launch; closed: thread_closed.
    rig.cmd_ok("stop", &json!({ "thread_id": live.t }), 200)
        .await;
    refuse(json!({ "thread_id": live.t }), "no_live_launch").await;
    rig.cmd_ok(
        "close",
        &json!({ "thread_id": live.t, "expected_launch_id": live.l }),
        200,
    )
    .await;
    refuse(json!({ "thread_id": live.t }), "thread_closed").await;
    rig.stop().await;
}

/// PR10b-CL-18: `input_cancel` acts only on `queued` and `input_resolve` only on
/// `unconfirmed` inputs; an agent cancels only inputs it created (403 on a readable thread,
/// 404 on another).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_commands() {
    let rig = SRig::new().await;
    let (live, _) = rig.spawn(Agent::Codex, None).await;
    let q = rig.send(&live, "queued one").await;
    // `input_resolve` on a queued input: input_state.
    let v = rig
        .cmd_ok(
            "input_resolve",
            &json!({ "input_id": q, "delivered": true }),
            409,
        )
        .await;
    assert_eq!(v["code"], json!("input_state"));
    // Another thread's agent token cannot read this thread: 404. This thread's agent token on
    // an owner-created input: 403; on its own input: cancelled.
    let (other, _) = rig.spawn(Agent::Codex, None).await;
    let stranger = rig
        .srv
        .token(TokenKind::Agent, Some(other.t), Some(other.l))
        .await;
    let r = rig
        .cmd_as(&stranger, "input_cancel", cid(), &json!({ "input_id": q }))
        .await;
    assert_eq!(r.status, 404, "{}", String::from_utf8_lossy(&r.body));
    let agent = rig
        .srv
        .token(TokenKind::Agent, Some(live.t), Some(live.l))
        .await;
    let r = rig
        .cmd_as(&agent, "input_cancel", cid(), &json!({ "input_id": q }))
        .await;
    assert_eq!(r.status, 403, "{}", String::from_utf8_lossy(&r.body));
    let r = rig
        .cmd_as(
            &agent,
            "send",
            cid(),
            &json!({ "thread_id": live.t, "text": "mine" }),
        )
        .await;
    let mine: InputId = serde_json::from_value(body(&r)["input_id"].clone()).unwrap();
    let r = rig
        .cmd_as(&agent, "input_cancel", cid(), &json!({ "input_id": mine }))
        .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(rig.input(mine).state, InputState::Cancelled);
    // The owner cancels a queued input; a second cancel is input_state.
    let v = rig
        .cmd_ok("input_cancel", &json!({ "input_id": q }), 200)
        .await;
    assert_eq!(v["state"], json!("cancelled"));
    let v = rig
        .cmd_ok("input_cancel", &json!({ "input_id": q }), 409)
        .await;
    assert_eq!(v["code"], json!("input_state"));
    // An unconfirmed input resolves; a sent one does not cancel.
    rig.paste(&live, true).await;
    rig.hook(&live, "SessionStart", &json!({})).await;
    rig.cmd_ok("ready", &json!({ "thread_id": live.t }), 200)
        .await;
    let s = rig.send(&live, "to be unconfirmed").await;
    rig.until_input(s, InputState::Sent).await;
    let v = rig
        .cmd_ok("input_cancel", &json!({ "input_id": s }), 409)
        .await;
    assert_eq!(v["code"], json!("input_state"));
    rig.clock
        .advance(super::super::ACK_WINDOW + std::time::Duration::from_secs(1));
    rig.tick().await;
    rig.until_input(s, InputState::Unconfirmed).await;
    let v = rig
        .cmd_ok(
            "input_resolve",
            &json!({ "input_id": s, "delivered": false }),
            200,
        )
        .await;
    assert_eq!(v["state"], json!("dismissed"));
    assert!(
        !rig.thread(live.t)
            .state
            .attention
            .contains(&chaldea_core::state::AttentionCause::InputUnconfirmed { input_id: s })
    );
    rig.stop().await;
}

/// PR10b-CL-17: a second request with the same `command_id` replays the first's reply, and
/// one `command_id` writes one effect, for each of the five commands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicates_write_once() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let id = cid();
    let args = json!({ "thread_id": live.t, "text": "once" });
    let (a, b) = tokio::join!(
        rig.cmd_as(&rig.owner, "send", id, &args),
        rig.cmd_as(&rig.owner, "send", id, &args)
    );
    let ok = if a.status == 200 { &a } else { &b };
    assert_eq!(ok.status, 200);
    let input: InputId = serde_json::from_value(body(ok)["input_id"].clone()).unwrap();
    rig.until_input(input, InputState::Sent).await;
    let replay = rig.cmd_as(&rig.owner, "send", id, &args).await;
    assert_eq!((replay.status, &replay.body), (200, &ok.body));
    assert_eq!(rig.of_kind(live.t, "input_created").len(), 1);
    // ready
    rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "once" }))
        .await;
    let id = cid();
    let args = json!({ "thread_id": live.t });
    let (a, b) = tokio::join!(
        rig.cmd_as(&rig.owner, "ready", id, &args),
        rig.cmd_as(&rig.owner, "ready", id, &args)
    );
    assert!(a.status == 200 || b.status == 200, "{a:?} {b:?}");
    let replay = rig.cmd_as(&rig.owner, "ready", id, &args).await;
    assert_eq!(replay.status, 200);
    let owner_asserted = rig
        .of_kind(live.t, "readiness_observed")
        .into_iter()
        .filter(|p| p["evidence"] == json!("owner_asserted"))
        .count();
    assert_eq!(owner_asserted, 1);
    // key
    let id = cid();
    let args = json!({ "thread_id": live.t, "launch_id": live.l, "keys": ["escape"] });
    let (a, b) = tokio::join!(
        rig.cmd_as(&rig.owner, "key", id, &args),
        rig.cmd_as(&rig.owner, "key", id, &args)
    );
    assert!(a.status == 200 || b.status == 200, "{a:?} {b:?}");
    let replay = rig.cmd_as(&rig.owner, "key", id, &args).await;
    assert_eq!(replay.status, 200);
    assert_eq!(rig.of_kind(live.t, "human_write").len(), 1);
    // input_cancel and input_resolve
    let q = rig.send(&live, "stays queued").await;
    let id = cid();
    let args = json!({ "input_id": q });
    let (a, b) = tokio::join!(
        rig.cmd_as(&rig.owner, "input_cancel", id, &args),
        rig.cmd_as(&rig.owner, "input_cancel", id, &args)
    );
    assert!(a.status == 200 || b.status == 200, "{a:?} {b:?}");
    assert_eq!(
        rig.cmd_as(&rig.owner, "input_cancel", id, &args)
            .await
            .status,
        200
    );
    let moves = |i: InputId| {
        rig.of_kind(live.t, "input_state_changed")
            .into_iter()
            .filter(|p| p["input_id"] == json!(i))
            .count()
    };
    assert_eq!(moves(q), 1);
    rig.stop().await;
}

/// PR10b-CL-17: `send`'s input and its receipt commit in one job: after a crash right after
/// that commit, the replay with the same `command_id` returns the same `input_id`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_receipt_atomic() {
    let rig = SRig::new().await;
    let (live, _) = rig.spawn(Agent::Codex, None).await;
    let id = cid();
    let args = json!({ "thread_id": live.t, "text": "kept" });
    let first = rig.cmd_as(&rig.owner, "send", id, &args).await;
    assert_eq!(first.status, 200);
    let input: InputId = serde_json::from_value(body(&first)["input_id"].clone()).unwrap();
    let SRig { rig, .. } = rig;
    let owner = rig.owner.clone();
    let (_, store) = rig.srv.into_store(std::time::Duration::from_secs(10)).await;
    drop(rig.sup);
    let rig2 = crate::supervisor::tests::Rig::on(store, HostOs::Macos, |_| {}).await;
    let replay = rig2.cmd_as(&owner, "send", id, &args).await;
    assert_eq!(
        replay.status,
        200,
        "{}",
        String::from_utf8_lossy(&replay.body)
    );
    assert_eq!(body(&replay)["input_id"], json!(input));
    let t = live.t;
    let n = rig2.srv.store.read(move |r| r.inputs(t, &ALL_STATES)).len();
    assert_eq!(n, 1);
    rig2.srv.stop().await;
}
