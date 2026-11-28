//! Drift check for the generated contracts: the committed TypeScript under
//! `packages/contracts/src/generated/` and JSON Schema under `packages/contracts/schema/`
//! must equal what the Rust types generate, byte for byte. The TypeScript includes
//! `schemaVersion.ts`, the exported `SCHEMA_VERSION` the PWA's decoder compares against.
//!
//! Regenerate the committed files after changing a public type, from the repository root:
//!
//! ```text
//! CHALDEA_BLESS=1 cargo test -p chaldea-core --test contracts_export
//! ```
//!
//! Output is deterministic by construction on every OS: one flat directory per format
//! (ts-rs writes `/` in import paths on Windows too), LF line endings written by the
//! generators and kept by `.gitattributes`, and JSON objects with sorted keys
//! (`serde_json` without `preserve_order`).

#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "this test writes and compares files; core itself stays Sans-IO"
)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use chaldea_core::attach::{AttachClientMessage, AttachServerMessage};
use chaldea_core::command::{
    CloseResult, InputCancelArgs, InputCommandResult, InputResolveArgs, KeyArgs, KeyResult,
    ReadyArgs, ReadyResult, ResumeArgs, ResumeResult, SendArgs, SendResult, SpawnArgs, SpawnResult,
    StopArgs, StopResult,
};
use chaldea_core::event::{EventEnvelope, GenericEvent, TypedEvent};
use chaldea_core::http::{
    CommandRequest, ErrorBody, PairCodeResponse, PairRequest, PairResponse, ThreadsResponse,
    WorktreesResponse, WsTicketRequest, WsTicketResponse,
};
use chaldea_core::sync::{
    EventsResponse, InspectOutput, ScreenResponse, WsClientMessage, WsServerMessage,
};
use chaldea_core::{MAX_SAFE_INTEGER, SCHEMA_VERSION};
use schemars::JsonSchema;
use serde_json::Value;
use ts_rs::{Config, TS};

const TS_DIR: &str = "packages/contracts/src/generated";
const SCHEMA_DIR: &str = "packages/contracts/schema";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn export_ts<T: TS + 'static>(cfg: &Config) {
    T::export_all(cfg).unwrap_or_else(|e| panic!("ts-rs export failed: {e}"));
}

/// schemars marks integer widths only with `format`, which validators treat as an
/// annotation. State each width's bounds as `minimum`/`maximum` so that a validator
/// refuses what Rust refuses; `u64` stops at `MAX_SAFE_INTEGER`, as Rust decoding does.
fn bound_integers(v: &mut Value) {
    match v {
        Value::Object(map) => {
            let bounds: Option<(i64, u64)> = match map.get("format").and_then(Value::as_str) {
                Some("uint8") => Some((0, u64::from(u8::MAX))),
                Some("uint16") => Some((0, u64::from(u16::MAX))),
                Some("uint32") => Some((0, u64::from(u32::MAX))),
                Some("uint64") => Some((0, MAX_SAFE_INTEGER)),
                Some("int32") => Some((i64::from(i32::MIN), i32::MAX.unsigned_abs().into())),
                _ => None,
            };
            if let Some((min, max)) = bounds {
                map.insert("minimum".to_owned(), min.into());
                map.insert("maximum".to_owned(), max.into());
            }
            map.values_mut().for_each(bound_integers);
        }
        Value::Array(items) => items.iter_mut().for_each(bound_integers),
        _ => {}
    }
}

fn export_schema<T: JsonSchema>(dir: &Path) {
    let mut schema = serde_json::to_value(schemars::schema_for!(T)).expect("schema serializes");
    bound_integers(&mut schema);
    let name = T::schema_name();
    let mut text = serde_json::to_string_pretty(&schema).expect("schema serializes");
    text.push('\n');
    fs::write(dir.join(format!("{name}.schema.json")), text).expect("write schema");
}

/// Drops the spaces ts-rs leaves at the end of a line (before a field's doc comment), so the
/// committed files pass `git diff --check`.
fn trim_line_ends(dir: &Path) {
    for entry in fs::read_dir(dir).expect("read ts dir") {
        let path = entry.expect("ts dir entry").path();
        let text = fs::read_to_string(&path).expect("read ts file");
        let trimmed: String = text
            .split_inclusive('\n')
            .map(|l| {
                let body = l.strip_suffix('\n').unwrap_or(l);
                let end = if l.ends_with('\n') { "\n" } else { "" };
                format!("{}{end}", body.trim_end_matches([' ', '\t']))
            })
            .collect();
        if trimmed != text {
            fs::write(&path, trimmed).expect("write ts file");
        }
    }
}

