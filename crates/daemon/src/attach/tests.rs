//! The attach socket through the production mount (`docs/design/t1-attach.md` §4, §5):
//! FakePty launches of the send tests' rig, a WebSocket client on the real listener.

use std::time::Duration;

use chaldea_core::attach::{
    AttachReason, AttachServerMessage, EndReason, PasteOutcome, PasteReason,
};
use chaldea_core::auth::TokenKind;
use chaldea_core::ids::ThreadId;
use chaldea_core::input::{MAX_PROMPT_BYTES, RejectReason};
use chaldea_pty_runtime::PtySize;
use chaldea_pty_runtime::fake::FakeEvent;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use crate::store::tests::support::http::WS_WAIT;
use crate::store::tests::support::waits::{bounded, bounded_at};
use crate::supervisor::send::tests::{Live, SRig};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A frame the server sent.
#[derive(Debug, PartialEq, Eq)]
enum Frame {
    Text(AttachServerMessage),
    Binary(Vec<u8>),
}

/// An attach client.
struct Client {
    ws: Ws,
    /// The close code, once the server closed (`Some(None)`: without a close frame).
    closed: Option<Option<u16>>,
}

impl Client {
    /// The next frame within the wait (a hang guard: one bound for the call, so pings do
    /// not extend it), or `None` once closed.
    async fn next(&mut self) -> Option<Frame> {
        let until = tokio::time::Instant::now() + WS_WAIT;
        loop {
            let m = bounded_at(until, self.ws.next())
                .await
                .expect("a frame or the close within the wait");
            match m {
                Some(Ok(WsMessage::Text(t))) => {
                    return Some(Frame::Text(
                        serde_json::from_str(t.as_str())
                            .unwrap_or_else(|e| panic!("not an attach frame ({e}): {t}")),
                    ));
                }
                Some(Ok(WsMessage::Binary(b))) => return Some(Frame::Binary(b.to_vec())),
                Some(Ok(WsMessage::Close(f))) => {
                    self.closed = Some(f.map(|f| u16::from(f.code)));
                    return None;
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => {
                    self.closed.get_or_insert(None);
                    return None;
                }
            }
        }
    }

    /// The next text frame, binary output skipped.
    async fn text(&mut self) -> Option<AttachServerMessage> {
        loop {
            match self.next().await? {
                Frame::Text(m) => return Some(m),
                Frame::Binary(_) => {}
            }
        }
    }

    /// Output bytes until `want` was seen; the bytes.
    async fn output_until(&mut self, want: &[u8]) -> Vec<u8> {
        let mut seen = Vec::new();
        while !seen.windows(want.len()).any(|w| w == want) {
            match self.next().await.expect("open") {
                Frame::Binary(b) => seen.extend_from_slice(&b),
                Frame::Text(m) => panic!("unexpected {m:?}"),
            }
        }
        seen
    }

    /// Reads until the server closes; the close code and the text frames before it.
    async fn until_closed(&mut self) -> (Option<u16>, Vec<AttachServerMessage>) {
        let mut texts = Vec::new();
        while let Some(f) = self.next().await {
            if let Frame::Text(m) = f {
                texts.push(m);
            }
        }
        (self.closed.flatten(), texts)
    }

    async fn binary(&mut self, b: &[u8]) {
        self.ws
            .send(WsMessage::Binary(b.to_vec().into()))
            .await
            .unwrap();
    }

    async fn json(&mut self, v: serde_json::Value) {
        self.ws
            .send(WsMessage::Text(v.to_string().into()))
            .await
            .unwrap();
    }
}

/// Opens the attach socket of `t` with a fresh ticket of `secret`.
async fn open_as(rig: &SRig, secret: &str, t: ThreadId) -> Result<Client, u16> {
    let ticket = rig
        .srv
        .ticket(secret, &format!("attach:{t}"))
        .await
        .map_err(|r| r.status)?;
    let url = format!(
        "ws://127.0.0.1:{}/api/attach/{t}?ticket={ticket}",
        rig.srv.port
    );
    match bounded(WS_WAIT, tokio_tungstenite::connect_async(url))
        .await
        .expect("the handshake ends within the wait")
    {
        Ok((ws, _)) => Ok(Client { ws, closed: None }),
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => Err(r.status().as_u16()),
        Err(e) => panic!("not an HTTP refusal: {e}"),
    }
}

/// Opens the socket as the owner and reads `attached`.
async fn attach(rig: &SRig, live: &Live) -> (Client, AttachServerMessage) {
    let mut c = open_as(rig, &rig.owner, live.t).await.expect("a 101");
    let first = c.text().await.expect("attached");
    assert!(
        matches!(first, AttachServerMessage::Attached { .. }),
        "{first:?}"
    );
    (c, first)
}

fn grid_of(m: &AttachServerMessage) -> Option<Vec<Vec<String>>> {
    match m {
        AttachServerMessage::Attached { grid, .. } => grid.clone(),
        _ => None,
    }
}

fn resizes(live: &Live) -> Vec<PtySize> {
    live.child
        .log()
        .into_iter()
        .filter_map(|e| match e {
            FakeEvent::Resize(s) => Some(s),
            _ => None,
        })
        .collect()
}

fn human_writes(rig: &SRig, live: &Live) -> usize {
    rig.of_kind(live.t, "human_write").len()
}

/// Waits until the child's input holds `want`.
async fn input_has(rig: &SRig, live: &Live, want: &[u8]) {
    rig.until("the input arrived", || {
        live.child.input().windows(want.len()).any(|w| w == want)
    })
    .await;
}

/// T1-2: `attached` is the snapshot, then exactly the later bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn initial_screen_gapless() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let rx = rig.watch(&live).await;
    live.child.emit(b"hello");
    SRig::delivered(&rx, b"hello").await;
    let (mut c, first) = attach(&rig, &live).await;
    let grid = grid_of(&first).expect("a grid");
    assert_eq!(grid[0].concat(), "hello");
    live.child.emit(b" world");
    assert_eq!(c.output_until(b" world").await, b" world");
    rig.stop().await;
}

/// T1-3: the first resize at an equal size nudges a row and back; a later equal size is a
/// no-op; each attempt answers `size`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_resize_forces_redraw() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, first) = attach(&rig, &live).await;
    let AttachServerMessage::Attached { cols, rows, .. } = first else {
        unreachable!()
    };
    c.json(json!({ "type": "resize", "cols": cols, "rows": rows }))
        .await;
    assert_eq!(
        c.text().await,
        Some(AttachServerMessage::Size {
            cols,
            rows,
            ok: true
        })
    );
    assert_eq!(
        resizes(&live),
        [
            PtySize::new(cols, rows + 1).unwrap(),
            PtySize::new(cols, rows).unwrap()
        ]
    );
    c.json(json!({ "type": "resize", "cols": cols, "rows": rows }))
        .await;
    assert_eq!(
        c.text().await,
        Some(AttachServerMessage::Size {
            cols,
            rows,
            ok: true
        })
    );
    assert_eq!(resizes(&live).len(), 2, "a later equal size is a no-op");
    c.json(json!({ "type": "resize", "cols": 100, "rows": 30 }))
        .await;
    assert_eq!(
        c.text().await,
        Some(AttachServerMessage::Size {
            cols: 100,
            rows: 30,
            ok: true
        })
    );
    assert_eq!(resizes(&live).len(), 3);
    rig.stop().await;
}

/// T1-3: a failed leg answers `ok: false`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nudge_second_leg_fails() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, first) = attach(&rig, &live).await;
    let AttachServerMessage::Attached { cols, rows, .. } = first else {
        unreachable!()
    };
    live.child.fail_next_resize();
    c.json(json!({ "type": "resize", "cols": cols, "rows": rows }))
        .await;
    let Some(AttachServerMessage::Size { ok, .. }) = c.text().await else {
        panic!("a size frame")
    };
    assert!(!ok);
    rig.stop().await;
}

/// T1-4: binary frames are written unchanged and in order, and record `human_write` once
/// for the interval.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raw_input_order() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    for b in [b"ab".as_slice(), b"c", "\u{3042}".as_bytes(), b"\x1b[A"] {
        c.binary(b).await;
    }
    let want = [b"abc".as_slice(), "\u{3042}".as_bytes(), b"\x1b[A"].concat();
    input_has(&rig, &live, &want).await;
    rig.until("recorded", || human_writes(&rig, &live) == 1)
        .await;
    rig.stop().await;
}

