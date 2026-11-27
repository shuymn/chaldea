//! The Claude adapter's hook side (C1b) through `ClaudeDecoder` and the real `/hook` route:
//! the captured fixtures (C1-3), their hygiene, prompt forms (C1-5), binding and readiness
//! (C1-2), session switches (C1-14), the card rule (C1-7), the ack window (C1-6), the resume
//! rebind (C1-10), the stall before `SessionStart` (C1-13), and a caught parser panic (C1-4).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chaldea_core::adapter::HostOs;
use chaldea_core::event::TypedEvent;
use chaldea_core::ids::{Agent, ApprovalId, InputId};
use chaldea_core::input::InputState;
use chaldea_core::input::encode::{Purpose, encode, expected_prompt, prompt_matches};
use chaldea_core::payload::{ApprovalOutcome, Content, Encoding, InputEncoding};
use serde_json::{Value, json};
use sha2::Digest as _;

use super::human::{key, outcome};
use super::outcomes::tick_at;
use super::{Live, SRig};
use crate::supervisor::send::claude::{Activity, ClaudeDecoder, Parsed, parse};
use crate::supervisor::send::fake_decoder::FakeDecoder;
use crate::supervisor::send::{ACK_WINDOW, Decoders, HookFact};
use crate::supervisor::tests::support::hook::Hook;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/claude");
const VERSION: &str = "2.1.331";

/// One captured hook: its case, event, body, and meta.
struct Fixture {
    case: String,
    event: String,
    json: Value,
    meta: Value,
}

fn version_dir() -> PathBuf {
    Path::new(FIXTURES).join(VERSION)
}

fn read_json(p: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap_or_else(|e| panic!("{p:?}: {e}"))
}

fn sorted(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    v.sort();
    v
}

/// Every captured hook, by case and file name.
fn fixtures() -> Vec<Fixture> {
    let mut out = Vec::new();
    for case in sorted(&version_dir()).into_iter().filter(|p| p.is_dir()) {
        for f in sorted(&case) {
            let name = f.file_name().unwrap().to_str().unwrap().to_owned();
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            if stem.ends_with(".meta") {
                continue;
            }
            let event = stem.rsplit_once('-').unwrap().0.to_owned();
            out.push(Fixture {
                case: case.file_name().unwrap().to_str().unwrap().to_owned(),
                event,
                json: read_json(&f),
                meta: read_json(&case.join(format!("{stem}.meta.json"))),
            });
        }
    }
    assert!(!out.is_empty());
    out
}

fn fixture(case: &str, file: &str) -> Fixture {
    fixtures()
        .into_iter()
        .find(|f| f.case == case && format!("{}-1", f.event) == file)
        .unwrap_or_else(|| panic!("{case}/{file}"))
}

/// `f` as a hook of `live`: its body with `session_id` set to `session`.
fn hook_with(rig: &SRig, live: &Live, f: &Fixture, session: &str) -> Hook {
    let mut h = rig.hook_of(live, &f.event, &json!({}));
    let mut j = f.json.clone();
    j["session_id"] = session.into();
    h.body = serde_json::to_vec(&j).unwrap();
    h
}

/// `f` as a hook of `live` in `live`'s own session.
fn own(rig: &SRig, live: &Live, f: &Fixture) -> Hook {
    hook_with(rig, live, f, &live.session)
}

/// Posts `h` for `live`, requiring 200.
async fn post(rig: &SRig, live: &Live, h: &Hook) {
    let r = rig.post_for(live, h).await;
    assert_eq!(r.status, 200, "{r:?}");
}

/// The fact as a fixture meta writes it.
fn fact_json(f: &HookFact) -> Value {
    match f {
        HookFact::SessionStart => json!("session_start"),
        HookFact::Stop => json!("stop"),
        HookFact::Other => json!("other"),
        HookFact::PromptSubmitted { prompt } => json!({ "prompt_submitted": { "prompt": prompt } }),
        HookFact::CardOpened {
            tool_name,
            tool_input,
            turn_ref,
            correlation: None,
        } => json!({ "card_opened": {
            "tool_name": tool_name, "tool_input": tool_input, "turn_ref": turn_ref,
        } }),
        HookFact::CardEvidence {
            tool_name,
            tool_input,
            tool_use_id,
        } => json!({ "card_evidence": {
            "tool_name": tool_name, "tool_input": tool_input, "tool_use_id": tool_use_id,
        } }),
        other => panic!("not a Claude fact: {other:?}"),
    }
}

fn activity_json(a: &Activity) -> Value {
    json!({
        "kind": "tool_activity",
        "phase": a.phase,
        "tool_name": a.tool_name,
        "tool_use_id": a.tool_use_id,
        "turn_ref": a.turn_ref,
        "input": a.input,
        "output": a.output,
    })
}

fn inline_json(c: &Content) -> Value {
    match c {
        Content::Inline {
            inline,
            encoding: Encoding::Utf8,
        } => serde_json::from_str(inline).unwrap(),
        other => panic!("not inline JSON: {other:?}"),
    }
}

