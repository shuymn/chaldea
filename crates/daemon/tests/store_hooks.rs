//! Hook deliveries: first reply, ordinals, routing, buffering, and promotion (PR6-CL-6).

mod support;

use chaldea_core::ids::{LaunchId, StreamId, ThreadId};
use chaldea_core::payload::Content;
use chaldea_daemon::store::{
    BufferedHook, HookDelivery, HookDeliveryLookup, HookReply, NewEvent, NewHookDelivery,
    NewRouting, Promotion, Routing, StoreError, WriteError,
};
use support::{TestStore, launch, raw_unknown, registered, reserved, thread};
use uuid::Uuid;

fn emit(n: u128) -> Uuid {
    Uuid::from_u128(0x4000_0000_0000_0000_0000_0000_0000_0000 | n)
}

fn setup() -> (TestStore, ThreadId, LaunchId) {
    let s = TestStore::new();
    let (t, l) = (thread(1), launch(1));
    s.append(vec![registered(t, false)]).unwrap();
    s.append(vec![reserved(t, l)]).unwrap();
    (s, t, l)
}

fn hook_event(t: ThreadId, e: Uuid, n: u32) -> NewEvent {
    raw_unknown(
        StreamId::Thread(t),
        &format!("hook:{e}:{n}"),
        Content::text(format!("hook {n}")),
    )
}

fn reply(body: &str) -> HookReply {
    HookReply {
        http_status: 200,
        body: body.into(),
    }
}

fn buffered(l: LaunchId, e: Uuid, ordinal: u64) -> NewHookDelivery {
    NewHookDelivery {
        launch_id: l,
        emit_id: e,
        receipt_ordinal: ordinal,
        first_reply: reply("{\"buffered\":true}"),
        routing: NewRouting::Buffered(BufferedHook {
            body: format!("payload {ordinal}").into_bytes(),
            meta: "{\"event\":\"UserPromptSubmit\"}".into(),
        }),
    }
}

fn record(s: &TestStore, d: NewHookDelivery) -> Result<HookDeliveryLookup, Box<WriteError>> {
    s.run(move |tx| Ok(tx.hook_delivery_record(d)?))
        .map_err(Box::new)
}

fn head(s: &TestStore, t: ThreadId) -> u64 {
    s.read(move |r| r.head(StreamId::Thread(t), 0)).head_seq
}

/// A retry with the same `(launch_id, emit_id)` gets the first reply and changes nothing.
#[test]
fn retry_replays_the_first_reply() {
    let (s, t, l) = setup();
    let e = emit(1);
    let first = NewHookDelivery {
        launch_id: l,
        emit_id: e,
        receipt_ordinal: 1,
        first_reply: reply("{\"first\":1}"),
        routing: NewRouting::Appended(vec![hook_event(t, e, 1)]),
    };
    let HookDeliveryLookup::New(stored) = record(&s, first).unwrap() else {
        panic!("not new")
    };
    let h = head(&s, t);
    let retry = NewHookDelivery {
        launch_id: l,
        emit_id: e,
        receipt_ordinal: 2,
        first_reply: reply("{\"second\":2}"),
        routing: NewRouting::Appended(vec![hook_event(t, e, 2)]),
    };
    let HookDeliveryLookup::Replay(replayed) = record(&s, retry).unwrap() else {
        panic!("not a replay")
    };
    assert_eq!(replayed, stored);
    assert_eq!(replayed.first_reply.body, "{\"first\":1}");
    assert_eq!(head(&s, t), h);
}

/// A retry after promotion sees the new routing and the original reply.
#[test]
fn retry_after_promotion_sees_new_routing() {
    let (s, t, l) = setup();
    let e = emit(1);
    record(&s, buffered(l, e, 1)).unwrap();
    let ev = hook_event(t, e, 1);
    let appended = s
        .run(move |tx| Ok(tx.hook_delivery_promote(l, e, Promotion::Appended(vec![ev]))?))
        .unwrap();
    let seq = appended[0].envelope().seq;
    let HookDeliveryLookup::Replay(d) = record(&s, buffered(l, e, 5)).unwrap() else {
        panic!("not a replay")
    };
    assert_eq!(d.first_reply.body, "{\"buffered\":true}");
    assert_eq!(d.receipt_ordinal, 1);
    assert_eq!(
        d.routing,
        Routing::Appended {
            stream_id: StreamId::Thread(t),
            seq
        }
    );
    // Promotion cleared the buffered data.
    assert!(s.read(move |r| r.buffered_deliveries(l)).is_empty());
}