/// T1-4: attach input closes the gate: a send stays queued until `ready`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_closes_gate() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    c.binary(b"x").await;
    rig.until("recorded", || human_writes(&rig, &live) == 1)
        .await;
    let id = rig.send(&live, "after").await;
    rig.pass(&live).await;
    assert_eq!(rig.input(id).state, chaldea_core::input::InputState::Queued);
    rig.cmd_ok("ready", &json!({ "thread_id": live.t }), 200)
        .await;
    rig.until_input(id, chaldea_core::input::InputState::Sent)
        .await;
    rig.stop().await;
}

/// T1-5: an accepted paste writes the encoder's `AttachPaste` text as one part, no submit,
/// and answers `written`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paste_one_part_without_submit() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    let before = live.child.input().len();
    c.json(json!({ "type": "paste", "paste_id": "p1", "text": "a\nb" }))
        .await;
    let Some(AttachServerMessage::PasteResult {
        paste_id,
        outcome,
        bytes_written,
        reason,
    }) = c.text().await
    else {
        panic!("a paste result")
    };
    let want = chaldea_core::input::encode::encode(
        chaldea_core::ids::Agent::Claude,
        rig.sup.host_os,
        "a\nb",
        chaldea_core::input::encode::Purpose::AttachPaste,
    )
    .unwrap()
    .text;
    assert_eq!(
        (paste_id.as_str(), outcome, bytes_written, reason),
        ("p1", PasteOutcome::Written, Some(want.len() as u64), None)
    );
    assert_eq!(live.child.input()[before..], want[..]);
    rig.stop().await;
}

/// T1-6: refused pastes write zero bytes and record nothing, each with its reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paste_refusals() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    let before = live.child.input().len();
    let too_large = "x".repeat(MAX_PROMPT_BYTES + 1);
    for (text, reason) in [
        ("", PasteReason::Rejected(RejectReason::EmptyPrompt)),
        (
            "a\u{7}b",
            PasteReason::Rejected(RejectReason::ControlCharacter),
        ),
        (
            too_large.as_str(),
            PasteReason::Attach(AttachReason::PromptTooLarge),
        ),
    ] {
        c.json(json!({ "type": "paste", "paste_id": "p", "text": text }))
            .await;
        let Some(AttachServerMessage::PasteResult {
            outcome, reason: r, ..
        }) = c.text().await
        else {
            panic!("a paste result")
        };
        assert_eq!((outcome, r), (PasteOutcome::Rejected, Some(reason)));
    }
    // 2004 off at encoding.
    rig.paste(&live, false).await;
    c.json(json!({ "type": "paste", "paste_id": "p", "text": "t" }))
        .await;
    let Some(AttachServerMessage::PasteResult {
        outcome, reason, ..
    }) = c.text().await
    else {
        panic!("a paste result")
    };
    assert_eq!(
        (outcome, reason),
        (
            PasteOutcome::Rejected,
            Some(PasteReason::Attach(AttachReason::BracketedPasteOff))
        )
    );
    assert_eq!(live.child.input().len(), before);
    assert_eq!(human_writes(&rig, &live), 0);
    rig.stop().await;
}

/// T1-8: output EOF sends `ended { eof }`, then 4001.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ends_on_eof() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    live.child.emit(b"last");
    live.child.close_output();
    let mut out = Vec::new();
    let mut texts = Vec::new();
    while let Some(f) = c.next().await {
        match f {
            Frame::Binary(b) => out.extend(b),
            Frame::Text(m) => texts.push(m),
        }
    }
    assert!(out.ends_with(b"last"));
    assert_eq!(
        texts,
        [AttachServerMessage::Ended {
            launch_id: live.l,
            reason: EndReason::Eof
        }]
    );
    assert_eq!(c.closed, Some(Some(4001)));
    rig.stop().await;
}

/// T1-8: a read error with the child alive ends with `ended { error }`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ends_on_error_child_alive() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    live.child.fail_output(std::io::ErrorKind::Other);
    let (code, texts) = c.until_closed().await;
    assert_eq!(
        texts,
        [AttachServerMessage::Ended {
            launch_id: live.l,
            reason: EndReason::Error
        }]
    );
    assert_eq!(code, Some(4001));
    rig.stop().await;
}

/// T1-8: a thread with no live launch closes 4000.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_live_launch() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let r = rig.cmd("stop", &json!({ "thread_id": live.t })).await;
    assert_eq!(r.status, 200);
    let mut c = open_as(&rig, &rig.owner, live.t).await.expect("a 101");
    let (code, texts) = c.until_closed().await;
    assert_eq!((code, texts), (Some(4000), Vec::new()));
    rig.stop().await;
}

/// T1-8: shutdown closes the socket 1001.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_closes_1001() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    let stopping = tokio::spawn(async move { rig.stop().await });
    let (code, _) = c.until_closed().await;
    assert_eq!(code, Some(u16::from(CloseCode::Away)));
    stopping.await.unwrap();
}

/// T1-1: a non-owner token gets no attach ticket.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn owner_only() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let reader = rig.srv.token(TokenKind::Rescue, None, None).await;
    assert_eq!(open_as(&rig, &reader, live.t).await.err(), Some(403));
    rig.stop().await;
}

/// D2, I-2: a newer socket takes over (4003); the old socket's unit, queued behind a held
/// write when the takeover moved the fence, is refused at its dispatch: zero bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn old_enter_after_disconnect_and_reattach() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    let (mut a, _) = attach(&rig, &live).await;
    // Another human unit holds the writer.
    let hold = live.child.hold_writes();
    let other = rig.attach(&live, b"k").await;
    assert!(hold.wait_entered(std::time::Instant::now() + Duration::from_secs(10)));
    // A's Enter is queued behind it.
    a.binary(b"\r").await;
    rig.until("A's unit queued", || sl.state().human_unsettled == 2)
        .await;
    let (_b, _) = attach(&rig, &live).await;
    let (code, texts) = a.until_closed().await;
    assert_eq!(code, Some(4003), "{texts:?}");
    drop(hold);
    bounded(WS_WAIT, other)
        .await
        .expect("the report within the wait")
        .unwrap();
    rig.until("settled", || sl.state().human_unsettled == 0)
        .await;
    assert!(!live.child.input().contains(&b'\r'), "no Enter of A");
    rig.stop().await;
}

/// T1-9: frames outside the format or the limits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_frames() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    for (frame, code) in [
        (WsMessage::Text("{".into()), 1003),
        (
            WsMessage::Text(r#"{"type":"resize","cols":1,"rows":10}"#.into()),
            1003,
        ),
        (
            WsMessage::Text(r#"{"type":"resize","cols":10,"rows":0}"#.into()),
            1003,
        ),
        (
            WsMessage::Text(r#"{"type":"resize","cols":513,"rows":10}"#.into()),
            1003,
        ),
    ] {
        let (mut c, _) = attach(&rig, &live).await;
        c.ws.send(frame).await.unwrap();
        assert_eq!(c.until_closed().await.0, Some(code));
    }
    rig.stop().await;
}

/// T1-9: a raw message above 64 KiB answers `too_large` and writes nothing; one at the
/// limit is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn frame_limits() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    let before = live.child.input().len();
    c.binary(&vec![b'x'; 64 * 1024 + 1]).await;
    assert_eq!(
        c.text().await,
        Some(AttachServerMessage::InputDropped {
            bytes: 64 * 1024 + 1,
            reason: AttachReason::TooLarge
        })
    );
    assert_eq!(live.child.input().len(), before);
    c.binary(&vec![b'y'; 64 * 1024]).await;
    rig.until("written", || live.child.input().len() == before + 64 * 1024)
        .await;
    rig.stop().await;
}

