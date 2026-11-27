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

/// T1-2: `attached` at exactly its bound keeps the grid; one byte less and the envelope
/// pushes it over: `grid: null`, everything else the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attached_at_cutoff() {
    for under in [0, 1] {
        let rig = SRig::new().await;
        let live = rig.claude().await;
        let rx = rig.watch(&live).await;
        live.child.emit(b"\x1b[2J\x1b[Hscreen \xe3\x81\x82 \"q\"");
        SRig::delivered(&rx, b"\"q\"").await;
        let (len, full) = attached_len(&rig, &live).await;
        rig.stop().await;

        let rig = SRig::with(|c| c.attach.attached_max = len - under).await;
        let live = rig.claude().await;
        let rx = rig.watch(&live).await;
        live.child.emit(b"\x1b[2J\x1b[Hscreen \xe3\x81\x82 \"q\"");
        SRig::delivered(&rx, b"\"q\"").await;
        let (_, got) = attached_len(&rig, &live).await;
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
        assert_eq!(got.4, under == 0, "under {under}");
        assert_eq!(
            (want.0, want.1, want.2, want.3),
            (got.0, got.1, got.2, got.3)
        );
        rig.stop().await;
    }
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
