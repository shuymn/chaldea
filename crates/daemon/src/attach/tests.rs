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

    /// One data message as `parts.len()` frames: the first of `opcode`, the rest
    /// continuations.
    async fn fragments(
        &mut self,
        opcode: tokio_tungstenite::tungstenite::protocol::frame::coding::Data,
        parts: &[&[u8]],
    ) {
        use tokio_tungstenite::tungstenite::protocol::frame::Frame;
        use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};
        for (i, p) in parts.iter().enumerate() {
            let op = if i == 0 {
                OpCode::Data(opcode)
            } else {
                OpCode::Data(Data::Continue)
            };
            let f = Frame::message(p.to_vec(), op, i + 1 == parts.len());
            self.ws.send(WsMessage::Frame(f)).await.unwrap();
        }
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

/// Opens the attach socket of `t` with `ticket`.
async fn open_ticket(rig: &SRig, t: ThreadId, ticket: &str) -> Result<Client, u16> {
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

/// T1-2 at the boundary: the drain holds the VT lock inside a piece while the socket's
/// subscription waits for it; once released, the piece is in the snapshot and not in the
/// binary output, and the next piece is exactly the later bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_boundary_held() {
    use chaldea_pty_runtime::fake::ProbePoint;
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let probe = rig.probe(&live).await;
    let park = probe.park(ProbePoint::PieceParsing);
    live.child.emit(b"hello");
    assert!(probe.wait(ProbePoint::PieceParsing, 1, soon()));
    let subscribing = probe.count(ProbePoint::Subscribing);
    let open = tokio::spawn({
        let ticket = rig
            .srv
            .ticket(&rig.owner, &format!("attach:{}", live.t))
            .await
            .unwrap();
        let url = format!(
            "ws://127.0.0.1:{}/api/attach/{}?ticket={ticket}",
            rig.srv.port, live.t
        );
        tokio_tungstenite::connect_async(url)
    });
    assert!(probe.wait(ProbePoint::Subscribing, subscribing + 1, soon()));
    drop(park);
    let (ws, _) = bounded(WS_WAIT, open).await.unwrap().unwrap().unwrap();
    let mut c = Client { ws, closed: None };
    let first = c.text().await.expect("attached");
    let grid = grid_of(&first).expect("a grid");
    assert!(grid[0].concat().ends_with("hello"), "{grid:?}");
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
    // The first leg is held after its backend call; the failure is armed for the next
    // call, the second leg.
    use chaldea_pty_runtime::fake::ProbePoint;
    let probe = rig.probe(&live).await;
    let park = probe.park(ProbePoint::ResizeDone);
    c.json(json!({ "type": "resize", "cols": cols, "rows": rows }))
        .await;
    assert!(probe.wait(ProbePoint::ResizeDone, 1, soon()));
    live.child.fail_next_resize();
    drop(park);
    let Some(AttachServerMessage::Size { ok, .. }) = c.text().await else {
        panic!("a size frame")
    };
    assert!(!ok);
    assert_eq!(resizes(&live), [PtySize::new(cols, rows + 1).unwrap()]);
    assert!(live.child.log().contains(&FakeEvent::ResizeFailed));
    rig.stop().await;
}

/// T1-4: binary frames are written unchanged and in order, and record `human_write` once
/// for the interval.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raw_input_order() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    let before = live.child.input().len();
    for b in [b"ab".as_slice(), b"c", "\u{3042}".as_bytes(), b"\x1b[A"] {
        c.binary(b).await;
    }
    let want = [b"abc".as_slice(), "\u{3042}".as_bytes(), b"\x1b[A"].concat();
    input_has(&rig, &live, &want).await;
    // Exactly these bytes, nothing around them.
    assert_eq!(live.child.input()[before..], want[..]);
    rig.until("recorded", || human_writes(&rig, &live) == 1)
        .await;
    rig.stop().await;
}

/// T1-4: attach input closes the gate: with same-turn evidence a send stays queued until
/// `ready`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_closes_gate() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    // Attach input inside a turn, then that turn's settle: evidence of the same turn as
    // the human write, which leaves the gate closed.
    rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "turn" }))
        .await;
    c.binary(b"x").await;
    rig.until("recorded", || human_writes(&rig, &live) == 1)
        .await;
    rig.hook(&live, "Stop", &json!({})).await;
    rig.clock.advance(crate::supervisor::send::SETTLE);
    rig.pass(&live).await;
    let id = rig.send(&live, "after").await;
    rig.pass(&live).await;
    assert_eq!(
        rig.input(id).state,
        chaldea_core::input::InputState::Queued,
        "same turn"
    );
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
    let writes = live.child.writes().len();
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
    // One part: one write call carries the whole text.
    assert_eq!(live.child.writes()[writes..], [want]);
    rig.stop().await;
}

/// T1-5 on a Codex row of a Windows host (`win_keys`, the host OS mapped, FakePty on every
/// runner): the paste is written as the encoder's key records, one part, no submit, and
/// answers `written`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paste_codex_win_keys() {
    use chaldea_core::adapter::HostOs;
    use chaldea_core::ids::Agent;
    use chaldea_core::input::encode::{Purpose, encode};
    let rig = SRig::on(HostOs::Windows, |_| {}).await;
    let live = rig.bound(Agent::Codex).await;
    let (mut c, _) = attach(&rig, &live).await;
    let before = live.child.writes().len();
    let text = "\u{3042}\u{1f642}\r\n\u{6b21}";
    c.json(json!({ "type": "paste", "paste_id": "w", "text": text }))
        .await;
    let want = encode(Agent::Codex, HostOs::Windows, text, Purpose::AttachPaste)
        .unwrap()
        .text;
    assert!(
        !want.starts_with(b"\x1b[200~"),
        "key records, not a bracketed paste"
    );
    assert_eq!(
        c.text().await,
        Some(AttachServerMessage::PasteResult {
            paste_id: "w".into(),
            outcome: PasteOutcome::Written,
            bytes_written: Some(want.len() as u64),
            reason: None,
        })
    );
    assert_eq!(live.child.writes()[before..], [want]);
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

