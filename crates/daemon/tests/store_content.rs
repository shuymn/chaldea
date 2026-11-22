//! `Content` preparation, validation at the store boundary, blobs and refs (PR6-CL-11, CL-12).

mod support;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chaldea_core::event::TypedEvent;
use chaldea_core::ids::{IdempotencyKey, StreamId, SurfaceId};
use chaldea_core::payload::{
    BlobRef, Component, Content, Encoding, RawUnknown, SurfaceAction, SurfaceActionKind,
    SurfaceScope, SurfaceUpdate, ToolActivity, ToolPhase,
};
use chaldea_daemon::store::{
    ContentError, NewEvent, Prepared, StoreError, WriteError, prepare_content,
};
use support::{TestStore, blob_input, event, raw_unknown, thread};

const TEXT: &str = "text/plain; charset=utf-8";
const JSON: &str = "application/json";

fn prep(bytes: Vec<u8>, media: &str) -> Prepared {
    prepare_content(bytes, media).unwrap()
}

/// A `raw_unknown` carrying `p` with its blob input, if any.
fn carrying(stream: StreamId, key: &str, p: &Prepared) -> NewEvent {
    let mut e = raw_unknown(stream, key, p.content.clone());
    e.blobs = p.blob.clone().into_iter().collect();
    e
}

fn sha(c: &Content) -> String {
    match c {
        Content::Blob { blob, .. } => blob.sha256.clone(),
        Content::Inline { .. } => panic!("not a blob: {c:?}"),
    }
}

fn blob_of(s: &TestStore, stream: StreamId, sha: &str) -> Option<chaldea_daemon::store::BlobBody> {
    let sha = sha.to_owned();
    s.read(move |r| r.blob(stream, &sha))
}

fn stored_raw(s: &TestStore, stream: StreamId, key: &str) -> Content {
    let key = IdempotencyKey(key.to_owned());
    let e = s.read(move |r| r.event_by_key(stream, &key)).unwrap();
    match TypedEvent::decode(&e).unwrap() {
        TypedEvent::RawUnknown(RawUnknown { raw, .. }) => raw,
        other => panic!("{other:?}"),
    }
}

fn refused(r: Result<Vec<chaldea_daemon::store::Appended>, WriteError>) -> ContentError {
    match r {
        Err(WriteError::RolledBack(StoreError::Content(e))) => e,
        other => panic!("expected a ContentError, got {other:?}"),
    }
}

fn head(s: &TestStore, stream: StreamId) -> u64 {
    s.read(move |r| r.head(stream, 0)).head_seq
}

/// At most 65 536 bytes stay inline (utf8 or base64); above it the content is a blob, and each
/// form round-trips through the store.
#[test]
fn boundary_inline_and_blob() {
    let s = TestStore::new();
    for (i, n) in [65_535usize, 65_536, 65_537].into_iter().enumerate() {
        for binary in [false, true] {
            let bytes = if binary { vec![0xFF; n] } else { vec![b'a'; n] };
            let p = prep(bytes.clone(), TEXT);
            let key = format!("k{i}{binary}");
            match (&p.content, n <= 65_536) {
                (Content::Inline { inline, encoding }, true) => {
                    assert!(p.blob.is_none());
                    if binary {
                        assert_eq!(*encoding, Encoding::Base64);
                        assert_eq!(STANDARD.decode(inline).unwrap(), bytes);
                    } else {
                        assert_eq!(*encoding, Encoding::Utf8);
                        assert_eq!(inline.as_bytes(), bytes.as_slice());
                    }
                }
                (
                    Content::Blob {
                        blob,
                        preview_lossy,
                        ..
                    },
                    false,
                ) => {
                    assert_eq!(blob.size, n as u64);
                    assert_eq!(blob.media_type, TEXT);
                    assert_eq!(*preview_lossy, binary);
                    assert_eq!(p.blob.as_ref().unwrap().data, bytes);
                }
                (c, _) => panic!("{n} bytes binary {binary}: {c:?}"),
            }
            s.append(vec![carrying(StreamId::Host, &key, &p)]).unwrap();
            assert_eq!(stored_raw(&s, StreamId::Host, &key), p.content);
            if n > 65_536 {
                let body = blob_of(&s, StreamId::Host, &sha(&p.content)).unwrap();
                assert_eq!(body.data, bytes);
                assert_eq!(body.media_type, TEXT);
            }
        }
    }
}

