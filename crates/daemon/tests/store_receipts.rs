//! Command receipts (PR6-CL-13).

mod support;

use std::sync::Arc;
use std::sync::Barrier;
use std::time::{Duration, Instant};

use chaldea_core::command::{CommandResult, ReceiptLookup, ReceiptStatus};
use chaldea_core::ids::{CommandId, StreamId, TokenId};
use chaldea_core::payload::Content;
use chaldea_daemon::store::{JobError, Store, WriteError};
use support::{TestStore, config, raw_unknown, soon};

const HASH: [u8; 32] = [7; 32];

fn cmd(n: u128) -> CommandId {
    CommandId::from_u128(0x5000_0000_0000_0000_0000_0000_0000_0000 | n)
}

fn token(n: u128) -> TokenId {
    TokenId::from_u128(0x6000_0000_0000_0000_0000_0000_0000_0000 | n)
}

fn begin(s: &Store, c: CommandId, t: TokenId, h: [u8; 32]) -> ReceiptLookup {
    s.submit(soon(), move |tx| Ok(tx.receipt_begin(c, t, h)?))
        .unwrap()
        .blocking_wait()
        .unwrap()
}

fn complete(s: &TestStore, c: CommandId, result: CommandResult) {
    s.run(move |tx| Ok(tx.receipt_complete(c, &result)?))
        .unwrap();
}

/// `receipt_begin` returns core's lookup for every stored state.
#[test]
fn lookup_cases() {
    let s = TestStore::new();
    let st = s.store();
    assert_eq!(begin(st, cmd(1), token(1), HASH), ReceiptLookup::New);
    assert_eq!(begin(st, cmd(1), token(1), HASH), ReceiptLookup::InProgress);
    assert_eq!(begin(st, cmd(1), token(2), HASH), ReceiptLookup::Conflict);
    assert_eq!(
        begin(st, cmd(1), token(1), [8; 32]),
        ReceiptLookup::Conflict
    );
    let result = CommandResult {
        http_status: 201,
        body: "{\"ok\":true}".into(),
    };
    complete(&s, cmd(1), result.clone());
    assert_eq!(
        begin(st, cmd(1), token(1), HASH),
        ReceiptLookup::Replay(result)
    );
    assert_eq!(begin(st, cmd(1), token(2), HASH), ReceiptLookup::Conflict);
    assert_eq!(begin(st, cmd(2), token(1), HASH), ReceiptLookup::New);
    s.run(|tx| Ok(tx.receipts_mark_unknown_after_crash()?))
        .unwrap();
    assert_eq!(
        begin(st, cmd(2), token(1), HASH),
        ReceiptLookup::UnknownAfterCrash
    );
    // A completed receipt cannot complete again.
    let r = s.run(move |tx| {
        Ok(tx.receipt_complete(
            cmd(1),
            &CommandResult {
                http_status: 500,
                body: String::new(),
            },
        )?)
    });
    assert!(matches!(r, Err(WriteError::RolledBack(_))), "{r:?}");
}

/// A failed intent job leaves neither the receipt nor its events.
#[test]
fn intent_and_receipt_commit_together() {
    let s = TestStore::new();
    let r = s.run(|tx| {
        assert_eq!(
            tx.receipt_begin(cmd(1), token(1), HASH)?,
            ReceiptLookup::New
        );
        tx.append(vec![raw_unknown(
            StreamId::Host,
            "command:1:1",
            Content::text("intent"),
        )])?;
        Err::<(), _>(JobError::Refused("precondition failed".into()))
    });
    assert!(matches!(r, Err(WriteError::Rejected(_))), "{r:?}");
    assert_eq!(s.read(|r| r.receipt(cmd(1))), None);
    assert_eq!(s.read(|r| r.head(StreamId::Host, 0)).head_seq, 0);
}

/// A completed receipt replays its status and body byte for byte.
#[test]
fn replay_is_exact() {
    let s = TestStore::new();
    let body = "{\"message\":\"日本語の本文 ✓\"}  \n\t ".to_owned();
    assert_eq!(begin(s.store(), cmd(1), token(1), HASH), ReceiptLookup::New);
    complete(
        &s,
        cmd(1),
        CommandResult {
            http_status: 202,
            body: body.clone(),
        },
    );
    let ReceiptLookup::Replay(r) = begin(s.store(), cmd(1), token(1), HASH) else {
        panic!("not a replay")
    };
    assert_eq!(r.http_status, 202);
    assert_eq!(r.body.as_bytes(), body.as_bytes());
    let stored = s.read(|r| r.receipt(cmd(1))).unwrap();
    assert_eq!(stored.status, ReceiptStatus::Completed(r));
}

fn race(stores: &[Store], c: CommandId) -> Vec<ReceiptLookup> {
    let barrier = Arc::new(Barrier::new(stores.len() * 4));
    let handles: Vec<_> = (0..stores.len() * 4)
        .map(|i| {
            let s = stores[i % stores.len()].clone();
            let b = barrier.clone();
            std::thread::spawn(move || {
                b.wait();
                begin(&s, c, token(1), HASH)
            })
        })
        .collect();
    handles.into_iter().map(|h| h.join().unwrap()).collect()
}

/// Concurrent duplicates insert one receipt, on one store and across two stores on one file.
#[test]
fn concurrent_duplicates_insert_once() {
    let s = TestStore::new();
    let out = race(std::slice::from_ref(s.store()), cmd(1));
    assert_eq!(
        out.iter().filter(|l| **l == ReceiptLookup::New).count(),
        1,
        "{out:?}"
    );
    assert!(
        out.iter()
            .all(|l| matches!(l, ReceiptLookup::New | ReceiptLookup::InProgress))
    );
    let second = Store::open(config(s.path(), s.clock.clone())).unwrap();
    let out = race(&[s.store().clone(), second.clone()], cmd(2));
    assert_eq!(
        out.iter().filter(|l| **l == ReceiptLookup::New).count(),
        1,
        "{out:?}"
    );
    assert!(
        out.iter()
            .all(|l| matches!(l, ReceiptLookup::New | ReceiptLookup::InProgress))
    );
    assert!(
        second
            .shutdown(Instant::now() + Duration::from_secs(20))
            .writer_exited
    );
}

/// A restart turns every `in_progress` receipt into `unknown_after_crash`, and only those.
#[test]
fn restart_marks_unknown_after_crash() {
    let mut s = TestStore::new();
    assert_eq!(begin(s.store(), cmd(1), token(1), HASH), ReceiptLookup::New);
    assert_eq!(begin(s.store(), cmd(2), token(1), HASH), ReceiptLookup::New);
    let done = CommandResult {
        http_status: 200,
        body: "{}".into(),
    };
    complete(&s, cmd(2), done.clone());
    s.reopen();
    assert_eq!(
        s.run(|tx| Ok(tx.receipts_mark_unknown_after_crash()?))
            .unwrap(),
        1
    );
    assert_eq!(
        s.read(|r| r.receipt(cmd(1))).unwrap().status,
        ReceiptStatus::UnknownAfterCrash
    );
    assert_eq!(
        s.read(|r| r.receipt(cmd(2))).unwrap().status,
        ReceiptStatus::Completed(done)
    );
    assert_eq!(
        begin(s.store(), cmd(1), token(1), HASH),
        ReceiptLookup::UnknownAfterCrash
    );
    assert_eq!(
        s.run(|tx| Ok(tx.receipts_mark_unknown_after_crash()?))
            .unwrap(),
        0
    );
}