/// The `tool_activity` event `e` as a fixture meta writes it.
fn event_json(e: &crate::hook::HookEvent) -> Value {
    let TypedEvent::ToolActivity(t) = &e.event else {
        panic!("not a tool_activity: {:?}", e.event);
    };
    assert_eq!(e.name, "tool_activity");
    json!({
        "kind": "tool_activity",
        "phase": t.phase,
        "tool_name": t.tool_name,
        "tool_use_id": t.tool_use_id,
        "turn_ref": t.turn_ref,
        "input": inline_json(&t.input),
        "output": t.output.as_ref().map(inline_json),
    })
}

/// C1-3: every fixture, through `HookDecoder::decode` with a launch row and a read
/// transaction of the store, gives its recorded fact and events; `parse` alone gives the
/// same, and no registered event decodes to `Other` without an event.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fixtures_decode() {
    use crate::supervisor::send::HookDecoder;
    let rig = claude_rig().await;
    let (live, _) = rig.spawn(Agent::Claude, None).await;
    let launch = rig.launch(live.l);
    let decoder = ClaudeDecoder::default();
    for f in fixtures() {
        let at = format!("{}/{}", f.case, f.event);
        let body = serde_json::to_vec(&f.json).unwrap();
        let hook = crate::hook::AcceptedHook {
            thread: live.t,
            launch: &launch,
            emit_id: uuid::Uuid::new_v4(),
            receipt_ordinal: 1,
            event_name: &f.event,
            session_id: &live.session,
            json: &f.json,
            body: &body,
            intake: None,
            attempt: Some(1),
            intake_reliable: true,
            binding_ordinal: 1,
            promoted: false,
            turn_watermark: 0,
        };
        let (fact, events) = rig.srv.store.read(|tx| decoder.decode(tx, &hook));
        assert_eq!(fact_json(&fact), f.meta["fact"], "{at}");
        let events: Vec<Value> = events.iter().map(event_json).collect();
        assert_eq!(Value::Array(events.clone()), f.meta["events"], "{at}");
        assert!(
            fact != HookFact::Other || !events.is_empty(),
            "{at}: a registered event read as nothing"
        );
        let Parsed::Read { fact: p, activity } = parse(&f.event, &f.json) else {
            panic!("{at}: malformed");
        };
        assert_eq!(p, fact, "{at}");
        assert_eq!(
            Value::Array(activity.iter().map(activity_json).collect()),
            Value::Array(events),
            "{at}"
        );
    }
    rig.stop().await;
}

/// C1-3: a registered hook missing a field its fact needs is `schema_mismatch`; an
/// unregistered event is `Other`.
#[test]
fn malformed_hooks() {
    let d = ClaudeDecoder::default();
    for (event, body) in [
        ("UserPromptSubmit", json!({})),
        ("UserPromptSubmit", json!({ "prompt": 1 })),
        ("PermissionRequest", json!({ "tool_input": {} })),
        ("PreToolUse", json!({ "tool_name": "Bash" })),
        ("PostToolUse", json!({ "tool_name": "Bash" })),
    ] {
        assert_eq!(
            d.read(event, &body),
            (HookFact::SchemaMismatch, None),
            "{event} {body}"
        );
    }
    assert_eq!(d.read("Notification", &json!({})), (HookFact::Other, None));
}

/// The fixtures stay publishable and consistent: no home directory, user temp directory, or
/// token; each body (a hook's `.json`, a transcript's `.jsonl`) has its meta and the reverse; every body is a registered event named as
/// its file; metas carry only their keys; a recorded prompt is its meta's expected form, and
/// every sent prompt is one of [`corpus`].
#[test]
fn fixture_hygiene() {
    const BANNED: [&str; 8] = [
        "/Users/",
        "/home/",
        "C:\\Users\\",
        "C:\\\\Users\\\\",
        "/var/folders/",
        "/private/tmp/",
        "CHALDEA_HOOK_TOKEN",
        "Bearer ",
    ];
    let mut stack = vec![PathBuf::from(FIXTURES)];
    let mut files = 0;
    while let Some(p) = stack.pop() {
        if p.is_dir() {
            stack.extend(sorted(&p));
            continue;
        }
        files += 1;
        let text = String::from_utf8(std::fs::read(&p).unwrap()).unwrap();
        for b in BANNED {
            assert!(!text.contains(b), "{p:?} contains {b:?}");
        }
        for prefix in ["chd_", "chdt_", "chdp_"] {
            let found = text.match_indices(prefix).any(|(i, _)| {
                text[i + prefix.len()..]
                    .chars()
                    .take(43)
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                    .count()
                    == 43
            });
            assert!(!found, "{p:?} holds a {prefix} token");
        }
        let name = p.file_name().unwrap().to_str().unwrap();
        if let Some(stem) = name.strip_suffix(".meta.json") {
            assert!(
                p.with_file_name(format!("{stem}.json")).is_file()
                    || p.with_file_name(format!("{stem}.jsonl")).is_file(),
                "{p:?} has no body"
            );
        } else if let Some(stem) = name
            .strip_suffix(".json")
            .or_else(|| name.strip_suffix(".jsonl"))
        {
            assert!(
                p.with_file_name(format!("{stem}.meta.json")).is_file(),
                "{p:?} has no meta"
            );
        } else {
            assert_eq!(name, "README.md", "{p:?}");
        }
    }
    assert!(files > 0);
    for f in fixtures() {
        let at = format!("{}/{}", f.case, f.event);
        assert!(
            chaldea_core::launch::CLAUDE_HOOK_EVENTS.contains(&f.event.as_str()),
            "{at}"
        );
        assert_eq!(f.json["hook_event_name"], json!(f.event), "{at}");
        let keys: Vec<&str> = f
            .meta
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let prompt = f.event == "UserPromptSubmit";
        for k in &keys {
            let allowed = match *k {
                "fact" | "events" | "route" => true,
                "sent" | "expected_prompt" => prompt,
                _ => false,
            };
            assert!(allowed, "{at}: meta key {k}");
        }
        for k in ["fact", "events", "route"] {
            assert!(keys.contains(&k), "{at}: no {k}");
        }
        assert!(
            ["bind", "interpret", "foreign_session"].contains(&f.meta["route"].as_str().unwrap()),
            "{at}"
        );
        if prompt {
            assert_eq!(f.json["prompt"], f.meta["expected_prompt"], "{at}");
            match f.meta["sent"].as_str() {
                Some(sent) => assert!(
                    corpus().iter().any(|(c, t)| *c == f.case && *t == sent),
                    "{at}: {sent:?} is not a corpus prompt"
                ),
                // Only the subagent's completion, which Claude generated, has no sent text:
                // `sent` is `null`, and the body is the one captured.
                None => {
                    assert!(keys.contains(&"sent") && f.meta["sent"].is_null(), "{at}");
                    assert_eq!(at, "subagent/UserPromptSubmit", "{at}");
                    let p = f.json["prompt"].as_str().unwrap();
                    assert_eq!(
                        sha2::Sha256::digest(p.as_bytes())
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<String>(),
                        SUBAGENT_NOTIFICATION_SHA256,
                        "{at}: {p:?}"
                    );
                }
            }
        }
    }
}