/// T1-7: a thread resumed while its old socket is still open (its close frame held): a new
/// socket attaches the new launch, its input reaches the new PTY only, and the old PTY's
/// input is unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_with_old_socket_held() {
    let rig = SRig::new().await;
    let old = rig.claude().await;
    let shared = rig.srv.server.shared().clone();
    let (mut a, _) = attach(&rig, &old).await;
    let (reached, held) = shared.faults.arm_close_frame_gate();
    rig.cmd_ok("stop", &json!({ "thread_id": old.t }), 200)
        .await;
    assert!(matches!(
        a.text().await,
        Some(AttachServerMessage::Ended { launch_id, .. }) if launch_id == old.l
    ));
    bounded(WS_WAIT, reached).await.unwrap().unwrap();
    let r = rig
        .cmd(
            "resume",
            &json!({ "thread_id": old.t, "expected_launch_id": old.l }),
        )
        .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    let l: chaldea_core::ids::LaunchId = serde_json::from_value(v["launch_id"].clone()).unwrap();
    let new = rig.adopt(old.t, l, chaldea_core::ids::Agent::Claude);
    let old_input = old.child.input();
    let (mut b, first) = attach(&rig, &new).await;
    assert!(
        matches!(first, AttachServerMessage::Attached { launch_id, .. } if launch_id == new.l),
        "{first:?}"
    );
    let before = new.child.input().len();
    a.binary(b"o").await;
    b.binary(b"n").await;
    rig.until("the new socket's input", || {
        new.child.input()[before..] == *b"n"
    })
    .await;
    assert_eq!(old.child.input(), old_input);
    drop(held);
    a.until_closed().await;
    rig.stop().await;
}

/// R-10: a paste whose first write call fails writes zero bytes: `incomplete` with 0 bytes,
/// and the `human_write` is still recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zero_byte_incomplete_records() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    let before = live.child.input().len();
    live.child
        .fail_writes_after(before as u64, std::io::ErrorKind::BrokenPipe);
    c.json(json!({ "type": "paste", "paste_id": "z", "text": "t" }))
        .await;
    assert_eq!(
        c.text().await,
        Some(AttachServerMessage::PasteResult {
            paste_id: "z".into(),
            outcome: PasteOutcome::Incomplete,
            bytes_written: Some(0),
            reason: Some(PasteReason::Attach(AttachReason::Incomplete)),
        })
    );
    assert_eq!(live.child.input().len(), before);
    rig.until("recorded", || human_writes(&rig, &live) == 1)
        .await;
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
    let mut frames = Vec::new();
    while let Some(f) = c.next().await {
        frames.push(f);
    }
    // In order: the output, then `ended` as the last frame.
    let last = frames.pop();
    assert_eq!(
        last,
        Some(Frame::Text(AttachServerMessage::Ended {
            launch_id: live.l,
            reason: EndReason::Eof
        }))
    );
    let out: Vec<u8> = frames
        .into_iter()
        .flat_map(|f| match f {
            Frame::Binary(b) => b,
            Frame::Text(m) => panic!("unexpected {m:?}"),
        })
        .collect();
    assert!(out.ends_with(b"last"));
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
    let ticket = rig
        .srv
        .ticket(&rig.owner, &format!("attach:{}", live.t))
        .await
        .unwrap();
    let mut c = open_ticket(&rig, live.t, &ticket).await.expect("a 101");
    let (code, texts) = c.until_closed().await;
    assert_eq!((code, texts), (Some(4000), Vec::new()));
    // The `101` spent the ticket.
    assert_eq!(open_ticket(&rig, live.t, &ticket).await.err(), Some(401));
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

/// I-2: a disconnect alone moves the grant: A's Enter, queued behind a held write, is
/// refused at its dispatch once A's socket has ended, with no other socket attached.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disconnect_moves_grant() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    let shared = rig.srv.server.shared().clone();
    let (mut a, _) = attach(&rig, &live).await;
    let hold = live.child.hold_writes();
    let other = rig.attach(&live, b"k").await;
    assert!(hold.wait_entered(soon()));
    a.binary(b"\r").await;
    rig.until("A's unit queued", || sl.state().human_unsettled == 2)
        .await;
    drop(a);
    rig.until("A's socket ended", || shared.sockets.held() == 0)
        .await;
    drop(hold);
    bounded(WS_WAIT, other).await.unwrap().unwrap();
    rig.until("settled", || sl.state().human_unsettled == 0)
        .await;
    assert!(!live.child.input().contains(&b'\r'), "no Enter of A");
    rig.stop().await;
}

