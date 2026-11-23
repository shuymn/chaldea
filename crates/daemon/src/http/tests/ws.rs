//! The socket loop's decisions (`SocketState`) and the cases of tickets and the loop that
//! need fault points (design §2.4 "Redemption", §2.6; PR7-CL-14, CL-16, CL-17).

use std::sync::mpsc;
use std::time::{Duration, Instant};

use chaldea_core::auth::TokenKind;
use chaldea_core::ids::{StreamId, ThreadId};
use chaldea_core::payload::Content;
use chaldea_core::sync::{StreamHeadEntry, WsServerMessage};

use super::support::http::{TestServer, WsClient};
use super::support::{raw_unknown, registered, soon};
use super::{eventually, mint_ticket, shared};
use crate::http::tickets::TicketPurpose;
use crate::http::ws::{ReadDone, ReadReq, SocketState, too_big};
use crate::http::{HttpConfig, Mounts};
use crate::store::TokenSelector;

const A: StreamId = StreamId::Host;

fn b() -> StreamId {
    StreamId::Thread(ThreadId::from_u128(0xB))
}

fn e(stream_id: StreamId, head_seq: u64) -> StreamHeadEntry {
    StreamHeadEntry {
        stream_id,
        head_seq,
    }
}

fn heads(entries: &[StreamHeadEntry]) -> Option<(WsServerMessage, bool)> {
    Some((
        WsServerMessage::Heads {
            heads: entries.to_vec(),
        },
        false,
    ))
}

fn advanced(stream_id: StreamId, head_seq: u64) -> Option<(WsServerMessage, bool)> {
    Some((
        WsServerMessage::StreamAdvanced {
            stream_id,
            head_seq,
        },
        false,
    ))
}

/// A state subscribed to `entries`' streams, its `heads` frame sent.
fn subscribed(entries: &[StreamHeadEntry]) -> SocketState {
    let mut st = SocketState::default();
    let ids: Vec<StreamId> = entries.iter().map(|x| x.stream_id).collect();
    st.on_subscribe(ids.clone());
    assert_eq!(st.next_read(), Some(ReadReq::Subscribe(ids)));
    assert!(st.read_done(ReadDone::Subscribed(entries.to_vec())));
    assert_eq!(st.next_frame(), heads(entries));
    assert_eq!(st.next_frame(), None);
    st
}

// ---- SocketState --------------------------------------------------------------------------

/// After a lag resync, old notifications still buffered change nothing.
#[test]
fn lag_with_buffered_old_notifications() {
    let mut st = subscribed(&[e(A, 5)]);
    st.on_commit(&[e(A, 3)]);
    assert_eq!(st.next_frame(), None);
    st.on_lag();
    assert_eq!(st.next_read(), Some(ReadReq::Heads(vec![A])));
    assert!(st.read_done(ReadDone::Heads(vec![e(A, 10)])));
    assert_eq!(st.next_frame(), heads(&[e(A, 10)]));
    for old in [7, 9, 10] {
        st.on_commit(&[e(A, old)]);
        assert_eq!(st.next_frame(), None, "{old}");
    }
    st.on_commit(&[e(A, 11)]);
    assert_eq!(st.next_frame(), advanced(A, 11));
}

/// Head 11 pending (its frame not yet selected), the refresh reads 12: `heads(12)` and no
/// `stream_advanced(11)` after it.
#[test]
fn periodic_snapshot_overtakes_pending() {
    let mut st = subscribed(&[e(A, 10)]);
    st.on_commit(&[e(A, 11)]);
    st.on_tick();
    assert_eq!(st.next_read(), Some(ReadReq::Heads(vec![A])));
    assert!(st.read_done(ReadDone::Heads(vec![e(A, 12)])));
    assert_eq!(
        st.next_frame(),
        Some((
            WsServerMessage::Heads {
                heads: vec![e(A, 12)]
            },
            true
        )),
        "the ping goes with the refresh frame"
    );
    assert_eq!(st.next_frame(), None);
    // A refresh with nothing new still sends `heads`, equal to what was announced.
    st.on_tick();
    assert_eq!(st.next_read(), Some(ReadReq::Heads(vec![A])));
    assert!(st.read_done(ReadDone::Heads(vec![e(A, 12)])));
    assert_eq!(
        st.next_frame(),
        Some((
            WsServerMessage::Heads {
                heads: vec![e(A, 12)]
            },
            true
        ))
    );
    assert_eq!(st.next_frame(), None);
}