/// Promotion appends exactly the caller's events, two for one delivery.
#[test]
fn promotion_appends_the_given_events() {
    let (s, t, l) = setup();
    let e = emit(1);
    record(&s, buffered(l, e, 1)).unwrap();
    let h = head(&s, t);
    let events = vec![hook_event(t, e, 1), hook_event(t, e, 2)];
    let out = s
        .run(move |tx| Ok(tx.hook_delivery_promote(l, e, Promotion::Appended(events))?))
        .unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(head(&s, t), h + 2);
    let d = s.read(move |r| r.hook_delivery(l, e)).unwrap();
    assert_eq!(
        d.routing,
        Routing::Appended {
            stream_id: StreamId::Thread(t),
            seq: h + 1
        }
    );
    // A second promotion is refused: it is no longer buffered.
    let ev = hook_event(t, e, 3);
    let r = s.run(move |tx| Ok(tx.hook_delivery_promote(l, e, Promotion::RawUnknown(ev))?));
    assert!(
        matches!(r, Err(WriteError::RolledBack(StoreError::NotBuffered))),
        "{r:?}"
    );
}

/// Buffered deliveries are read in receipt order, not commit order.
#[test]
fn buffered_read_in_receipt_order() {
    let (s, _, l) = setup();
    record(&s, buffered(l, emit(2), 2)).unwrap();
    record(&s, buffered(l, emit(1), 1)).unwrap();
    let got = s.read(move |r| r.buffered_deliveries(l));
    let order: Vec<(Uuid, u64)> = got.iter().map(|d| (d.emit_id, d.receipt_ordinal)).collect();
    assert_eq!(order, vec![(emit(1), 1), (emit(2), 2)]);
    assert_eq!(got[0].hook.body, b"payload 1");
    let commit = |e| {
        s.read(move |r| r.hook_delivery(l, e))
            .unwrap()
            .commit_ordinal
    };
    assert_eq!((commit(emit(2)), commit(emit(1))), (1, 2));
    assert_eq!(
        s.read(|r: &chaldea_daemon::store::ReadTx<'_>| r.buffered_launches()),
        vec![l]
    );
}

/// A routing whose events fail stores no delivery.
#[test]
fn failed_routing_stores_no_delivery() {
    let (s, t, l) = setup();
    let e = emit(1);
    // An event on a thread that does not exist fails the stream-membership check.
    let mut bad = hook_event(t, e, 1);
    bad.stream_id = StreamId::Thread(thread(9));
    let d = NewHookDelivery {
        launch_id: l,
        emit_id: e,
        receipt_ordinal: 1,
        first_reply: reply("{}"),
        routing: NewRouting::Appended(vec![hook_event(t, e, 0), bad]),
    };
    assert!(matches!(
        record(&s, d).map_err(|e| *e),
        Err(WriteError::RolledBack(_))
    ));
    assert_eq!(s.read(move |r| r.hook_delivery(l, e)), None::<HookDelivery>);
    // A raw_unknown routing whose event is of another kind is refused too.
    let mut other = hook_event(t, e, 2);
    other.event = support::assistant("x");
    let d = NewHookDelivery {
        launch_id: l,
        emit_id: e,
        receipt_ordinal: 1,
        first_reply: reply("{}"),
        routing: NewRouting::RawUnknown(other),
    };
    assert!(matches!(
        record(&s, d).map_err(|e| *e),
        Err(WriteError::RolledBack(StoreError::BadRouting(_)))
    ));
    assert_eq!(s.read(move |r| r.hook_delivery(l, e)), None::<HookDelivery>);
}

/// A promotion or routing without events is refused and changes nothing.
#[test]
fn empty_promotion_is_refused() {
    let (s, _, l) = setup();
    let e = emit(1);
    record(&s, buffered(l, e, 1)).unwrap();
    let r = s.run(move |tx| Ok(tx.hook_delivery_promote(l, e, Promotion::Appended(Vec::new()))?));
    assert!(
        matches!(r, Err(WriteError::RolledBack(StoreError::BadRouting(_)))),
        "{r:?}"
    );
    assert_eq!(
        s.read(move |r| r.hook_delivery(l, e)).unwrap().routing,
        Routing::Buffered
    );
    let d = NewHookDelivery {
        launch_id: l,
        emit_id: emit(2),
        receipt_ordinal: 2,
        first_reply: reply("{}"),
        routing: NewRouting::Appended(Vec::new()),
    };
    assert!(matches!(
        record(&s, d).map_err(|e| *e),
        Err(WriteError::RolledBack(StoreError::BadRouting(_)))
    ));
}
