//! Replays the committed Q-B traces against the real `decide_replay` and `check_receipt`
//! (`docs/design/formal-specs.md` §7.5): `specs/traces/sync/*.itf.json`, exported from
//! `specs/sync_traces.qnt`, and `specs/traces/receipts/*.itf.json`, exported from
//! `specs/receipts_traces.qnt`.
//!
//! Each model step records the calls it made into its mirror of the function with the
//! mirror's result (`last.calls`). This test calls the real function with the same
//! arguments and asserts the same result. The store, the HTTP layer, and the client are
//! not replayed: the store is PR 6's, the HTTP and WebSocket code PR 7's (its tests and
//! PR 17's `stateright` model check them), the client PR 18's.
//!
//! `decide_replay`'s bounds are scaled in the model: one model seq stands for
//! [`SEQ_SCALE`] seqs and one size unit for [`BYTE_SCALE`] bytes, fixed here from the
//! contract's bounds (10 000 events, 8 MiB). Each sync trace's `Init` names the model
//! bounds, and the test checks that they scale to the contract's and that core's constants
//! are the contract's. Every comparison in `decide_replay` is linear in seqs and bytes, so
//! the scaled call takes the same branch; the real boundaries (a bound and the bound + 1)
//! and the branch order on unscaled values are `tests/sync_command.rs`'s.
//! `pnpm run traces:check` in `specs/` keeps the traces equal to the specs.

#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "this test reads the committed trace files; core itself stays Sans-IO"
)]

use std::fs;
use std::path::{Path, PathBuf};

use chaldea_core::command::{
    CommandReceipt, CommandResult, ReceiptLookup, ReceiptStatus, check_receipt,
};
use chaldea_core::ids::{CommandId, Epoch, StreamId, ThreadId, TokenId};
use chaldea_core::sync::{
    EventsQuery, QueryError, REPLAY_MAX_BYTES, REPLAY_MAX_EVENTS, ReplayPlan, SnapshotReason,
    StreamHead, decide_replay,
};
use serde_json::Value;

fn traces_dir(sub: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../specs/traces")
        .join(sub)
}

// ITF encoding (https://apalache-mc.org/docs/adr/015adr-trace.html): integers as
// `{"#bigint": "<n>"}`, sum type values as `{"tag": .., "value": ..}`, records as objects,
// lists as arrays.

fn int(v: &Value) -> i64 {
    let s = v["#bigint"]
        .as_str()
        .unwrap_or_else(|| panic!("not an ITF integer: {v}"));
    s.parse().unwrap_or_else(|e| panic!("bad integer {s}: {e}"))
}

fn uint(v: &Value) -> u64 {
    u64::try_from(int(v)).unwrap_or_else(|_| panic!("negative integer: {v}"))
}

fn list(v: &Value) -> &[Value] {
    v.as_array().unwrap_or_else(|| panic!("not a list: {v}"))
}

fn tag(v: &Value) -> (&str, &Value) {
    let t = v["tag"]
        .as_str()
        .unwrap_or_else(|| panic!("not a variant: {v}"));
    (t, &v["value"])
}

/// The model writes -1 for an omitted query parameter.
fn opt(v: &Value) -> Option<u64> {
    let n = int(v);
    (n >= 0).then(|| u64::try_from(n).expect("non-negative"))
}

fn epoch(n: u64) -> Epoch {
    Epoch(format!("epoch-{n}"))
}

fn read_traces(sub: &str) -> Vec<(String, Value)> {
    let mut files: Vec<PathBuf> = fs::read_dir(traces_dir(sub))
        .unwrap_or_else(|e| panic!("specs/traces/{sub}: {e}"))
        .map(|e| e.expect("directory entry").path())
        .filter(|p| p.to_str().is_some_and(|s| s.ends_with(".itf.json")))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no trace under specs/traces/{sub}");
    files
        .iter()
        .map(|path| {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .expect("UTF-8 file name")
                .to_owned();
            let text = fs::read_to_string(path).expect("read trace");
            let itf = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
            (name, itf)
        })
        .collect()
}

