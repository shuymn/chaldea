//! Rust decoding and the committed JSON Schema must agree (CL-14): `packages/contracts`
//! validates with the schema, the daemon and CLI decode with Rust, and a value one side
//! accepts and the other refuses would render differently in the PWA and the CLI.
//!
//! Each case is one input fed to both sides, with the verdict both must give.

#![allow(
    clippy::disallowed_methods,
    reason = "this test reads the committed schema files; core itself stays Sans-IO"
)]

use std::fs;
use std::path::Path;

use chaldea_core::event::{EventEnvelope, TypedEvent};
use chaldea_core::sync::EventsResponse;
use serde_json::{Value, json};

/// 2^53 - 1, the largest integer a JSON number keeps exactly in JavaScript.
const MAX_SAFE: u64 = 9_007_199_254_740_991;

const UUID: &str = "0193abcd-ef01-7000-8000-000000000001";

fn schema(name: &str) -> jsonschema::Validator {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/contracts/schema")
        .join(format!("{name}.schema.json"));
    let text = fs::read_to_string(&path).expect("committed schema");
    let schema: Value = serde_json::from_str(&text).expect("schema is JSON");
    jsonschema::validator_for(&schema).expect("schema compiles")
}

fn envelope(kind: &str, payload: &Value, seq: &Value) -> Value {
    json!({
        "stream_id": format!("thread:{UUID}"),
        "seq": seq,
        "event_id": UUID,
        "schema_version": 1,
        "idempotency_key": "command:x:1",
        "kind": kind,
        "thread_id": UUID,
        "launch_id": UUID,
        "command_id": null,
        "source": { "kind": "process", "agent": "codex", "cli_version": null, "transcript": null },
        "occurred_at": "2026-11-22T09:45:00.123Z",
        "recorded_at": "2026-11-22T09:45:00.123Z",
        "payload": payload,
    })
}

fn rust_decodes(kind: &str, payload: &Value) -> bool {
    let text = envelope(kind, payload, &json!(1)).to_string();
    let env: EventEnvelope = serde_json::from_str(&text).expect("envelope parses");
    TypedEvent::decode(&env).is_ok()
}

fn launch_started(pid: &Value) -> Value {
    json!({ "launch_id": UUID, "pid": pid, "start_time": "1", "pgid_or_job": "1", "cli_version": null })
}

fn launch_exited(code: &Value) -> Value {
    json!({ "launch_id": UUID, "code": code, "signal": null, "reason": "exit", "death_confirmed": true })
}

fn with_failed_start(failed: Value) -> Value {
    let mut v = launch_exited(&json!(null));
    v["failed_start"] = failed;
    v
}

fn generation(g: &Value) -> Value {
    json!({ "generation": g, "path": "/work/s.jsonl", "file_id": "1:2" })
}

fn vt(cols: &Value) -> Value {
    json!({
        "grid": { "inline": "", "encoding": "utf8" },
        "cols": cols, "rows": 24,
        "cursor": { "row": 0, "col": 0, "visible": true },
        "modes": { "bracketed_paste": false, "alternate_screen": false },
    })
}