/// I-4: while reads are paused at the watermark, pings are not read either: a writer stuck
/// past the idle timeout drops the socket though the peer keeps pinging.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stuck_writer_idle_drop() {
    let rig = SRig::on_http(
        chaldea_core::adapter::HostOs::Macos,
        |_| {},
        |h| {
            h.ws.idle_timeout = Duration::from_secs(2);
        },
    )
    .await;
    let live = rig.claude().await;
    let h = hooks(&rig);
    let (mut c, _) = attach(&rig, &live).await;
    let hold = live.child.hold_writes();
    c.binary(b"a").await;
    assert!(hold.wait_entered(soon()));
    let paused = h.paused.load(std::sync::atomic::Ordering::SeqCst);
    c.binary(&[b'x'; 64 * 1024]).await;
    rig.until("the reads paused", || {
        h.paused.load(std::sync::atomic::Ordering::SeqCst) > paused
    })
    .await;
    let (mut tx, mut rx) = c.ws.split();
    #[allow(
        clippy::disallowed_methods,
        reason = "the peer's ping pace; the drop does not depend on it"
    )]
    let pinger = tokio::spawn(async move {
        loop {
            if tx.send(WsMessage::Ping(Vec::new().into())).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });
    let ended = bounded(WS_WAIT, async {
        while let Some(Ok(m)) = rx.next().await {
            if matches!(m, WsMessage::Close(_)) {
                return Some(m);
            }
        }
        None
    })
    .await
    .expect("dropped at the idle timeout");
    pinger.abort();
    assert_eq!(ended, None, "dropped without a close frame");
    drop(hold);
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
    // The bound is the message's: two fragments of 40 KiB are one 80 KiB message.
    use tokio_tungstenite::tungstenite::protocol::frame::coding::Data;
    let part = vec![b'f'; 40 * 1024];
    c.fragments(Data::Binary, &[&part, &part]).await;
    assert_eq!(
        c.text().await,
        Some(AttachServerMessage::InputDropped {
            bytes: 80 * 1024,
            reason: AttachReason::TooLarge
        })
    );
    let part = vec![b'g'; 32 * 1024];
    c.fragments(Data::Binary, &[&part, &part]).await;
    rig.until("the fragmented message written", || {
        live.child.input().len() == before + 128 * 1024
    })
    .await;
    assert_eq!(live.child.input()[before + 64 * 1024..], [b'g'; 64 * 1024]);
    rig.stop().await;
}

/// T1-9: a 256 KiB paste with every byte `\u00XX`-escaped fits the attach inbound limit,
/// in one frame and in fragments, and the PTY gets exactly the encoder's wrapped text.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worst_case_escape_fits() {
    use tokio_tungstenite::tungstenite::protocol::frame::coding::Data;
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    let text = "a".repeat(MAX_PROMPT_BYTES);
    let escaped = format!(
        r#"{{ "type" : "paste" , "paste_id" : "p" , "text" : "{}" }}"#,
        "\\u0061".repeat(MAX_PROMPT_BYTES)
    );
    assert!(escaped.len() <= crate::http::ATTACH_INBOUND_BYTES);
    let want = chaldea_core::input::encode::encode(
        chaldea_core::ids::Agent::Claude,
        rig.sup.host_os,
        &text,
        chaldea_core::input::encode::Purpose::AttachPaste,
    )
    .unwrap()
    .text;
    for fragmented in [false, true] {
        let before = live.child.input().len();
        if fragmented {
            let (a, b) = escaped.as_bytes().split_at(escaped.len() / 2);
            c.fragments(Data::Text, &[a, b]).await;
        } else {
            c.ws.send(WsMessage::Text(escaped.clone().into()))
                .await
                .unwrap();
        }
        let Some(AttachServerMessage::PasteResult { outcome, .. }) = c.text().await else {
            panic!("a paste result")
        };
        assert_eq!(outcome, PasteOutcome::Written, "{fragmented}");
        assert!(live.child.input()[before..] == want[..], "{fragmented}");
    }
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

/// R-11, T1-1 against the real attach handler: a foreign `Host` (421) or `Origin` (403)
/// leaves the ticket unspent, and a matching `Origin` redeems it; of 16 concurrent valid
/// redemptions of one ticket exactly one gets a `101`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn production_mount_layers() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let ticket = rig
        .srv
        .ticket(&rig.owner, &format!("attach:{}", live.t))
        .await
        .unwrap();
    let path = format!("/api/attach/{}?ticket={ticket}", live.t);
    let r = rig
        .srv
        .send(&rig.srv.ws_handshake(&path).host(Some("evil:1")))
        .await;
    assert_eq!(r.outcome(), (421, Some("misdirected_request".into())));
    let r = rig
        .srv
        .send(&rig.srv.ws_handshake(&path).header("Origin", "http://evil"))
        .await;
    assert_eq!(r.outcome(), (403, Some("origin_rejected".into())));
    let mut req = format!("ws://127.0.0.1:{}{path}", rig.srv.port)
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "Origin",
        format!("http://127.0.0.1:{}", rig.srv.port)
            .parse()
            .unwrap(),
    );
    let (ws, _) = bounded(WS_WAIT, tokio_tungstenite::connect_async(req))
        .await
        .unwrap()
        .expect("the ticket was unspent");
    let mut c = Client { ws, closed: None };
    assert!(matches!(
        c.text().await,
        Some(AttachServerMessage::Attached { .. })
    ));
    drop(c);

    let ticket = rig
        .srv
        .ticket(&rig.owner, &format!("attach:{}", live.t))
        .await
        .unwrap();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(16));
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let (barrier, url) = (
            barrier.clone(),
            format!(
                "ws://127.0.0.1:{}/api/attach/{}?ticket={ticket}",
                rig.srv.port, live.t
            ),
        );
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            match tokio_tungstenite::connect_async(url).await {
                Ok((ws, _)) => Ok(ws),
                Err(tokio_tungstenite::tungstenite::Error::Http(r)) => Err(r.status().as_u16()),
                Err(e) => panic!("{e}"),
            }
        }));
    }
    let mut ok = Vec::new();
    for t in tasks {
        match bounded(WS_WAIT, t).await.unwrap().unwrap() {
            Ok(ws) => ok.push(ws),
            Err(status) => assert_eq!(status, 401),
        }
    }
    assert_eq!(ok.len(), 1);
    drop(ok);
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
    // The first piece's send is held; the second goes over the budget meanwhile; the peer
    // then resumes reading.
    let (reached, held) = rig.srv.server.shared().faults.arm_socket_send_gate();
    live.child.emit(b"abc");
    bounded(WS_WAIT, reached).await.unwrap().unwrap();
    live.child.emit(&[b'z'; 64]);
    let h = hooks(&rig);
    rig.until("the lag latched", || {
        h.latched.load(std::sync::atomic::Ordering::SeqCst) > 0
    })
    .await;
    held.send(()).unwrap();
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