/// The old set holds until the subscribe's read merges; then only the new set is named.
#[test]
fn resubscribe_takes_effect_at_merge() {
    let mut st = subscribed(&[e(A, 5)]);
    st.on_subscribe(vec![b()]);
    assert_eq!(st.next_read(), Some(ReadReq::Subscribe(vec![b()])));
    // During the read: `A` is still subscribed, `b` not yet.
    st.on_commit(&[e(A, 6), e(b(), 3)]);
    assert_eq!(st.next_frame(), advanced(A, 6));
    assert_eq!(st.next_frame(), None);
    assert!(st.read_done(ReadDone::Subscribed(vec![e(b(), 4)])));
    assert_eq!(st.next_frame(), heads(&[e(b(), 4)]));
    st.on_commit(&[e(A, 7)]);
    assert_eq!(st.next_frame(), None);
    st.on_commit(&[e(b(), 5)]);
    assert_eq!(st.next_frame(), advanced(b(), 5));
    // Resubscribing to `A` starts it afresh from the read.
    st.on_subscribe(vec![A]);
    assert_eq!(st.next_read(), Some(ReadReq::Subscribe(vec![A])));
    assert!(st.read_done(ReadDone::Subscribed(vec![e(A, 7)])));
    assert_eq!(st.next_frame(), heads(&[e(A, 7)]));
}

/// Two subscribes read while the sink is busy: one `heads` frame, for the second set; a
/// subscribe replaced before its read starts is never read.
#[test]
fn subscribes_coalesce() {
    let mut st = SocketState::default();
    st.on_subscribe(vec![A]);
    assert_eq!(st.next_read(), Some(ReadReq::Subscribe(vec![A])));
    assert!(st.read_done(ReadDone::Subscribed(vec![e(A, 1)])));
    // The sink is busy: no frame is selected before the next subscribe's merge.
    st.on_subscribe(vec![A, b()]);
    st.on_subscribe(vec![b()]);
    assert_eq!(st.next_read(), Some(ReadReq::Subscribe(vec![b()])));
    assert_eq!(st.next_read(), None, "one read at a time");
    assert!(st.read_done(ReadDone::Subscribed(vec![e(b(), 2)])));
    assert_eq!(st.next_frame(), heads(&[e(b(), 2)]));
    assert_eq!(st.next_frame(), None);
    assert_eq!(st.next_read(), None);
}

/// Heads time fires while a subscribe read is in flight and another subscribe is pending:
/// the refresh read starts next, and its frame comes before the pending subscribe's.
#[test]
fn tick_during_read_refreshes_next() {
    let mut st = subscribed(&[e(A, 1)]);
    st.on_subscribe(vec![A, b()]);
    assert_eq!(st.next_read(), Some(ReadReq::Subscribe(vec![A, b()])));
    st.on_subscribe(vec![b()]);
    st.on_tick();
    st.on_nudge();
    assert_eq!(st.next_read(), None, "a read is in flight");
    assert!(st.read_done(ReadDone::Subscribed(vec![e(A, 2), e(b(), 3)])));
    // The refresh first; it re-verifies the token, so it serves the nudge too.
    assert_eq!(st.next_read(), Some(ReadReq::Heads(vec![A, b()])));
    assert!(st.read_done(ReadDone::Heads(vec![e(A, 4), e(b(), 3)])));
    assert_eq!(
        st.next_frame(),
        Some((
            WsServerMessage::Heads {
                heads: vec![e(A, 4), e(b(), 3)]
            },
            true
        ))
    );
    assert_eq!(st.next_read(), Some(ReadReq::Subscribe(vec![b()])));
    assert!(st.read_done(ReadDone::Subscribed(vec![e(b(), 3)])));
    assert_eq!(st.next_frame(), heads(&[e(b(), 3)]));
    assert_eq!(st.next_read(), None, "the nudge was served by the refresh");
}