fn preview_of(bytes: Vec<u8>) -> (String, bool) {
    match prep(bytes, TEXT).content {
        Content::Blob {
            preview,
            preview_lossy,
            ..
        } => (preview, preview_lossy),
        c @ Content::Inline { .. } => panic!("{c:?}"),
    }
}

fn padded(head: &[u8]) -> Vec<u8> {
    let mut v = head.to_vec();
    v.resize(70_000, b'a');
    v
}

/// A valid character crossing byte 1024 is left out without setting `preview_lossy`, and the
/// store accepts that preview.
#[test]
fn preview_cut_valid_character() {
    let mut head = vec![b'a'; 1023];
    head.extend_from_slice("あ".as_bytes());
    let bytes = padded(&head);
    assert_eq!(preview_of(bytes.clone()), ("a".repeat(1023), false));
    let s = TestStore::new();
    s.append(vec![carrying(StreamId::Host, "k", &prep(bytes, TEXT))])
        .unwrap();
}

/// An invalid sequence at the boundary (0xE3 0x20 at 1023–1024) is U+FFFD and sets the flag.
#[test]
fn preview_invalid_byte_at_the_boundary() {
    let mut head = vec![b'a'; 1023];
    head.extend_from_slice(&[0xE3, 0x20]);
    let bytes = padded(&head);
    assert_eq!(
        preview_of(bytes.clone()),
        (format!("{}\u{FFFD}", "a".repeat(1023)), true)
    );
    let s = TestStore::new();
    s.append(vec![carrying(StreamId::Host, "k", &prep(bytes, TEXT))])
        .unwrap();
}

/// An invalid byte inside the window is U+FFFD and sets the flag.
#[test]
fn preview_invalid_byte_before_the_boundary() {
    let mut head = vec![b'a'; 10];
    head.push(0xFF);
    let (preview, lossy) = preview_of(padded(&head));
    assert!(lossy);
    assert_eq!(
        preview,
        format!("{}\u{FFFD}{}", "a".repeat(10), "a".repeat(1013))
    );
}

/// A U+FFFD present in the content is valid UTF-8 and does not set the flag.
#[test]
fn preview_keeps_a_literal_replacement_character() {
    let (preview, lossy) = preview_of(padded("\u{FFFD}".as_bytes()));
    assert!(!lossy);
    assert!(preview.starts_with('\u{FFFD}'));
    assert_eq!(preview.len(), 1024);
}