/// Every prompt the captures sent, by case: synthetic instructions and the corpus of the
/// README, written here independently of the fixtures.
/// The SHA-256 of the one generated prompt the captures hold: the `<task-notification>` of
/// `subagent`'s completion (1,016 bytes).
const SUBAGENT_NOTIFICATION_SHA256: &str =
    "10809841b79e9eec15e8f50bc7c3f2a9999f6b99dd433b8a0d5c2f2a78cc72f4";

fn corpus() -> Vec<(&'static str, String)> {
    let lit = |c: &'static str, t: &str| (c, t.to_owned());
    vec![
        lit("plain-turn", "Reply with exactly the word ok."),
        lit("clear", "Reply with exactly the word cleared."),
        lit("resume-in-session", "Reply with exactly the word resumed."),
        lit("resume", "Reply with exactly the word again."),
        lit(
            "permission-approved",
            "Use the Bash tool to run exactly this command and nothing else: printf c1b > c1b-approved.txt",
        ),
        lit(
            "permission-denied",
            "Use the Bash tool to run exactly this command and nothing else: printf c1b > c1b-denied.txt",
        ),
        lit(
            "subagent",
            "Use the Agent tool to start one general-purpose subagent whose only task is to use \
             the Bash tool to run exactly: printf sub > c1b-sub.txt . Do nothing else yourself.",
        ),
        lit("corpus-fa19", "\u{fa19}"),
        lit("corpus-ends", "\n  both ends  \n"),
        lit("corpus-tab", "a\tb"),
        (
            "corpus-wrapped",
            "e\u{301}\t\u{304b}\u{3099} line\n".repeat(300),
        ),
        ("corpus-wrapped-small", "y".repeat(801)),
        lit("corpus-del", "a\u{7f}b"),
        lit("corpus-c1", "a\u{80}b\u{9f}c"),
        lit("corpus-hash", "#zz corpus"),
    ]
}

