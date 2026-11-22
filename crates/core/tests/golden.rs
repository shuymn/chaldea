//! Golden round trips and forward-compatible decoding (`docs/mvp-design.md` §5 "Tests",
//! CL-14).
//!
//! Every file under `tests/golden/` is the JSON of one sample value built below. A sample
//! must serialize to its file and the file must decode back to the sample. Regenerate the
//! files after an intended wire change, from the repository root:
//!
//! ```text
//! CHALDEA_BLESS=1 cargo test -p chaldea-core --test golden
//! ```

#![allow(
    clippy::disallowed_methods,
    reason = "this test reads and writes its golden files; core itself stays Sans-IO"
)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use chaldea_core::event::{
    EventEnvelope, GenericEvent, GenericReason, Source, TranscriptPos, TypedEvent,
};
use chaldea_core::ids::{
    Actor, Agent, ApprovalId, CommandId, Epoch, EventId, EventKindTag, IdempotencyKey, InputId,
    LaunchId, MessageId, StreamId, SurfaceId, ThreadId, Timestamp, WorkspaceId, WorktreeId,
};
use chaldea_core::input::{InputReason, InputState, RejectReason};
use chaldea_core::payload::{
    ApprovalCorrelation, ApprovalOutcome, ApprovalRequested, ApprovalResolved, AssistantMessage,
    BlobRef, Component, Content, Encoding, ExitReason, HumanWrite, HumanWriteOrigin, InputCreated,
    InputEncoding, InputInitialState, InputOrigin, InputStateChanged, LaunchDeathConfirmed,
    LaunchExited, LaunchReserved, LaunchStarted, MailboxMessage, RawUnknown, RawUnknownReason,
    ReadinessEvidence, ReadinessObserved, SessionBound, SourceKind, SurfaceAction,
    SurfaceActionKind, SurfaceScope, SurfaceUpdate, TaskSet, ThreadClosed, ThreadRegistered,
    ThreadRole, ToolActivity, ToolPhase, TranscriptGeneration, UserMessage, VtCursor, VtModes,
    VtSnapshotTaken, WorktreeChanged, WorktreeState,
};
use chaldea_core::state::{
    AttentionCause, ProcessState, StatusCause, StatusChanged, TaskState, TurnState,
};
use chaldea_core::sync::{
    EventsResponse, GateEvidence, HostSnapshot, InputInfo, LaunchInfo, SessionInfo, SnapshotReason,
    StreamHeadEntry, StreamSnapshot, ThreadInfo, ThreadSnapshot, WsClientMessage, WsServerMessage,
};
use chaldea_core::{MAX_SAFE_INTEGER, SCHEMA_VERSION};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

const THREAD: ThreadId = ThreadId::from_u128(0x0193_abcd_ef01_7000_8000_0000_0000_0001);
const CHILD: ThreadId = ThreadId::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0002);
const LAUNCH: LaunchId = LaunchId::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0010);
const PREV_LAUNCH: LaunchId = LaunchId::from_u128(0x0193_0000_0000_7000_8000_0000_0000_000f);
const INPUT: InputId = InputId::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0020);
const APPROVAL: ApprovalId = ApprovalId::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0030);
const COMMAND: CommandId = CommandId::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0040);

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn bless() -> bool {
    std::env::var_os("CHALDEA_BLESS").is_some()
}

/// Checks `value` against `golden/<name>.json` both ways.
fn round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(
    name: &str,
    value: &T,
) {
    let path = golden_dir().join(format!("{name}.json"));
    let json: Value = serde_json::to_value(value).expect("serializes");
    if bless() {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut text = serde_json::to_string_pretty(&json).unwrap();
        text.push('\n');
        fs::write(&path, text).unwrap();
    }
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}; regenerate with CHALDEA_BLESS=1", path.display()));
    let file: Value = serde_json::from_str(&text).expect("golden is JSON");
    assert_eq!(
        json, file,
        "{name}: serialization differs from the golden file"
    );
    let back: T = serde_json::from_str(&text).expect("golden decodes");
    assert_eq!(&back, value, "{name}: golden file decodes to another value");
}