/// I-8: the first terminal event wins across the sink and the gone poll. The session has
/// left (its output end held back in the drain) and the loop parks right after sampling
/// the latch (nothing latched); the sink then lags, and the gone poll is ready when the
/// loop resumes: still 4002, no `ended`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lag_before_gone_poll_keeps_4002() {
    use chaldea_pty_runtime::fake::ProbePoint;
    let rig = SRig::with(|c| c.attach.out_max = 16).await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    let h = hooks(&rig);
    let probe = rig.probe(&live).await;
    let rx = rig.watch(&live).await;
    // Every send stalls, and verification reads hold: only the gone poll can be ready.
    let shared = rig.srv.server.shared().clone();
    let (reached, held) = shared.faults.arm_socket_send_gate();
    let (verifying, gos) = park_verify_reads(&rig, 2);
    // The over-budget piece waits in the drain, so its lag comes after the sample.
    let piece = probe.park(ProbePoint::PieceParsing);
    live.child.emit(&[b'z'; 64]);
    assert!(probe.wait(ProbePoint::PieceParsing, 1, soon()));
    live.child.exit(chaldea_pty_runtime::ChildExit {
        code: Some(0),
        signal: None,
    });
    rig.until("the session left", || {
        rig.sup.live(live.t, live.l).is_none()
    })
    .await;
    // The exit's nudge was taken (its read holds) before the parked iteration.
    next_entered(&verifying);
    let (at, go) = park_sync(&h.sampled);
    h.tick();
    entered(&at);
    drop(piece);
    SRig::delivered(&rx, &[b'z'; 64]).await;
    // The lag is latched (under the buffer's lock) before the gone poll runs: the poll is
    // requested now and is ready at the loop's first select after the park, ahead of the
    // sink's wake.
    h.gone_poll();
    go.send(()).unwrap();
    bounded(WS_WAIT, reached).await.unwrap().unwrap();
    rig.until("the gone poll ran", || {
        h.gone_polls.load(std::sync::atomic::Ordering::SeqCst) > 0
    })
    .await;
    held.send(()).unwrap();
    let mut texts = Vec::new();
    while let Some(f) = c.next().await {
        if let Frame::Text(m) = f {
            texts.push(m);
        }
    }
    drop(gos);
    assert_eq!((c.closed, texts), (Some(Some(4002)), Vec::new()));
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

/// R-9, T1-2 at the production bound: one cell's long zero-width run brings `attached` to
/// exactly 192 KiB, which keeps the grid; the same screen one byte larger gives
/// `grid: null` within the bound; a run that alone is over the budget gives `grid: null`
/// too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn production_cap_with_zero_width_run() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let max = rig.sup.cfg.attach.attached_max;
    assert_eq!(max, 192 * 1024);
    let (sink, rx) = chaldea_pty_runtime::channel_sink(1 << 20);
    let e = rig.sup.entry(live.l).unwrap();
    e.with_session(&rig.sup, rig.sup.now() + Duration::from_secs(1), |s| {
        s.subscribe(sink, soon())
    })
    .await
    .unwrap()
    .unwrap();
    // Clears the screen and writes `e` followed by `extra` bytes of the same cell's text:
    // combining marks on `e`, two UTF-8 bytes each, one of three bytes when odd.
    let screen = async |extra: usize| {
        let mut piece = "\x1b[2J\x1b[He".to_owned();
        let three = extra % 2;
        piece.push_str(&"\u{20d0}".repeat(three));
        piece.push_str(&"\u{301}".repeat((extra - 3 * three) / 2));
        // A sequence the grid does not show ends the screen, so the wait is for all of it
        // (a tail of marks would match any chunk the drain read).
        piece.push_str("\x1b[?25h");
        live.child.emit(piece.as_bytes());
        SRig::delivered(&rx, b"\x1b[?25h").await;
        attached_len(&rig, &live).await
    };
    let (base, _) = screen(0).await;
    let (len, m) = screen(max - base).await;
    assert_eq!(len, max);
    assert!(grid_of(&m).is_some(), "kept at exactly the bound");
    let (len, m) = screen(max - base + 1).await;
    assert!(len <= max, "{len}");
    assert!(
        matches!(m, AttachServerMessage::Attached { grid: None, .. }),
        "one byte over"
    );
    // A run that alone is far over the budget.
    let (len, m) = screen(2 * max).await;
    assert!(len <= max, "{len}");
    assert!(matches!(
        m,
        AttachServerMessage::Attached { grid: None, .. }
    ));
    rig.stop().await;
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
    let h = hooks(&rig);
    let paused = h.paused.load(std::sync::atomic::Ordering::SeqCst);
    let text = "p".repeat(100 * 1024);
    c.json(json!({ "type": "paste", "paste_id": "big", "text": text }))
        .await;
    rig.until("the reads paused at the watermark", || {
        h.paused.load(std::sync::atomic::Ordering::SeqCst) > paused
    })
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