/// The captured prompts: `(case, sent, recorded)` for every `UserPromptSubmit` the daemon sent.
fn sent_prompts() -> Vec<(String, String, String)> {
    fixtures()
        .into_iter()
        .filter(|f| f.event == "UserPromptSubmit" && f.meta["sent"].is_string())
        .map(|f| {
            (
                f.case.clone(),
                f.meta["sent"].as_str().unwrap().to_owned(),
                f.meta["expected_prompt"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// C1-5: each captured prompt is `prompt_matches` its sent text, the two wrapped ones (4,800
/// bytes, and 801 bytes, which Claude folds by its length in characters) included; the others
/// equal `expected_prompt` literally. No sent text matches another case's recorded form.
#[test]
fn captured_prompt_forms() {
    let p = InputEncoding::BracketedPaste;
    let all = sent_prompts();
    let wrapped = ["corpus-wrapped", "corpus-wrapped-small"];
    for w in wrapped {
        assert!(all.iter().any(|(c, ..)| c == w), "{w}");
    }
    for (case, sent, recorded) in &all {
        assert!(prompt_matches(Agent::Claude, p, sent, recorded), "{case}");
        if !wrapped.contains(&case.as_str()) {
            assert_eq!(
                expected_prompt(Agent::Claude, p, sent).as_deref(),
                Some(recorded.as_str()),
                "{case}"
            );
        }
        for (other, _, r) in &all {
            if other != case && r != recorded {
                assert!(
                    !prompt_matches(Agent::Claude, p, sent, r),
                    "{case} vs {other}"
                );
            }
        }
    }
}

/// A rig whose Claude launches use `ClaudeDecoder` (Codex keeps `FakeDecoder`).
async fn claude_rig() -> SRig {
    SRig::with(claude_decoders).await
}

fn claude_decoders(c: &mut crate::supervisor::SupervisorConfig) {
    c.decoders = Decoders {
        claude: Arc::new(ClaudeDecoder::default()),
        codex: Arc::new(FakeDecoder),
    };
}

/// A Claude launch, DECSET 2004 on, bound by the captured `SessionStart`.
async fn bound(rig: &SRig) -> Live {
    let (live, _) = rig.spawn(Agent::Claude, None).await;
    rig.paste(&live, true).await;
    post(
        rig,
        &live,
        &own(rig, &live, &fixture("plain-turn", "SessionStart-1")),
    )
    .await;
    live
}

fn parts(text: &str) -> Vec<Vec<u8>> {
    let e = encode(Agent::Claude, HostOs::Macos, text, Purpose::Send).unwrap();
    vec![e.text, e.submit.unwrap().bytes.to_vec()]
}

/// The `raw_unknown` reasons of `live`'s thread, in order.
fn raw_reasons(rig: &SRig, live: &Live) -> Vec<String> {
    rig.of_kind(live.t, "raw_unknown")
        .into_iter()
        .map(|p| p["reason"].as_str().unwrap().to_owned())
        .collect()
}

/// C1-5: each captured corpus prompt, sent by `send`, is written once (one paste, one
/// submit) and its captured `UserPromptSubmit` acks it; the first prompt of `spawn --prompt`
/// likewise.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corpus_prompts_ack() {
    for f in fixtures()
        .into_iter()
        .filter(|f| f.event == "UserPromptSubmit" && f.meta["sent"].is_string())
    {
        let sent = f.meta["sent"].as_str().unwrap().to_owned();
        let rig = claude_rig().await;
        let live = bound(&rig).await;
        let id = rig.send(&live, &sent).await;
        rig.until_input(id, InputState::Sent).await;
        post(&rig, &live, &own(&rig, &live, &f)).await;
        assert_eq!(rig.input(id).state, InputState::Acked, "{}", f.case);
        assert_eq!(live.child.writes(), parts(&sent), "{}", f.case);
        assert_eq!(rig.of_kind(live.t, "user_message").len(), 1, "{}", f.case);
        rig.stop().await;
    }
    let f = fixture("plain-turn", "UserPromptSubmit-1");
    let sent = f.meta["sent"].as_str().unwrap().to_owned();
    let rig = claude_rig().await;
    let (live, v) = rig.spawn(Agent::Claude, Some(&sent)).await;
    let id: InputId = serde_json::from_value(v["input_id"].clone()).unwrap();
    rig.paste(&live, true).await;
    post(
        &rig,
        &live,
        &own(&rig, &live, &fixture("plain-turn", "SessionStart-1")),
    )
    .await;
    rig.until_input(id, InputState::Sent).await;
    post(&rig, &live, &own(&rig, &live, &f)).await;
    assert_eq!(rig.input(id).state, InputState::Acked);
    assert_eq!(live.child.writes(), parts(&sent));
    rig.stop().await;
}

/// C1 D16: the two captured prompts Claude ran as commands (README "Prompt corpus"), and a
/// slash command after a space, which Claude trims, are refused at `send` (`rejected`,
/// `lossy_text`) with zero bytes written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn command_prefix_refused() {
    let rig = claude_rig().await;
    let live = bound(&rig).await;
    for text in ["!echo corpus", "/zzexample corpus", " /clear"] {
        let r = rig.send_text(&live, text).await;
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(
            (v["state"].clone(), v["reject_reason"].clone()),
            (json!("rejected"), json!("lossy_text")),
            "{text}"
        );
        let id: InputId = serde_json::from_value(v["input_id"].clone()).unwrap();
        assert_eq!(rig.input(id).state, InputState::Rejected);
    }
    rig.pass(&live).await;
    assert!(live.child.input().is_empty(), "no byte");
    rig.stop().await;
}

/// C1-2: the captured `SessionStart` binds the launch and, with intake facts, records
/// `session_start` readiness; without them it binds and records none, so a send waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_start_binds() {
    let rig = claude_rig().await;
    let live = bound(&rig).await;
    assert_eq!(
        rig.thread(live.t).session_id.as_deref(),
        Some(live.session.as_str())
    );
    assert!(
        rig.launch(live.l).readiness.is_some(),
        "{:?}",
        rig.kinds(live.t)
    );
    rig.stop().await;

    let rig = claude_rig().await;
    let (live, _) = rig.spawn(Agent::Claude, None).await;
    rig.paste(&live, true).await;
    let sl = rig.sl(&live);
    *crate::supervisor::send::lock(&sl.hooks.observe) = Some(Box::new(|_, _| {
        crate::supervisor::live::LaunchObservation::Unobservable
    }));
    post(
        &rig,
        &live,
        &own(&rig, &live, &fixture("plain-turn", "SessionStart-1")),
    )
    .await;
    assert!(crate::supervisor::send::lock(&sl.hooks.observe).is_none());
    assert_eq!(
        rig.thread(live.t).session_id.as_deref(),
        Some(live.session.as_str())
    );
    assert!(rig.launch(live.l).readiness.is_none());
    let id = rig.send(&live, "waits").await;
    rig.pass(&live).await;
    assert_eq!(rig.input(id).state, InputState::Queued);
    assert!(live.child.input().is_empty(), "no byte");
    rig.stop().await;
}

/// Spawns Claude held at `BeforePublish` and adopts the launch: the held point, the spawn's
/// reply, and the launch.
async fn held_before_publish(
    rig: &SRig,
) -> (
    crate::supervisor::tests::Armed,
    tokio::task::JoinHandle<crate::supervisor::tests::support::http::Reply>,
    Live,
) {
    let held = rig.pauses.arm(crate::supervisor::Pause::BeforePublish);
    let args = json!({ "agent": "claude", "cwd": rig.cwd.path().to_string_lossy() });
    let spawn = crate::supervisor::tests::support::http::spawn_send(
        &rig.srv,
        &rig.srv.command(
            "spawn",
            &rig.owner,
            crate::supervisor::tests::cid(),
            &args.to_string(),
        ),
    );
    tokio::task::block_in_place(|| held.wait());
    let (t, l) = rig.srv.store.read(|r| {
        let t = r.threads()?.pop().unwrap();
        Ok((t.thread_id, t.current_launch_id.unwrap()))
    });
    (held, spawn, rig.adopt(t, l, Agent::Claude))
}

/// SF-8: a `SessionStart` taken in after the child was created and before the launch was
/// published (a CLI that emits it at once) waits for the publication: published while the
/// intake waits, its facts are present, so it binds the launch and records `session_start`.
/// The facts deadline outlasts every bound here, so the publication cannot outlive the wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_start_before_publication() {
    let rig = SRig::with_intake_wait(super::LONG_INTAKE_WAIT, claude_decoders).await;
    let (held, spawn, live) = held_before_publish(&rig).await;
    let entry = rig.sup.entry(live.l).unwrap();
    let (waiting_tx, waiting) = std::sync::mpsc::channel();
    *crate::supervisor::lock(&entry.hooks.publication_waiting) = Some(Box::new(move || {
        let _ = waiting_tx.send(());
    }));
    let hook = own(&rig, &live, &fixture("plain-turn", "SessionStart-1"));
    let post = crate::supervisor::tests::support::http::spawn_send(&rig.srv, &hook.req(&rig.srv));
    crate::supervisor::tests::support::http::reached_sync(waiting).await;
    held.go();
    assert_eq!(spawn.await.unwrap().status, 200);
    assert_eq!(post.await.unwrap().status, 200);
    assert_eq!(
        rig.thread(live.t).session_id.as_deref(),
        Some(live.session.as_str())
    );
    assert_eq!(super::evidence::evidences(&rig, &live), ["session_start"]);
    rig.stop().await;
}

/// SF-8 (kept): the same `SessionStart` with the publication held past its facts deadline
/// has no intake facts: it binds the launch, records no readiness, and, with no event of its
/// own, is kept as `raw_unknown` `unknown_event`, so a send waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_start_before_publication_after_deadline() {
    let rig = claude_rig().await;
    let (held, spawn, live) = held_before_publish(&rig).await;
    let hook = own(&rig, &live, &fixture("plain-turn", "SessionStart-1"));
    // The intake (its ordinal, facts, and the pid wait's origin) has ended, its facts taken
    // while the launch is unpublished, before the publication is let go.
    let (after_intake, go_on) = rig.ingest.points.arm_intake(1);
    let post = crate::supervisor::tests::support::http::spawn_send(&rig.srv, &hook.req(&rig.srv));
    crate::supervisor::tests::support::http::reached(after_intake).await;
    held.go();
    go_on.send(()).unwrap();
    assert_eq!(spawn.await.unwrap().status, 200);
    assert_eq!(post.await.unwrap().status, 200);
    assert_eq!(
        rig.thread(live.t).session_id.as_deref(),
        Some(live.session.as_str())
    );
    assert!(
        rig.launch(live.l).readiness.is_none(),
        "{:?}",
        rig.kinds(live.t)
    );
    assert_eq!(raw_reasons(&rig, &live), ["unknown_event"]);
    rig.paste(&live, true).await;
    let id = rig.send(&live, "waits").await;
    rig.pass(&live).await;
    assert_eq!(rig.input(id).state, InputState::Queued);
    assert!(live.child.input().is_empty(), "no byte");
    rig.stop().await;
}