/// Writes every generated file under `out/ts` and `out/schema`.
fn generate(out: &Path) -> (PathBuf, PathBuf) {
    let ts = out.join("ts");
    let schema = out.join("schema");
    fs::create_dir_all(&ts).expect("create ts dir");
    fs::create_dir_all(&schema).expect("create schema dir");

    // JSON numbers, not bigint: every u64 on the wire (seq, sizes) stays below 2^53.
    let cfg = Config::new().with_out_dir(&ts).with_large_int("number");
    export_ts::<EventEnvelope>(&cfg);
    export_ts::<TypedEvent>(&cfg);
    export_ts::<GenericEvent>(&cfg);
    export_ts::<EventsResponse>(&cfg);
    export_ts::<WsServerMessage>(&cfg);
    export_ts::<WsClientMessage>(&cfg);
    export_ts::<AttachServerMessage>(&cfg);
    export_ts::<AttachClientMessage>(&cfg);
    export_ts::<CommandRequest>(&cfg);
    export_ts::<ErrorBody>(&cfg);
    export_ts::<WsTicketRequest>(&cfg);
    export_ts::<WsTicketResponse>(&cfg);
    export_ts::<PairCodeResponse>(&cfg);
    export_ts::<PairRequest>(&cfg);
    export_ts::<PairResponse>(&cfg);
    export_ts::<ThreadsResponse>(&cfg);
    export_ts::<WorktreesResponse>(&cfg);
    export_ts::<SpawnArgs>(&cfg);
    export_ts::<SpawnResult>(&cfg);
    export_ts::<ResumeArgs>(&cfg);
    export_ts::<ResumeResult>(&cfg);
    export_ts::<StopArgs>(&cfg);
    export_ts::<StopResult>(&cfg);
    export_ts::<CloseResult>(&cfg);
    export_ts::<SendArgs>(&cfg);
    export_ts::<SendResult>(&cfg);
    export_ts::<ReadyArgs>(&cfg);
    export_ts::<ReadyResult>(&cfg);
    export_ts::<KeyArgs>(&cfg);
    export_ts::<KeyResult>(&cfg);
    export_ts::<InputResolveArgs>(&cfg);
    export_ts::<InputCancelArgs>(&cfg);
    export_ts::<InputCommandResult>(&cfg);
    export_ts::<ScreenResponse>(&cfg);
    export_ts::<InspectOutput>(&cfg);
    trim_line_ends(&ts);
    fs::write(
        ts.join("schemaVersion.ts"),
        format!(
            "// Generated by crates/core/tests/contracts_export.rs from `chaldea_core::SCHEMA_VERSION`. Do not edit this file manually.\n\n\
             /**\n * The newest event schema version this reader decodes (`TypedEvent::decode`).\n */\n\
             export const SCHEMA_VERSION = {SCHEMA_VERSION};\n"
        ),
    )
    .expect("write schemaVersion.ts");

    export_schema::<EventEnvelope>(&schema);
    export_schema::<TypedEvent>(&schema);
    export_schema::<EventsResponse>(&schema);
    export_schema::<WsServerMessage>(&schema);
    export_schema::<WsClientMessage>(&schema);
    export_schema::<AttachServerMessage>(&schema);
    export_schema::<AttachClientMessage>(&schema);
    export_schema::<CommandRequest>(&schema);
    export_schema::<ErrorBody>(&schema);
    export_schema::<WsTicketRequest>(&schema);
    export_schema::<WsTicketResponse>(&schema);
    export_schema::<PairCodeResponse>(&schema);
    export_schema::<PairRequest>(&schema);
    export_schema::<PairResponse>(&schema);
    export_schema::<ThreadsResponse>(&schema);
    export_schema::<WorktreesResponse>(&schema);
    export_schema::<SpawnArgs>(&schema);
    export_schema::<SpawnResult>(&schema);
    export_schema::<ResumeArgs>(&schema);
    export_schema::<ResumeResult>(&schema);
    export_schema::<StopArgs>(&schema);
    export_schema::<StopResult>(&schema);
    export_schema::<CloseResult>(&schema);
    export_schema::<SendArgs>(&schema);
    export_schema::<SendResult>(&schema);
    export_schema::<ReadyArgs>(&schema);
    export_schema::<ReadyResult>(&schema);
    export_schema::<KeyArgs>(&schema);
    export_schema::<KeyResult>(&schema);
    export_schema::<InputResolveArgs>(&schema);
    export_schema::<InputCancelArgs>(&schema);
    export_schema::<InputCommandResult>(&schema);
    export_schema::<ScreenResponse>(&schema);
    export_schema::<InspectOutput>(&schema);

    (ts, schema)
}

/// Files under `dir`, keyed by their `/`-separated relative path.
fn read_tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                let rel = path.strip_prefix(base).expect("under base");
                let key = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                out.insert(key, fs::read(&path).expect("read file"));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