/// T1-9: a 256 KiB paste with every byte `\u00XX`-escaped fits the attach inbound limit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worst_case_escape_fits() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    let text = "\u{7f}".repeat(MAX_PROMPT_BYTES);
    let escaped = format!(
        r#"{{ "type" : "paste" , "paste_id" : "p" , "text" : "{}" }}"#,
        "\\u007f".repeat(MAX_PROMPT_BYTES)
    );
    assert!(escaped.len() <= crate::http::ATTACH_INBOUND_BYTES);
    assert_eq!(text.len(), MAX_PROMPT_BYTES);
    c.ws.send(WsMessage::Text(escaped.into())).await.unwrap();
    let Some(AttachServerMessage::PasteResult { outcome, .. }) = c.text().await else {
        panic!("a paste result")
    };
    assert_eq!(outcome, PasteOutcome::Written);
    rig.stop().await;
}

/// R-10: every writer refusal and submission error maps to its reason.
#[test]
fn refusal_mapping() {
    use chaldea_pty_runtime::{Refusal, SubmitError};

    use super::{refusal_reason, submit_reason};
    assert_eq!(refusal_reason(&Refusal::Expired), AttachReason::Expired);
    assert_eq!(
        refusal_reason(&Refusal::BracketedPasteChanged),
        AttachReason::BracketedPasteOff
    );
    for r in [
        Refusal::Cancelled,
        Refusal::Closed,
        Refusal::ChildExited,
        Refusal::InputBroken,
        Refusal::FenceMoved,
        Refusal::WriteSinceBasis { seq: 1 },
        Refusal::BasisUnknown { basis: 1, head: 2 },
        Refusal::Recheck("x".into()),
        Refusal::RecheckPanicked,
    ] {
        assert_eq!(refusal_reason(&r), AttachReason::NotWritten, "{r:?}");
    }
    assert_eq!(
        submit_reason(SubmitError::QueueFull),
        AttachReason::QueueFull
    );
    for e in [
        SubmitError::Closed,
        SubmitError::BadShape("x"),
        SubmitError::UnitTooLarge,
        SubmitError::ReplyOrigin,
    ] {
        assert_eq!(submit_reason(e), AttachReason::NotWritten, "{e:?}");
    }
}

/// The index of the first event matching `f` in the child's log.
fn first_in_log(live: &Live, f: impl Fn(&FakeEvent) -> bool) -> Option<usize> {
    live.child.log().iter().position(f)
}

/// I-3: a send that committed its open holds the lease: a resize arriving meanwhile starts
/// only after the unit's writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resize_waits_for_opened_send() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    let sl = rig.sl(&live);
    let (entered_tx, entered) = std::sync::mpsc::channel::<()>();
    let (go, wait) = std::sync::mpsc::channel::<()>();
    crate::supervisor::send::tests::set_hook(&sl.hooks.before_submit, move || {
        let _ = entered_tx.send(());
        tokio::task::block_in_place(|| {
            let _ = wait.recv();
        });
    });
    let id = rig.send(&live, "prompt").await;
    tokio::task::block_in_place(|| entered.recv_timeout(Duration::from_secs(30)))
        .expect("the send parked before its submission");
    assert_eq!(
        rig.input(id).state,
        chaldea_core::input::InputState::Sending
    );
    c.json(json!({ "type": "resize", "cols": 100, "rows": 30 }))
        .await;
    rig.until("the resize waits for the lease", || {
        sl.lease_waits.load(std::sync::atomic::Ordering::SeqCst) > 0
    })
    .await;
    assert!(resizes(&live).is_empty());
    go.send(()).unwrap();
    assert!(matches!(
        c.text().await,
        Some(AttachServerMessage::Size { ok: true, .. })
    ));
    let write = first_in_log(&live, |e| matches!(e, FakeEvent::Write(_))).unwrap();
    let resize = first_in_log(&live, |e| matches!(e, FakeEvent::Resize(_))).unwrap();
    assert!(write < resize, "{:?}", live.child.log());
    rig.until_input(id, chaldea_core::input::InputState::Sent)
        .await;
    rig.stop().await;
}

/// I-3: a resize held in the backend keeps the lease: the queued input stays queued and
/// opens after the resize returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_waits_for_resize() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    let hold = live.child.hold_resizes();
    c.json(json!({ "type": "resize", "cols": 100, "rows": 30 }))
        .await;
    assert!(hold.wait_entered(std::time::Instant::now() + Duration::from_secs(10)));
    let id = rig.send(&live, "prompt").await;
    rig.pass(&live).await;
    assert_eq!(rig.input(id).state, chaldea_core::input::InputState::Queued);
    drop(hold);
    assert!(matches!(
        c.text().await,
        Some(AttachServerMessage::Size { ok: true, .. })
    ));
    rig.until_input(id, chaldea_core::input::InputState::Sent)
        .await;
    rig.stop().await;
}

/// I-7: a revocation closes the socket 1008.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_closes_1008() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let owner = rig.srv.token(TokenKind::Owner, None, None).await;
    let mut c = open_as(&rig, &owner, live.t).await.expect("a 101");
    assert!(matches!(
        c.text().await,
        Some(AttachServerMessage::Attached { .. })
    ));
    rig.srv.revoke(&owner).await;
    let (code, _) = c.until_closed().await;
    assert_eq!(code, Some(1008));
    rig.stop().await;
}

/// T1-1: against the real attach handler, a ticket of another purpose or thread is 401
/// and stays unspent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn production_mount_admission() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let other = rig.claude().await;
    let events = rig.srv.ticket(&rig.owner, "events").await.unwrap();
    let attach_t = rig
        .srv
        .ticket(&rig.owner, &format!("attach:{}", live.t))
        .await
        .unwrap();
    for (path, ticket) in [
        (format!("/api/attach/{}", live.t), &events),
        (format!("/api/attach/{}", other.t), &attach_t),
    ] {
        let url = format!("ws://127.0.0.1:{}{path}?ticket={ticket}", rig.srv.port);
        match tokio_tungstenite::connect_async(url).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(r)) => {
                assert_eq!(r.status().as_u16(), 401, "{path}");
            }
            other => panic!("{path}: {other:?}"),
        }
    }
    let url = format!(
        "ws://127.0.0.1:{}/api/attach/{}?ticket={attach_t}",
        rig.srv.port, live.t
    );
    let (ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("unspent");
    let mut c = Client { ws, closed: None };
    assert!(matches!(
        c.text().await,
        Some(AttachServerMessage::Attached { .. })
    ));
    rig.stop().await;
}

/// T1-9: `/api/ws` keeps its 64 KiB inbound bound; attach refuses only above its own (the
/// 1009 is best effort: the peer may still be writing when the socket closes).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_limit_unchanged() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let mut ws = rig.srv.ws(&rig.owner).await;
    ws.text(&"x".repeat(64 * 1024 + 1)).await;
    assert_eq!(ws.until_closed().await.0, Some(1009));
    let (mut c, _) = attach(&rig, &live).await;
    let before = live.child.input().len();
    let _ =
        c.ws.send(WsMessage::Text(
            format!(
                r#"{{"type":"paste","paste_id":"p","text":"{}"}}"#,
                "x".repeat(crate::http::ATTACH_INBOUND_BYTES)
            )
            .into(),
        ))
        .await;
    let (code, texts) = c.until_closed().await;
    assert!(matches!(code, Some(1009) | None), "{code:?}");
    assert!(texts.is_empty(), "{texts:?}");
    assert_eq!(live.child.input().len(), before);
    rig.stop().await;
}

/// T1-9: a text frame that is not UTF-8 closes 1007.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_utf8_closes_1007() {
    use tokio_tungstenite::tungstenite::protocol::frame::Frame;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    let f = Frame::message(vec![0xff, 0xfe], OpCode::Data(Data::Text), true);
    c.ws.send(WsMessage::Frame(f)).await.unwrap();
    assert_eq!(c.until_closed().await.0, Some(1007));
    rig.stop().await;
}

/// D8, I-8: output over the buffer's budget sends exactly the prefix before it, then 4002.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lag_prefix_then_4002() {
    let rig = SRig::with(|c| c.attach.out_max = 16).await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    // Two pieces: the first is parsed (and buffered for the socket) before the second.
    let rx = rig.watch(&live).await;
    live.child.emit(b"abc");
    SRig::delivered(&rx, b"abc").await;
    live.child.emit(&[b'z'; 64]);
    let mut out = Vec::new();
    while let Some(f) = c.next().await {
        match f {
            Frame::Binary(b) => out.extend(b),
            Frame::Text(m) => panic!("unexpected {m:?}"),
        }
    }
    assert_eq!(out, b"abc");
    assert_eq!(c.closed, Some(Some(4002)));
    rig.stop().await;
}