/// SF-10: a publication made after the publication wait's clock check, at the facts
/// deadline, selects the final attempt and before that attempt's read is used: the
/// `SessionStart` has its facts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn published_at_facts_deadline() {
    let rig = claude_rig().await;
    let (held, spawn, live) = held_before_publish(&rig).await;
    let entry = rig.sup.entry(live.l).unwrap();
    let e = entry.clone();
    *crate::supervisor::lock(&entry.hooks.final_wake) = Some(Box::new(move || {
        held.go();
        let end = std::time::Instant::now() + Duration::from_secs(10);
        while e.published().is_none() {
            assert!(std::time::Instant::now() < end, "published");
            std::thread::yield_now();
        }
    }));
    let hook = own(&rig, &live, &fixture("plain-turn", "SessionStart-1"));
    // The worker parked through the intake: the start commit's wake cannot have it hold the
    // state during the intake's one remaining attempt.
    let r = rig.worker_parked(&live, rig.post(&hook)).await;
    assert_eq!(r.status, 200);
    assert!(
        crate::supervisor::lock(&entry.hooks.final_wake).is_none(),
        "the final wake ran"
    );
    assert_eq!(spawn.await.unwrap().status, 200);
    assert_eq!(super::evidence::evidences(&rig, &live), ["session_start"]);
    rig.stop().await;
}

