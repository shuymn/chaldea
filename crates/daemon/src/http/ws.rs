//! `POST /api/ws-ticket` and the `GET /api/ws` socket loop (design §2.4 "Tickets", §2.6).
//!
//! The loop is split in two: [`SocketState`] is the socket's decisions without I/O (the one
//! `merge` of every source of heads, frames built at send selection, the order of reads),
//! and [`events_socket`] drives it with the socket, the store's commit notifications, the
//! revocation nudge, and the deadlines, each read a `spawn_blocking` handle held across
//! iterations so control deadlines fire while a read or a send is in flight.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::Poll;
use std::time::Instant;

use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::http::{Request, Response};
use chaldea_core::auth::{Action, TokenKind, grant};
use chaldea_core::http::{WsTicketRequest, WsTicketResponse};
use chaldea_core::ids::StreamId;
use chaldea_core::sync::{StreamHeadEntry, WsServerMessage};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::sleep_until;

use super::Shared;
use super::auth::{AuthFailure, verify_hash_in};
use super::ctx::{Joined, ReadCtx, spawn_read, take_read};
use super::error::{ApiError, in_source_chain};
use super::pair::secs;
use super::scope::ReadScope;
use super::sync_api::{json, store};
use super::tickets::{TicketEntry, TicketPurpose};
use super::upgrade::UpgradedSocket;
use crate::store::{Clock, Committed, ReadError, ReadTx, StoreError, SystemClock};

// ---- POST /api/ws-ticket -----------------------------------------------------------------

/// `POST /api/ws-ticket` with `WsTicketRequest { purpose }`: `events`, or `attach:<thread>`
/// for an `owner` token and an existing thread. A body that is not the request, or another
/// purpose, is 400; the caps are 503 `busy`.
pub(crate) async fn ws_ticket(
    ctx: ReadCtx,
    req: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    let shared = ctx.shared().clone();
    let body = ctx
        .body_within(req.into_body(), shared.cfg.max_body_bytes)
        .await?;
    let request: WsTicketRequest =
        serde_json::from_slice(&body).map_err(|_| ApiError::bad_request())?;
    let p = ctx.principal().ok_or_else(ApiError::internal)?.clone();
    let purpose = if request.purpose == "events" {
        TicketPurpose::Events
    } else {
        let Some(StreamId::Thread(t)) = request
            .purpose
            .strip_prefix("attach:")
            .and_then(|t| format!("thread:{t}").parse::<StreamId>().ok())
        else {
            return Err(ApiError::bad_request());
        };
        if p.kind != TokenKind::Owner || grant(p.kind, Action::Attach).is_none() {
            return Err(ApiError::forbidden());
        }
        let exists = ctx
            .read(move |tx, _| Ok(tx.thread(t).map_err(store)?.is_some()))
            .await?;
        if !exists {
            return Err(ApiError::not_found());
        }
        TicketPurpose::Attach(t)
    };
    let now = shared.cfg.clock.now();
    let ttl = shared.cfg.ticket_ttl;
    let ticket = shared
        .tickets
        .mint(
            TicketEntry {
                token_id: p.token_id,
                token_sha256: p.token_sha256,
                purpose,
                expires_at: now + ttl,
            },
            now,
        )
        .map_err(|_| ApiError::busy())?;
    json(&WsTicketResponse {
        ticket: ticket.expose().to_owned(),
        expires_in: secs(ttl),
    })
}

// ---- the socket's decisions ---------------------------------------------------------------

/// A read the socket asks for; every one re-verifies the token first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReadReq {
    /// The token only (startup, a nudge).
    Verify,
    /// The token, the readable existing streams among these, and their heads.
    Subscribe(Vec<StreamId>),
    /// The token and the heads of the subscription set (a refresh or a lag resync).
    Heads(Vec<StreamId>),
}

/// A read's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReadDone {
    Revoked,
    Verified,
    /// The kept streams with their heads.
    Subscribed(Vec<StreamHeadEntry>),
    Heads(Vec<StreamHeadEntry>),
}

