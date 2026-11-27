//! One attach socket (`docs/design/t1-attach.md` §2-§4): startup, the takeover, the loop of
//! output, input units, resizes, verification, and the terminal drain.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::ws::Message;
use chaldea_core::adapter::HostOs;
use chaldea_core::attach::{
    AttachClientMessage, AttachModes, AttachReason, AttachServerMessage, EndReason, PasteOutcome,
    PasteReason,
};
use chaldea_core::ids::{Agent, LaunchId, ThreadId};
use chaldea_core::input::MAX_PROMPT_BYTES;
use chaldea_core::input::encode::{Purpose, check, encode};
use chaldea_core::payload::{InputEncoding, VtCursor};
use chaldea_pty_runtime::{
    Fence, Guard, PtySize, ScreenMeta, SessionHandle, SubscribeError, Subscribed, WriteOrigin,
    WriteTicket, WriteUnit, grid_rows,
};
use futures_util::StreamExt;
use tokio::sync::oneshot;

use super::input::{Item, Pending, Verdict, advance_tail, filter};
use super::output::{OutBuf, Taken};
use super::registry::{Candidate, LaunchAttach, Removed, Replaced};
use super::{Attach, AttachConfig, paste_reason, refusal_reason, submit_reason};
use crate::http::ws::{
    Out, Sink, bad_utf8, before, close, code, pending_opt, send, socket_timer, too_big,
};
use crate::http::{
    AuthFailure, Joined, TicketPurpose, UpgradedSocket, spawn_read, take_read, verify_hash_in,
};
use crate::store::{ReadTx, StoreError};
use crate::supervisor::Supervisor;
use crate::supervisor::live::{LiveLaunch, SessionError};
use crate::supervisor::send::{KeyReport, LeaseKind, READ_WAIT, SendLaunch};

/// Close codes of the attach socket (§2 "Close codes").
mod close_code {
    pub(crate) const NO_LIVE_LAUNCH: u16 = 4000;
    pub(crate) const LAUNCH_ENDED: u16 = 4001;
    pub(crate) const LAGGED: u16 = 4002;
    pub(crate) const REPLACED: u16 = 4003;
    pub(crate) const BUSY: u16 = 4004;
}

/// The startup read's answer.
enum Startup {
    Revoked,
    NoLive,
    Live(LaunchId),
}