fn first_difference(a: &[u8], b: &[u8]) -> String {
    let (a, b) = (String::from_utf8_lossy(a), String::from_utf8_lossy(b));
    for (n, (la, lb)) in a.split('\n').zip(b.split('\n')).enumerate() {
        if la != lb {
            return format!("line {}: committed {la:?}, generated {lb:?}", n + 1);
        }
    }
    "line count differs".to_owned()
}

fn compare(label: &str, committed: &Path, generated: &Path) -> Vec<String> {
    let want = read_tree(generated);
    let have = read_tree(committed);
    let mut problems = Vec::new();
    for (name, bytes) in &want {
        match have.get(name) {
            None => problems.push(format!("{label}/{name}: missing")),
            Some(h) if h != bytes => {
                problems.push(format!("{label}/{name}: {}", first_difference(h, bytes)));
            }
            Some(_) => {}
        }
    }
    for name in have.keys().filter(|n| !want.contains_key(*n)) {
        problems.push(format!("{label}/{name}: not generated (stale)"));
    }
    problems
}

fn replace_dir(target: &Path, source: &Path) {
    if target.exists() {
        fs::remove_dir_all(target).expect("remove old output");
    }
    fs::create_dir_all(target).expect("create output dir");
    for (name, bytes) in read_tree(source) {
        let path = target.join(&name);
        fs::create_dir_all(path.parent().expect("has parent")).expect("create parent");
        fs::write(path, bytes).expect("write output");
    }
}

#[test]
fn committed_contracts_equal_generated_output() {
    let tmp = std::env::temp_dir().join(format!("chaldea-contracts-{}", std::process::id()));
    if tmp.exists() {
        fs::remove_dir_all(&tmp).expect("clear temp dir");
    }
    let (ts, schema) = generate(&tmp);
    let root = repo_root();

    if std::env::var_os("CHALDEA_BLESS").is_some() {
        replace_dir(&root.join(TS_DIR), &ts);
        replace_dir(&root.join(SCHEMA_DIR), &schema);
    }

    let mut problems = compare(TS_DIR, &root.join(TS_DIR), &ts);
    problems.extend(compare(SCHEMA_DIR, &root.join(SCHEMA_DIR), &schema));
    fs::remove_dir_all(&tmp).expect("remove temp dir");

    assert!(
        problems.is_empty(),
        "generated contracts drifted from the committed files; regenerate with \
         `CHALDEA_BLESS=1 cargo test -p chaldea-core --test contracts_export`:\n{}",
        problems.join("\n")
    );
}

/// `StopResult`'s `exit_code` and `signal` and `CloseResult`'s `stop` are always present on
/// the wire, as `null` when unknown or when no stop ran: each schema lists them in
/// `required`.
#[test]
fn nullable_result_fields_are_required() {
    let required = |schema: Value| -> Vec<String> {
        schema["required"]
            .as_array()
            .expect("a required list")
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect()
    };
    let stop = required(serde_json::to_value(schemars::schema_for!(StopResult)).unwrap());
    for f in [
        "launch_id",
        "outcome",
        "death_confirmed",
        "confirmed_within_deadline",
        "exit_code",
        "signal",
    ] {
        assert!(stop.iter().any(|r| r == f), "StopResult: {f} not required");
    }
    let close = required(serde_json::to_value(schemars::schema_for!(CloseResult)).unwrap());
    for f in ["thread_id", "stop"] {
        assert!(
            close.iter().any(|r| r == f),
            "CloseResult: {f} not required"
        );
    }
    let send = required(serde_json::to_value(schemars::schema_for!(SendResult)).unwrap());
    for f in ["input_id", "state", "reject_reason"] {
        assert!(send.iter().any(|r| r == f), "SendResult: {f} not required");
    }
    let key = required(serde_json::to_value(schemars::schema_for!(KeyResult)).unwrap());
    for f in ["outcome", "bytes_written"] {
        assert!(key.iter().any(|r| r == f), "KeyResult: {f} not required");
    }
}

/// No success type of the launch commands has a top-level `code` property: a client decodes
/// `ErrorBody` only from a non-2xx status, and a success body must not look like one.
#[test]
fn success_schemas_have_no_code() {
    for (name, schema) in [
        ("SpawnResult", schemars::schema_for!(SpawnResult)),
        ("ResumeResult", schemars::schema_for!(ResumeResult)),
        ("StopResult", schemars::schema_for!(StopResult)),
        ("CloseResult", schemars::schema_for!(CloseResult)),
        ("SendResult", schemars::schema_for!(SendResult)),
        ("ReadyResult", schemars::schema_for!(ReadyResult)),
        ("KeyResult", schemars::schema_for!(KeyResult)),
        (
            "InputCommandResult",
            schemars::schema_for!(InputCommandResult),
        ),
    ] {
        let v = serde_json::to_value(schema).unwrap();
        let props = v["properties"].as_object().expect("an object schema");
        assert!(!props.is_empty(), "{name}: no properties");
        assert!(!props.contains_key("code"), "{name} has a top-level code");
    }
}