/// C1-14 (D5): `/clear` and an in-session `/resume` start another session; its hooks are
/// `foreign_session` and the launch keeps its binding, so its own session's hooks still
/// count. `/compact` keeps the session: interpreted, and not evidence again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_switch_keeps_binding() {
    for case in ["clear", "resume-in-session"] {
        let rig = claude_rig().await;
        let live = bound(&rig).await;
        for file in ["SessionStart-1", "UserPromptSubmit-1", "Stop-1"] {
            let f = fixture(case, file);
            assert_eq!(f.meta["route"], json!("foreign_session"));
            let other = f.json["session_id"].as_str().unwrap().to_owned();
            assert_ne!(other, live.session);
            post(&rig, &live, &hook_with(&rig, &live, &f, &other)).await;
        }
        assert_eq!(raw_reasons(&rig, &live), ["foreign_session"; 3], "{case}");
        assert_eq!(
            rig.thread(live.t).session_id.as_deref(),
            Some(live.session.as_str())
        );
        assert!(rig.of_kind(live.t, "user_message").is_empty(), "{case}");
        post(
            &rig,
            &live,
            &own(&rig, &live, &fixture("plain-turn", "UserPromptSubmit-1")),
        )
        .await;
        assert_eq!(rig.of_kind(live.t, "user_message").len(), 1, "{case}");
        rig.stop().await;
    }
    let rig = claude_rig().await;
    let live = bound(&rig).await;
    let f = fixture("compact", "SessionStart-1");
    assert_eq!(f.meta["route"], json!("interpret"));
    post(&rig, &live, &own(&rig, &live, &f)).await;
    // Interpreted, and with no event of its own kept as `unknown_event`.
    assert_eq!(raw_reasons(&rig, &live), ["unknown_event"]);
    assert_eq!(rig.of_kind(live.t, "readiness_observed").len(), 1);
    assert_eq!(
        rig.thread(live.t).session_id.as_deref(),
        Some(live.session.as_str())
    );
    rig.stop().await;
}

/// C1-10 (hook half): a resumed launch's captured `SessionStart` (`--resume`, same session)
/// binds the new launch to the thread's session and records readiness.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_rebinds() {
    let rig = claude_rig().await;
    let live = bound(&rig).await;
    let r = rig.cmd("stop", &json!({ "thread_id": live.t })).await;
    assert_eq!(r.status, 200);
    let v = rig
        .cmd_ok(
            "resume",
            &json!({ "thread_id": live.t, "expected_launch_id": live.l }),
            200,
        )
        .await;
    let l2 = serde_json::from_value(v["launch_id"].clone()).unwrap();
    let mut live2 = rig.adopt(live.t, l2, Agent::Claude);
    live2.session.clone_from(&live.session);
    rig.paste(&live2, true).await;
    post(
        &rig,
        &live2,
        &own(&rig, &live2, &fixture("resume", "SessionStart-1")),
    )
    .await;
    assert_eq!(
        rig.launch(l2).session_id.as_deref(),
        Some(live.session.as_str())
    );
    assert!(rig.launch(l2).readiness.is_some());
    let f = fixture("resume", "UserPromptSubmit-1");
    let id = rig.send(&live2, f.meta["sent"].as_str().unwrap()).await;
    rig.until_input(id, InputState::Sent).await;
    post(&rig, &live2, &own(&rig, &live2, &f)).await;
    assert_eq!(rig.input(id).state, InputState::Acked);
    rig.stop().await;
}

/// D11: a hook kept as `raw_unknown` carries the launch's probed CLI version.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn version_in_raw_unknown() {
    let rig = SRig::with(|c| {
        c.decoders = Decoders {
            claude: Arc::new(ClaudeDecoder::default()),
            codex: Arc::new(FakeDecoder),
        };
        let argv = crate::supervisor::tests::version::fake_program(&[
            "--version-text",
            "2.1.331 (Claude Code)",
        ]);
        c.program = Some(Arc::new(move |_| Some(argv.clone())));
    })
    .await;
    let live = bound(&rig).await;
    let f = fixture("clear", "SessionStart-1");
    let other = f.json["session_id"].as_str().unwrap().to_owned();
    post(&rig, &live, &hook_with(&rig, &live, &f, &other)).await;
    let raws = rig.of_kind(live.t, "raw_unknown");
    assert_eq!(raws.len(), 1);
    assert_eq!(
        (raws[0]["reason"].clone(), raws[0]["cli_version"].clone()),
        (json!("foreign_session"), json!("2.1.331"))
    );
    rig.stop().await;
}

/// C1-6: a Claude input not acknowledged in its ack window ends `unconfirmed`, raises its
/// attention, and is never written again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unacked_is_unconfirmed() {
    let rig = claude_rig().await;
    let live = bound(&rig).await;
    let id = rig.send(&live, "nobody hears").await;
    rig.until_input(id, InputState::Sent).await;
    rig.clock.stop();
    let sl = rig.sl(&live);
    rig.until("the window started", || {
        sl.state().sent_at.contains_key(&id)
    })
    .await;
    let t0 = sl.state().sent_at[&id];
    tick_at(&rig, &live, t0 + ACK_WINDOW).await;
    rig.until_input(id, InputState::Unconfirmed).await;
    assert!(
        rig.thread(live.t)
            .state
            .attention
            .contains(&chaldea_core::state::AttentionCause::InputUnconfirmed { input_id: id })
    );
    tick_at(&rig, &live, t0 + ACK_WINDOW * 4).await;
    assert_eq!(
        live.child.writes(),
        parts("nobody hears"),
        "no second submit"
    );
    assert_eq!(rig.input(id).state, InputState::Unconfirmed);
    rig.stop().await;
}