/// The startup read (R-11): the token, the open thread, and its current launch without an
/// exit, in one snapshot.
fn startup_read(
    sha: [u8; 32],
    thread: ThreadId,
) -> impl FnOnce(&ReadTx<'_>) -> Result<Startup, StoreError> + Send + 'static {
    move |tx| {
        match verify_hash_in(tx, &sha) {
            Ok(_) => {}
            Err(AuthFailure::Invalid) => return Ok(Startup::Revoked),
            Err(AuthFailure::Store(e)) => return Err(e),
        }
        let Some(t) = tx.thread(thread)? else {
            return Ok(Startup::NoLive);
        };
        let Some(l) = t.current_launch_id.filter(|_| !t.closed) else {
            return Ok(Startup::NoLive);
        };
        match tx.launch(l)? {
            Some(row) if row.exit_reason.is_none() => Ok(Startup::Live(l)),
            _ => Ok(Startup::NoLive),
        }
    }
}

/// A verification read: whether the token is still valid.
fn verify_read(
    sha: [u8; 32],
) -> impl FnOnce(&ReadTx<'_>) -> Result<bool, StoreError> + Send + 'static {
    move |tx| match verify_hash_in(tx, &sha) {
        Ok(_) => Ok(true),
        Err(AuthFailure::Invalid) => Ok(false),
        Err(AuthFailure::Store(e)) => Err(e),
    }
}

/// The `attached` frame of `s`, its grid kept only while the whole frame fits `max`.
fn attached(thread: ThreadId, launch: LaunchId, s: &Subscribed, max: usize) -> String {
    let frame =
        |grid| serde_json::to_string(&envelope(thread, launch, &s.meta, grid)).unwrap_or_default();
    if let Some(lines) = &s.lines {
        let text = frame(Some(grid_rows(lines)));
        if text.len() <= max {
            return text;
        }
    }
    frame(None)
}

fn envelope(
    thread: ThreadId,
    launch: LaunchId,
    m: &ScreenMeta,
    grid: Option<Vec<Vec<String>>>,
) -> AttachServerMessage {
    AttachServerMessage::Attached {
        thread_id: thread,
        launch_id: launch,
        cols: m.cols,
        rows: m.rows,
        cursor: VtCursor {
            row: m.cursor.row,
            col: m.cursor.col,
            visible: m.cursor.visible,
        },
        modes: AttachModes {
            bracketed_paste: m.modes.bracketed_paste,
            alt_screen: m.modes.alt_screen,
            show_cursor: m.modes.show_cursor,
            app_cursor: m.modes.app_cursor,
            app_keypad: m.modes.app_keypad,
            kitty_flags: m.modes.kitty_flags,
        },
        grid,
    }
}

/// The grid budget for `meta`: what the frame may hold besides its envelope (whose empty
/// grid `[]` the grid replaces).
fn grid_budget(thread: ThreadId, launch: LaunchId, meta: &ScreenMeta, max: usize) -> usize {
    let empty = serde_json::to_string(&envelope(thread, launch, meta, Some(Vec::new())))
        .map_or(usize::MAX, |t| t.len());
    (max + 2).saturating_sub(empty)
}

/// Removes a subscription on the blocking pool; never awaited inline.
fn unsubscribe(sup: &Supervisor, removed: Option<Removed>) {
    if let Some((h, id)) = removed {
        let task = sup.rt.spawn_blocking(move || h.unsubscribe(id));
        drop(task);
    }
}

/// The socket's cleanup on every exit path: its grant revoked, its entry cleared, its
/// subscription removed on the blocking pool.
struct Exit {
    sup: Arc<Supervisor>,
    a: Arc<Attach>,
    la: Arc<LaunchAttach>,
    buf: Arc<OutBuf>,
    socket: u64,
    thread: ThreadId,
    launch: LaunchId,
}

impl Drop for Exit {
    fn drop(&mut self) {
        self.buf.stop();
        unsubscribe(&self.sup, self.la.end(self.socket));
        let live = self.sup.live(self.thread, self.launch).is_some();
        self.a.reg.forget_if_idle(self.launch, live);
    }
}

/// What a unit carries.
#[derive(Debug, Clone)]
enum Unit {
    Raw(usize),
    Paste(String),
}

/// A unit's submission, inside `with_session`.
type Submitted = Result<(WriteTicket, oneshot::Receiver<KeyReport>), Refused>;

/// Why a unit was not queued.
#[derive(Debug, Clone, Copy)]
enum Refused {
    /// Not written: the reason.
    NotWritten(AttachReason),
    /// A paste refused before it was queued (2004 off at encoding).
    Rejected(AttachReason),
}

/// The outstanding unit's phase.
enum Phase {
    Submitting(Pin<Box<dyn Future<Output = Submitted> + Send>>),
    /// Waiting for the report until `by` (supervisor clock).
    Waiting {
        rx: oneshot::Receiver<KeyReport>,
        ticket: WriteTicket,
        by: Instant,
    },
    /// `pending` was reported: nothing more is submitted until the report arrives.
    Stuck(Option<oneshot::Receiver<KeyReport>>),
}

struct Outstanding {
    unit: Unit,
    phase: Phase,
}

/// A verification read in flight.
struct Verifying {
    handle: tokio::task::JoinHandle<Result<bool, crate::store::ReadError>>,
    deadline: Instant,
    timer: tokio::time::Instant,
    /// The nudge count when it started: an older read does not answer a newer nudge.
    generation: u64,
}

/// A resize's answer: the PTY's size after it and whether every leg succeeded.
type ResizeOut = (u16, u16, bool);

/// A resize in flight.
type Resizing = Pin<Box<dyn Future<Output = Result<ResizeOut, tokio::task::JoinError>> + Send>>;

/// The latched terminal event (R-8).
#[derive(Debug, Clone, Copy)]
enum Ending {
    Ended(EndReason),
    Lagged,
}

/// What woke the loop.
enum Ev {
    Shutdown,
    Replaced,
    Dropped,
    Nudge,
    Tick,
    Verified(usize, Joined<bool>),
    Sent(Sink, Result<(), axum::Error>),
    Inbound(Option<Result<Message, axum::Error>>),
    Output,
    GonePoll,
    HoldEnd,
    Submitted(Submitted),
    Report(Result<KeyReport, oneshot::error::RecvError>),
    ReportLate,
    Resized(Result<ResizeOut, tokio::task::JoinError>),
}

/// The pieces a unit's submission needs.
struct Writer {
    sup: Arc<Supervisor>,
    entry: Arc<LiveLaunch>,
    sl: Arc<SendLaunch>,
    fence: Fence,
    cfg: AttachConfig,
    agent: Agent,
    host_os: HostOs,
    paste_on_2004: bool,
}

impl Writer {
    /// Submits `bytes` as one attach unit (R-4): its guard carries the socket's grant, a
    /// 30 s dispatch deadline, and for a paste on a bracketed-paste row the mode epoch.
    fn submit(
        &self,
        bytes: Vec<u8>,
        paste: bool,
    ) -> Pin<Box<dyn Future<Output = Submitted> + Send>> {
        let (sup, entry, sl, fence) = (
            self.sup.clone(),
            self.entry.clone(),
            self.sl.clone(),
            self.fence.clone(),
        );
        let wait = self.cfg.unit_wait;
        let needs_2004 = paste && self.paste_on_2004;
        Box::pin(async move {
            let now = sup.now();
            let not_after = sup.real(now + wait);
            let (tx, rx) = oneshot::channel();
            let r = entry
                .with_session(&sup, now + READ_WAIT, move |s| {
                    let modes = s.modes();
                    if needs_2004 && !modes.modes.bracketed_paste {
                        return Err(Refused::Rejected(AttachReason::BracketedPasteOff));
                    }
                    let guard = Guard {
                        not_after,
                        basis: None,
                        bracketed_paste: needs_2004.then_some(modes.bracketed_paste_epoch),
                        recheck: None,
                        fence: Some(fence),
                    };
                    sl.submit_human(
                        s,
                        WriteUnit::bytes(WriteOrigin::Attach, bytes),
                        guard,
                        Some(tx),
                    )
                    .map_err(|e| Refused::NotWritten(submit_reason(e)))
                })
                .await;
            match r {
                Ok(Ok(ticket)) => Ok((ticket, rx)),
                Ok(Err(e)) => Err(e),
                Err(SessionError::Busy) => Err(Refused::NotWritten(AttachReason::Busy)),
                Err(SessionError::Gone) => Err(Refused::NotWritten(AttachReason::NotWritten)),
            }
        })
    }

    /// The bytes of a paste: the encoder's `AttachPaste` text.
    fn paste_bytes(&self, text: &str) -> Result<Vec<u8>, PasteReason> {
        encode(self.agent, self.host_os, text, Purpose::AttachPaste)
            .map(|e| e.text)
            .map_err(|r| PasteReason::Rejected(r.reason))
    }
}

/// Runs one resize attempt under the launch lease (D7): waits for the lease, then on the
/// blocking pool each leg under the socket's fence; the blocking task releases the lease
/// by its id when the backend call returns, even after the socket ended.
fn resize(
    sl: Arc<SendLaunch>,
    handle: SessionHandle,
    fence: Fence,
    socket: u64,
    legs: Vec<PtySize>,
    newer: Arc<Mutex<Option<PtySize>>>,
) -> Resizing {
    Box::pin(async move {
        let id = loop {
            let mut free = pin!(sl.lease_free.notified());
            free.as_mut().enable();
            if let Some(id) = sl.take_lease(&mut sl.state(), LeaseKind::Resize(socket)) {
                break id;
            }
            free.await;
        };
        tokio::task::spawn_blocking(move || {
            let mut ok = true;
            for (i, leg) in legs.iter().enumerate() {
                // A newer size replaces the nudge's second leg.
                if i > 0 && crate::supervisor::lock(&newer).is_some() {
                    break;
                }
                if handle.resize(*leg, Some(&fence)).is_err() {
                    ok = false;
                    break;
                }
            }
            sl.release_lease(&mut sl.state(), id);
            let s = handle.size();
            (s.cols(), s.rows(), ok)
        })
        .await
    })
}

/// The two legs that force a redraw at an equal size: one row more (one less at the
/// largest) and back.
fn nudge_legs(size: PtySize) -> Vec<PtySize> {
    let rows = if size.rows() >= chaldea_core::attach::MAX_ROWS {
        size.rows() - 1
    } else {
        size.rows() + 1
    };
    match PtySize::new(size.cols(), rows) {
        Ok(n) => vec![n, size],
        Err(_) => vec![size],
    }
}

fn text(m: &AttachServerMessage) -> Option<Message> {
    serde_json::to_string(m)
        .ok()
        .map(|t| Message::Text(t.into()))
}

/// The attach socket task.
#[allow(
    clippy::too_many_lines,
    reason = "one loop over every source, as events_socket"
)]
pub(crate) async fn run(a: Arc<Attach>, s: UpgradedSocket) {
    let UpgradedSocket {
        socket,
        grant,
        mut shutdown,
        shared,
    } = s;
    let TicketPurpose::Attach(thread) = grant.purpose else {
        return;
    };
    let cfg = a.cfg.clone();
    let wcfg = shared.cfg.ws.clone();
    // The nudge marked seen before the startup read: a revocation after the read fires it.
    let mut nudge = shared.revocations.subscribe();
    nudge.borrow_and_update();
    let (sink, mut stream) = socket.split();
    let out = Out::Free(sink);
    let sha = grant.token_sha256;
    let Some(sup) = a.sup.upgrade() else {
        return close(&shared, out, &mut stream, code::AWAY, "shutting_down").await;
    };

    // Startup: the token, the thread, its live launch.
    let deadline = shared.cfg.clock.now() + shared.cfg.short_deadline;
    let startup = spawn_read(&shared, deadline, startup_read(sha, thread));
    let done = tokio::select! {
        biased;
        () = shutdown.wait() => {
            return close(&shared, out, &mut stream, code::AWAY, "shutting_down").await;
        }
        () = socket_timer(shared.timer(deadline)) => return,
        j = startup => take_read(&shared, deadline, j),
    };
    let launch = match done {
        Ok(Startup::Live(l)) => l,
        Ok(Startup::Revoked) => {
            return close(&shared, out, &mut stream, code::POLICY, "token_revoked").await;
        }
        Ok(Startup::NoLive) => {
            return close(
                &shared,
                out,
                &mut stream,
                close_code::NO_LIVE_LAUNCH,
                "no_live_launch",
            )
            .await;
        }
        Err(_) => return,
    };
    let no_live = close_code::NO_LIVE_LAUNCH;
    let (Some(entry), Some(sl)) = (sup.live(thread, launch), sup.send.launch(launch)) else {
        return close(&shared, out, &mut stream, no_live, "no_live_launch").await;
    };
    // The handle under a short `with_session`; the subscription runs on the blocking pool.
    let handle = match entry
        .with_session(
            &sup,
            sup.now() + cfg.subscribe_wait,
            chaldea_pty_runtime::Session::handle,
        )
        .await
    {
        Ok(h) => h,
        Err(SessionError::Gone) => {
            return close(&shared, out, &mut stream, no_live, "no_live_launch").await;
        }
        Err(SessionError::Busy) => {
            return close(&shared, out, &mut stream, close_code::BUSY, "busy").await;
        }
    };
    let la = a.reg.launch(launch);
    let me = a.reg.socket_id();
    let buf = OutBuf::new(cfg.out_max);
    let replaced = Arc::new(Replaced::default());
    let _exit = Exit {
        sup: sup.clone(),
        a: a.clone(),
        la: la.clone(),
        buf: buf.clone(),
        socket: me,
        thread,
        launch,
    };
    let sub_deadline = sup.real(sup.now() + cfg.subscribe_wait);
    let (h2, sink2, max) = (handle.clone(), buf.sink(), cfg.attached_max);
    let mut subscribing = sup.rt.spawn_blocking(move || {
        h2.subscribe(sink2, sub_deadline, |m| grid_budget(thread, launch, m, max))
    });
    let joined = tokio::select! {
        biased;
        () = shutdown.wait() => None,
        j = &mut subscribing => Some(j),
    };
    let Some(joined) = joined else {
        // The task may still register: its sink is stopped, and its subscription removed.
        buf.stop();
        let h = handle.clone();
        sup.rt.spawn(async move {
            if let Ok(Ok(s)) = subscribing.await {
                let _ = tokio::task::spawn_blocking(move || h.unsubscribe(s.id)).await;
            }
        });
        return close(&shared, out, &mut stream, code::AWAY, "shutting_down").await;
    };
    let subscribed = match joined {
        Ok(Ok(s)) => s,
        Ok(Err(SubscribeError::Closed)) => {
            return close(&shared, out, &mut stream, no_live, "no_live_launch").await;
        }
        Ok(Err(SubscribeError::VtUnavailable | SubscribeError::TooManySubscribers)) | Err(_) => {
            return close(&shared, out, &mut stream, close_code::BUSY, "busy").await;
        }
    };
    let first = attached(thread, launch, &subscribed, cfg.attached_max);
    // The takeover: the candidate subscribed, so it becomes current.
    let (granted, removed) = la.swap(Candidate {
        socket: me,
        sub: subscribed.id,
        active: buf.active.clone(),
        handle: handle.clone(),
        replaced: replaced.clone(),
    });
    unsubscribe(&sup, removed);
    let fence = la.fence(granted);
    let writer = Writer {
        sup: sup.clone(),
        entry: entry.clone(),
        sl: sl.clone(),
        fence: fence.clone(),
        cfg: cfg.clone(),
        agent: entry.agent,
        host_os: sup.host_os,
        paste_on_2004: entry.caps.pty_encoding() == InputEncoding::BracketedPaste,
    };

    let mut out = match out {
        Out::Free(sink) => Out::Sending(
            send(&shared, sink, vec![Message::Text(first.into())]),
            tokio::time::Instant::now() + wcfg.send_timeout,
        ),
        o @ Out::Sending(..) => o,
    };
    let mut ctl: VecDeque<AttachServerMessage> = VecDeque::new();
    let mut pending = Pending::default();
    let mut held: Vec<u8> = Vec::new();
    let mut hold_until: Option<tokio::time::Instant> = None;
    let mut outstanding: Option<Outstanding> = None;
    let size_wanted: Arc<Mutex<Option<PtySize>>> = Arc::new(Mutex::new(None));
    let mut resizing: Option<Resizing> = None;
    let mut resized_once = false;
    let mut verifying: [Option<Verifying>; 2] = [None, None];
    let mut generation: u64 = 0;
    let mut verified_generation: u64 = 0;
    // Set when a nudge found both read slots taken: the socket drops at this instant unless
    // a read started after the nudge answers first.
    let mut verify_by: Option<tokio::time::Instant> = None;
    let mut verify_due = false;
    let mut ending: Option<(Ending, tokio::time::Instant)> = None;
    let mut ended_sent = false;
    let mut ping_due = false;
    let mut idle_at: Option<tokio::time::Instant> = None;
    let mut next_tick = tokio::time::Instant::now() + wcfg.heads_interval;
    let mut next_gone = sup.timer(sup.now() + cfg.gone_poll);

    loop {
        // Latch a terminal event (the first wins): the sink stops, the grant is revoked,
        // and one deadline covers the rest.
        if ending.is_none() {
            let latched = if buf.lagged() {
                Some(Ending::Lagged)
            } else {
                buf.end().map(Ending::Ended)
            };
            if let Some(e) = latched {
                buf.stop();
                la.revoke(me);
                ending = Some((e, tokio::time::Instant::now() + cfg.drain));
            }
        }
        let drain_at = ending.map(|(_, at)| at);
        // A fresh verification on a nudge or a tick, even with one read in flight.
        if verify_due && let Some(slot) = verifying.iter_mut().find(|v| v.is_none()) {
            verify_due = false;
            let deadline = shared.cfg.clock.now() + shared.cfg.short_deadline;
            *slot = Some(Verifying {
                handle: spawn_read(&shared, deadline, verify_read(sha)),
                deadline,
                timer: shared.timer(deadline),
                generation,
            });
        } else if verify_due && verify_by.is_none() {
            verify_by = verifying.iter().flatten().map(|v| v.timer).min();
        }
        // The next unit, when none is outstanding (D6).
        if outstanding.is_none() && ending.is_none() {
            outstanding = match pending.next() {
                Some(Item::Raw(b)) => Some(Outstanding {
                    unit: Unit::Raw(b.len()),
                    phase: Phase::Submitting(writer.submit(b, false)),
                }),
                Some(Item::Paste { id, text }) => match writer.paste_bytes(&text) {
                    Ok(b) => Some(Outstanding {
                        unit: Unit::Paste(id),
                        phase: Phase::Submitting(writer.submit(b, true)),
                    }),
                    Err(reason) => {
                        ctl.push_back(AttachServerMessage::PasteResult {
                            paste_id: id,
                            outcome: PasteOutcome::Rejected,
                            bytes_written: Some(0),
                            reason: Some(reason),
                        });
                        continue;
                    }
                },
                None => None,
            };
        }
        // The next resize: one in flight, the latest size wins.
        if resizing.is_none() && ending.is_none() {
            let want = crate::supervisor::lock(&size_wanted).take();
            if let Some(size) = want {
                let current = handle.size();
                if size == current && resized_once {
                    ctl.push_back(AttachServerMessage::Size {
                        cols: size.cols(),
                        rows: size.rows(),
                        ok: true,
                    });
                } else {
                    let legs = if size == current {
                        nudge_legs(size)
                    } else {
                        vec![size]
                    };
                    resized_once = true;
                    resizing = Some(resize(
                        sl.clone(),
                        handle.clone(),
                        fence.clone(),
                        me,
                        legs,
                        size_wanted.clone(),
                    ));
                }
            }
        }
        // The next frame, when the sink is free.
        out = match out {
            Out::Free(sink) => {
                let mut msgs = Vec::new();
                if let Some(m) = ctl.pop_front().filter(|_| ending.is_none()) {
                    msgs.extend(text(&m));
                } else if let Taken::Bytes(b) = buf.take(cfg.frame_out) {
                    msgs.push(Message::Binary(b.into()));
                } else if let Some((e, _)) = ending {
                    match e {
                        Ending::Lagged => {
                            return close(
                                &shared,
                                Out::Free(sink),
                                &mut stream,
                                close_code::LAGGED,
                                "lagged",
                            )
                            .await;
                        }
                        Ending::Ended(reason) if ended_sent => {
                            let _ = reason;
                            return close(
                                &shared,
                                Out::Free(sink),
                                &mut stream,
                                close_code::LAUNCH_ENDED,
                                "launch_ended",
                            )
                            .await;
                        }
                        Ending::Ended(reason) => {
                            ended_sent = true;
                            msgs.extend(text(&AttachServerMessage::Ended {
                                launch_id: launch,
                                reason,
                            }));
                        }
                    }
                }
                if ping_due && (ending.is_none() || !msgs.is_empty()) {
                    ping_due = false;
                    msgs.push(Message::Ping(Vec::new().into()));
                }
                if msgs.is_empty() {
                    Out::Free(sink)
                } else {
                    let mut at = tokio::time::Instant::now() + wcfg.send_timeout;
                    if let Some(d) = drain_at {
                        at = at.min(d);
                    }
                    Out::Sending(send(&shared, sink, msgs), at)
                }
            }
            sending @ Out::Sending(..) => sending,
        };
        let (sending, send_deadline) = match &mut out {
            Out::Sending(f, d) => (Some(f), Some(drain_at.map_or(*d, |e| e.min(*d)))),
            Out::Free(_) => (None, None),
        };
        // Reads pause at the watermark (pongs too); never during the drain.
        let reading = pending.bytes() < cfg.watermark;
        let report_by = match &outstanding {
            Some(Outstanding {
                phase: Phase::Waiting { by, .. },
                ..
            }) => Some(sup.timer(*by)),
            _ => None,
        };
        let (submitting, report_rx) = match &mut outstanding {
            Some(Outstanding {
                phase: Phase::Submitting(f),
                ..
            }) => (Some(f), None),
            Some(Outstanding {
                phase: Phase::Waiting { rx, .. } | Phase::Stuck(Some(rx)),
                ..
            }) => (None, Some(rx)),
            _ => (None, None),
        };
        let [v0, v1] = &mut verifying;
        let verify_timer = verifying_min(v0.as_ref(), v1.as_ref());
        let ev = tokio::select! {
            biased;
            () = shutdown.wait() => Ev::Shutdown,
            () = pending_opt(verify_by.map(socket_timer)) => Ev::Dropped,
            () = pending_opt(verify_timer.map(socket_timer)) => Ev::Dropped,
            j = pending_opt(v0.as_mut().map(|v| &mut v.handle)) => Ev::Verified(0, j),
            j = pending_opt(v1.as_mut().map(|v| &mut v.handle)) => Ev::Verified(1, j),
            _ = nudge.changed() => Ev::Nudge,
            () = replaced.wait() => Ev::Replaced,
            () = pending_opt(send_deadline.map(socket_timer)) => Ev::Dropped,
            () = pending_opt(idle_at.map(socket_timer)) => Ev::Dropped,
            () = socket_timer(next_tick) => Ev::Tick,
            sent = pending_opt(sending.zip(send_deadline).map(|(f, d)| before(d, f))) => match sent {
                Some((sink, r)) => Ev::Sent(sink, r),
                None => Ev::Dropped,
            },
            () = socket_timer(next_gone) => Ev::GonePoll,
            () = buf.wake.notified(), if ending.is_none() => Ev::Output,
            r = pending_opt(submitting) => Ev::Submitted(r),
            r = pending_opt(report_rx) => Ev::Report(r),
            () = pending_opt(report_by.map(socket_timer)) => Ev::ReportLate,
            r = pending_opt(resizing.as_mut()) => Ev::Resized(r),
            () = pending_opt(hold_until.map(socket_timer)) => Ev::HoldEnd,
            m = stream.next(), if reading => Ev::Inbound(m),
        };
        let refusal: (u16, &str) = match ev {
            Ev::Shutdown => (code::AWAY, "shutting_down"),
            Ev::Dropped => return,
            Ev::Replaced => (close_code::REPLACED, "replaced"),
            Ev::Nudge => {
                generation += 1;
                verify_due = true;
                continue;
            }
            Ev::Tick => {
                next_tick += wcfg.heads_interval;
                generation += 1;
                verify_due = true;
                ping_due = true;
                continue;
            }
            Ev::Verified(i, j) => {
                let Some(v) = verifying[i].take() else { return };
                match take_read(&shared, v.deadline, j) {
                    Ok(true) => {
                        verified_generation = verified_generation.max(v.generation);
                        if verified_generation >= generation {
                            verify_by = None;
                        }
                        continue;
                    }
                    Ok(false) => {
                        la.revoke(me);
                        (code::POLICY, "token_revoked")
                    }
                    Err(_) => return,
                }
            }
            Ev::Sent(sink, r) => {
                if r.is_err() {
                    return;
                }
                if idle_at.is_none() {
                    idle_at = Some(tokio::time::Instant::now() + wcfg.idle_timeout);
                }
                out = Out::Free(sink);
                continue;
            }
            Ev::Output => continue,
            Ev::GonePoll => {
                next_gone = sup.timer(sup.now() + cfg.gone_poll);
                if sup.live(thread, launch).is_none() && ending.is_none() {
                    buf.stop();
                    la.revoke(me);
                    ending = Some((
                        Ending::Ended(EndReason::Gone),
                        tokio::time::Instant::now() + cfg.drain,
                    ));
                }
                continue;
            }
            Ev::HoldEnd => {
                hold_until = None;
                let h = std::mem::take(&mut held);
                advance_tail(&mut crate::supervisor::lock(&la.tail), &h);
                pending.raw(&h);
                continue;
            }
            Ev::Submitted(r) => {
                let Some(o) = outstanding.take() else {
                    continue;
                };
                match r {
                    Ok((ticket, rx)) => {
                        let by = sup.now() + cfg.unit_wait + READ_WAIT;
                        outstanding = Some(Outstanding {
                            unit: o.unit,
                            phase: Phase::Waiting { rx, ticket, by },
                        });
                    }
                    Err(e) => ctl.push_back(refused_frame(&o.unit, e)),
                }
                continue;
            }
            Ev::Report(r) => {
                let Some(o) = outstanding.take() else {
                    continue;
                };
                match (o.phase, r) {
                    // A late report after `pending` settles the unit silently.
                    (Phase::Stuck(_), Ok(_)) => {}
                    // The channel is gone after `pending`: no settlement can come.
                    (Phase::Stuck(_), Err(_)) => {
                        outstanding = Some(Outstanding {
                            unit: o.unit,
                            phase: Phase::Stuck(None),
                        });
                    }
                    (_, Ok(report)) => ctl.extend(report_frame(&o.unit, &report)),
                    (Phase::Waiting { ticket, .. }, Err(_)) => {
                        outstanding = settle_lost(o.unit, &ticket, &mut ctl);
                    }
                    (Phase::Submitting(_), Err(_)) => {}
                }
                continue;
            }
            Ev::ReportLate => {
                let Some(o) = outstanding.take() else {
                    continue;
                };
                if sup.now() < report_by_of(&o).unwrap_or_else(|| sup.now()) {
                    outstanding = Some(o);
                    continue;
                }
                if let Phase::Waiting { ticket, rx, .. } = o.phase {
                    outstanding = settle_late(o.unit, &ticket, rx, &mut ctl);
                }
                continue;
            }
            Ev::Resized(r) => {
                resizing = None;
                let (cols, rows, ok) = match r {
                    Ok(v) => v,
                    Err(_) => {
                        let s = handle.size();
                        (s.cols(), s.rows(), false)
                    }
                };
                ctl.push_back(AttachServerMessage::Size { cols, rows, ok });
                continue;
            }
            Ev::Inbound(m) => {
                if idle_at.is_some() {
                    idle_at = Some(tokio::time::Instant::now() + wcfg.idle_timeout);
                }
                match m {
                    Some(Ok(Message::Binary(b))) => {
                        if ending.is_some() || b.is_empty() {
                            continue;
                        }
                        if b.len() > cfg.raw_max {
                            ctl.push_back(AttachServerMessage::InputDropped {
                                bytes: b.len() as u64,
                                reason: AttachReason::TooLarge,
                            });
                            continue;
                        }
                        let mut tail = crate::supervisor::lock(&la.tail);
                        match filter(&tail, &held, &b) {
                            Verdict::Refused => {
                                drop(tail);
                                ctl.push_back(AttachServerMessage::InputDropped {
                                    bytes: b.len() as u64,
                                    reason: AttachReason::PasteMarker,
                                });
                            }
                            Verdict::Accepted { write, held: h } => {
                                advance_tail(&mut tail, &write);
                                drop(tail);
                                pending.raw(&write);
                                held = h;
                                hold_until = (!held.is_empty())
                                    .then(|| tokio::time::Instant::now() + cfg.hold);
                            }
                        }
                        continue;
                    }
                    Some(Ok(Message::Text(t))) => match AttachClientMessage::parse(t.as_str()) {
                        Some(AttachClientMessage::Resize { cols, rows }) => {
                            if let Ok(size) = PtySize::new(cols, rows) {
                                *crate::supervisor::lock(&size_wanted) = Some(size);
                            }
                            continue;
                        }
                        Some(AttachClientMessage::Paste { paste_id, text }) => {
                            if ending.is_some() {
                                continue;
                            }
                            if let Some(reason) =
                                paste_refusal(&writer, entry.caps.pty_encoding(), &text)
                            {
                                ctl.push_back(AttachServerMessage::PasteResult {
                                    paste_id,
                                    outcome: PasteOutcome::Rejected,
                                    bytes_written: Some(0),
                                    reason: Some(reason),
                                });
                            } else {
                                // The held partial marker goes first: input stays in order.
                                if !held.is_empty() {
                                    let h = std::mem::take(&mut held);
                                    hold_until = None;
                                    advance_tail(&mut crate::supervisor::lock(&la.tail), &h);
                                    pending.raw(&h);
                                }
                                pending.paste(paste_id, text);
                            }
                            continue;
                        }
                        None => (code::UNSUPPORTED, "invalid_message"),
                    },
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Err(e)) if too_big(&e) => (code::TOO_BIG, "message_too_big"),
                    Some(Err(e)) if bad_utf8(&e) => (code::INVALID_DATA, "invalid_utf8"),
                    Some(Err(_)) => return,
                }
            }
        };
        la.revoke(me);
        return close(&shared, out, &mut stream, refusal.0, refusal.1).await;
    }
}