#[test]
fn nudge_reads_the_token_and_revocation_ends() {
    let mut st = subscribed(&[e(A, 1)]);
    st.on_nudge();
    assert_eq!(st.next_read(), Some(ReadReq::Verify));
    assert!(st.read_done(ReadDone::Verified));
    assert_eq!(st.next_frame(), None);
    st.on_nudge();
    assert_eq!(st.next_read(), Some(ReadReq::Verify));
    assert!(!st.read_done(ReadDone::Revoked));
}

// ---- the driver and admission ---------------------------------------------------------------

async fn redeem_status(srv: &TestServer, ticket: &str) -> u16 {
    let url = format!("ws://127.0.0.1:{}/api/ws?ticket={ticket}", srv.port);
    let done = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async(url),
    )
    .await
    .expect("the redemption's handshake ends within 10 s");
    match done {
        Ok((_, resp)) => resp.status().as_u16(),
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => r.status().as_u16(),
        Err(e) => panic!("{e}"),
    }
}

async fn wait(what: &str, rx: tokio::sync::oneshot::Receiver<()>) {
    tokio::time::timeout(Duration::from_secs(10), rx)
        .await
        .unwrap_or_else(|_| panic!("{what}"))
        .unwrap();
}

/// Arms the next store read to block (on its blocking thread) until the returned sender is
/// used or dropped; `entered` fires when it blocks.
fn hold_next_read(srv: &TestServer) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (entered_tx, entered) = mpsc::channel();
    let (release, release_rx) = mpsc::channel::<()>();
    shared(srv).faults.arm_before_read(0, move || {
        let _ = entered_tx.send(());
        let _ = release_rx.recv_timeout(Duration::from_secs(30));
    });
    (entered, release)
}

async fn entered(rx: &mpsc::Receiver<()>) {
    eventually(Duration::from_secs(10), "the held read was entered", || {
        rx.try_recv().is_ok()
    })
    .await;
}

/// Waits until no socket task is registered and every socket slot is free.
async fn socket_gone(srv: &TestServer, base: usize) {
    let s = shared(srv);
    eventually(Duration::from_secs(10), "the socket task ended", || {
        srv.server.tasks() <= base && s.sockets.held() == 0
    })
    .await;
}

/// The idle deadline fires while a send and a read are both pending: the socket is dropped
/// with neither released.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_deadlines_fire_during_a_pending_send_and_read() {
    let srv = TestServer::start_with(Mounts::mvp(), |c: &mut HttpConfig| {
        c.ws.idle_timeout = Duration::from_millis(300);
        c.ws.heads_interval = Duration::from_secs(600);
    })
    .await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let ticket = mint_ticket(&srv, &owner, TicketPurpose::Events);
    let s = shared(&srv);
    let base = srv.server.tasks();
    // `hello`'s send held at the gate, after the startup read.
    let (reached, _send_held) = s.faults.arm_socket_send_gate();
    let mut ws = srv.ws_with(&ticket).await.expect("a 101");
    wait("the send reached the gate", reached).await;
    // A subscribe's read held.
    let (read_entered, _read_held) = hold_next_read(&srv);
    ws.subscribe(&[A]).await;
    entered(&read_entered).await;
    let at = Instant::now();
    socket_gone(&srv, base).await;
    assert!(
        at.elapsed() < Duration::from_secs(2),
        "the idle deadline, not the read deadline"
    );
    srv.stop().await;
}

/// A socket read held past its deadline at a fault point drops the socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_read_drops_the_socket() {
    let srv = TestServer::start_with(Mounts::mvp(), |c: &mut HttpConfig| {
        c.ws.heads_interval = Duration::from_secs(600);
    })
    .await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let ticket = mint_ticket(&srv, &owner, TicketPurpose::Events);
    let base = srv.server.tasks();
    let mut ws = srv.ws_with(&ticket).await.expect("a 101");
    assert!(matches!(
        ws.next().await,
        Some(WsServerMessage::Hello { .. })
    ));
    let (read_entered, held) = hold_next_read(&srv);
    ws.subscribe(&[A]).await;
    entered(&read_entered).await;
    // Dropped without a close frame, the read still held.
    assert_eq!((ws.next().await, ws.closed), (None, Some(None)));
    socket_gone(&srv, base).await;
    drop(held);
    srv.stop().await;
}