/// C1-13: a Claude launch that never sends `SessionStart` carries `session_start_timeout`
/// at 20 s, and its first prompt is never written, DECSET 2004 on and an early hook of its
/// session taken in included.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stall_before_session_start() {
    let rig = claude_rig().await;
    let (live, v) = rig.spawn(Agent::Claude, Some("first")).await;
    let id: InputId = serde_json::from_value(v["input_id"].clone()).unwrap();
    rig.paste(&live, true).await;
    post(
        &rig,
        &live,
        &own(&rig, &live, &fixture("plain-turn", "Stop-1")),
    )
    .await;
    let start = rig.sup.entry(live.l).unwrap().committed().unwrap().at;
    rig.sup.tick(start + Duration::from_secs(20)).await;
    rig.until("raised", || {
        rig.events(live.t)
            .iter()
            .any(|(_, p)| p.to_string().contains("session_start_timeout"))
    })
    .await;
    rig.pass(&live).await;
    assert_eq!(rig.input(id).state, InputState::Queued);
    assert!(live.child.input().is_empty(), "no byte");
    rig.stop().await;
}

/// The captured card hooks of `permission-approved`, with `prompt_id` set to `turn`.
fn turn_hooks(turn: &str) -> (Fixture, Fixture) {
    let mut pr = fixture("permission-approved", "PermissionRequest-1");
    let mut post = fixture("permission-approved", "PostToolUse-1");
    pr.json["prompt_id"] = turn.into();
    post.json["prompt_id"] = turn.into();
    (pr, post)
}

/// A turn start of `live`.
async fn start(rig: &SRig, live: &Live, turn: &str) {
    let mut f = fixture("permission-approved", "UserPromptSubmit-1");
    f.json["prompt_id"] = turn.into();
    f.json["prompt"] = turn.into();
    post(rig, live, &own(rig, live, &f)).await;
}

/// A card opened by the captured `PermissionRequest` (no `tool_use_id`).
async fn open(rig: &SRig, live: &Live, f: &Fixture) -> ApprovalId {
    let h = own(rig, live, f);
    post(rig, live, &h).await;
    ApprovalId(h.emit)
}

/// C1-7 (D15) control: the call's captured `PostToolUse`, received after the card, resolves
/// the one equal open card `ran`; its key was written before. A denial (no hook after the
/// card) leaves the card open until the turn boundary supersedes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn card_ran_and_denied() {
    let rig = claude_rig().await;
    let live = bound(&rig).await;
    start(&rig, &live, "t1").await;
    let (pr, done) = turn_hooks("t1");
    let c = open(&rig, &live, &pr).await;
    let r = key(&rig, &live, &["enter"], Some(c)).await;
    assert_eq!(
        (r.0, r.1["outcome"].clone()),
        (200, json!("written")),
        "{r:?}"
    );
    post(&rig, &live, &own(&rig, &live, &done)).await;
    assert_eq!(outcome(&rig, c), Some(ApprovalOutcome::Ran));
    assert_eq!(rig.of_kind(live.t, "tool_activity").len(), 1);
    post(
        &rig,
        &live,
        &own(&rig, &live, &fixture("plain-turn", "Stop-1")),
    )
    .await;
    // Denied: Escape, then nothing until the next turn.
    start(&rig, &live, "t2").await;
    let d = fixture("permission-denied", "PermissionRequest-1");
    let c = open(&rig, &live, &d).await;
    assert_eq!(key(&rig, &live, &["escape"], Some(c)).await.0, 200);
    assert_eq!(outcome(&rig, c), None);
    start(&rig, &live, "t3").await;
    assert_eq!(outcome(&rig, c), Some(ApprovalOutcome::Superseded));
    rig.stop().await;
}