/// I-4: retained paste ids and queued responses count against the watermark: with the
/// outbound send held, rejected pastes with long ids stop the reads; with a write held,
/// accepted pastes with long ids and short texts do too. While reading, the socket never
/// retained more than the watermark plus one message.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watermark_counts_ids_and_responses() {
    use std::sync::atomic::Ordering::SeqCst;
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let h = hooks(&rig);
    let watermark = rig.sup.cfg.attach.watermark as u64;
    let id = "i".repeat(16 * 1024);
    // One message: its id twice (the frame and the echo) plus framing.
    let message = 2 * id.len() as u64 + 256;
    for rejected in [true, false] {
        let (mut c, _) = attach(&rig, &live).await;
        let shared = rig.srv.server.shared().clone();
        let paused = h.paused.load(SeqCst);
        let (_reached, held) = shared.faults.arm_socket_send_gate();
        let hold = live.child.hold_writes();
        if !rejected {
            c.binary(b"a").await;
            assert!(hold.wait_entered(soon()));
        }
        let text = if rejected { "" } else { "t" };
        for _ in 0..12 {
            c.json(json!({ "type": "paste", "paste_id": id, "text": text }))
                .await;
        }
        rig.until("the reads paused", || h.paused.load(SeqCst) > paused)
            .await;
        let max = h.retained_max.load(SeqCst);
        drop(hold);
        drop(held);
        assert!(max < watermark + message, "{rejected}: {max}");
    }
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
    let h = hooks(&rig);
    let paused = h.paused.load(std::sync::atomic::Ordering::SeqCst);
    c.json(json!({ "type": "paste", "paste_id": "max", "text": "m".repeat(MAX_PROMPT_BYTES) }))
        .await;
    // The paste was taken in (pending, over the watermark) while the raw unit is held.
    rig.until("the paste taken in", || {
        h.paused.load(std::sync::atomic::Ordering::SeqCst) > paused
    })
    .await;
    drop(hold);
    let Some(AttachServerMessage::PasteResult { outcome, .. }) = c.text().await else {
        panic!("a paste result")
    };
    assert_eq!(outcome, PasteOutcome::Written);
    rig.stop().await;
}

// ---- timers on the test clock, takeover, revocation, drain -----------------------------------

/// A rig whose sends and closes have far longer bounds than any wait of the test: only the
/// drain deadline (on the test clock) can end a stalled send.
async fn slow_sends(tune: impl FnOnce(&mut crate::supervisor::SupervisorConfig)) -> SRig {
    SRig::on_http(chaldea_core::adapter::HostOs::Macos, tune, |h| {
        h.ws.send_timeout = Duration::from_secs(600);
        h.ws.close_grace = Duration::from_secs(600);
    })
    .await
}

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
            let _ = wait.recv();
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
    let rig = slow_sends(|_| {}).await;
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
    let h = hooks(&rig);
    let waiting = h.waiting.load(std::sync::atomic::Ordering::SeqCst);
    c.binary(b"a").await;
    assert!(hold.wait_entered(soon()));
    // The report deadline is installed before the clock moves.
    rig.until("the report wait began", || {
        h.waiting.load(std::sync::atomic::Ordering::SeqCst) > waiting
    })
    .await;
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
    // The leading `k` records the interval's one event; the refused paste adds none.
    rig.until("recorded", || human_writes(&rig, &live) >= 1)
        .await;
    assert_eq!(human_writes(&rig, &live), before_events.max(1));
    rig.stop().await;
}

/// T1-6, D5: the mode is the one at encoding. A paste encoded with 2004 on whose
/// submission waits while 2004 goes off and on again is refused `bracketed_paste_off`
/// (zero bytes, no event); one encoded with 2004 off whose submission waits while 2004
/// comes on is still rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paste_2004_toggled_after_encoding() {
    use super::super::supervisor::send::tests::{PASTE_OFF, PASTE_ON};
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let rx = rig.watch(&live).await;
    let (mut c, _) = attach(&rig, &live).await;
    let before = live.child.input().len();
    for (start_on, toggles, want) in [
        (
            true,
            [PASTE_OFF, PASTE_ON].concat(),
            PasteOutcome::NotWritten,
        ),
        (false, PASTE_ON.to_vec(), PasteOutcome::Rejected),
    ] {
        rig.paste(&live, start_on).await;
        let (at, go) = park_sync(&hooks(&rig).submit);
        c.json(json!({ "type": "paste", "paste_id": "p", "text": "t" }))
            .await;
        entered(&at);
        live.child.emit(&toggles);
        SRig::delivered(&rx, &toggles).await;
        go.send(()).unwrap();
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
                want,
                Some(0),
                Some(PasteReason::Attach(AttachReason::BracketedPasteOff))
            ),
            "{start_on}"
        );
    }
    assert_eq!(live.child.input().len(), before);
    assert_eq!(human_writes(&rig, &live), 0);
    rig.stop().await;
}