fn verifying_min(a: Option<&Verifying>, b: Option<&Verifying>) -> Option<tokio::time::Instant> {
    [a, b].into_iter().flatten().map(|v| v.timer).min()
}

fn report_by_of(o: &Outstanding) -> Option<Instant> {
    match &o.phase {
        Phase::Waiting { by, .. } => Some(*by),
        _ => None,
    }
}

/// A paste refused at receipt: too large, or the encoder's check.
fn paste_refusal(w: &Writer, encoding: InputEncoding, text: &str) -> Option<PasteReason> {
    if text.len() > MAX_PROMPT_BYTES {
        return Some(paste_reason(AttachReason::PromptTooLarge));
    }
    check(w.agent, encoding, Purpose::AttachPaste, text)
        .err()
        .map(|r| PasteReason::Rejected(r.reason))
}

/// The frame of a unit that was not queued.
fn refused_frame(unit: &Unit, e: Refused) -> AttachServerMessage {
    match (unit, e) {
        (Unit::Paste(id), Refused::Rejected(r)) => AttachServerMessage::PasteResult {
            paste_id: id.clone(),
            outcome: PasteOutcome::Rejected,
            bytes_written: Some(0),
            reason: Some(paste_reason(r)),
        },
        (Unit::Paste(id), Refused::NotWritten(r)) => AttachServerMessage::PasteResult {
            paste_id: id.clone(),
            outcome: PasteOutcome::NotWritten,
            bytes_written: Some(0),
            reason: Some(paste_reason(r)),
        },
        (Unit::Raw(n), Refused::NotWritten(r) | Refused::Rejected(r)) => {
            AttachServerMessage::InputDropped {
                bytes: *n as u64,
                reason: r,
            }
        }
    }
}

