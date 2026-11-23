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

mod itf;

use chaldea_core::command::{
    CommandReceipt, CommandResult, ReceiptLookup, ReceiptStatus, check_receipt,
};
use chaldea_core::ids::{CommandId, Epoch, StreamId, ThreadId, TokenId};
use chaldea_core::sync::{
    EventsQuery, QueryError, REPLAY_MAX_BYTES, REPLAY_MAX_EVENTS, ReplayPlan, SnapshotReason,
    StreamHead, decide_replay,
};
use itf::{int, list, tag, uint};
use serde_json::Value;

/// The model writes -1 for an omitted query parameter.
fn opt(v: &Value) -> Option<u64> {
    let n = int(v);
    (n >= 0).then(|| u64::try_from(n).expect("non-negative"))
}

fn epoch(n: u64) -> Epoch {
    Epoch(format!("epoch-{n}"))
}

/// Every call of every step, in order, with where it was made.
fn calls<'a>(name: &'a str, itf: &'a Value) -> impl Iterator<Item = (String, &'a Value)> + 'a {
    list(&itf["states"])
        .iter()
        .enumerate()
        .flat_map(move |(k, s)| {
            let last = &s["last"];
            list(&last["calls"])
                .iter()
                .enumerate()
                .map(move |(j, c)| (format!("{name} state {k} call {j} ({})", last["act"]), c))
        })
}

// ---- decide_replay ----------------------------------------------------------------------------

/// The contract's replay bounds (mvp §2.4), written out rather than read from core so that
/// a changed constant fails here.
const CONTRACT_MAX_EVENTS: u64 = 10_000;
const CONTRACT_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Seqs per model seq and bytes per model size unit.
const SEQ_SCALE: u64 = 5_000;
const BYTE_SCALE: u64 = 2 * 1024 * 1024;

/// Checks that the trace's model bounds times the fixed scales are the contract's.
fn check_bounds(name: &str, itf: &Value) {
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
}

fn query(v: &Value) -> EventsQuery {
    EventsQuery {
        stream_id: StreamId::Thread(ThreadId::from_u128(1)),
        after_seq: opt(&v["after"]).map(|n| n * SEQ_SCALE),
        before_seq: opt(&v["before"]).map(|n| n * SEQ_SCALE),
        limit: opt(&v["limit"]).map(|n| u32::try_from(n).expect("limit fits u32")),
        epoch: Some(uint(&v["epoch"])).filter(|&e| e != 0).map(epoch),
    }
}

fn head(v: &Value) -> StreamHead {
    StreamHead {
        head_seq: uint(&v["headSeq"]) * SEQ_SCALE,
        cum_bytes_at_head: uint(&v["cumHead"]) * BYTE_SCALE,
        cum_bytes_at_after: uint(&v["cumAfter"]) * BYTE_SCALE,
    }
}

fn plan(v: &Value) -> Result<ReplayPlan, QueryError> {
    let limit = |p: &Value| u32::try_from(uint(&p["limit"])).expect("limit fits u32");
    match tag(v) {
        ("PForward", p) => Ok(ReplayPlan::Forward {
            after_seq: uint(&p["after"]) * SEQ_SCALE,
            limit: limit(p),
        }),
        ("PBackward", p) => Ok(ReplayPlan::Backward {
            before_seq: uint(&p["before"]) * SEQ_SCALE,
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
    for (name, itf) in itf::read_traces("sync") {
        check_bounds(&name, &itf);
        for (at, call) in calls(&name, &itf) {
            let (kind, c) = tag(call);
            assert_eq!(kind, "DecideReplay", "{at}: unknown call");
            let q = query(&c["q"]);
            let h = head(&c["head"]);
            assert_eq!(
                decide_replay(&q, h, &epoch(uint(&c["epoch"]))),
                plan(&c["plan"]),
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
    for (name, itf) in itf::read_traces("receipts") {
        for (at, call) in calls(&name, &itf) {
            let (kind, c) = tag(call);
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