/// C1-7 (D15): an equal `PostToolUse` resolves nothing when it was received below the turn
/// watermark (the previous turn's, committed late), below the card (taken in before it,
/// committed after), or with two equal cards open (one of two equal calls denied).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn coarse_refusals() {
    // The previous turn's `PostToolUse`, committed after the next turn's start and card.
    let rig = claude_rig().await;
    let live = bound(&rig).await;
    start(&rig, &live, "t1").await;
    let (_, late) = turn_hooks("t1");
    let (got, release) = rig.ingest.points.arm_job(rig.next_ordinal(&live));
    let h = own(&rig, &live, &late);
    let reply = crate::supervisor::tests::support::http::spawn_send(&rig.srv, &h.req(&rig.srv));
    got.await.unwrap();
    start(&rig, &live, "t2").await;
    let (pr, _) = turn_hooks("t2");
    let c = open(&rig, &live, &pr).await;
    release.send(()).unwrap();
    assert_eq!(reply.await.unwrap().status, 200);
    assert_eq!(outcome(&rig, c), None, "below the watermark");
    rig.stop().await;

    // The same turn's `PostToolUse` taken in before the card, committed after it.
    let rig = claude_rig().await;
    let live = bound(&rig).await;
    start(&rig, &live, "t1").await;
    let (pr, done) = turn_hooks("t1");
    let (got, release) = rig.ingest.points.arm_job(rig.next_ordinal(&live));
    let h = own(&rig, &live, &done);
    let reply = crate::supervisor::tests::support::http::spawn_send(&rig.srv, &h.req(&rig.srv));
    got.await.unwrap();
    let c = open(&rig, &live, &pr).await;
    release.send(()).unwrap();
    assert_eq!(reply.await.unwrap().status, 200);
    assert_eq!(outcome(&rig, c), None, "below the card");
    rig.stop().await;

    // Two equal cards, one denied with Escape: the other call's evidence resolves neither.
    let rig = claude_rig().await;
    let live = bound(&rig).await;
    start(&rig, &live, "t1").await;
    let (pr, done) = turn_hooks("t1");
    let (a, b) = (open(&rig, &live, &pr).await, open(&rig, &live, &pr).await);
    assert_eq!(key(&rig, &live, &["escape"], Some(a)).await.0, 200);
    post(&rig, &live, &own(&rig, &live, &done)).await;
    assert_eq!(
        (outcome(&rig, a), outcome(&rig, b)),
        (None, None),
        "two equal cards"
    );
    rig.stop().await;
}

/// C1-7 (D15): with the send side gone (the launch left the live map, its exit job refused,
/// so the store still holds it live and the card open), the turn watermark reads `u64::MAX`:
/// an equal `PostToolUse` taken in after the card and committed then resolves nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_send_side_resolves_nothing() {
    let rig = SRig::with(|c| {
        c.decoders = Decoders {
            claude: Arc::new(ClaudeDecoder::default()),
            codex: Arc::new(FakeDecoder),
        };
        c.exit_steps.push(Arc::new(super::turn_order::RefuseExit));
    })
    .await;
    let live = bound(&rig).await;
    rig.clock.stop();
    start(&rig, &live, "t1").await;
    let (pr, done) = turn_hooks("t1");
    let c = open(&rig, &live, &pr).await;
    let held = super::turn_order::hold(&rig, &live, own(&rig, &live, &done), true).await;
    live.child.exit(chaldea_pty_runtime::ChildExit {
        code: Some(0),
        signal: None,
    });
    let sl = rig.sl(&live);
    rig.until("the launch left the live map", || {
        rig.sup.entry(live.l).is_none()
    })
    .await;
    sl.wake.notify_one();
    rig.until("the send side is gone", || {
        rig.sup.send.launch(live.l).is_none()
    })
    .await;
    assert_eq!(
        rig.launch(live.l).exit_reason,
        None,
        "the exit is unrecorded"
    );
    assert_eq!(held.finish().await.status, 200);
    assert_eq!(outcome(&rig, c), None);
    rig.stop().await;
}

/// C1-7: two cards of different calls resolve one at a time; the turn leaves
/// `awaiting_approval` only with the last, back to `running`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cards_resolve_in_turn() {
    let rig = claude_rig().await;
    let live = bound(&rig).await;
    start(&rig, &live, "t1").await;
    let (pr1, done1) = turn_hooks("t1");
    let (mut pr2, mut done2) = turn_hooks("t1");
    for f in [&mut pr2, &mut done2] {
        f.json["tool_input"]["command"] = "printf other > other.txt".into();
    }
    let (a, b) = (open(&rig, &live, &pr1).await, open(&rig, &live, &pr2).await);
    let awaiting = |rig: &SRig| {
        super::turn_order::turn(rig, &live) == chaldea_core::state::TurnState::AwaitingApproval
    };
    assert!(awaiting(&rig));
    post(&rig, &live, &own(&rig, &live, &done2)).await;
    assert_eq!(
        (outcome(&rig, a), outcome(&rig, b)),
        (None, Some(ApprovalOutcome::Ran))
    );
    assert!(awaiting(&rig), "one card still open");
    post(&rig, &live, &own(&rig, &live, &done1)).await;
    assert_eq!(outcome(&rig, a), Some(ApprovalOutcome::Ran));
    assert_eq!(
        super::turn_order::turn(&rig, &live),
        chaldea_core::state::TurnState::Running
    );
    rig.stop().await;
}

/// C1-4 (hook half): a panic in the payload parser keeps the hook as `raw_unknown`
/// `schema_mismatch`, and the next hook is served and interpreted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parser_panic_is_schema_mismatch() {
    fn boom(event: &str, j: &Value) -> Parsed {
        assert!(event != "Stop", "the parser panicked");
        parse(event, j)
    }
    let rig = SRig::with(|c| {
        c.decoders = Decoders {
            claude: Arc::new(ClaudeDecoder::with_parse(boom)),
            codex: Arc::new(FakeDecoder),
        };
    })
    .await;
    let live = bound(&rig).await;
    post(
        &rig,
        &live,
        &own(&rig, &live, &fixture("plain-turn", "Stop-1")),
    )
    .await;
    assert_eq!(raw_reasons(&rig, &live), ["schema_mismatch"]);
    post(
        &rig,
        &live,
        &own(&rig, &live, &fixture("plain-turn", "UserPromptSubmit-1")),
    )
    .await;
    assert_eq!(rig.of_kind(live.t, "user_message").len(), 1);
    rig.stop().await;
}