/// The frame of a unit's report: a paste always answers; raw input only when not every
/// byte was written.
fn report_frame(unit: &Unit, r: &KeyReport) -> Option<AttachServerMessage> {
    let (outcome, written, reason) = match r {
        KeyReport::Recorded {
            complete: true,
            bytes,
        } => (PasteOutcome::Written, *bytes, None),
        KeyReport::Recorded {
            complete: false,
            bytes,
        } => (
            PasteOutcome::Incomplete,
            *bytes,
            Some(AttachReason::Incomplete),
        ),
        KeyReport::NotStarted(refusal) => {
            (PasteOutcome::NotWritten, 0, Some(refusal_reason(refusal)))
        }
    };
    match unit {
        Unit::Paste(id) => Some(AttachServerMessage::PasteResult {
            paste_id: id.clone(),
            outcome,
            bytes_written: Some(written),
            reason: reason.map(paste_reason),
        }),
        Unit::Raw(n) => reason.map(|reason| AttachServerMessage::InputDropped {
            bytes: (*n as u64).saturating_sub(written),
            reason,
        }),
    }
}

/// The frame of a unit with no settlement: `not_written` when the cancel took it, else
/// `pending` (whole unit).
fn pending_frame(unit: &Unit, cancelled: bool) -> AttachServerMessage {
    match (unit, cancelled) {
        (Unit::Paste(id), true) => AttachServerMessage::PasteResult {
            paste_id: id.clone(),
            outcome: PasteOutcome::NotWritten,
            bytes_written: Some(0),
            reason: Some(paste_reason(AttachReason::NotWritten)),
        },
        (Unit::Paste(id), false) => AttachServerMessage::PasteResult {
            paste_id: id.clone(),
            outcome: PasteOutcome::Pending,
            bytes_written: None,
            reason: None,
        },
        (Unit::Raw(n), true) => AttachServerMessage::InputDropped {
            bytes: *n as u64,
            reason: AttachReason::NotWritten,
        },
        (Unit::Raw(n), false) => AttachServerMessage::InputDropped {
            bytes: *n as u64,
            reason: AttachReason::Pending,
        },
    }
}

/// The report channel dropped (R-10): the cancel decides `not_written`; otherwise the unit
/// is `pending` and stays outstanding with no settlement to come (R-4).
fn settle_lost(
    unit: Unit,
    ticket: &WriteTicket,
    ctl: &mut VecDeque<AttachServerMessage>,
) -> Option<Outstanding> {
    let cancelled = ticket.try_cancel() == Some(true);
    ctl.push_back(pending_frame(&unit, cancelled));
    (!cancelled).then_some(Outstanding {
        unit,
        phase: Phase::Stuck(None),
    })
}

/// No report by `not_after` + 1 s (R-4, R-10): the cancel decides `not_written` (the
/// worker still records the cancelled unit); otherwise `pending`, and the unit stays
/// outstanding until its report arrives.
fn settle_late(
    unit: Unit,
    ticket: &WriteTicket,
    rx: oneshot::Receiver<KeyReport>,
    ctl: &mut VecDeque<AttachServerMessage>,
) -> Option<Outstanding> {
    let cancelled = ticket.try_cancel() == Some(true);
    ctl.push_back(pending_frame(&unit, cancelled));
    (!cancelled).then_some(Outstanding {
        unit,
        phase: Phase::Stuck(Some(rx)),
    })
}