/// With every commit notification dropped, the next periodic `heads` carries the commit,
/// within `heads_interval + 2 × read deadline + 2 × send_timeout`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_notification_recovered_by_heads() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let t = ThreadId::from_u128(0x7001);
    srv.append(vec![registered(t, false)]).await;
    let stream = StreamId::Thread(t);
    let ticket = mint_ticket(&srv, &owner, TicketPurpose::Events);
    let mut ws = srv.ws_with(&ticket).await.expect("a 101");
    assert!(matches!(
        ws.next().await,
        Some(WsServerMessage::Hello { .. })
    ));
    ws.subscribe(&[stream]).await;
    // Once a frame has carried the old head, the new head can come only from a refresh
    // read after the append. A periodic frame may come before the subscribe's (empty, the
    // set not yet replaced), so wait, bounded, for the one that names the stream.
    let old = srv.store.read(move |tx| tx.head(stream, 0)).head_seq;
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let m = ws.next().await.expect("open");
        assert!(Instant::now() < until, "no `heads` with the old head");
        match m {
            WsServerMessage::Heads { heads } if heads == [e(stream, old)] => break,
            WsServerMessage::Heads { heads } => assert!(heads.is_empty(), "{heads:?}"),
            other => panic!("only `heads` before the subscribe's: {other:?}"),
        }
    }
    let s = shared(&srv);
    s.faults.drop_notifications(true);
    srv.append(vec![raw_unknown(stream, "lost", Content::text("x"))])
        .await;
    let want = srv.store.read(move |tx| tx.head(stream, 0)).head_seq;
    let at = Instant::now();
    let cfg = &s.cfg;
    let bound = cfg.ws.heads_interval + 2 * cfg.short_deadline + 2 * cfg.ws.send_timeout;
    loop {
        match ws.next().await.expect("open") {
            WsServerMessage::Heads { heads } if heads == [e(stream, want)] => break,
            WsServerMessage::Heads { .. } => {}
            other => panic!("only `heads` while notifications are lost: {other:?}"),
        }
        assert!(at.elapsed() < bound);
    }
    assert!(at.elapsed() < bound);
    srv.stop().await;
}

/// A send held past its `send_timeout`: the socket is dropped at the send deadline without a
/// close frame, and its task and slot are released.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_deadline_drops_without_a_close_frame() {
    let srv = TestServer::start_with(Mounts::mvp(), |c: &mut HttpConfig| {
        c.ws.send_timeout = Duration::from_millis(300);
        c.ws.idle_timeout = Duration::from_secs(600);
        c.ws.heads_interval = Duration::from_secs(600);
    })
    .await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let ticket = mint_ticket(&srv, &owner, TicketPurpose::Events);
    let base = srv.server.tasks();
    let (reached, _held) = shared(&srv).faults.arm_socket_send_gate();
    let mut ws = srv.ws_with(&ticket).await.expect("a 101");
    wait("the send reached the gate", reached).await;
    socket_gone(&srv, base).await;
    assert_eq!(
        (ws.next().await, ws.closed),
        (None, Some(None)),
        "no close frame"
    );
    assert!(!shared(&srv).faults.steps().contains(&"socket close"));
    srv.stop().await;
}

/// A revocation while a send is held: the close path keeps the send's own deadline, so the
/// socket is dropped by it, long before a `close_grace` that is far longer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_keeps_the_send_deadline() {
    let srv = TestServer::start_with(Mounts::mvp(), |c: &mut HttpConfig| {
        c.ws.send_timeout = Duration::from_secs(2);
        c.ws.close_grace = Duration::from_secs(30);
        c.ws.idle_timeout = Duration::from_secs(600);
        c.ws.heads_interval = Duration::from_secs(600);
    })
    .await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let ticket = mint_ticket(&srv, &owner, TicketPurpose::Events);
    let base = srv.server.tasks();
    let (reached, _held) = shared(&srv).faults.arm_socket_send_gate();
    let _ws = srv.ws_with(&ticket).await.expect("a 101");
    wait("the send reached the gate", reached).await;
    let sent_at = Instant::now();
    srv.revoke(&owner).await;
    let s = shared(&srv);
    eventually(Duration::from_secs(10), "the close path entered", || {
        s.faults.steps().contains(&"socket close")
    })
    .await;
    eventually(
        Duration::from_secs(8),
        "dropped by the send deadline",
        || srv.server.tasks() <= base && s.sockets.held() == 0,
    )
    .await;
    assert!(sent_at.elapsed() < Duration::from_secs(8));
    srv.stop().await;
}