/// I-8: a lag latched before the output's end still closes 4002 (the first event wins).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lag_then_gone_keeps_4002() {
    let rig = SRig::with(|c| c.attach.out_max = 16).await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    live.child.emit(&[b'z'; 64]);
    live.child.close_output();
    let (code, texts) = c.until_closed().await;
    assert_eq!((code, texts), (Some(4002), Vec::new()));
    rig.stop().await;
}

/// The serialized `attached` of a fresh socket on `live`.
async fn attached_len(rig: &SRig, live: &Live) -> (usize, AttachServerMessage) {
    let (_c, first) = attach(rig, live).await;
    (serde_json::to_string(&first).unwrap().len(), first)
}

/// The production path at `attached_max` = the full frame's length less `under`: whether
/// the grid was kept, and the rest of the frame equal to the full one.
async fn cutoff(under: usize) -> bool {
    let screen = b"\x1b[2J\x1b[Hscreen \xe3\x81\x82 \"q\"";
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let rx = rig.watch(&live).await;
    live.child.emit(screen);
    SRig::delivered(&rx, b"\"q\"").await;
    let (len, full) = attached_len(&rig, &live).await;
    rig.stop().await;

    let rig = SRig::with(|c| c.attach.attached_max = len - under).await;
    let live = rig.claude().await;
    let rx = rig.watch(&live).await;
    live.child.emit(screen);
    SRig::delivered(&rx, b"\"q\"").await;
    let (got_len, got) = attached_len(&rig, &live).await;
    assert!(got_len <= len - under);
    let strip = |m: AttachServerMessage| match m {
        AttachServerMessage::Attached {
            cols,
            rows,
            cursor,
            modes,
            grid,
            ..
        } => (cols, rows, cursor, modes, grid.is_some()),
        other => panic!("{other:?}"),
    };
    let (want, got) = (strip(full), strip(got));
    assert_eq!(
        (want.0, want.1, want.2, want.3),
        (got.0, got.1, got.2, got.3)
    );
    rig.stop().await;
    got.4
}

/// T1-2: `attached` at exactly its bound keeps the grid.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attached_at_cutoff() {
    assert!(cutoff(0).await);
}

/// T1-2: the grid alone fits, the envelope pushes the frame one byte over: `grid: null`,
/// the rest the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn envelope_tips_over() {
    assert!(!cutoff(1).await);
}

/// R-9: a screen whose grid encodes above 256 KiB attaches with `grid: null` within the
/// bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attached_over_256k_is_null_grid() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    // A watcher whose budget takes the whole screen's output.
    let (sink, rx) = chaldea_pty_runtime::channel_sink(1 << 20);
    let e = rig.sup.entry(live.l).unwrap();
    e.with_session(&rig.sup, rig.sup.now() + Duration::from_secs(1), |s| {
        s.subscribe(sink, soon())
    })
    .await
    .unwrap()
    .unwrap();
    let mut out = String::new();
    for _ in 0..7 {
        out.push('e');
        for _ in 0..20_000 {
            out.push('\u{301}');
        }
        out.push(' ');
    }
    out.push('!');
    live.child.emit(out.as_bytes());
    SRig::delivered(&rx, b"!").await;
    let (len, first) = attached_len(&rig, &live).await;
    assert!(len <= 192 * 1024, "{len}");
    assert_eq!(grid_of(&first), None);
    rig.stop().await;
}

fn frame_meta() -> chaldea_pty_runtime::ScreenMeta {
    chaldea_pty_runtime::ScreenMeta {
        cols: 4,
        rows: 2,
        cursor: chaldea_pty_runtime::Cursor {
            row: 1,
            col: 2,
            visible: true,
        },
        modes: chaldea_pty_runtime::TermModes::default(),
        output_offset: 7,
    }
}

/// D12: the subscription's grid budget is exactly the frame's bound less the envelope: a
/// grid of `budget` bytes makes a frame of exactly `max`.
#[test]
fn grid_budget_is_the_frame_bound() {
    use chaldea_core::ids::LaunchId;
    let (t, l) = (ThreadId::from_u128(1), LaunchId::from_u128(2));
    let lines = chaldea_pty_runtime::decode_grid(r#"[["a","\""],["c"]]"#, 4, 2).unwrap();
    let grid = serde_json::to_string(&chaldea_pty_runtime::grid_rows(&lines))
        .unwrap()
        .len();
    let m = frame_meta();
    let full = super::socket::attached(t, l, &m, Some(&lines), usize::MAX).len();
    assert_eq!(super::socket::grid_budget(t, l, &m, full), grid);
    assert_eq!(super::socket::grid_budget(t, l, &m, full - 1), grid - 1);
}

/// D12: the frame's own check: lines above the bound give `grid: null`, the rest the same.
#[test]
fn attached_frame_check() {
    use chaldea_core::ids::LaunchId;
    let (t, l) = (ThreadId::from_u128(1), LaunchId::from_u128(2));
    let lines = chaldea_pty_runtime::decode_grid(r#"[["a","b"],["c"]]"#, 4, 2).unwrap();
    let m = frame_meta();
    let full = super::socket::attached(t, l, &m, Some(&lines), usize::MAX);
    let at = super::socket::attached(t, l, &m, Some(&lines), full.len());
    assert_eq!(at, full);
    let over = super::socket::attached(t, l, &m, Some(&lines), full.len() - 1);
    let none = super::socket::attached(t, l, &m, None, usize::MAX);
    assert_eq!(over, none);
}

/// I-6: a DSR the child emits is answered by the daemon's responder alone; the attached
/// client sends nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dsr_answered_once() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (_c, _) = attach(&rig, &live).await;
    let before = live.child.input().len();
    live.child.emit(b"\x1b[6n");
    rig.until("answered", || live.child.input().ends_with(b"R"))
        .await;
    let reply = live.child.input()[before..].to_vec();
    assert!(reply.starts_with(b"\x1b["), "{reply:?}");
    assert_eq!(reply.iter().filter(|b| **b == b'R').count(), 1, "{reply:?}");
    rig.stop().await;
}

/// I-4: a raw unit outstanding, 60 KiB pending, a 100 KiB paste crossing the watermark: it
/// is kept and written after, in order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crossing_paste_kept() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    let before = live.child.input().len();
    let hold = live.child.hold_writes();
    c.binary(b"a").await;
    assert!(hold.wait_entered(std::time::Instant::now() + Duration::from_secs(10)));
    let raw = vec![b'x'; 60 * 1024];
    c.binary(&raw).await;
    let text = "p".repeat(100 * 1024);
    c.json(json!({ "type": "paste", "paste_id": "big", "text": text }))
        .await;
    drop(hold);
    let Some(AttachServerMessage::PasteResult { outcome, .. }) = c.text().await else {
        panic!("a paste result")
    };
    assert_eq!(outcome, PasteOutcome::Written);
    let pasted = chaldea_core::input::encode::encode(
        chaldea_core::ids::Agent::Claude,
        rig.sup.host_os,
        &text,
        chaldea_core::input::encode::Purpose::AttachPaste,
    )
    .unwrap()
    .text;
    let want = [b"a".as_slice(), &raw, &pasted].concat();
    assert_eq!(live.child.input()[before..], want[..]);
    rig.stop().await;
}

/// I-4: a 256 KiB paste with a raw unit outstanding is accepted and written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_paste_with_raw_outstanding() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    let hold = live.child.hold_writes();
    c.binary(b"a").await;
    assert!(hold.wait_entered(std::time::Instant::now() + Duration::from_secs(10)));
    c.json(json!({ "type": "paste", "paste_id": "max", "text": "m".repeat(MAX_PROMPT_BYTES) }))
        .await;
    drop(hold);
    let Some(AttachServerMessage::PasteResult { outcome, .. }) = c.text().await else {
        panic!("a paste result")
    };
    assert_eq!(outcome, PasteOutcome::Written);
    rig.stop().await;
}

// ---- timers on the test clock, takeover, revocation, drain -----------------------------------