/// R-10: the report wait ends at the unit's own cutoff, taken before its submission. The
/// submission waits for the op lock while the clock moves half a second; once the clock is
/// past `not_after` + 1 s (and short of that plus the half second), the held unit answers
/// `pending`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn report_wait_from_cutoff() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    let before = live.child.input().len();
    let hold = live.child.hold_writes();
    let ops = rig
        .sup
        .entry(live.l)
        .unwrap()
        .ops
        .clone()
        .lock_owned()
        .await;
    let (at, go) = park_sync(&hooks(&rig).submit);
    c.json(json!({ "type": "paste", "paste_id": "p", "text": "t" }))
        .await;
    entered(&at);
    go.send(()).unwrap();
    let half = Duration::from_millis(500);
    rig.clock.advance(half);
    drop(ops);
    assert!(hold.wait_entered(soon()));
    let wait = rig.sup.cfg.attach.unit_wait + crate::supervisor::send::READ_WAIT;
    rig.clock.advance(wait - half + half / 2);
    let _p = poker(&rig);
    assert_eq!(
        c.text().await,
        Some(AttachServerMessage::PasteResult {
            paste_id: "p".into(),
            outcome: PasteOutcome::Pending,
            bytes_written: None,
            reason: None,
        })
    );
    drop(hold);
    input_has(&rig, &live, b"t").await;
    assert!(live.child.input().len() > before);
    rig.stop().await;
}

/// R-10: a report that comes only after `not_after` + 1 s is late even when the loop finds
/// it ready together with the deadline: `pending`, as for `key`; the unit is settled.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_report_is_pending() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    let h = hooks(&rig);
    let (mut c, _) = attach(&rig, &live).await;
    let hold = live.child.hold_writes();
    c.json(json!({ "type": "paste", "paste_id": "p", "text": "t" }))
        .await;
    assert!(hold.wait_entered(soon()));
    // The loop parks before its select; the report becomes ready after the deadline.
    let (at, go) = park_sync(&h.sampled);
    h.tick();
    entered(&at);
    rig.clock
        .advance(rig.sup.cfg.attach.unit_wait + crate::supervisor::send::READ_WAIT);
    drop(hold);
    rig.until("the unit settled", || sl.state().human_unsettled == 0)
        .await;
    go.send(()).unwrap();
    assert_eq!(
        c.text().await,
        Some(AttachServerMessage::PasteResult {
            paste_id: "p".into(),
            outcome: PasteOutcome::Pending,
            bytes_written: None,
            reason: None,
        })
    );
    // Settled: the socket submits again.
    c.binary(b"q").await;
    input_has(&rig, &live, b"q").await;
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

/// I-2: a candidate whose subscription finds the VT lock held past its wait closes 4004 and
/// changes nothing: the current socket keeps operating.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vt_unavailable_candidate_keeps_current() {
    use chaldea_pty_runtime::fake::ProbePoint;
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut a, _) = attach(&rig, &live).await;
    let probe = rig.probe(&live).await;
    // The drain holds the VT lock inside a piece; the candidate's wait for it ends.
    let park = probe.park(ProbePoint::PieceParsing);
    live.child.emit(b"p");
    assert!(probe.wait(ProbePoint::PieceParsing, 1, soon()));
    // The subscription's wait passes on the running clock; the lock stays held throughout.
    rig.clock.resume();
    let mut b = open_as(&rig, &rig.owner, live.t).await.expect("a 101");
    assert_eq!(b.until_closed().await.0, Some(4004));
    drop(park);
    let before = live.child.input().len();
    a.binary(b"still").await;
    rig.until("A still writes", || {
        live.child.input()[before..] == *b"still"
    })
    .await;
    rig.stop().await;
}