fn envelope(seq: u64, stream_id: StreamId, event: &TypedEvent) -> EventEnvelope {
    EventEnvelope {
        stream_id,
        seq,
        event_id: EventId::from_u128(0x0193_0000_0000_7000_8000_0000_0001_0000 + u128::from(seq)),
        schema_version: SCHEMA_VERSION,
        idempotency_key: IdempotencyKey(format!("command:{COMMAND}:{seq}")),
        kind: EventKindTag::from(event.kind()),
        thread_id: Some(THREAD),
        launch_id: Some(LAUNCH),
        command_id: Some(COMMAND),
        source: if let TypedEvent::AssistantMessage(_) = event {
            Source {
                kind: SourceKind::Transcript.into(),
                agent: Some(Agent::Claude.into()),
                cli_version: Some("2.1.326".to_owned()),
                transcript: Some(TranscriptPos {
                    generation: 2,
                    path: "/work/.claude/projects/x/s.jsonl".to_owned(),
                    file_id: "16777230:9876543".to_owned(),
                    offset_end: 18_432,
                }),
            }
        } else {
            Source {
                kind: SourceKind::Command.into(),
                agent: Some(Agent::Claude.into()),
                cli_version: Some("2.1.326".to_owned()),
                transcript: None,
            }
        },
        occurred_at: Timestamp::from("2026-11-22T09:45:00.123Z"),
        recorded_at: Timestamp::from("2026-11-22T09:45:00.125Z"),
        payload: event.to_payload().expect("payload serializes"),
    }
}

fn text(s: &str) -> Content {
    Content::text(s)
}

fn worktree() -> WorktreeChanged {
    WorktreeChanged {
        worktree_id: WorktreeId::from("a1b2c3d4"),
        repo: "/work/chaldea".to_owned(),
        workspace_id: WorkspaceId::from("ws-5f2c"),
        path: "/work/state/chaldea/wt/a1b2c3d4".to_owned(),
        branch: "chaldea/a1b2c3d4".to_owned(),
        base_ref: "main".to_owned(),
        base_commit: "cee7556d0a1b2c3d4e5f60718293a4b5c6d7e8f9".to_owned(),
        owner_thread_id: Some(CHILD),
        created_by: Actor::Thread(THREAD),
        state: WorktreeState::Active,
    }
}

fn surface() -> SurfaceUpdate {
    SurfaceUpdate {
        surface_id: SurfaceId::from("rescue-panel"),
        scope: SurfaceScope::Thread { thread_id: THREAD },
        components: vec![
            Component::Markdown {
                markdown: "**Stalled** on a trust prompt".to_owned(),
            },
            Component::List {
                items: vec![Component::Text {
                    text: "Press 1 to trust".to_owned(),
                }],
            },
            Component::TextInput {
                name: "note".to_owned(),
                label: "Note".to_owned(),
            },
            Component::VtSnapshot {
                grid: text("Do you trust this folder?"),
                cols: 80,
                rows: 24,
            },
            Component::Button {
                label: "Trust".to_owned(),
                action: SurfaceAction {
                    action_id: "trust".to_owned(),
                    kind: SurfaceActionKind::SendKey,
                    args: serde_json::json!({ "keys": "1" }),
                },
            },
        ],
    }
}

fn vt_snapshot() -> VtSnapshotTaken {
    VtSnapshotTaken {
        grid: text("> 日本語のプロンプト\n"),
        cols: 120,
        rows: 40,
        cursor: VtCursor {
            row: 0,
            col: 22,
            visible: true,
        },
        modes: VtModes {
            bracketed_paste: true,
            alternate_screen: true,
        },
    }
}

