//! `decide_replay` and `check_receipt` tables (`docs/mvp-design.md` §5 "Tests").

use chaldea_core::command::{
    CommandReceipt, CommandResult, ReceiptLookup, ReceiptStatus, check_receipt,
};
use chaldea_core::ids::{CommandId, Epoch, StreamId, ThreadId, TokenId};
use chaldea_core::sync::{
    EventsQuery, LIMIT_DEFAULT, QueryError, REPLAY_MAX_BYTES, REPLAY_MAX_EVENTS, ReplayPlan,
    SnapshotReason, StreamHead, decide_replay,
};

fn query(
    after: Option<u64>,
    before: Option<u64>,
    limit: Option<u32>,
    epoch: Option<&str>,
) -> EventsQuery {
    EventsQuery {
        stream_id: StreamId::Thread(ThreadId::from_u128(1)),
        after_seq: after,
        before_seq: before,
        limit,
        epoch: epoch.map(Epoch::from),
    }
}

fn head(head_seq: u64) -> StreamHead {
    StreamHead {
        head_seq,
        cum_bytes_at_head: head_seq * 100,
        cum_bytes_at_after: 0,
    }
}

#[test]
fn decide_replay_table() {
    let epoch = Epoch::from("e1");
    let fwd = |after_seq, limit| Ok(ReplayPlan::Forward { after_seq, limit });
    let snap = |reason| Ok(ReplayPlan::Snapshot { reason });
    let cases: Vec<(
        &str,
        EventsQuery,
        StreamHead,
        Result<ReplayPlan, QueryError>,
    )> = vec![
        (
            "first fetch, epoch absent",
            query(Some(0), None, None, None),
            head(5),
            fwd(0, LIMIT_DEFAULT),
        ),
        (
            "after_seq absent means 0",
            query(None, None, Some(10), None),
            head(5),
            fwd(0, 10),
        ),
        (
            "matching epoch",
            query(Some(3), None, None, Some("e1")),
            head(5),
            fwd(3, LIMIT_DEFAULT),
        ),
        (
            "after_seq = head",
            query(Some(5), None, None, Some("e1")),
            head(5),
            fwd(5, LIMIT_DEFAULT),
        ),
        (
            "empty stream",
            query(Some(0), None, None, None),
            head(0),
            fwd(0, LIMIT_DEFAULT),
        ),
        (
            "epoch changed",
            query(Some(3), None, None, Some("e0")),
            head(5),
            snap(SnapshotReason::EpochChanged),
        ),
        (
            "after_seq above head",
            query(Some(6), None, None, Some("e1")),
            head(5),
            snap(SnapshotReason::CursorAhead),
        ),
        (
            "after_seq above head of empty stream",
            query(Some(1), None, None, None),
            head(0),
            snap(SnapshotReason::CursorAhead),
        ),
        (
            "too many events",
            query(Some(0), None, None, None),
            head(REPLAY_MAX_EVENTS + 1),
            snap(SnapshotReason::TooManyEvents),
        ),
        (
            "exactly the event bound replays",
            query(Some(0), None, None, None),
            StreamHead {
                head_seq: REPLAY_MAX_EVENTS,
                cum_bytes_at_head: 0,
                cum_bytes_at_after: 0,
            },
            fwd(0, LIMIT_DEFAULT),
        ),
        (
            "too many bytes",
            query(Some(10), None, None, None),
            StreamHead {
                head_seq: 20,
                cum_bytes_at_head: REPLAY_MAX_BYTES + 501,
                cum_bytes_at_after: 500,
            },
            snap(SnapshotReason::TooManyBytes),
        ),
        (
            "exactly the byte bound replays",
            query(Some(10), None, None, None),
            StreamHead {
                head_seq: 20,
                cum_bytes_at_head: REPLAY_MAX_BYTES + 500,
                cum_bytes_at_after: 500,
            },
            fwd(10, LIMIT_DEFAULT),
        ),
        (
            "limit 0",
            query(Some(0), None, Some(0), None),
            head(5),
            Err(QueryError::LimitOutOfRange),
        ),
        (
            "limit 1",
            query(Some(0), None, Some(1), None),
            head(5),
            fwd(0, 1),
        ),
        (
            "limit 1000",
            query(Some(0), None, Some(1000), None),
            head(5),
            fwd(0, 1000),
        ),
        (
            "limit 1001",
            query(Some(0), None, Some(1001), None),
            head(5),
            Err(QueryError::LimitOutOfRange),
        ),
        (
            "after_seq with before_seq",
            query(Some(0), Some(3), None, None),
            head(5),
            Err(QueryError::AfterAndBefore),
        ),
        (
            "limit is checked before the cursor pair",
            query(Some(0), Some(3), Some(0), None),
            head(5),
            Err(QueryError::LimitOutOfRange),
        ),
        (
            "backward page",
            query(None, Some(3), Some(2), Some("e1")),
            head(5),
            Ok(ReplayPlan::Backward {
                before_seq: 3,
                limit: 2,
            }),
        ),
        (
            "backward page at 1",
            query(None, Some(1), None, None),
            head(5),
            Ok(ReplayPlan::Backward {
                before_seq: 1,
                limit: LIMIT_DEFAULT,
            }),
        ),
        (
            "backward page with another epoch is still a page; the client resyncs forward",
            query(None, Some(3), None, Some("e0")),
            head(5),
            Ok(ReplayPlan::Backward {
                before_seq: 3,
                limit: LIMIT_DEFAULT,
            }),
        ),
    ];
    for (name, q, h, want) in cases {
        assert_eq!(decide_replay(&q, h, &epoch), want, "{name}");
    }
}

fn receipt(token: u128, hash: u8, status: ReceiptStatus) -> CommandReceipt {
    CommandReceipt {
        command_id: CommandId::from_u128(9),
        token: TokenId::from_u128(token),
        request_sha256: [hash; 32],
        status,
    }
}

#[test]
fn check_receipt_table() {
    let done = CommandResult {
        http_status: 200,
        body: r#"{"input_id":"x"}"#.to_owned(),
    };
    let me = TokenId::from_u128(1);
    let cases: Vec<(&str, Option<CommandReceipt>, ReceiptLookup)> = vec![
        ("no receipt", None, ReceiptLookup::New),
        (
            "in progress",
            Some(receipt(1, 7, ReceiptStatus::InProgress)),
            ReceiptLookup::InProgress,
        ),
        (
            "completed, same token and hash",
            Some(receipt(1, 7, ReceiptStatus::Completed(done.clone()))),
            ReceiptLookup::Replay(done.clone()),
        ),
        (
            "unknown after crash",
            Some(receipt(1, 7, ReceiptStatus::UnknownAfterCrash)),
            ReceiptLookup::UnknownAfterCrash,
        ),
        (
            "same token, other hash",
            Some(receipt(1, 8, ReceiptStatus::Completed(done.clone()))),
            ReceiptLookup::Conflict,
        ),
        (
            "other token, same hash, completed",
            Some(receipt(2, 7, ReceiptStatus::Completed(done.clone()))),
            ReceiptLookup::Conflict,
        ),
        (
            "other token, same hash, in progress reveals nothing",
            Some(receipt(2, 7, ReceiptStatus::InProgress)),
            ReceiptLookup::Conflict,
        ),
        (
            "other token, same hash, unknown after crash reveals nothing",
            Some(receipt(2, 7, ReceiptStatus::UnknownAfterCrash)),
            ReceiptLookup::Conflict,
        ),
    ];
    for (name, existing, want) in cases {
        assert_eq!(
            check_receipt(existing.as_ref(), me, &[7; 32]),
            want,
            "{name}"
        );
    }
}