/// A socket whose send is held when shutdown begins, with a shutdown budget shorter than
/// `send_timeout` and `close_grace`: it is still running at the deadline, gets an abort
/// request, and is counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_aborts_a_blocked_socket() {
    let mut srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let ticket = mint_ticket(&srv, &owner, TicketPurpose::Events);
    let (reached, _held) = shared(&srv).faults.arm_socket_send_gate();
    let _ws = srv.ws_with(&ticket).await.expect("a 101");
    wait("the send reached the gate", reached).await;
    let deadline = Instant::now() + Duration::from_millis(300);
    let report = srv.shutdown_http(deadline).await;
    assert!(
        Instant::now() < deadline + Duration::from_millis(500),
        "late"
    );
    assert_eq!(report.abort_requested.socket, 1, "{report:?}");
    srv.store.shutdown();
}

/// A revocation committed between the credential read and the final admission read: 401,
/// no `101`, and the ticket is not spent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoked_before_final_admission() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let ticket = mint_ticket(&srv, &owner, TicketPurpose::Events);
    let s = shared(&srv);
    let id = srv.token_row(&owner).unwrap().token_id;
    let store = srv.store.store().clone();
    // After the credential read: the revocation commits before the final read starts.
    s.faults.arm_after_read(0, move || {
        let job = store
            .submit(soon(), move |tx| {
                Ok(tx.token_revoke(TokenSelector::Token(id))?)
            })
            .unwrap();
        assert_eq!(tokio::runtime::Handle::current().block_on(job).unwrap(), 1);
    });
    assert_eq!(s.tickets.len(), 1);
    assert_eq!(redeem_status(&srv, &ticket).await, 401);
    assert_eq!(s.tickets.len(), 1, "the ticket is not spent");
    srv.stop().await;
}

/// A final admission read that fails (here at its cutoff) does not spend the ticket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_admission_failure_keeps_ticket() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let ticket = mint_ticket(&srv, &owner, TicketPurpose::Events);
    let s = shared(&srv);
    let clock = srv.clock.clone();
    // The final admission read returns past its deadline on the server's clock.
    s.faults
        .arm_after_read(1, move || clock.skew(Duration::from_secs(5)));
    assert_eq!(redeem_status(&srv, &ticket).await, 408);
    assert_eq!(s.tickets.len(), 1);
    let mut ws = srv.ws_with(&ticket).await.expect("a 101");
    assert!(matches!(
        ws.next().await,
        Some(WsServerMessage::Hello { .. })
    ));
    assert_eq!(s.tickets.len(), 0);
    srv.stop().await;
}

/// A revocation committed after the final admission read: `101`, then the startup read
/// closes the socket with 1008 before `hello`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoked_between_admission_and_startup() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let ticket = mint_ticket(&srv, &owner, TicketPurpose::Events);
    let s = shared(&srv);
    let (reached, release) = s.faults.arm_removal_gate();
    let url = format!("ws://127.0.0.1:{}/api/ws?ticket={ticket}", srv.port);
    let client = tokio::spawn(tokio_tungstenite::connect_async(url));
    wait("admission reached the gate", reached).await;
    srv.revoke_quietly(&owner).await;
    drop(release);
    let (ws, resp) = tokio::time::timeout(Duration::from_secs(10), client)
        .await
        .expect("the handshake ends")
        .unwrap()
        .expect("a 101");
    assert_eq!(resp.status(), 101);
    let mut ws = WsClient { ws, closed: None };
    assert_eq!((ws.next().await, ws.closed), (None, Some(Some(1008))));
    srv.stop().await;
}