/// The store refuses every `Content` that breaks §2.5's rules.
#[test]
fn store_refuses_bad_content() {
    let s = TestStore::new();
    let big = prep(vec![b'b'; 70_000], TEXT);
    let with = |key: &str, c: Content, blobs: Vec<Vec<u8>>| {
        let mut e = raw_unknown(StreamId::Host, key, c);
        e.blobs = blobs.iter().map(|b| blob_input(b)).collect();
        e
    };
    let bytes = big.blob.clone().unwrap().data;
    let edit = |f: &dyn Fn(&mut BlobRef, &mut String, &mut bool)| {
        let Content::Blob {
            mut blob,
            mut preview,
            mut preview_lossy,
        } = big.content.clone()
        else {
            unreachable!()
        };
        f(&mut blob, &mut preview, &mut preview_lossy);
        Content::Blob {
            blob,
            preview,
            preview_lossy,
        }
    };
    let cases: Vec<(&str, NewEvent)> = vec![
        (
            "inline above 64 KiB",
            with("a", Content::text("x".repeat(65_537)), vec![]),
        ),
        (
            "base64 of UTF-8",
            with(
                "b",
                Content::Inline {
                    inline: STANDARD.encode("hello"),
                    encoding: Encoding::Base64,
                },
                vec![],
            ),
        ),
        (
            "forged hash",
            with(
                "c",
                edit(&|b, _, _| b.sha256 = "0".repeat(64)),
                vec![bytes.clone()],
            ),
        ),
        (
            "size mismatch",
            with("d", edit(&|b, _, _| b.size += 1), vec![bytes.clone()]),
        ),
        (
            "wrong preview",
            with("e", edit(&|_, p, _| p.push('x')), vec![bytes.clone()]),
        ),
        (
            "wrong preview_lossy",
            with("f", edit(&|_, _, l| *l = true), vec![bytes.clone()]),
        ),
        (
            "unreferenced blob input",
            with("g", Content::text("small"), vec![bytes.clone()]),
        ),
    ];
    let expected: [fn(&ContentError) -> bool; 7] = [
        |e| matches!(e, ContentError::InlineTooLarge(65_537)),
        |e| matches!(e, ContentError::Base64OfUtf8),
        |e| matches!(e, ContentError::BlobNotFound(_)),
        |e| matches!(e, ContentError::BlobMismatch { field: "size", .. }),
        |e| {
            matches!(
                e,
                ContentError::BlobMismatch {
                    field: "preview",
                    ..
                }
            )
        },
        |e| {
            matches!(
                e,
                ContentError::BlobMismatch {
                    field: "preview_lossy",
                    ..
                }
            )
        },
        |e| matches!(e, ContentError::UnreferencedBlob(_)),
    ];
    for ((what, e), ok) in cases.into_iter().zip(expected) {
        let err = refused(s.append(vec![e]));
        assert!(ok(&err), "{what}: {err:?}");
    }
    assert_eq!(head(&s, StreamId::Host), 0);
    assert!(blob_of(&s, StreamId::Host, &sha(&big.content)).is_none());
}

/// A blob, its event, and its ref commit together and read back in full.
#[test]
fn blob_round_trip() {
    let s = TestStore::new();
    let t = StreamId::Thread(thread(1));
    let bytes: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    let p = prep(bytes.clone(), "application/octet-stream");
    s.append(vec![carrying(t, "k", &p)]).unwrap();
    let body = blob_of(&s, t, &sha(&p.content)).unwrap();
    assert_eq!(body.data, bytes);
    assert_eq!(body.media_type, "application/octet-stream");
}

fn tool(input: Content, output: Content) -> TypedEvent {
    TypedEvent::ToolActivity(ToolActivity {
        phase: ToolPhase::Completed,
        tool_name: "Bash".into(),
        tool_use_id: None,
        turn_ref: None,
        input,
        output: Some(output),
    })
}

/// One blob referenced twice in one event (tool input and output) is stored once.
#[test]
fn same_blob_twice_in_one_event() {
    let s = TestStore::new();
    let t = StreamId::Thread(thread(1));
    let p = prep(vec![b'q'; 80_000], TEXT);
    let mut e = event(t, "k", tool(p.content.clone(), p.content.clone()));
    e.blobs = p.blob.clone().into_iter().collect();
    s.append(vec![e]).unwrap();
    assert_eq!(blob_of(&s, t, &sha(&p.content)).unwrap().data.len(), 80_000);
}

/// A later event references a stored blob without passing its bytes again.
#[test]
fn same_blob_across_events() {
    let s = TestStore::new();
    let t = StreamId::Thread(thread(1));
    let p = prep(vec![b'r'; 80_000], TEXT);
    s.append(vec![carrying(t, "k1", &p)]).unwrap();
    s.append(vec![raw_unknown(t, "k2", p.content.clone())])
        .unwrap();
    // And in another stream, which gains its own ref.
    let u = StreamId::Thread(thread(2));
    assert!(blob_of(&s, u, &sha(&p.content)).is_none());
    s.append(vec![raw_unknown(u, "k3", p.content.clone())])
        .unwrap();
    assert!(blob_of(&s, u, &sha(&p.content)).is_some());
    assert_eq!(head(&s, t), 2);
}