fn approval() -> ApprovalRequested {
    ApprovalRequested {
        approval_id: APPROVAL,
        launch_id: LAUNCH,
        turn_ref: Some("turn-3".to_owned()),
        tool_name: "Bash".to_owned(),
        tool_input: text(r#"{"command":"cargo test"}"#),
        correlation: Some(ApprovalCorrelation {
            tool_use_id: "toolu_01".to_owned(),
        }),
    }
}

/// One sample per kind, in `TypedEvent::KINDS` order.
fn samples() -> Vec<(StreamId, TypedEvent)> {
    let host = StreamId::Host;
    let thread = StreamId::Thread(THREAD);
    vec![
        (
            host,
            TypedEvent::ThreadRegistered(ThreadRegistered {
                thread_id: THREAD,
                agent: Agent::Claude,
                cwd: "/work/chaldea".to_owned(),
                workspace_id: WorkspaceId::from("ws-5f2c"),
                parent_thread_id: None,
                role: ThreadRole::Standard,
                task: Some("Implement PR 1".to_owned()),
            }),
        ),
        (
            host,
            TypedEvent::ThreadClosed(ThreadClosed {
                thread_id: THREAD,
                by: Actor::Human,
            }),
        ),
        (
            thread,
            TypedEvent::LaunchReserved(LaunchReserved {
                launch_id: LAUNCH,
                expected_previous: Some(PREV_LAUNCH),
                agent: Agent::Codex,
                argv_digest: "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
                    .to_owned(),
                manual: false,
                worktree_id: Some(WorktreeId::from("a1b2c3d4")),
            }),
        ),
        (
            thread,
            TypedEvent::LaunchStarted(LaunchStarted {
                launch_id: LAUNCH,
                pid: 4242,
                start_time: "1763804700".to_owned(),
                pgid_or_job: "4242".to_owned(),
                cli_version: Some("0.171.0".to_owned()),
            }),
        ),
        (
            thread,
            TypedEvent::LaunchExited(LaunchExited {
                launch_id: LAUNCH,
                code: None,
                signal: Some(15),
                reason: ExitReason::Stopped,
                death_confirmed: true,
            }),
        ),
        (
            thread,
            TypedEvent::LaunchDeathConfirmed(LaunchDeathConfirmed {
                launch_id: LAUNCH,
                method: "session_scan".to_owned(),
            }),
        ),
        (
            thread,
            TypedEvent::SessionBound(SessionBound {
                launch_id: LAUNCH,
                session_id: "0193aaaa-0000-7000-8000-000000000001".to_owned(),
                transcript_path: None,
            }),
        ),
        (
            thread,
            TypedEvent::TranscriptGeneration(TranscriptGeneration {
                generation: 2,
                path: "/work/.claude/projects/x/s.jsonl".to_owned(),
                file_id: "16777230:9876543".to_owned(),
            }),
        ),
        (
            thread,
            TypedEvent::ReadinessObserved(ReadinessObserved {
                launch_id: LAUNCH,
                evidence: ReadinessEvidence::StopSettled,
                by: Actor::Daemon,
            }),
        ),
        (
            thread,
            TypedEvent::HumanWrite(HumanWrite {
                launch_id: LAUNCH,
                origin: HumanWriteOrigin::CardKey,
                approval_id: Some(APPROVAL),
            }),
        ),
        (
            thread,
            TypedEvent::UserMessage(UserMessage {
                prompt: text("日本語と絵文字 🎌\nを送る"),
                input_id: Some(INPUT),
            }),
        ),
        (
            thread,
            TypedEvent::AssistantMessage(AssistantMessage {
                text: text("Done."),
                turn_ref: None,
            }),
        ),
        (
            thread,
            TypedEvent::ToolActivity(ToolActivity {
                phase: ToolPhase::Completed,
                tool_name: "Bash".to_owned(),
                tool_use_id: Some("toolu_01".to_owned()),
                turn_ref: Some("turn-3".to_owned()),
                input: text(r#"{"command":"cargo test"}"#),
                output: Some(Content::Blob {
                    blob: BlobRef {
                        sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                            .to_owned(),
                        size: 200_000,
                        media_type: "text/plain; charset=utf-8".to_owned(),
                    },
                    preview: "running 42 tests\u{fffd}".to_owned(),
                    preview_lossy: true,
                }),
            }),
        ),
        (
            thread,
            TypedEvent::StatusChanged(StatusChanged::Attention {
                from: vec![],
                to: vec![
                    AttentionCause::SessionStartTimeout,
                    AttentionCause::InputUnconfirmed { input_id: INPUT },
                ],
                cause: StatusCause::AttentionRaised,
            }),
        ),
        (thread, TypedEvent::ApprovalRequested(approval())),
        (
            thread,
            TypedEvent::ApprovalResolved(ApprovalResolved {
                approval_id: APPROVAL,
                outcome: ApprovalOutcome::Superseded,
            }),
        ),
        (
            thread,
            TypedEvent::InputCreated(InputCreated {
                input_id: INPUT,
                launch_id: Some(LAUNCH),
                initial_state: InputInitialState::Queued,
                text: text("テスト\r\n"),
                encoding: InputEncoding::BracketedPaste,
                origin: InputOrigin::Pwa,
                created_by: Actor::Human,
            }),
        ),
        (
            thread,
            TypedEvent::InputStateChanged(InputStateChanged {
                input_id: INPUT,
                from: InputState::Queued,
                to: InputState::Rejected,
                reason: InputReason::Rejected {
                    reject: RejectReason::ControlCharacter,
                },
                sent_after_seq: None,
                by: None,
            }),
        ),
        (
            StreamId::Thread(CHILD),
            TypedEvent::MailboxMessage(MailboxMessage {
                message_id: MessageId::from_u128(0x50),
                from_thread_id: Some(THREAD),
                to_thread_id: CHILD,
                body: text("Please rebase on main."),
                in_reply_to: None,
            }),
        ),
        (
            thread,
            TypedEvent::TaskSet(TaskSet {
                to: TaskState::Completed,
                by: Actor::Thread(THREAD),
                summary: Some("PR 1 merged".to_owned()),
            }),
        ),
        (thread, TypedEvent::SurfaceUpdate(surface())),
        (thread, TypedEvent::VtSnapshotTaken(vt_snapshot())),
        (host, TypedEvent::WorktreeChanged(worktree())),
        (
            thread,
            TypedEvent::RawUnknown(RawUnknown {
                source: SourceKind::Hook,
                reason: RawUnknownReason::UnknownEvent,
                agent: Some(Agent::Codex),
                cli_version: Some("0.171.0".to_owned()),
                content_type: "application/json".to_owned(),
                raw: Content::Inline {
                    inline: "/wD+".to_owned(),
                    encoding: Encoding::Base64,
                },
            }),
        ),
    ]
}

#[test]
fn golden_round_trip_for_every_kind() {
    let samples = samples();
    let kinds: Vec<_> = samples.iter().map(|(_, e)| e.kind()).collect();
    assert_eq!(kinds, TypedEvent::KINDS, "one sample per kind, in order");

    for (n, (stream, event)) in samples.iter().enumerate() {
        let env = envelope(n as u64 + 1, *stream, event);
        round_trip(&format!("events/{}", event.kind()), &env);
        let text =
            fs::read_to_string(golden_dir().join(format!("events/{}.json", event.kind()))).unwrap();
        let back: EventEnvelope = serde_json::from_str(&text).unwrap();
        assert_eq!(
            &TypedEvent::decode(&back).expect("known kind decodes"),
            event
        );
    }

    // No stale golden event files.
    let files: BTreeSet<String> = fs::read_dir(golden_dir().join("events"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    let want: BTreeSet<String> = TypedEvent::KINDS
        .iter()
        .map(|k| format!("{k}.json"))
        .collect();
    assert_eq!(files, want);
}

#[test]
fn golden_round_trip_for_every_status_field() {
    let cases = [
        StatusChanged::ProcessState {
            from: ProcessState::Starting,
            to: ProcessState::Running,
            cause: StatusCause::Spawned,
        },
        StatusChanged::TurnState {
            from: TurnState::Running,
            to: TurnState::AwaitingApproval,
            cause: StatusCause::ApprovalOpened,
        },
        StatusChanged::TaskState {
            from: TaskState::Active,
            to: TaskState::Blocked,
            cause: StatusCause::TaskSet,
        },
    ];
    for (name, c) in ["process_state", "turn_state", "task_state"]
        .iter()
        .zip(cases)
    {
        round_trip(&format!("status_changed/{name}"), &c);
    }
}

#[test]
fn golden_round_trip_for_content_forms() {
    round_trip("content/inline_utf8", &text("café ☕ 🎌"));
    round_trip(
        "content/inline_base64",
        &Content::Inline {
            inline: "gICA".to_owned(),
            encoding: Encoding::Base64,
        },
    );
    round_trip(
        "content/blob",
        &Content::Blob {
            blob: BlobRef {
                sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                    .to_owned(),
                size: 70_000,
                media_type: "application/octet-stream".to_owned(),
            },
            preview: "PK\u{fffd}".to_owned(),
            preview_lossy: true,
        },
    );
}

fn thread_info() -> ThreadInfo {
    ThreadInfo {
        thread_id: THREAD,
        agent: Agent::Claude.into(),
        cwd: "/work/chaldea".to_owned(),
        workspace_id: WorkspaceId::from("ws-5f2c"),
        parent_thread_id: None,
        role: ThreadRole::Standard,
        closed: false,
        process_state: ProcessState::Running,
        turn_state: TurnState::AwaitingApproval,
        task_state: TaskState::Active,
        attention: vec![AttentionCause::InputGateTimeout { input_id: INPUT }],
    }
}

#[test]
fn golden_round_trip_for_events_response() {
    let stream = StreamId::Thread(THREAD);
    let samples = samples();
    let events: Vec<_> = samples[2..4]
        .iter()
        .enumerate()
        .map(|(n, (_, e))| envelope(n as u64 + 41, stream, e))
        .collect();
    round_trip(
        "events_response/replay",
        &EventsResponse::Replay {
            stream_id: stream,
            epoch: Epoch::from("ep-7c1d"),
            events: events.clone(),
            head_seq: 42,
            has_more: false,
            has_more_before: true,
        },
    );
    round_trip(
        "events_response/snapshot_thread",
        &EventsResponse::Snapshot {
            stream_id: stream,
            epoch: Epoch::from("ep-7c1d"),
            snapshot: StreamSnapshot::Thread(Box::new(ThreadSnapshot {
                thread: thread_info(),
                launch: Some(LaunchInfo {
                    launch_id: LAUNCH,
                    pid: Some(4242),
                    manual: false,
                    exit_reason: None,
                    death_confirmed: false,
                }),
                session: Some(SessionInfo {
                    session_id: "0193aaaa-0000-7000-8000-000000000001".to_owned(),
                    transcript_path: Some("/work/.claude/projects/x/s.jsonl".to_owned()),
                }),
                gate: GateEvidence {
                    readiness_seq: Some(30),
                    readiness_evidence: Some(ReadinessEvidence::SessionStart),
                    last_write_seq: Some(31),
                    last_human_write_seq: Some(28),
                    last_turn_start_seq: Some(29),
                },
                inputs: vec![InputInfo {
                    input_id: INPUT,
                    launch_id: Some(LAUNCH),
                    state: InputState::Queued,
                    text: text("次へ"),
                    encoding: InputEncoding::BracketedPaste,
                    created_by: Actor::Human,
                    sent_after_seq: None,
                }],
                open_approvals: vec![approval()],
                worktree: Some(worktree()),
                surfaces: vec![surface()],
                vt_snapshot: Some(vt_snapshot()),
                recent_events: events,
            })),
            as_of_seq: 42,
            reason: SnapshotReason::EpochChanged,
        },
    );
    round_trip(
        "events_response/snapshot_host",
        &EventsResponse::Snapshot {
            stream_id: StreamId::Host,
            epoch: Epoch::from("ep-7c1d"),
            snapshot: StreamSnapshot::Host(HostSnapshot {
                threads: vec![thread_info()],
                worktrees: vec![worktree()],
                surfaces: vec![SurfaceUpdate {
                    surface_id: SurfaceId::from("host-status"),
                    scope: SurfaceScope::Workspace {
                        workspace_id: WorkspaceId::from("ws-5f2c"),
                    },
                    components: vec![],
                }],
            }),
            as_of_seq: 7,
            reason: SnapshotReason::TooManyEvents,
        },
    );
}

#[test]
fn golden_round_trip_for_ws_messages() {
    round_trip(
        "ws/hello",
        &WsServerMessage::Hello {
            epoch: Epoch::from("ep-7c1d"),
            server_time: Timestamp::from("2026-11-22T09:45:00.000Z"),
        },
    );
    round_trip(
        "ws/stream_advanced",
        &WsServerMessage::StreamAdvanced {
            stream_id: StreamId::Thread(THREAD),
            head_seq: 43,
        },
    );
    round_trip(
        "ws/heads",
        &WsServerMessage::Heads {
            heads: vec![
                StreamHeadEntry {
                    stream_id: StreamId::Host,
                    head_seq: 7,
                },
                StreamHeadEntry {
                    stream_id: StreamId::Thread(THREAD),
                    head_seq: 43,
                },
            ],
        },
    );
    round_trip(
        "ws/subscribe",
        &WsClientMessage::Subscribe {
            stream_ids: vec![StreamId::Host, StreamId::Thread(THREAD)],
        },
    );
}

// ---- forward-compatible decoding ---------------------------------------------------------------

/// A replay page whose first event comes from an agent and a source kind this reader does
/// not know still decodes, and every event of a known kind stays typed (§2.1 open
/// envelope; P-19).
#[test]
fn replay_page_with_unknown_agent_and_source_kind_decodes() {
    let stream = StreamId::Thread(THREAD);
    let samples = samples();
    let events: Vec<_> = samples[2..4]
        .iter()
        .enumerate()
        .map(|(n, (_, e))| envelope(n as u64 + 1, stream, e))
        .collect();
    let page = EventsResponse::Replay {
        stream_id: stream,
        epoch: Epoch::from("ep-7c1d"),
        events,
        head_seq: 2,
        has_more: false,
        has_more_before: false,
    };
    let mut v = serde_json::to_value(&page).unwrap();
    v["events"][0]["source"]["agent"] = "gemini".into();
    v["events"][0]["source"]["kind"] = "relay".into();
    let text = v.to_string();

    let back: EventsResponse = serde_json::from_str(&text).expect("the page still decodes");
    let EventsResponse::Replay { events, .. } = back else {
        panic!("replay page");
    };
    for (env, (_, want)) in events.iter().zip(&samples[2..4]) {
        assert_eq!(
            &TypedEvent::decode(env).expect("known kind stays typed"),
            want
        );
    }
    let first = serde_json::to_value(&events[0]).unwrap();
    assert_eq!(first["source"]["agent"], "gemini", "unknown agent is kept");
    assert_eq!(
        first["source"]["kind"], "relay",
        "unknown source kind is kept"
    );
}

/// A host snapshot listing a thread of an unknown agent still decodes, and the agent
/// string is kept.
#[test]
fn host_snapshot_with_unknown_agent_decodes() {
    let snap = EventsResponse::Snapshot {
        stream_id: StreamId::Host,
        epoch: Epoch::from("ep-7c1d"),
        snapshot: StreamSnapshot::Host(HostSnapshot {
            threads: vec![thread_info(), thread_info()],
            worktrees: vec![],
            surfaces: vec![],
        }),
        as_of_seq: 7,
        reason: SnapshotReason::EpochChanged,
    };
    let mut v = serde_json::to_value(&snap).unwrap();
    v["snapshot"]["threads"][1]["agent"] = "gemini".into();

    let back: EventsResponse =
        serde_json::from_str(&v.to_string()).expect("the snapshot still decodes");
    let again = serde_json::to_value(&back).unwrap();
    assert_eq!(again["snapshot"]["threads"][0]["agent"], "claude");
    assert_eq!(again["snapshot"]["threads"][1]["agent"], "gemini");
}

/// Nothing above 2^53 - 1 can be emitted: serializing it errors, as decoding it does, so a
/// writer never produces a value its own readers refuse. A value at the bound round-trips.
#[test]
fn integers_above_the_safe_bound_do_not_serialize() {
    let page = |head_seq| EventsResponse::Replay {
        stream_id: StreamId::Host,
        epoch: Epoch::from("ep-7c1d"),
        events: vec![],
        head_seq,
        has_more: false,
        has_more_before: false,
    };
    assert!(serde_json::to_string(&page(MAX_SAFE_INTEGER + 2)).is_err());
    let text = serde_json::to_string(&page(MAX_SAFE_INTEGER)).expect("at the bound");
    let back: EventsResponse = serde_json::from_str(&text).expect("decodes at the bound");
    assert_eq!(back, page(MAX_SAFE_INTEGER));

    let samples = samples();
    let mut env = envelope(1, StreamId::Thread(THREAD), &samples[10].1);
    env.seq = MAX_SAFE_INTEGER + 1;
    assert!(serde_json::to_string(&env).is_err(), "envelope seq");

    let changed = InputStateChanged {
        input_id: INPUT,
        from: InputState::Queued,
        to: InputState::Sending,
        reason: InputReason::GateOpened,
        sent_after_seq: Some(MAX_SAFE_INTEGER + 1),
        by: None,
    };
    assert!(serde_json::to_string(&changed).is_err(), "optional field");
}

/// Types holding an envelope decode from a parsed `serde_json::Value` as well as from text.
#[test]
fn envelope_holders_decode_from_a_value() {
    let page = serde_json::from_str::<Value>(
        &fs::read_to_string(golden_dir().join("events_response/snapshot_thread.json")).unwrap(),
    )
    .unwrap();
    let back: EventsResponse = serde_json::from_value(page.clone()).expect("from a Value");
    assert_eq!(serde_json::to_value(&back).unwrap(), page);
}

fn known_envelope_json() -> Value {
    let samples = samples();
    let (stream, event) = &samples[10]; // user_message
    serde_json::to_value(envelope(11, *stream, event)).unwrap()
}

#[allow(clippy::result_large_err, reason = "mirrors TypedEvent::decode")]
fn decode_json(v: &Value) -> Result<TypedEvent, GenericEvent> {
    let env: EventEnvelope =
        serde_json::from_str(&v.to_string()).expect("the open envelope still parses");
    TypedEvent::decode(&env)
}

#[test]
fn unknown_kind_is_generic() {
    let mut v = known_envelope_json();
    v["kind"] = "artifact_diff".into();
    v["payload"] = serde_json::json!({ "files": [1, 2, 3] });
    let g = decode_json(&v).unwrap_err();
    assert_eq!(g.why, GenericReason::UnknownKind);
    assert_eq!(g.envelope.kind.as_str(), "artifact_diff");
    assert_eq!(g.envelope.payload.get(), r#"{"files":[1,2,3]}"#);
}

#[test]
fn newer_schema_version_is_generic_even_for_a_known_kind() {
    let mut v = known_envelope_json();
    v["schema_version"] = (SCHEMA_VERSION + 1).into();
    let g = decode_json(&v).unwrap_err();
    assert_eq!(g.why, GenericReason::NewerSchema);
}

#[test]
fn invalid_payload_is_generic() {
    let mut v = known_envelope_json();
    v["payload"]["prompt"] = 42.into();
    assert_eq!(
        decode_json(&v).unwrap_err().why,
        GenericReason::PayloadInvalid
    );

    let mut v = known_envelope_json();
    v["payload"] = "not an object".into();
    assert_eq!(
        decode_json(&v).unwrap_err().why,
        GenericReason::PayloadInvalid
    );
}

#[test]
fn unknown_fields_in_envelope_and_payload_are_tolerated() {
    let mut v = known_envelope_json();
    v["relay_hint"] = "later".into();
    v["payload"]["turn_ref"] = "t-1".into();
    assert!(matches!(decode_json(&v), Ok(TypedEvent::UserMessage(_))));
}

#[test]
fn stream_id_and_actor_have_one_spelling() {
    use std::str::FromStr;
    let t = THREAD.to_string();
    assert_eq!(
        StreamId::from_str(&format!("thread:{t}")),
        Ok(StreamId::Thread(THREAD))
    );
    assert!(StreamId::from_str(&format!("thread:{}", t.to_uppercase())).is_err());
    assert!(StreamId::from_str(&format!("thread:{}", t.replace('-', ""))).is_err());
    assert!(StreamId::from_str("thread:").is_err());
    assert!(StreamId::from_str("Host").is_err());
    assert_eq!(
        Actor::from_str(&format!("thread:{t}")),
        Ok(Actor::Thread(THREAD))
    );
    assert!(Actor::from_str("owner").is_err());
}