fn hooks(rig: &SRig) -> std::sync::Arc<super::AttachHooks> {
    rig.sup.cfg.attach.hooks.clone()
}

/// Pokes every socket loop until dropped, so each recomputes its timers from the test
/// clock (which a stopped clock never fires by itself).
struct Poker(tokio::task::JoinHandle<()>);

impl Drop for Poker {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "the poke loop's pace; no result depends on it"
)]
fn poker(rig: &SRig) -> Poker {
    let h = hooks(rig);
    Poker(tokio::spawn(async move {
        loop {
            h.poke.notify_waiters();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }))
}

/// Parks a synchronous test point (blocking its thread in place) until released; the
/// receiver hears it entered.
fn park_sync(
    slot: &std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (entered_tx, entered) = std::sync::mpsc::channel::<()>();
    let (go, wait) = std::sync::mpsc::channel::<()>();
    *slot.lock().unwrap() = Some(Box::new(move || {
        let _ = entered_tx.send(());
        tokio::task::block_in_place(|| {
            let _ = wait.recv_timeout(Duration::from_secs(30));
        });
    }));
    (entered, go)
}

fn entered(rx: &std::sync::mpsc::Receiver<()>) {
    tokio::task::block_in_place(|| rx.recv_timeout(Duration::from_secs(30)))
        .expect("the test point was reached");
}

fn soon() -> std::time::Instant {
    std::time::Instant::now() + Duration::from_secs(10)
}

/// A Size frame, skipping others.
async fn size_frame(c: &mut Client) -> (u16, u16, bool) {
    loop {
        if let AttachServerMessage::Size { cols, rows, ok } = c.text().await.expect("open") {
            return (cols, rows, ok);
        }
    }
}

/// T1-8: output ends never reach the socket, and the session leaves: `ended { gone }` once
/// the gone poll runs on the test clock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ends_on_gone() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    let probe = rig.probe(&live).await;
    // The drain parks inside a piece (VT lock held): no output end reaches any sink.
    let park = probe.park(chaldea_pty_runtime::fake::ProbePoint::PieceParsing);
    live.child.emit(b"x");
    assert!(probe.wait(
        chaldea_pty_runtime::fake::ProbePoint::PieceParsing,
        1,
        soon()
    ));
    live.child.exit(chaldea_pty_runtime::ChildExit {
        code: Some(0),
        signal: None,
    });
    rig.until("the session left", || {
        rig.sup.live(live.t, live.l).is_none()
    })
    .await;
    rig.clock.advance(rig.sup.cfg.attach.gone_poll);
    let _p = poker(&rig);
    let (code, texts) = c.until_closed().await;
    drop(park);
    assert_eq!(
        (code, texts),
        (
            Some(4001),
            vec![AttachServerMessage::Ended {
                launch_id: live.l,
                reason: EndReason::Gone
            }]
        )
    );
    rig.stop().await;
}

/// I-8: a send held across the 5 s drain deadline drops the socket: no `ended`, no close
/// frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_cutoff_drops() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    let (reached, _held) = rig.srv.server.shared().faults.arm_socket_send_gate();
    live.child.emit(b"x");
    bounded(WS_WAIT, reached).await.unwrap().unwrap();
    live.child.close_output();
    let h = hooks(&rig);
    rig.until("the end latched", || {
        h.latched.load(std::sync::atomic::Ordering::SeqCst) > 0
    })
    .await;
    rig.clock.advance(rig.sup.cfg.attach.drain);
    let _p = poker(&rig);
    let (code, texts) = c.until_closed().await;
    assert_eq!((code, texts), (None, Vec::new()));
    rig.stop().await;
}

/// R-6: a partial marker is held until the hold ends on the test clock, then written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alt_bracket_written_within_50ms() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, first) = attach(&rig, &live).await;
    let AttachServerMessage::Attached { cols, rows, .. } = first else {
        unreachable!()
    };
    let before = live.child.input().len();
    c.binary(b"\x1b[").await;
    // A later frame was handled: the partial is still held.
    c.json(json!({ "type": "resize", "cols": cols, "rows": rows }))
        .await;
    size_frame(&mut c).await;
    assert_eq!(live.child.input().len(), before);
    rig.clock.advance(rig.sup.cfg.attach.hold);
    let _p = poker(&rig);
    input_has(&rig, &live, b"\x1b[").await;
    rig.stop().await;
}

/// R-6: the written tail is the launch's: A writes a lone Escape, B takes over, and B's
/// frame starting `[200~` is refused whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn marker_tail_across_takeover() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut a, _) = attach(&rig, &live).await;
    a.binary(b"\x1b").await;
    input_has(&rig, &live, b"\x1b").await;
    let (mut b, _) = attach(&rig, &live).await;
    assert_eq!(a.until_closed().await.0, Some(4003));
    let before = live.child.input().len();
    b.binary(b"[200~x").await;
    assert_eq!(
        b.text().await,
        Some(AttachServerMessage::InputDropped {
            bytes: 6,
            reason: AttachReason::PasteMarker
        })
    );
    b.binary(b"\x1b").await;
    rig.until("written", || live.child.input().len() > before)
        .await;
    assert_eq!(live.child.input()[before..], *b"\x1b");
    rig.stop().await;
}

/// I-6: the marker rule's tail is the bytes the PTY received. A writes a lone Escape, then
/// `xxxxx`, whose submission waits at its submit point while B takes over: A's `xxxxx` is
/// refused (fence), so B's `[200~` would complete a marker after the Escape and is refused;
/// B can still type.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refused_old_raw_does_not_hide_marker_prefix() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut a, _) = attach(&rig, &live).await;
    let before = live.child.input().len();
    a.binary(b"\x1b").await;
    rig.until("the lone Escape reached the PTY", || {
        live.child.input()[before..] == *b"\x1b"
    })
    .await;
    let (at, go) = park_sync(&hooks(&rig).submit);
    a.binary(b"xxxxx").await;
    entered(&at);
    let (mut b, _) = attach(&rig, &live).await;
    go.send(()).unwrap();
    assert_eq!(a.until_closed().await.0, Some(4003));
    b.binary(b"[200~").await;
    b.binary(b"q").await;
    rig.until("the new socket can still type", || {
        live.child.input()[before..].ends_with(b"q")
    })
    .await;
    let written = live.child.input()[before..].to_vec();
    rig.stop().await;
    assert_eq!(written.as_slice(), b"\x1bq");
}

/// I-6: an Escape that was queued and never written is not context: A's Escape waits
/// behind a held write when B takes over, so the writer refuses it (fence), and B's
/// `[200~` is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unwritten_escape_does_not_refuse() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    let (mut a, _) = attach(&rig, &live).await;
    let before = live.child.input().len();
    let hold = live.child.hold_writes();
    let other = rig.attach(&live, b"k").await;
    assert!(hold.wait_entered(soon()));
    a.binary(b"\x1b").await;
    rig.until("A's Escape queued", || sl.state().human_unsettled == 2)
        .await;
    let (mut b, _) = attach(&rig, &live).await;
    assert_eq!(a.until_closed().await.0, Some(4003));
    b.binary(b"[200~").await;
    b.binary(b"q").await;
    drop(hold);
    bounded(WS_WAIT, other).await.unwrap().unwrap();
    rig.until("B's input arrived", || {
        live.child.input()[before..].ends_with(b"q")
    })
    .await;
    let written = live.child.input()[before..].to_vec();
    rig.stop().await;
    assert_eq!(written.as_slice(), b"k[200~q");
}

/// I-4: a unit with no report by `not_after` + 1 s and not cancellable is `pending`; the
/// socket submits nothing more until its report arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stuck_writer_after_pending() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    let (mut c, first) = attach(&rig, &live).await;
    let AttachServerMessage::Attached { cols, rows, .. } = first else {
        unreachable!()
    };
    let hold = live.child.hold_writes();
    c.binary(b"a").await;
    assert!(hold.wait_entered(soon()));
    rig.clock
        .advance(rig.sup.cfg.attach.unit_wait + crate::supervisor::send::READ_WAIT);
    let p = poker(&rig);
    assert_eq!(
        c.text().await,
        Some(AttachServerMessage::InputDropped {
            bytes: 1,
            reason: AttachReason::Pending
        })
    );
    drop(p);
    c.binary(b"b").await;
    c.json(json!({ "type": "resize", "cols": cols, "rows": rows }))
        .await;
    size_frame(&mut c).await;
    assert_eq!(sl.state().human_unsettled, 1, "nothing more submitted");
    drop(hold);
    input_has(&rig, &live, b"ab").await;
    rig.stop().await;
}