/// I-2: three candidates all subscribed and held before any swap; released together,
/// exactly one stays current and can write, the others close 4003.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn takeover_atomic() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (entered_tx, reached) = std::sync::mpsc::channel::<()>();
    let mut gos = Vec::new();
    {
        let h = hooks(&rig);
        let mut v = h.subscribes.lock().unwrap();
        for _ in 0..3 {
            let (go, wait) = std::sync::mpsc::channel::<()>();
            gos.push(go);
            let tx = entered_tx.clone();
            v.push(Box::new(move || {
                let _ = tx.send(());
                let _ = wait.recv();
            }));
        }
    }
    let (closed_tx, mut closed_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut writes = Vec::new();
    for i in 0..3 {
        let ticket = rig
            .srv
            .ticket(&rig.owner, &format!("attach:{}", live.t))
            .await
            .unwrap();
        let url = format!(
            "ws://127.0.0.1:{}/api/attach/{}?ticket={ticket}",
            rig.srv.port, live.t
        );
        let (write_tx, mut write_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        writes.push(write_tx);
        let closed_tx = closed_tx.clone();
        tokio::spawn(async move {
            let (ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
            let (mut tx, mut rx) = ws.split();
            loop {
                tokio::select! {
                    b = write_rx.recv() => {
                        let Some(b) = b else { return };
                        let _ = tx.send(WsMessage::Binary(b.into())).await;
                    }
                    m = rx.next() => match m {
                        Some(Ok(WsMessage::Close(f))) => {
                            let _ = closed_tx.send((i, f.map(|f| u16::from(f.code))));
                            return;
                        }
                        Some(Ok(_)) => {}
                        _ => {
                            let _ = closed_tx.send((i, None));
                            return;
                        }
                    },
                }
            }
        });
    }
    for _ in 0..3 {
        entered(&reached);
    }
    // Every candidate subscribed before any swap.
    drop(gos);
    let mut closed = Vec::new();
    for _ in 0..2 {
        closed.push(bounded(WS_WAIT, closed_rx.recv()).await.unwrap().unwrap());
    }
    assert!(closed.iter().all(|(_, c)| *c == Some(4003)), "{closed:?}");
    let survivor = (0..3).find(|i| closed.iter().all(|(j, _)| j != i)).unwrap();
    let before = live.child.input().len();
    writes[survivor].send(b"s".to_vec()).unwrap();
    rig.until("the survivor writes", || {
        live.child.input()[before..] == *b"s"
    })
    .await;
    drop(writes);
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

/// T1-3: the nudge is decided under the lease. A's resize to 100x30 is held in the backend
/// when B takes over and asks for 100x30 as its first resize; B waits for the lease, and
/// once A's resize returns the size is equal, so B nudges.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nudge_decided_under_lease() {
    use std::sync::atomic::Ordering::SeqCst;
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    let (mut a, _) = attach(&rig, &live).await;
    let hold = live.child.hold_resizes();
    a.json(json!({ "type": "resize", "cols": 100, "rows": 30 }))
        .await;
    assert!(hold.wait_entered(soon()));
    let (mut b, _) = attach(&rig, &live).await;
    assert_eq!(a.until_closed().await.0, Some(4003));
    let waits = sl.lease_waits.load(SeqCst);
    b.json(json!({ "type": "resize", "cols": 100, "rows": 30 }))
        .await;
    rig.until("B waits for the lease", || {
        sl.lease_waits.load(SeqCst) > waits
    })
    .await;
    drop(hold);
    assert_eq!(size_frame(&mut b).await, (100, 30, true));
    assert_eq!(
        resizes(&live),
        [
            PtySize::new(100, 30).unwrap(),
            PtySize::new(100, 31).unwrap(),
            PtySize::new(100, 30).unwrap()
        ]
    );
    rig.stop().await;
}

/// A panic in the resize's backend call still releases the lease: the attempt answers
/// `ok: false`, and the next resize gets the lease.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resize_panic_releases_lease() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, _) = attach(&rig, &live).await;
    live.child.panic_on_next_resize();
    c.json(json!({ "type": "resize", "cols": 100, "rows": 30 }))
        .await;
    assert!(!size_frame(&mut c).await.2);
    c.json(json!({ "type": "resize", "cols": 90, "rows": 20 }))
        .await;
    assert_eq!(size_frame(&mut c).await, (90, 20, true));
    assert_eq!(resizes(&live), [PtySize::new(90, 20).unwrap()]);
    rig.stop().await;
}

/// T1-3: a newer size replaces the nudge's second leg.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nudge_superseded() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut c, first) = attach(&rig, &live).await;
    let AttachServerMessage::Attached { cols, rows, .. } = first else {
        unreachable!()
    };
    let hold = live.child.hold_resizes();
    c.json(json!({ "type": "resize", "cols": cols, "rows": rows }))
        .await;
    assert!(hold.wait_entered(soon()));
    c.json(json!({ "type": "resize", "cols": 90, "rows": 20 }))
        .await;
    // A later frame handled: the newer size was read while the first leg is held.
    c.binary(b"z").await;
    input_has(&rig, &live, b"z").await;
    drop(hold);
    assert_eq!(size_frame(&mut c).await, (cols, rows + 1, true));
    assert_eq!(size_frame(&mut c).await, (90, 20, true));
    assert_eq!(
        resizes(&live),
        [
            PtySize::new(cols, rows + 1).unwrap(),
            PtySize::new(90, 20).unwrap()
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
    let ticket = rig
        .srv
        .ticket(&rig.owner, &format!("attach:{}", live.t))
        .await
        .unwrap();
    let mut c = open_ticket(&rig, live.t, &ticket).await.expect("a 101");
    let (code, _) = c.until_closed().await;
    drop(held);
    assert_eq!(code, Some(4004));
    assert_eq!(open_ticket(&rig, live.t, &ticket).await.err(), Some(401));
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

/// I-7: B's startup read succeeded and its subscription returns after B's token was
/// revoked (nudged): B takes over only after a read started after that nudge answers. While
/// that read is held, A still operates; then B closes 1008 without `attached`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_before_startup_takeover() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (mut a, _) = attach(&rig, &live).await;
    let owner = rig.srv.token(TokenKind::Owner, None, None).await;
    let (at, go) = park_sync(&hooks(&rig).subscribed);
    let connect = tokio::spawn({
        let ticket = rig
            .srv
            .ticket(&owner, &format!("attach:{}", live.t))
            .await
            .unwrap();
        let url = format!(
            "ws://127.0.0.1:{}/api/attach/{}?ticket={ticket}",
            rig.srv.port, live.t
        );
        tokio_tungstenite::connect_async(url)
    });
    entered(&at);
    rig.srv.revoke(&owner).await;
    let (entered_read, gos) = park_verify_reads(&rig, 1);
    go.send(()).unwrap();
    next_entered(&entered_read);
    // The fresh read is held: A is still current and its input is written.
    let before = live.child.input().len();
    a.binary(b"z").await;
    rig.until("A's input written", || {
        live.child.input()[before..] == *b"z"
    })
    .await;
    drop(gos);
    let (ws, _) = bounded(WS_WAIT, connect).await.unwrap().unwrap().unwrap();
    let mut b = Client { ws, closed: None };
    assert_eq!(b.until_closed().await, (Some(1008), Vec::new()));
    a.binary(b"y").await;
    rig.until("A still operates", || {
        live.child.input()[before..] == *b"zy"
    })
    .await;
    rig.stop().await;
}

/// I-7: a binary frame whose send had not reached the transport when the revocation was
/// detected is discarded: the peer gets the 1008 close and not the frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_send_discarded() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let owner = rig.srv.token(TokenKind::Owner, None, None).await;
    let mut a = open_as(&rig, &owner, live.t).await.expect("a 101");
    assert!(matches!(
        a.text().await,
        Some(AttachServerMessage::Attached { .. })
    ));
    let shared = rig.srv.server.shared().clone();
    let (reached, held) = shared.faults.arm_socket_send_gate();
    live.child.emit(b"secret");
    bounded(WS_WAIT, reached).await.unwrap().unwrap();
    rig.srv.revoke(&owner).await;
    rig.until("the socket is closing", || {
        shared.faults.steps().contains(&"socket close")
    })
    .await;
    held.send(()).unwrap();
    let mut binary = Vec::new();
    while let Some(f) = a.next().await {
        if let Frame::Binary(b) = f {
            binary.extend(b);
        }
    }
    assert_eq!((a.closed, binary), (Some(Some(1008)), Vec::new()));
    rig.stop().await;
}

/// A socket task aborted while its startup subscription is still on the blocking pool:
/// the subscription that registers after the abort is removed (no output needed).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aborted_startup_removes_late_subscription() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let handle = rig
        .sup
        .entry(live.l)
        .unwrap()
        .with_session(
            &rig.sup,
            rig.sup.now() + Duration::from_secs(5),
            chaldea_pty_runtime::Session::handle,
        )
        .await
        .unwrap();
    let base = handle.subscriber_count();
    let (at, go) = park_sync(&hooks(&rig).subscribed);
    let connect = tokio::spawn({
        let ticket = rig
            .srv
            .ticket(&rig.owner, &format!("attach:{}", live.t))
            .await
            .unwrap();
        let url = format!(
            "ws://127.0.0.1:{}/api/attach/{}?ticket={ticket}",
            rig.srv.port, live.t
        );
        tokio_tungstenite::connect_async(url)
    });
    entered(&at);
    assert_eq!(
        handle.subscriber_count(),
        base + 1,
        "registered, then parked"
    );
    let shared = rig.srv.server.shared().clone();
    shared.registry.abort_remaining();
    rig.until("the socket task ended", || shared.sockets.held() == 0)
        .await;
    go.send(()).unwrap();
    rig.until("the late subscription removed", || {
        handle.subscriber_count() == base
    })
    .await;
    drop(connect);
    rig.stop().await;
}