/// A blob reference with neither stored bytes nor a blob input aborts the event.
#[test]
fn missing_blob_aborts_the_event() {
    let s = TestStore::new();
    let t = StreamId::Thread(thread(1));
    let p = prep(vec![b's'; 80_000], TEXT);
    let err = refused(s.append(vec![
        raw_unknown(t, "ok", Content::text("fine")),
        raw_unknown(t, "k", p.content.clone()),
    ]));
    assert!(matches!(err, ContentError::BlobNotFound(_)), "{err:?}");
    assert_eq!(head(&s, t), 0);
}

fn blob_json(c: &Content) -> serde_json::Value {
    serde_json::to_value(c).unwrap()
}

/// A blob-shaped object in `SurfaceAction.args` is plain data: no ref, no refusal, no access.
#[test]
fn blob_shape_in_surface_args_is_data() {
    let s = TestStore::new();
    let t = StreamId::Thread(thread(1));
    let theirs = prep(vec![b't'; 80_000], TEXT);
    s.append(vec![carrying(t, "k", &theirs)]).unwrap();
    let unknown = prep(vec![b'u'; 80_000], TEXT);
    let args = serde_json::json!({
        "a": blob_json(&theirs.content),
        "b": [blob_json(&unknown.content)],
        "c": { "type": "vt_snapshot", "grid": blob_json(&unknown.content) },
        "d": [{ "type": "vt_snapshot", "grid": blob_json(&theirs.content) }],
    });
    let e = event(
        StreamId::Host,
        "surface",
        TypedEvent::SurfaceUpdate(SurfaceUpdate {
            surface_id: SurfaceId("s".into()),
            scope: SurfaceScope::Host,
            components: vec![Component::Button {
                label: "go".into(),
                action: SurfaceAction {
                    action_id: "a".into(),
                    kind: SurfaceActionKind::SendInput,
                    args,
                },
            }],
        }),
    );
    s.append(vec![e]).unwrap();
    assert_eq!(head(&s, StreamId::Host), 1);
    assert!(blob_of(&s, StreamId::Host, &sha(&theirs.content)).is_none());
    assert!(blob_of(&s, StreamId::Host, &sha(&unknown.content)).is_none());
}

fn grid_surface(thread_id: chaldea_core::ids::ThreadId, grid: Content) -> TypedEvent {
    TypedEvent::SurfaceUpdate(SurfaceUpdate {
        surface_id: SurfaceId("grid".into()),
        scope: SurfaceScope::Thread { thread_id },
        components: vec![Component::List {
            items: vec![Component::List {
                items: vec![Component::VtSnapshot {
                    grid,
                    cols: 80,
                    rows: 24,
                }],
            }],
        }],
    })
}

/// A `vt_snapshot` grid blob reached through `list` items gets a ref and is readable.
#[test]
fn nested_list_grid_needs_a_ref() {
    let s = TestStore::new();
    let tid = thread(1);
    let t = StreamId::Thread(tid);
    let p = prep(vec![b'g'; 80_000], JSON);
    let mut e = event(t, "k", grid_surface(tid, p.content.clone()));
    e.blobs = p.blob.clone().into_iter().collect();
    s.append(vec![e]).unwrap();
    let body = blob_of(&s, t, &sha(&p.content)).unwrap();
    assert_eq!(body.data.len(), 80_000);
    assert_eq!(body.media_type, JSON);
    // Without the bytes and unstored, the nested grid aborts the event.
    let q = prep(vec![b'h'; 80_000], JSON);
    let err = refused(s.append(vec![event(t, "k2", grid_surface(tid, q.content.clone()))]));
    assert!(matches!(err, ContentError::BlobNotFound(_)), "{err:?}");
}