/// The socket's state (design §2.6 "State"): the subscription set `S`, the newest head it
/// knows and the newest it announced per stream of `S`, and what is due.
#[derive(Debug, Default)]
pub(crate) struct SocketState {
    subs: BTreeSet<StreamId>,
    known: BTreeMap<StreamId, u64>,
    announced: BTreeMap<StreamId, u64>,
    heads_due: bool,
    refresh_due: bool,
    nudge_due: bool,
    /// The latest subscribe not yet read; a later one replaces it.
    pending_subscribe: Option<Vec<StreamId>>,
    reading: bool,
    ping_due: bool,
}

impl SocketState {
    pub(crate) fn on_subscribe(&mut self, ids: Vec<StreamId>) {
        self.pending_subscribe = Some(ids);
    }

    /// Heads time: a refresh becomes due, and a ping goes with the next frame.
    pub(crate) fn on_tick(&mut self) {
        self.refresh_due = true;
        self.ping_due = true;
    }

    /// The commit receiver lagged: resync from heads.
    pub(crate) fn on_lag(&mut self) {
        self.refresh_due = true;
    }

    pub(crate) fn on_nudge(&mut self) {
        self.nudge_due = true;
    }

    pub(crate) fn on_commit(&mut self, heads: &[StreamHeadEntry]) {
        self.merge(heads);
    }

    /// The one merge of every source of heads: a stream of `S` takes the larger head.
    fn merge(&mut self, heads: &[StreamHeadEntry]) {
        for e in heads {
            if self.subs.contains(&e.stream_id) {
                let k = self.known.entry(e.stream_id).or_insert(0);
                *k = (*k).max(e.head_seq);
            }
        }
    }

    /// The next read, if none is in flight: a due refresh first, then a pending subscribe,
    /// then a pending nudge. Every read re-verifies the token, so starting one serves a
    /// pending nudge.
    pub(crate) fn next_read(&mut self) -> Option<ReadReq> {
        if self.reading {
            return None;
        }
        let req = if self.refresh_due {
            self.refresh_due = false;
            ReadReq::Heads(self.subs.iter().copied().collect())
        } else if let Some(ids) = self.pending_subscribe.take() {
            ReadReq::Subscribe(ids)
        } else if self.nudge_due {
            ReadReq::Verify
        } else {
            return None;
        };
        self.nudge_due = false;
        self.reading = true;
        Some(req)
    }

    /// Applies a read's result; `false` when the token is no longer valid.
    pub(crate) fn read_done(&mut self, done: ReadDone) -> bool {
        self.reading = false;
        match done {
            ReadDone::Revoked => return false,
            ReadDone::Verified => {}
            ReadDone::Subscribed(heads) => {
                // The replacement takes effect at this merge.
                self.subs = heads.iter().map(|e| e.stream_id).collect();
                let subs = &self.subs;
                self.known.retain(|s, _| subs.contains(s));
                self.announced.retain(|s, _| subs.contains(s));
                self.merge(&heads);
                self.heads_due = true;
            }
            ReadDone::Heads(heads) => {
                self.merge(&heads);
                self.heads_due = true;
            }
        }
        true
    }

    /// The frame to send now that the sink is free, built from the current `known`: `heads`
    /// with every stream of `S` when one is due, else `stream_advanced` for one stream whose
    /// known head is newer than the one announced. The flag says a ping goes with it.
    pub(crate) fn next_frame(&mut self) -> Option<(WsServerMessage, bool)> {
        let frame = if self.heads_due {
            self.heads_due = false;
            let heads = self
                .subs
                .iter()
                .map(|s| StreamHeadEntry {
                    stream_id: *s,
                    head_seq: self.known.get(s).copied().unwrap_or(0),
                })
                .collect::<Vec<_>>();
            for e in &heads {
                self.announced.insert(e.stream_id, e.head_seq);
            }
            WsServerMessage::Heads { heads }
        } else {
            let (stream_id, head_seq) = self
                .subs
                .iter()
                .filter_map(|s| self.known.get(s).map(|k| (*s, *k)))
                .find(|(s, k)| self.announced.get(s).is_none_or(|a| k > a))?;
            self.announced.insert(stream_id, head_seq);
            WsServerMessage::StreamAdvanced {
                stream_id,
                head_seq,
            }
        };
        Some((frame, std::mem::take(&mut self.ping_due)))
    }
}