/// Every call of every step, with where it was made.
fn calls(name: &str, itf: &Value) -> Vec<(String, Value)> {
    list(&itf["states"])
        .iter()
        .enumerate()
        .flat_map(|(k, s)| {
            let last = &s["last"];
            list(&last["calls"])
                .iter()
                .enumerate()
                .map(|(j, c)| {
                    (
                        format!("{name} state {k} call {j} ({})", last["act"]),
                        c.clone(),
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

// ---- decide_replay ----------------------------------------------------------------------------

/// The contract's replay bounds (mvp §2.4), written out rather than read from core so that
/// a changed constant fails here.
const CONTRACT_MAX_EVENTS: u64 = 10_000;
const CONTRACT_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Seqs per model seq and bytes per model size unit.
const SEQ_SCALE: u64 = 5_000;
const BYTE_SCALE: u64 = 2 * 1024 * 1024;

struct Scale {
    seq: u64,
    bytes: u64,
}

impl Scale {
    fn of(name: &str, itf: &Value) -> Scale {
        let (init, v) = tag(&list(&itf["states"])[0]["last"]["act"]);
        assert_eq!(init, "Init", "{name}: the first state is not Init");
        assert_eq!(
            uint(&v["replayMaxEvents"]) * SEQ_SCALE,
            CONTRACT_MAX_EVENTS,
            "{name}: the model's event bound does not scale to the contract's"
        );
        assert_eq!(
            uint(&v["replayMaxBytes"]) * BYTE_SCALE,
            CONTRACT_MAX_BYTES,
            "{name}: the model's byte bound does not scale to the contract's"
        );
        Scale {
            seq: SEQ_SCALE,
            bytes: BYTE_SCALE,
        }
    }
}

fn query(v: &Value, scale: &Scale) -> EventsQuery {
    EventsQuery {
        stream_id: StreamId::Thread(ThreadId::from_u128(1)),
        after_seq: opt(&v["after"]).map(|n| n * scale.seq),
        before_seq: opt(&v["before"]).map(|n| n * scale.seq),
        limit: opt(&v["limit"]).map(|n| u32::try_from(n).expect("limit fits u32")),
        epoch: Some(uint(&v["epoch"])).filter(|&e| e != 0).map(epoch),
    }
}

fn head(v: &Value, scale: &Scale) -> StreamHead {
    StreamHead {
        head_seq: uint(&v["headSeq"]) * scale.seq,
        cum_bytes_at_head: uint(&v["cumHead"]) * scale.bytes,
        cum_bytes_at_after: uint(&v["cumAfter"]) * scale.bytes,
    }
}

fn plan(v: &Value, scale: &Scale) -> Result<ReplayPlan, QueryError> {
    let limit = |p: &Value| u32::try_from(uint(&p["limit"])).expect("limit fits u32");
    match tag(v) {
        ("PForward", p) => Ok(ReplayPlan::Forward {
            after_seq: uint(&p["after"]) * scale.seq,
            limit: limit(p),
        }),
        ("PBackward", p) => Ok(ReplayPlan::Backward {
            before_seq: uint(&p["before"]) * scale.seq,
            limit: limit(p),
        }),
        ("PSnapshot", r) => Ok(ReplayPlan::Snapshot {
            reason: match tag(r).0 {
                "EpochChanged" => SnapshotReason::EpochChanged,
                "CursorAhead" => SnapshotReason::CursorAhead,
                "TooManyEvents" => SnapshotReason::TooManyEvents,
                "TooManyBytes" => SnapshotReason::TooManyBytes,
                t => panic!("unknown snapshot reason {t}"),
            },
        }),
        ("PRefused", e) => Err(match tag(e).0 {
            "LimitOutOfRange" => QueryError::LimitOutOfRange,
            "AfterAndBefore" => QueryError::AfterAndBefore,
            t => panic!("unknown query error {t}"),
        }),
        (t, _) => panic!("unknown plan {t}"),
    }
}

#[test]
fn sync_traces_replay_against_decide_replay() {
    assert_eq!(REPLAY_MAX_EVENTS, CONTRACT_MAX_EVENTS);
    assert_eq!(REPLAY_MAX_BYTES, CONTRACT_MAX_BYTES);
    let mut checked = 0;
    for (name, itf) in read_traces("sync") {
        let scale = Scale::of(&name, &itf);
        for (at, call) in calls(&name, &itf) {
            let (kind, c) = tag(&call);
            assert_eq!(kind, "DecideReplay", "{at}: unknown call");
            let q = query(&c["q"], &scale);
            let h = head(&c["head"], &scale);
            assert_eq!(
                decide_replay(&q, h, &epoch(uint(&c["epoch"]))),
                plan(&c["plan"], &scale),
                "{at}: decide_replay({q:?}, {h:?})"
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "no decide_replay call to check");
}

// ---- check_receipt ----------------------------------------------------------------------------

fn token(v: &Value) -> TokenId {
    TokenId::from_u128(u128::from(uint(v)))
}

fn request_sha256(v: &Value) -> [u8; 32] {
    [u8::try_from(uint(v)).expect("hash fits u8"); 32]
}

/// A model result is a stored HTTP status and a number standing for the body.
fn result(v: &Value) -> CommandResult {
    CommandResult {
        http_status: u16::try_from(uint(&v["status"])).expect("status fits u16"),
        body: format!("{{\"result\":{}}}", uint(&v["n"])),
    }
}

fn receipt(v: &Value) -> Option<CommandReceipt> {
    match tag(v) {
        ("NoReceipt", _) => None,
        ("Has", r) => Some(CommandReceipt {
            command_id: CommandId::from_u128(1),
            token: token(&r["token"]),
            request_sha256: request_sha256(&r["hash"]),
            status: match tag(&r["status"]) {
                ("InProgress", _) => ReceiptStatus::InProgress,
                ("Completed", res) => ReceiptStatus::Completed(result(res)),
                ("UnknownAfterCrash", _) => ReceiptStatus::UnknownAfterCrash,
                (t, _) => panic!("unknown receipt status {t}"),
            },
        }),
        (t, _) => panic!("unknown existing receipt {t}"),
    }
}

fn lookup(v: &Value) -> ReceiptLookup {
    match tag(v) {
        ("New", _) => ReceiptLookup::New,
        ("LInProgress", _) => ReceiptLookup::InProgress,
        ("Replay", res) => ReceiptLookup::Replay(result(res)),
        ("Conflict", _) => ReceiptLookup::Conflict,
        ("LUnknownAfterCrash", _) => ReceiptLookup::UnknownAfterCrash,
        (t, _) => panic!("unknown lookup {t}"),
    }
}

#[test]
fn receipts_traces_replay_against_check_receipt() {
    let mut checked = 0;
    for (name, itf) in read_traces("receipts") {
        for (at, call) in calls(&name, &itf) {
            let (kind, c) = tag(&call);
            assert_eq!(kind, "CheckReceipt", "{at}: unknown call");
            let existing = receipt(&c["existing"]);
            assert_eq!(
                check_receipt(
                    existing.as_ref(),
                    token(&c["token"]),
                    &request_sha256(&c["hash"])
                ),
                lookup(&c["lookup"]),
                "{at}: check_receipt({existing:?})"
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "no check_receipt call to check");
}