#[test]
fn payload_verdicts_agree() {
    let typed = schema("TypedEvent");
    let cases: Vec<(&str, &str, Value, bool)> = vec![
        (
            "valid thread_closed",
            "thread_closed",
            json!({ "thread_id": UUID, "by": "human" }),
            true,
        ),
        (
            "struct payload as an array",
            "thread_closed",
            json!([UUID, "human"]),
            false,
        ),
        ("payload as a string", "thread_closed", json!("x"), false),
        (
            "u32 at its maximum",
            "launch_started",
            launch_started(&json!(4_294_967_295_u64)),
            true,
        ),
        (
            "u32 overflow",
            "launch_started",
            launch_started(&json!(4_294_967_296_u64)),
            false,
        ),
        (
            "negative u32",
            "launch_started",
            launch_started(&json!(-1)),
            false,
        ),
        (
            "i32 at its minimum",
            "launch_exited",
            launch_exited(&json!(-2_147_483_648_i64)),
            true,
        ),
        (
            "i32 overflow",
            "launch_exited",
            launch_exited(&json!(2_147_483_648_i64)),
            false,
        ),
        (
            "launch_exited without failed_start",
            "launch_exited",
            launch_exited(&json!(0)),
            true,
        ),
        (
            "launch_exited with a full failed_start",
            "launch_exited",
            with_failed_start(json!({ "pid": 42, "start_time": "1", "pgid_or_job": "42" })),
            true,
        ),
        (
            "launch_exited with a bare failed_start pid",
            "launch_exited",
            with_failed_start(json!({ "pid": 42 })),
            true,
        ),
        (
            "launch_exited with failed_start null",
            "launch_exited",
            with_failed_start(json!(null)),
            false,
        ),
        (
            "failed_start as an array",
            "launch_exited",
            with_failed_start(json!([42, "1", "42"])),
            false,
        ),
        (
            "failed_start with a null part",
            "launch_exited",
            with_failed_start(json!({ "pid": 42, "start_time": null })),
            false,
        ),
        (
            "failed_start with a null pgid_or_job",
            "launch_exited",
            with_failed_start(json!({ "pid": 42, "pgid_or_job": null })),
            false,
        ),
        (
            "failed_start without its pid",
            "launch_exited",
            with_failed_start(json!({ "start_time": "1" })),
            false,
        ),
        (
            "failed_start pid overflow",
            "launch_exited",
            with_failed_start(json!({ "pid": 4_294_967_296_u64 })),
            false,
        ),
        (
            "u16 at its maximum",
            "vt_snapshot_taken",
            vt(&json!(65_535)),
            true,
        ),
        (
            "u16 overflow",
            "vt_snapshot_taken",
            vt(&json!(65_536)),
            false,
        ),
        (
            "u64 at 2^53 - 1",
            "transcript_generation",
            generation(&json!(MAX_SAFE)),
            true,
        ),
        (
            "u64 at 2^53",
            "transcript_generation",
            generation(&json!(MAX_SAFE + 1)),
            false,
        ),
        (
            "u64 at its maximum",
            "transcript_generation",
            generation(&json!(u64::MAX)),
            false,
        ),
    ];
    // `FailedStart` itself decodes only from an object.
    assert!(
        serde_json::from_value::<chaldea_core::payload::FailedStart>(json!([42, "1", "42"]))
            .is_err()
    );
    assert!(
        serde_json::from_value::<chaldea_core::payload::FailedStart>(json!({ "pid": 42 })).is_ok()
    );
    for (name, kind, payload, want) in cases {
        let rust = rust_decodes(kind, &payload);
        let schema = typed.is_valid(&json!({ "kind": kind, "payload": payload }));
        assert_eq!(rust, want, "{name}: Rust");
        assert_eq!(schema, want, "{name}: JSON Schema");
    }
}

#[test]
fn envelope_and_response_verdicts_agree() {
    let env_schema = schema("EventEnvelope");
    let page_schema = schema("EventsResponse");
    let payload = json!({ "thread_id": UUID, "by": "human" });
    for (seq, want) in [
        (json!(1), true),
        (json!(MAX_SAFE), true),
        (json!(MAX_SAFE + 1), false),
    ] {
        let env = envelope("thread_closed", &payload, &seq);
        let rust = serde_json::from_str::<EventEnvelope>(&env.to_string()).is_ok();
        assert_eq!(rust, want, "envelope seq {seq}: Rust");
        assert_eq!(
            env_schema.is_valid(&env),
            want,
            "envelope seq {seq}: JSON Schema"
        );

        let page = json!({
            "mode": "replay", "stream_id": "host", "epoch": "e", "events": [],
            "head_seq": seq, "has_more": false, "has_more_before": false,
        });
        let rust = serde_json::from_str::<EventsResponse>(&page.to_string()).is_ok();
        assert_eq!(rust, want, "page head_seq {seq}: Rust");
        assert_eq!(
            page_schema.is_valid(&page),
            want,
            "page head_seq {seq}: JSON Schema"
        );
    }
}
