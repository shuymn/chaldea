//! Read snapshots and undecodable rows (PR6-CL-18).

use chaldea_core::event::{GenericReason, TypedEvent};
use chaldea_core::ids::StreamId;

use super::helpers::{RawEvent, raw};
use super::support::{TestStore, registered, soon, thread};

/// A commit by the writer between two statements of one read is invisible to the second;
/// a new read sees it.
#[test]
fn commit_between_selects_is_invisible() {
    let s = TestStore::new();
    s.append(vec![registered(thread(1), false)]).unwrap();
    let store = s.store().clone();
    let (first, second) = s.read(move |r| {
        let first = r.head(StreamId::Host, 0)?;
        // Forced through the writer while this read transaction holds its snapshot.
        store
            .submit(soon(), |tx| {
                Ok(tx.append(vec![registered(thread(2), false)])?)
            })
            .unwrap()
            .blocking_wait()
            .unwrap();
        let second = r.head(StreamId::Host, 0)?;
        let page = r.events_after(StreamId::Host, 0, 10)?;
        Ok((first, (second, page.events.len())))
    });
    assert_eq!(first.head_seq, 1);
    assert_eq!(second.0, first, "the second statement saw the commit");
    assert_eq!(second.1, 1);
    let now = s.read(|r| r.head(StreamId::Host, 0));
    assert_eq!(now.head_seq, 2);
    assert!(now.cum_bytes_at_head > first.cum_bytes_at_head);
}

/// Unknown kinds, newer schema versions, and invalid payloads come back as stored; decoding
/// them is the reader's.
#[test]
fn undecodable_events_are_returned() {
    let mut s = TestStore::new();
    s.append(vec![registered(thread(1), false)]).unwrap();
    s.shutdown();
    let c = raw(&s.path());
    let t = format!("thread:{}", thread(1));
    RawEvent::new(&t, "k1", "future_kind", r#"{"anything":[1,2]}"#)
        .insert(&c)
        .unwrap();
    let mut newer = RawEvent::new(
        &t,
        "k2",
        "assistant_message",
        r#"{"text":{"inline":"x","encoding":"utf8"},"turn_ref":null,"more":1}"#,
    );
    newer.schema_version = 2;
    newer.insert(&c).unwrap();
    RawEvent::new(&t, "k3", "user_message", r#"{"prompt":7}"#)
        .insert(&c)
        .unwrap();
    drop(c);
    s.reopen();
    let page = s.read(|r| r.events_after(StreamId::Thread(thread(1)), 0, 10));
    let reasons: Vec<GenericReason> = page
        .events
        .iter()
        .map(|e| TypedEvent::decode(e).expect_err("undecodable").why)
        .collect();
    assert_eq!(
        reasons,
        [
            GenericReason::UnknownKind,
            GenericReason::NewerSchema,
            GenericReason::PayloadInvalid
        ]
    );
    assert_eq!(page.events[0].payload.get(), r#"{"anything":[1,2]}"#);
    assert_eq!(page.events[1].schema_version, 2);
}
