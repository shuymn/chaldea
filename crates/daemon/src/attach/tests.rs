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
use crate::store::tests::support::waits::bounded;
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
    /// The next frame within the wait (a hang guard), or `None` once closed.
    async fn next(&mut self) -> Option<Frame> {
        loop {
            let m = bounded(WS_WAIT, self.ws.next())
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

/// D2, I-2: a newer socket takes over (4003); the old socket's queued input is never
/// written after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn old_enter_after_disconnect_and_reattach() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut a, _) = attach(&rig, &live).await;
    let hold = live.child.hold_writes();
    a.binary(b"q").await;
    assert!(hold.wait_entered(std::time::Instant::now() + Duration::from_secs(10)));
    // The held write is permitted; the next unit of A queues behind it.
    a.binary(b"\r").await;
    let (_b, _) = attach(&rig, &live).await;
    let (code, _) = a.until_closed().await;
    assert_eq!(code, Some(4003));
    drop(hold);
    rig.until("settled", || rig.sl(&live).state().human_unsettled == 0)
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