/// The expiry is sampled immediately before the removal: valid at the credential step,
/// expired when the removal samples the clock: 401.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ticket_expiry_sampled_at_removal() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let f = Instant::now();
    srv.clock.freeze(f);
    let ticket = mint_ticket(&srv, &owner, TicketPurpose::Events);
    srv.clock.freeze(f + Duration::from_millis(29_999));
    let s = shared(&srv);
    let (reached, release) = s.faults.arm_removal_gate();
    let srv = std::sync::Arc::new(srv);
    let redeem = {
        let (srv, ticket) = (srv.clone(), ticket.clone());
        tokio::spawn(async move { redeem_status(&srv, &ticket).await })
    };
    wait("admission reached the gate", reached).await;
    srv.clock.freeze(f + Duration::from_secs(30));
    drop(release);
    assert_eq!(redeem.await.unwrap(), 401);
    std::sync::Arc::into_inner(srv).unwrap().stop().await;
}

/// An error whose text is "wrapped" and whose source is the axum error it holds.
#[derive(Debug)]
struct Wrapped(axum::Error);

impl std::fmt::Display for Wrapped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("wrapped")
    }
}

impl std::error::Error for Wrapped {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// The 1009 classifier finds tungstenite's capacity error by type, through axum's wrapping
/// and any further layer, and nothing else, whatever an error's text says.
#[test]
fn too_big_is_the_typed_capacity_error() {
    use tungstenite::error::CapacityError;
    let capacity = || {
        axum::Error::new(tungstenite::Error::Capacity(
            CapacityError::MessageTooLong {
                size: 65,
                max_size: 64,
            },
        ))
    };
    assert!(too_big(&capacity()));
    assert!(
        too_big(&Wrapped(capacity())),
        "under a layer whose text says nothing"
    );
    let lookalike = axum::Error::new(std::io::Error::other("Space limit exceeded"));
    assert!(!too_big(&lookalike), "the text alone is not the error");
    assert!(!too_big(&axum::Error::new(
        tungstenite::Error::ConnectionClosed
    )));
}

/// Holds `hello` at the send gate on a one-thread runtime, then blocks that thread past
/// the send deadline so the send and its deadline are both due at the next poll; `revoke`
/// first makes the close path the one holding the send. The send path must not deliver it.
/// This covers the send path only: frames other paths flush are the known limit of design
/// §2.6 "Bounded disconnect".
async fn send_due_after_its_deadline(revoke: bool) {
    let srv = TestServer::start_with(Mounts::mvp(), |c: &mut HttpConfig| {
        c.ws.send_timeout = Duration::from_secs(1);
        c.ws.close_grace = Duration::from_secs(4);
        c.ws.idle_timeout = Duration::from_secs(600);
        c.ws.heads_interval = Duration::from_secs(600);
    })
    .await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let ticket = mint_ticket(&srv, &owner, TicketPurpose::Events);
    let s = shared(&srv);
    let (reached, held) = s.faults.arm_socket_send_gate();
    let mut ws = srv.ws_with(&ticket).await.expect("a 101");
    wait("hello is held", reached).await;
    if revoke {
        srv.revoke(&owner).await;
        eventually(Duration::from_millis(800), "the close path entered", || {
            s.faults.steps().contains(&"socket close")
        })
        .await;
    }
    // The server shares this runtime thread: block it past the send deadline, then make
    // the send ready before the socket task is polled again.
    std::thread::sleep(Duration::from_millis(1100));
    held.send(()).unwrap();
    assert_eq!(
        (ws.next().await, ws.closed),
        (None, Some(None)),
        "dropped, no frame"
    );
    srv.stop().await;
}

/// The loop's send path never delivers a send that became ready after its deadline.
#[tokio::test(flavor = "current_thread")]
async fn loop_drops_a_send_ready_after_its_deadline() {
    send_due_after_its_deadline(false).await;
}

/// The close path's send never delivers a send that became ready after its deadline.
#[tokio::test(flavor = "current_thread")]
async fn close_drops_a_send_ready_after_its_deadline() {
    send_due_after_its_deadline(true).await;
}