/// T1-6: 2004 off and on again between the paste's encoding and its dispatch: refused
/// `bracketed_paste_off`, zero bytes, no event of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paste_2004_toggled() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    let (mut c, _) = attach(&rig, &live).await;
    let hold = live.child.hold_writes();
    let other = rig.attach(&live, b"k").await;
    assert!(hold.wait_entered(soon()));
    c.json(json!({ "type": "paste", "paste_id": "p", "text": "t" }))
        .await;
    rig.until("the paste queued", || sl.state().human_unsettled == 2)
        .await;
    let before_events = human_writes(&rig, &live);
    live.child
        .emit(super::super::supervisor::send::tests::PASTE_OFF);
    live.child
        .emit(super::super::supervisor::send::tests::PASTE_ON);
    let rx = rig.watch(&live).await;
    live.child.emit(b"sync");
    SRig::delivered(&rx, b"sync").await;
    let before = live.child.input().len();
    drop(hold);
    bounded(WS_WAIT, other).await.unwrap().unwrap();
    let Some(AttachServerMessage::PasteResult {
        outcome,
        bytes_written,
        reason,
        ..
    }) = c.text().await
    else {
        panic!("a paste result")
    };
    assert_eq!(
        (outcome, bytes_written, reason),
        (
            PasteOutcome::NotWritten,
            Some(0),
            Some(PasteReason::Attach(AttachReason::BracketedPasteOff))
        )
    );
    assert_eq!(live.child.input()[before..], *b"k");
    rig.until("settled", || sl.state().human_unsettled == 0)
        .await;
    assert!(human_writes(&rig, &live) <= before_events.max(1));
    rig.stop().await;
}

/// I-2: A's submission runs after B's takeover: its grant is the one fixed at A's swap, so
/// the writer refuses it (`FenceMoved`), zero bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delayed_old_submission() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    let (mut a, _) = attach(&rig, &live).await;
    let (at, go) = park_sync(&hooks(&rig).submit);
    a.binary(b"\r").await;
    entered(&at);
    let (_b, _) = attach(&rig, &live).await;
    go.send(()).unwrap();
    assert_eq!(a.until_closed().await.0, Some(4003));
    rig.until("settled", || sl.state().human_unsettled == 0)
        .await;
    assert!(!live.child.input().contains(&b'\r'));
    rig.stop().await;
}

/// I-2: A's resize parked before the backend while B takes over: no resize of A reaches
/// the PTY.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn old_resize_parked_before_backend() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut a, _) = attach(&rig, &live).await;
    let probe = rig.probe(&live).await;
    let park = probe.park(chaldea_pty_runtime::fake::ProbePoint::ResizeLocked);
    a.json(json!({ "type": "resize", "cols": 100, "rows": 30 }))
        .await;
    assert!(probe.wait(
        chaldea_pty_runtime::fake::ProbePoint::ResizeLocked,
        1,
        soon()
    ));
    let (_b, _) = attach(&rig, &live).await;
    assert_eq!(a.until_closed().await.0, Some(4003));
    drop(park);
    let sl = rig.sl(&live);
    rig.until("the lease released", || sl.state().lease.is_none())
        .await;
    assert!(resizes(&live).is_empty());
    rig.stop().await;
}

/// I-2: a candidate whose subscription fails changes nothing: the current socket keeps
/// operating.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_candidate_keeps_current() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut a, _) = attach(&rig, &live).await;
    // Take every other subscriber slot.
    let e = rig.sup.entry(live.l).unwrap();
    let mut held = Vec::new();
    loop {
        let (sink, rx) = chaldea_pty_runtime::channel_sink(64);
        let r = e
            .with_session(&rig.sup, rig.sup.now() + Duration::from_secs(1), |s| {
                s.subscribe(sink, soon())
            })
            .await
            .unwrap();
        match r {
            Ok(_) => held.push(rx),
            Err(chaldea_pty_runtime::SubscribeError::TooManySubscribers) => break,
            Err(other) => panic!("{other:?}"),
        }
    }
    let mut b = open_as(&rig, &rig.owner, live.t).await.expect("a 101");
    assert_eq!(b.until_closed().await.0, Some(4004));
    a.binary(b"still").await;
    input_has(&rig, &live, b"still").await;
    rig.stop().await;
}

/// I-2: overlapping candidates: exactly one stays current, the others close 4003.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn takeover_atomic() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let mut tickets = Vec::new();
    for _ in 0..3 {
        tickets.push(
            rig.srv
                .ticket(&rig.owner, &format!("attach:{}", live.t))
                .await
                .unwrap(),
        );
    }
    let port = rig.srv.port;
    let t = live.t;
    let opens = tickets.into_iter().map(|ticket| async move {
        let url = format!("ws://127.0.0.1:{port}/api/attach/{t}?ticket={ticket}");
        let (ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        let mut c = Client { ws, closed: None };
        assert!(matches!(
            c.text().await,
            Some(AttachServerMessage::Attached { .. })
        ));
        c
    });
    let clients = futures_util::future::join_all(opens).await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut tasks = Vec::new();
    for (i, mut c) in clients.into_iter().enumerate() {
        let tx = tx.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                match c.ws.next().await {
                    Some(Ok(WsMessage::Close(f))) => {
                        let _ = tx.send((i, f.map(|f| u16::from(f.code))));
                        return;
                    }
                    Some(Ok(_)) => {}
                    _ => {
                        let _ = tx.send((i, None));
                        return;
                    }
                }
            }
        }));
    }
    let mut closed = Vec::new();
    for _ in 0..2 {
        closed.push(bounded(WS_WAIT, rx.recv()).await.unwrap().unwrap());
    }
    assert!(closed.iter().all(|(_, c)| *c == Some(4003)), "{closed:?}");
    // The third is current: it still operates.
    let survivor = (0..3).find(|i| closed.iter().all(|(j, _)| j != i)).unwrap();
    assert!(!tasks[survivor].is_finished());
    for t in tasks {
        t.abort();
    }
    rig.stop().await;
}

/// R-3: while a resize holds the lease the latest size waits; after the release only the
/// latest is applied.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn latest_size_after_release() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    let hold = live.child.hold_resizes();
    c.json(json!({ "type": "resize", "cols": 100, "rows": 30 }))
        .await;
    assert!(hold.wait_entered(soon()));
    c.json(json!({ "type": "resize", "cols": 90, "rows": 20 }))
        .await;
    c.json(json!({ "type": "resize", "cols": 70, "rows": 10 }))
        .await;
    // A later frame handled: both sizes were read while the first is held.
    c.binary(b"z").await;
    input_has(&rig, &live, b"z").await;
    drop(hold);
    assert_eq!(size_frame(&mut c).await, (100, 30, true));
    assert_eq!(size_frame(&mut c).await, (70, 10, true));
    assert_eq!(
        resizes(&live),
        [
            PtySize::new(100, 30).unwrap(),
            PtySize::new(70, 10).unwrap()
        ]
    );
    rig.stop().await;
}

/// I-1, I-3: `stop` with a resize held in the backend meets its budget; the resize returns
/// after.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_with_resize_held() {
    let rig = SRig::with(|c| {
        c.close_bounds = chaldea_pty_runtime::CloseBounds {
            kill: Duration::from_millis(300),
            input: Duration::from_millis(300),
            close: Duration::from_millis(300),
            drain: Duration::from_millis(300),
            join: Duration::from_millis(300),
        };
    })
    .await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    let hold = live.child.hold_resizes();
    c.json(json!({ "type": "resize", "cols": 100, "rows": 30 }))
        .await;
    assert!(hold.wait_entered(soon()));
    let r = bounded(
        3 * WS_WAIT,
        rig.cmd("stop", &json!({ "thread_id": live.t })),
    )
    .await
    .expect("stop returns while the resize is held");
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    drop(hold);
    let (code, _) = c.until_closed().await;
    assert_eq!(code, Some(4001));
    rig.stop().await;
}