/// A launch whose last socket left while it was live is retired once it ends: the next
/// socket of another launch finds only its own entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registry_retires_ended_launch() {
    use std::sync::atomic::Ordering::SeqCst;
    let rig = SRig::new().await;
    let first = rig.claude().await;
    let shared = rig.srv.server.shared().clone();
    let (a, _) = attach(&rig, &first).await;
    drop(a);
    rig.until("the socket ended", || shared.sockets.held() == 0)
        .await;
    first.child.exit(chaldea_pty_runtime::ChildExit {
        code: Some(0),
        signal: None,
    });
    rig.until("the launch left", || rig.sup.entry(first.l).is_none())
        .await;
    let second = rig.claude().await;
    let (_b, _) = attach(&rig, &second).await;
    assert_eq!(hooks(&rig).registry_len.load(SeqCst), 1);
    rig.stop().await;
}

/// I-7: a revocation whose nudge is lost is found by the periodic verification (a tick the
/// test asks for), while the launch writes output all along: verification goes first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_lost_nudge_under_load() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let owner = rig.srv.token(TokenKind::Owner, None, None).await;
    let mut a = open_as(&rig, &owner, live.t).await.expect("a 101");
    assert!(matches!(
        a.text().await,
        Some(AttachServerMessage::Attached { .. })
    ));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let load = {
        let (child, stop) = (live.child.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                child.emit(&[b'o'; 1024]);
            }
        })
    };
    a.next().await.expect("output under way");
    rig.srv.revoke_quietly(&owner).await;
    hooks(&rig).tick();
    let code = a.until_closed().await.0;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    load.join().unwrap();
    assert_eq!(code, Some(1008));
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
        let _ = wait.recv();
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
    let rig = slow_sends(|c| c.attach.out_max = 16).await;
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
            let _ = wait.recv();
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
    hooks(&rig).tick();
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
    let (entered, gos) = park_verify_reads(&rig, 3);
    let h = hooks(&rig);
    h.tick();
    let first = next_entered(&entered);
    h.tick();
    let second = next_entered(&entered);
    // The stale reads' deadline is the short deadline from now; fresh reads start later.
    let short = rig.srv.server.shared().cfg.short_deadline;
    rig.clock.advance(Duration::from_secs(1));
    rig.srv.revoke(&owner).await;
    rig.until("the nudge found both slots taken", || {
        h.saturated.load(std::sync::atomic::Ordering::SeqCst) == 1
    })
    .await;
    // Both stale reads answer `true`; the nudge's fresh read takes a freed slot and parks.
    gos[first].send(()).unwrap();
    gos[second].send(()).unwrap();
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

/// I-8: `ended` is the last application frame, and the 5 s drain deadline cuts off the
/// close frame too: with the close's grace far longer, a close frame stalled after `ended`
/// is dropped at the drain deadline (the slot released), and releasing it afterwards
/// sends nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_stalled_after_ended() {
    let rig = slow_sends(|_| {}).await;
    let live = rig.claude().await;
    let shared = rig.srv.server.shared().clone();
    let (mut c, _) = attach(&rig, &live).await;
    let (reached, held) = shared.faults.arm_close_frame_gate();
    live.child.close_output();
    assert_eq!(
        c.text().await,
        Some(AttachServerMessage::Ended {
            launch_id: live.l,
            reason: EndReason::Eof
        })
    );
    bounded(WS_WAIT, reached).await.unwrap().unwrap();
    rig.clock.advance(rig.sup.cfg.attach.drain);
    let _p = poker(&rig);
    rig.until("the slot released", || shared.sockets.held() == 0)
        .await;
    let _ = held.send(());
    assert_eq!(c.until_closed().await, (None, Vec::new()));
    rig.stop().await;
}