/// Why an inbound text frame closes the socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    /// Not a `WsClientMessage`: 1003.
    Invalid,
    /// More than `max_subscriptions` ids: 1008.
    TooMany,
}

/// A `subscribe`'s ids, the malformed ones dropped; the count is of the ids as sent.
pub(crate) fn parse_subscribe(text: &str, max: usize) -> Result<Vec<StreamId>, Refused> {
    #[derive(Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
    enum Raw {
        Subscribe { stream_ids: Vec<serde_json::Value> },
    }
    let Ok(Raw::Subscribe { stream_ids }) = serde_json::from_str::<Raw>(text) else {
        return Err(Refused::Invalid);
    };
    if stream_ids.len() > max {
        return Err(Refused::TooMany);
    }
    let set: BTreeSet<StreamId> = stream_ids
        .iter()
        .filter_map(|v| v.as_str()?.parse().ok())
        .collect();
    Ok(set.into_iter().collect())
}

/// The read behind a [`ReadReq`], in one snapshot: the token first, then the request.
fn read_fn(
    sha: [u8; 32],
    req: ReadReq,
) -> impl FnOnce(&ReadTx<'_>) -> Result<ReadDone, StoreError> + Send + 'static {
    move |tx| {
        let auth = match verify_hash_in(tx, &sha) {
            Ok(a) => a,
            Err(AuthFailure::Invalid) => return Ok(ReadDone::Revoked),
            Err(AuthFailure::Store(e)) => return Err(e),
        };
        match req {
            ReadReq::Verify => Ok(ReadDone::Verified),
            ReadReq::Subscribe(ids) => {
                // Unreadable and nonexistent streams are dropped, so `S` and every `heads`
                // frame name only streams that exist (a stream never stops existing within
                // one database).
                let scope = ReadScope::of(tx, &auth, Action::ReadStream)?;
                let mut kept = Vec::new();
                for id in ids {
                    if scope.readable_stream(tx, id)? {
                        kept.push(id);
                    }
                }
                Ok(ReadDone::Subscribed(tx.heads(&kept)?))
            }
            ReadReq::Heads(ids) => Ok(ReadDone::Heads(tx.heads(&ids)?)),
        }
    }
}

// ---- the driver ---------------------------------------------------------------------------

type Sink = SplitSink<WebSocket, Message>;
type Sending = Pin<Box<dyn Future<Output = (Sink, Result<(), axum::Error>)> + Send>>;

/// Close codes (RFC 6455 §7.4.1).
pub(crate) mod code {
    pub(crate) const AWAY: u16 = 1001;
    pub(crate) const UNSUPPORTED: u16 = 1003;
    pub(crate) const POLICY: u16 = 1008;
    pub(crate) const TOO_BIG: u16 = 1009;
}

/// The sink: free, or inside the send in flight with its deadline.
enum Out {
    Free(Sink),
    Sending(Sending, tokio::time::Instant),
}

struct InFlightRead {
    handle: tokio::task::JoinHandle<Result<ReadDone, ReadError>>,
    /// On the server's clock: the result is taken only before it.
    deadline: Instant,
    timer: tokio::time::Instant,
}

/// Sends `msgs` as one send; held by the loop until it completes or its deadline passes.
fn send(shared: &Arc<Shared>, mut sink: Sink, msgs: Vec<Message>) -> Sending {
    #[cfg(test)]
    let s = shared.clone();
    #[cfg(not(test))]
    let _ = shared;
    Box::pin(async move {
        #[cfg(test)]
        s.faults.before_socket_send().await;
        let mut r = Ok(());
        for m in msgs {
            r = sink.feed(m).await;
            if r.is_err() {
                break;
            }
        }
        if r.is_ok() {
            r = sink.flush().await;
        }
        (sink, r)
    })
}

/// Sends a close frame best effort within `close_grace`, then reads until the peer's close
/// or the grace ends. A send in flight keeps its own deadline: it gets the earlier of that
/// deadline and the grace to complete, else the socket is dropped without a close frame.
async fn close(
    shared: &Arc<Shared>,
    out: Out,
    stream: &mut SplitStream<WebSocket>,
    code: u16,
    reason: &str,
) {
    #[cfg(test)]
    shared.faults.step("socket close");
    let until = tokio::time::Instant::now() + shared.cfg.ws.close_grace;
    let sink = match out {
        Out::Free(s) => Some(s),
        Out::Sending(f, send_deadline) => match before(until.min(send_deadline), f).await {
            Some((s, Ok(()))) => Some(s),
            _ => None,
        },
    };
    let Some(mut sink) = sink else {
        return;
    };
    let frame = Message::Close(Some(CloseFrame {
        code,
        reason: reason.into(),
    }));
    if !matches!(before(until, sink.send(frame)).await, Some(Ok(()))) {
        return;
    }
    let _ = before(until, async {
        while let Some(Ok(m)) = stream.next().await {
            if matches!(m, Message::Close(_)) {
                break;
            }
        }
    })
    .await;
}

/// `f`'s output if it completes before `deadline`, else `None`. The deadline is checked
/// before every poll of `f`, so a future that became ready after its deadline is dropped
/// unpolled; `timeout_at` polls the future first and would deliver it.
async fn before<F: Future>(deadline: tokio::time::Instant, f: F) -> Option<F::Output> {
    let mut f = pin!(f);
    let mut timer = pin!(sleep_until(deadline));
    std::future::poll_fn(|cx| {
        if tokio::time::Instant::now() >= deadline || timer.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        f.as_mut().poll(cx).map(Some)
    })
    .await
}

/// Whether a socket read error is an inbound message or frame over the size bound:
/// tungstenite's `Capacity` error, found by type in the error's source chain (axum passes
/// it on boxed).
pub(crate) fn too_big(e: &(dyn std::error::Error + 'static)) -> bool {
    in_source_chain(e, |err| {
        matches!(
            err.downcast_ref::<tungstenite::Error>(),
            Some(tungstenite::Error::Capacity(_))
        )
    })
}

async fn pending_opt<T>(f: Option<impl Future<Output = T>>) -> T {
    match f {
        Some(f) => f.await,
        None => std::future::pending().await,
    }
}

/// What woke the loop.
enum Ev {
    Shutdown,
    /// A send, read, or idle deadline passed.
    Dropped,
    Nudge,
    Tick,
    Read(Joined<ReadDone>),
    Sent(Sink, Result<(), axum::Error>),
    Inbound(Option<Result<Message, axum::Error>>),
    Commit(Result<Committed, RecvError>),
}

/// The `GET /api/ws` socket task (design §2.6).
pub(crate) async fn events_socket(s: UpgradedSocket) {
    let UpgradedSocket {
        socket,
        grant,
        mut shutdown,
        shared,
    } = s;
    let wcfg = shared.cfg.ws.clone();
    // The receiver first, then the nudge (marked seen), then the startup read: a commit is
    // in a later snapshot or arrives as a notification; a revocation after the startup read
    // fires the nudge this socket already watches.
    let mut commits = shared.store.subscribe();
    let mut nudge = shared.revocations.subscribe();
    nudge.borrow_and_update();
    let (sink, mut stream) = socket.split();
    let mut out = Out::Free(sink);
    let sha = grant.token_sha256;
    let start_read = |req: ReadReq| {
        let deadline = shared.cfg.clock.now() + shared.cfg.short_deadline;
        InFlightRead {
            handle: spawn_read(&shared, deadline, read_fn(sha, req)),
            deadline,
            timer: shared.timer(deadline),
        }
    };

    // Startup: the token, then `hello`.
    let startup = start_read(ReadReq::Verify);
    let done = tokio::select! {
        biased;
        () = shutdown.wait() => {
            return close(&shared, out, &mut stream, code::AWAY, "shutting_down").await;
        }
        () = sleep_until(startup.timer) => return,
        j = startup.handle => take_read(&shared, startup.deadline, j),
    };
    match done {
        Ok(ReadDone::Verified) => {}
        Ok(_) => return close(&shared, out, &mut stream, code::POLICY, "token_revoked").await,
        Err(_) => return,
    }
    let hello = WsServerMessage::Hello {
        epoch: shared.store.epoch().clone(),
        server_time: SystemClock.now().1,
    };
    let Ok(text) = serde_json::to_string(&hello) else {
        return;
    };
    if let Out::Free(sink) = out {
        out = Out::Sending(
            send(&shared, sink, vec![Message::Text(text.into())]),
            tokio::time::Instant::now() + wcfg.send_timeout,
        );
    }

    let mut st = SocketState::default();
    let mut read: Option<InFlightRead> = None;
    let mut idle_at = tokio::time::Instant::now() + wcfg.idle_timeout;
    let mut next_heads_at = tokio::time::Instant::now() + wcfg.heads_interval;
    loop {
        if read.is_none()
            && let Some(req) = st.next_read()
        {
            read = Some(start_read(req));
        }
        out = match out {
            Out::Free(sink) => match st.next_frame() {
                Some((frame, ping)) => {
                    let Ok(text) = serde_json::to_string(&frame) else {
                        return;
                    };
                    let mut msgs = vec![Message::Text(text.into())];
                    if ping {
                        msgs.push(Message::Ping(Vec::new().into()));
                    }
                    Out::Sending(
                        send(&shared, sink, msgs),
                        tokio::time::Instant::now() + wcfg.send_timeout,
                    )
                }
                None => Out::Free(sink),
            },
            sending @ Out::Sending(..) => sending,
        };
        let (sending, send_deadline) = match &mut out {
            Out::Sending(f, d) => (Some(f), Some(*d)),
            Out::Free(_) => (None, None),
        };
        let read_timer = read.as_ref().map(|r| r.timer);
        let ev = tokio::select! {
            biased;
            () = shutdown.wait() => Ev::Shutdown,
            () = pending_opt(send_deadline.map(sleep_until)) => Ev::Dropped,
            () = pending_opt(read_timer.map(sleep_until)) => Ev::Dropped,
            () = sleep_until(idle_at) => Ev::Dropped,
            _ = nudge.changed() => Ev::Nudge,
            () = sleep_until(next_heads_at) => Ev::Tick,
            j = pending_opt(read.as_mut().map(|r| &mut r.handle)) => Ev::Read(j),
            sent = pending_opt(sending.zip(send_deadline).map(|(f, d)| before(d, f))) => match sent {
                Some((sink, r)) => Ev::Sent(sink, r),
                None => Ev::Dropped,
            },
            m = stream.next() => Ev::Inbound(m),
            n = commits.recv() => Ev::Commit(n),
        };
        let refusal = match ev {
            Ev::Shutdown | Ev::Commit(Err(RecvError::Closed)) => (code::AWAY, "shutting_down"),
            Ev::Dropped => return,
            Ev::Nudge => {
                st.on_nudge();
                continue;
            }
            Ev::Tick => {
                next_heads_at += wcfg.heads_interval;
                st.on_tick();
                continue;
            }
            Ev::Read(j) => {
                let Some(r) = read.take() else { return };
                // A read error or a late result drops the socket.
                let Ok(done) = take_read(&shared, r.deadline, j) else {
                    return;
                };
                if st.read_done(done) {
                    continue;
                }
                (code::POLICY, "token_revoked")
            }
            Ev::Sent(sink, r) => {
                if r.is_err() {
                    return;
                }
                out = Out::Free(sink);
                continue;
            }
            Ev::Inbound(m) => {
                idle_at = tokio::time::Instant::now() + wcfg.idle_timeout;
                match m {
                    Some(Ok(Message::Text(t))) => {
                        match parse_subscribe(t.as_str(), wcfg.max_subscriptions) {
                            Ok(ids) => {
                                st.on_subscribe(ids);
                                continue;
                            }
                            Err(Refused::TooMany) => (code::POLICY, "too_many_subscriptions"),
                            Err(Refused::Invalid) => (code::UNSUPPORTED, "invalid_message"),
                        }
                    }
                    Some(Ok(Message::Binary(_))) => (code::UNSUPPORTED, "binary_message"),
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Err(e)) if too_big(&e) => (code::TOO_BIG, "message_too_big"),
                    Some(Err(_)) => return,
                }
            }
            Ev::Commit(Ok(c)) => {
                #[cfg(test)]
                if shared.faults.notifications_dropped() {
                    continue;
                }
                st.on_commit(&c.heads);
                continue;
            }
            Ev::Commit(Err(RecvError::Lagged(_))) => {
                st.on_lag();
                continue;
            }
        };
        return close(&shared, out, &mut stream, refusal.0, refusal.1).await;
    }
}