/// T1-8: the op lock not taken at startup closes 4004.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn startup_busy() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let e = rig.sup.entry(live.l).unwrap();
    let held = e.ops.clone().lock_owned().await;
    // The startup wait passes on the running clock; the lock stays held throughout.
    rig.clock.resume();
    let mut c = open_as(&rig, &rig.owner, live.t).await.expect("a 101");
    let (code, _) = c.until_closed().await;
    drop(held);
    assert_eq!(code, Some(4004));
    rig.stop().await;
}

/// I-7: a unit queued behind a held write when the token is revoked is refused at its
/// dispatch: zero bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_with_held_write() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    let owner = rig.srv.token(TokenKind::Owner, None, None).await;
    let mut a = open_as(&rig, &owner, live.t).await.expect("a 101");
    assert!(matches!(
        a.text().await,
        Some(AttachServerMessage::Attached { .. })
    ));
    let hold = live.child.hold_writes();
    let other = rig.attach(&live, b"k").await;
    assert!(hold.wait_entered(soon()));
    a.binary(b"\r").await;
    rig.until("A's unit queued", || sl.state().human_unsettled == 2)
        .await;
    rig.srv.revoke(&owner).await;
    assert_eq!(a.until_closed().await.0, Some(1008));
    drop(hold);
    bounded(WS_WAIT, other).await.unwrap().unwrap();
    rig.until("settled", || sl.state().human_unsettled == 0)
        .await;
    assert!(!live.child.input().contains(&b'\r'));
    rig.stop().await;
}

/// I-7: a revocation whose nudge is lost is found by the periodic verification.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_lost_nudge() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let owner = rig.srv.token(TokenKind::Owner, None, None).await;
    let mut a = open_as(&rig, &owner, live.t).await.expect("a 101");
    assert!(matches!(
        a.text().await,
        Some(AttachServerMessage::Attached { .. })
    ));
    rig.srv.revoke_quietly(&owner).await;
    assert_eq!(a.until_closed().await.0, Some(1008));
    rig.stop().await;
}

/// I-7: a revocation committed before the startup read: no `attached`, close 1008.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_at_startup() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let owner = rig.srv.token(TokenKind::Owner, None, None).await;
    let ticket = rig
        .srv
        .ticket(&owner, &format!("attach:{}", live.t))
        .await
        .unwrap();
    // The third read (after the guard's lookup and the admission read) is the startup read:
    // parked until the revocation committed, with no nudge, so only it can see it.
    let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
    let (go, wait) = std::sync::mpsc::channel::<()>();
    rig.srv.server.shared().faults.arm_before_read(2, move || {
        let _ = entered_tx.send(());
        let _ = wait.recv_timeout(Duration::from_secs(30));
    });
    let url = format!(
        "ws://127.0.0.1:{}/api/attach/{}?ticket={ticket}",
        rig.srv.port, live.t
    );
    let connect = tokio::spawn(tokio_tungstenite::connect_async(url));
    entered(&entered_rx);
    rig.srv.revoke_quietly(&owner).await;
    go.send(()).unwrap();
    let (ws, _) = bounded(WS_WAIT, connect).await.unwrap().unwrap().unwrap();
    let mut c = Client { ws, closed: None };
    let (code, texts) = c.until_closed().await;
    assert_eq!((code, texts), (Some(1008), Vec::new()));
    rig.stop().await;
}

/// I-8: a read error with the child alive revokes the grant at the latch: a unit queued
/// behind a held write is not written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_error_revokes_held_input() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    let (mut a, _) = attach(&rig, &live).await;
    let hold = live.child.hold_writes();
    let other = rig.attach(&live, b"k").await;
    assert!(hold.wait_entered(soon()));
    a.binary(b"\r").await;
    rig.until("A's unit queued", || sl.state().human_unsettled == 2)
        .await;
    live.child.fail_output(std::io::ErrorKind::Other);
    // The latch is seen; the socket has not ended (its close is not acknowledged yet).
    assert_eq!(
        a.text().await,
        Some(AttachServerMessage::Ended {
            launch_id: live.l,
            reason: EndReason::Error
        })
    );
    drop(hold);
    bounded(WS_WAIT, other).await.unwrap().unwrap();
    rig.until("settled", || sl.state().human_unsettled == 0)
        .await;
    assert!(!live.child.input().contains(&b'\r'));
    assert_eq!(a.until_closed().await.0, Some(4001));
    rig.stop().await;
}

/// I-8: the socket's stalled send does not hold up the PTY's drain: another subscriber
/// still gets every byte.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_progresses_while_output_stalled() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (_c, _) = attach(&rig, &live).await;
    let (reached, _held) = rig.srv.server.shared().faults.arm_socket_send_gate();
    live.child.emit(b"first");
    bounded(WS_WAIT, reached).await.unwrap().unwrap();
    // The socket's buffer holds a piece it cannot send; later pieces still drain.
    let rx = rig.watch(&live).await;
    live.child.emit(b"second");
    SRig::delivered(&rx, b"second").await;
    live.child.emit(b"later bytes");
    SRig::delivered(&rx, b"later bytes").await;
    rig.stop().await;
}

/// I-8: a lagging socket whose peer never reads releases its slot once the drain deadline
/// passes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lag_stalled_releases_slot() {
    let rig = SRig::with(|c| c.attach.out_max = 16).await;
    let live = rig.claude().await;
    let shared = rig.srv.server.shared().clone();
    let (mut c, _) = attach(&rig, &live).await;
    let (reached, _held) = shared.faults.arm_socket_send_gate();
    live.child.emit(b"abc");
    bounded(WS_WAIT, reached).await.unwrap().unwrap();
    live.child.emit(&[b'z'; 64]);
    let h = hooks(&rig);
    rig.until("the lag latched", || {
        h.latched.load(std::sync::atomic::Ordering::SeqCst) > 0
    })
    .await;
    assert_eq!(shared.sockets.held(), 1);
    rig.clock.advance(rig.sup.cfg.attach.drain);
    let _p = poker(&rig);
    rig.until("the slot released", || shared.sockets.held() == 0)
        .await;
    assert_eq!(c.until_closed().await, (None, Vec::new()));
    rig.stop().await;
}

/// Pushes `n` verification-read parks; returns the channel that hears each park's index
/// as it is entered, and the releases by index.
fn park_verify_reads(
    rig: &SRig,
    n: usize,
) -> (
    std::sync::mpsc::Receiver<usize>,
    Vec<std::sync::mpsc::Sender<()>>,
) {
    let (entered_tx, entered) = std::sync::mpsc::channel::<usize>();
    let mut gos = Vec::new();
    let h = hooks(rig);
    let mut v = h.verify.lock().unwrap();
    for i in 0..n {
        let (go, wait) = std::sync::mpsc::channel::<()>();
        gos.push(go);
        let tx = entered_tx.clone();
        v.push(Box::new(move || {
            let _ = tx.send(i);
            let _ = wait.recv_timeout(Duration::from_secs(30));
        }));
    }
    (entered, gos)
}

fn next_entered(rx: &std::sync::mpsc::Receiver<usize>) -> usize {
    tokio::task::block_in_place(|| rx.recv_timeout(Duration::from_secs(30)))
        .expect("a verification read parked")
}

/// I-7: a verification parked after its snapshot when the token is revoked: the nudge
/// starts a fresh read at once, which closes the socket while the stale one is parked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_read_parked_in_snapshot() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let owner = rig.srv.token(TokenKind::Owner, None, None).await;
    let mut a = open_as(&rig, &owner, live.t).await.expect("a 101");
    assert!(matches!(
        a.text().await,
        Some(AttachServerMessage::Attached { .. })
    ));
    let (entered, gos) = park_verify_reads(&rig, 1);
    next_entered(&entered);
    rig.srv.revoke(&owner).await;
    assert_eq!(a.until_closed().await.0, Some(1008));
    drop(gos);
    rig.stop().await;
}