/// A stream without a ref to a blob cannot read it.
#[test]
fn unreferenced_stream_cannot_read() {
    let s = TestStore::new();
    let (a, b) = (StreamId::Thread(thread(1)), StreamId::Thread(thread(2)));
    let p = prep(vec![b'v'; 80_000], TEXT);
    s.append(vec![carrying(a, "k", &p)]).unwrap();
    assert!(blob_of(&s, a, &sha(&p.content)).is_some());
    assert!(blob_of(&s, b, &sha(&p.content)).is_none());
    assert!(blob_of(&s, StreamId::Host, &sha(&p.content)).is_none());
}

/// The same bytes under two media types never cost an event; each stream's download type is
/// its agreed type or `application/octet-stream`, whatever the insertion order.
#[test]
fn media_types_per_reference() {
    let bytes = br#"{"k":"v"}"#.repeat(8_000);
    let as_text = prep(bytes.clone(), TEXT);
    let as_json = prep(bytes.clone(), JSON);
    let h = sha(&as_text.content);
    assert_eq!(h, sha(&as_json.content));
    let (a, b) = (StreamId::Thread(thread(1)), StreamId::Thread(thread(2)));
    for first_text in [true, false] {
        let (first, second) = if first_text {
            (&as_text, &as_json)
        } else {
            (&as_json, &as_text)
        };
        let check_events = |s: &TestStore, stream: StreamId, keys: &[(&str, &Prepared)]| {
            for (k, p) in keys {
                assert_eq!(stored_raw(s, stream, k), p.content);
            }
        };
        // Different streams.
        let s = TestStore::new();
        s.append(vec![carrying(a, "1", first)]).unwrap();
        s.append(vec![carrying(b, "2", second)]).unwrap();
        check_events(&s, a, &[("1", first)]);
        check_events(&s, b, &[("2", second)]);
        let (ta, tb) = (blob_of(&s, a, &h).unwrap(), blob_of(&s, b, &h).unwrap());
        assert_eq!(
            (ta.data.as_slice(), tb.data.as_slice()),
            (bytes.as_slice(), bytes.as_slice())
        );
        let media = |p: &Prepared| match &p.content {
            Content::Blob { blob, .. } => blob.media_type.clone(),
            Content::Inline { .. } => unreachable!(),
        };
        assert_eq!(ta.media_type, media(first));
        assert_eq!(tb.media_type, media(second));
        // One stream, two jobs.
        let s = TestStore::new();
        s.append(vec![carrying(a, "1", first)]).unwrap();
        s.append(vec![carrying(a, "2", second)]).unwrap();
        check_events(&s, a, &[("1", first), ("2", second)]);
        let body = blob_of(&s, a, &h).unwrap();
        assert_eq!(body.data, bytes);
        assert_eq!(body.media_type, "application/octet-stream");
        // One stream, one job.
        let s = TestStore::new();
        s.append(vec![carrying(a, "1", first), carrying(a, "2", second)])
            .unwrap();
        check_events(&s, a, &[("1", first), ("2", second)]);
        let body = blob_of(&s, a, &h).unwrap();
        assert_eq!(body.data, bytes);
        assert_eq!(body.media_type, "application/octet-stream");
    }
}

/// Two separately prepared references to the same bytes in one event: one body, a ref per
/// media type.
#[test]
fn separately_prepared_equal_blobs_commit_both_refs() {
    let s = TestStore::new();
    let t = StreamId::Thread(thread(1));
    let bytes = format!("\"{}\"", "x".repeat(79_998)).into_bytes();
    let a = prep(bytes.clone(), TEXT);
    let b = prep(bytes.clone(), JSON);
    let hash = sha(&a.content);
    let mut e = event(t, "k", tool(a.content, b.content));
    e.blobs = vec![a.blob.unwrap(), b.blob.unwrap()];

    s.append(vec![e]).unwrap();
    let body = blob_of(&s, t, &hash).unwrap();
    assert_eq!(body.data, bytes);
    assert_eq!(body.media_type, "application/octet-stream");
}