/// I-7: both read slots held by reads from before the revocation; the nudge's fresh read
/// can start only after a stale one ends, and the socket drops by the stale reads' own
/// deadline even though fresh reads are still in flight; no stale success keeps it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn saturated_reads_drop_by_original_deadline() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let owner = rig.srv.token(TokenKind::Owner, None, None).await;
    let mut a = open_as(&rig, &owner, live.t).await.expect("a 101");
    assert!(matches!(
        a.text().await,
        Some(AttachServerMessage::Attached { .. })
    ));
    let (entered, gos) = park_verify_reads(&rig, 4);
    let first = next_entered(&entered);
    let second = next_entered(&entered);
    // The stale reads' deadline is the short deadline from now; fresh reads start later.
    let short = rig.srv.server.shared().cfg.short_deadline;
    rig.clock.advance(Duration::from_secs(1));
    rig.srv.revoke(&owner).await;
    // Both stale reads answer `true`; fresh reads take their slots and park.
    gos[first].send(()).unwrap();
    gos[second].send(()).unwrap();
    next_entered(&entered);
    next_entered(&entered);
    rig.clock.advance(short - Duration::from_secs(1));
    let _p = poker(&rig);
    let (code, texts) = a.until_closed().await;
    assert_eq!((code, texts), (None, Vec::new()));
    drop(gos);
    rig.stop().await;
}

/// I-3: an open job whose commit result is unknown keeps the lease (the input may be
/// `sending`): a resize waits for it and never reaches the PTY meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_result_unrecorded() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    let (mut c, _) = attach(&rig, &live).await;
    let store = rig.srv.store.store().clone();
    crate::supervisor::send::tests::set_hook(&sl.hooks.open_job, move || {
        *store.faults().commit.lock().unwrap() =
            Some(crate::store::test_faults::CommitFault::IoErrAfterCommit);
    });
    let _ = rig.send_text(&live, "prompt").await;
    rig.until("the open's commit is unknown", || {
        rig.srv
            .store
            .store()
            .faults()
            .commit
            .lock()
            .unwrap()
            .is_none()
            && !rig.sink.0.lock().unwrap().is_empty()
    })
    .await;
    rig.until("the open ended", || {
        sl.state()
            .lease
            .is_some_and(|l| matches!(l.kind, crate::supervisor::send::LeaseKind::Send(_)))
            && sl.state().running.is_none()
    })
    .await;
    c.json(json!({ "type": "resize", "cols": 100, "rows": 30 }))
        .await;
    rig.until("the resize waits for the lease", || {
        sl.lease_waits.load(std::sync::atomic::Ordering::SeqCst) > 0
    })
    .await;
    let sl2 = sl.clone();
    rig.until_async("the worker's pass after the open", || {
        let sl = sl2.clone();
        async move { sl.passes.load(std::sync::atomic::Ordering::SeqCst) > 0 }
    })
    .await;
    assert!(resizes(&live).is_empty());
    assert!(sl.state().lease.is_some());
    rig.stop().await;
}

/// I-7: the socket's cleanup (its unsubscribe) blocked on the VT lock does not hold its
/// slot: the slot is released when the socket ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slot_with_cleanup_held() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let shared = rig.srv.server.shared().clone();
    let (c, _) = attach(&rig, &live).await;
    let probe = rig.probe(&live).await;
    let park = probe.park(chaldea_pty_runtime::fake::ProbePoint::PieceParsing);
    live.child.emit(b"x");
    assert!(probe.wait(
        chaldea_pty_runtime::fake::ProbePoint::PieceParsing,
        1,
        soon()
    ));
    assert_eq!(shared.sockets.held(), 1);
    drop(c);
    rig.until("the slot released", || shared.sockets.held() == 0)
        .await;
    drop(park);
    rig.stop().await;
}

/// I-2: repeated takeovers with no output never leave old subscriptions behind: every
/// takeover past the subscriber limit still attaches.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replacement_without_output() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut prev, _) = attach(&rig, &live).await;
    for _ in 0..12 {
        let (next, _) = attach(&rig, &live).await;
        assert_eq!(prev.until_closed().await.0, Some(4003));
        prev = next;
    }
    prev.binary(b"ok").await;
    input_has(&rig, &live, b"ok").await;
    rig.stop().await;
}

/// R-10, R-4: the report channel lost with the unit still queued: the cancel takes it,
/// `not_written`; with the unit dispatched: `pending`, and the socket submits nothing more
/// (no settlement can come).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn report_channel_lost() {
    use crate::supervisor::send::DropReport;
    // Queued.
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    let (mut c, _) = attach(&rig, &live).await;
    let hold = live.child.hold_writes();
    let other = rig.attach(&live, b"k").await;
    assert!(hold.wait_entered(soon()));
    *sl.drop_report.lock().unwrap() = Some(DropReport::Submit);
    c.binary(b"x").await;
    assert_eq!(
        c.text().await,
        Some(AttachServerMessage::InputDropped {
            bytes: 1,
            reason: AttachReason::NotWritten
        })
    );
    drop(hold);
    bounded(WS_WAIT, other).await.unwrap().unwrap();
    rig.until("settled", || sl.state().human_unsettled == 0)
        .await;
    assert!(!live.child.input().contains(&b'x'));
    rig.stop().await;

    // Dispatched.
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    let (mut c, first) = attach(&rig, &live).await;
    let AttachServerMessage::Attached { cols, rows, .. } = first else {
        unreachable!()
    };
    *sl.drop_report.lock().unwrap() = Some(DropReport::Fire);
    c.binary(b"x").await;
    assert_eq!(
        c.text().await,
        Some(AttachServerMessage::InputDropped {
            bytes: 1,
            reason: AttachReason::Pending
        })
    );
    c.binary(b"y").await;
    c.json(json!({ "type": "resize", "cols": cols, "rows": rows }))
        .await;
    size_frame(&mut c).await;
    rig.until("settled", || sl.state().human_unsettled == 0)
        .await;
    assert!(
        !live.child.input().contains(&b'y'),
        "nothing more submitted"
    );
    rig.stop().await;
}

/// R-3: an open job whose attempts end `Busy` until the completion budget is spent is a
/// known non-admission: the lease is released, the input stays `queued` and opens on a
/// later pass, and a resize proceeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_budget_recovers() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    let store = rig.srv.store.store().clone();
    let armed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let a2 = armed.clone();
    crate::supervisor::send::tests::set_hook(&sl.hooks.open_job, move || {
        *store.faults().commit.lock().unwrap() =
            Some(crate::store::test_faults::CommitFault::BusyBeforeCommit);
        a2.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let id = rig.send(&live, "prompt").await;
    rig.until("the open job backs off after its Busy attempt", || {
        armed.load(std::sync::atomic::Ordering::SeqCst)
            && rig.pauses.passed(crate::supervisor::Pause::RecordBackoff)
    })
    .await;
    // The backoff waits on the stopped clock; past the budget the job is unrecorded.
    assert!(sl.state().lease.is_some());
    rig.clock
        .advance(crate::http::COMPLETION_BUDGET + Duration::from_secs(1));
    rig.until("the lease released", || sl.state().lease.is_none())
        .await;
    assert_eq!(rig.input(id).state, chaldea_core::input::InputState::Queued);
    rig.pass(&live).await;
    rig.until_input(id, chaldea_core::input::InputState::Sent)
        .await;
    // A resize proceeds once the unit's record released the lease.
    rig.until("the unit recorded", || sl.state().running.is_none())
        .await;
    let (mut c, _) = attach(&rig, &live).await;
    c.json(json!({ "type": "resize", "cols": 100, "rows": 30 }))
        .await;
    assert_eq!(size_frame(&mut c).await, (100, 30, true));
    rig.stop().await;
}

/// I-8: `ended` is the last application frame; a close frame stalled after it is not
/// waited for past the close's grace: the socket ends without it and releases its slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_stalled_after_ended() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let shared = rig.srv.server.shared().clone();
    let (mut c, _) = attach(&rig, &live).await;
    let (reached, _held) = shared.faults.arm_close_frame_gate();
    live.child.close_output();
    assert_eq!(
        c.text().await,
        Some(AttachServerMessage::Ended {
            launch_id: live.l,
            reason: EndReason::Eof
        })
    );
    bounded(WS_WAIT, reached).await.unwrap().unwrap();
    rig.until("the slot released", || shared.sockets.held() == 0)
        .await;
    assert_eq!(c.until_closed().await, (None, Vec::new()));
    rig.stop().await;
}
