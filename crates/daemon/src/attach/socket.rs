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
    Fence, Guard, PtySize, ScreenMeta, SessionHandle, SubscribeError, Subscribed, SubscriptionId,
    WriteOrigin, WriteTicket, WriteUnit, grid_rows,
};
use futures_util::StreamExt;
use tokio::sync::oneshot;

use super::input::{Item, Pending, Verdict, advance_tail, completes_marker, filter};
use super::output::{Ending, OutBuf, Taken};
use super::registry::{Candidate, LaunchAttach, Removed, Replaced};
use super::{Attach, AttachConfig, paste_reason, refusal_reason, submit_reason};
use crate::http::ws::{
    Out, Sink, bad_utf8, before, close, code, pending_opt, send_unless, socket_timer, too_big,
};
use crate::http::{
    AuthFailure, Joined, Shared, TicketPurpose, UpgradedSocket, spawn_read, take_read,
    verify_hash_in,
};
use crate::store::{ReadTx, StoreError};
use crate::supervisor::Supervisor;
use crate::supervisor::live::{LiveLaunch, SessionError};
use crate::supervisor::send::{KeyReport, LeaseKind, READ_WAIT, RecordHook, SendLaunch};

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
    #[cfg(test)] hooks: Arc<super::AttachHooks>,
) -> impl FnOnce(&ReadTx<'_>) -> Result<bool, StoreError> + Send + 'static {
    move |tx| {
        let r = match verify_hash_in(tx, &sha) {
            Ok(_) => Ok(true),
            Err(AuthFailure::Invalid) => Ok(false),
            Err(AuthFailure::Store(e)) => Err(e),
        };
        // After the read's snapshot was taken.
        #[cfg(test)]
        {
            let hook = crate::supervisor::lock(&hooks.verify).pop();
            if let Some(f) = hook {
                f();
            }
        }
        r
    }
}

/// The `attached` frame of `meta` and `lines`, its grid kept only while the whole frame
/// fits `max`. The subscription's budget ([`grid_budget`]) already stops the copy at the
/// same bound; this check is the frame's own, so the bound holds even if the walk's count
/// and the serialization ever disagree.
pub(super) fn attached(
    thread: ThreadId,
    launch: LaunchId,
    meta: &ScreenMeta,
    lines: Option<&[chaldea_pty_runtime::Line]>,
    max: usize,
) -> String {
    let frame =
        |grid| serde_json::to_string(&envelope(thread, launch, meta, grid)).unwrap_or_default();
    if let Some(lines) = lines {
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
pub(super) fn grid_budget(
    thread: ThreadId,
    launch: LaunchId,
    meta: &ScreenMeta,
    max: usize,
) -> usize {
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

/// The startup subscription until the takeover: on any other exit (the task aborted
/// included) its sink stops and the subscription is removed, also when it registers late.
struct Candidacy {
    sup: Arc<Supervisor>,
    handle: SessionHandle,
    buf: Arc<OutBuf>,
    pending: Option<tokio::task::JoinHandle<Result<Subscribed, SubscribeError>>>,
    sub: Option<SubscriptionId>,
}

impl Drop for Candidacy {
    fn drop(&mut self) {
        self.buf.stop();
        if let Some(id) = self.sub.take() {
            unsubscribe(&self.sup, Some((self.handle.clone(), id)));
        }
        if let Some(j) = self.pending.take() {
            let h = self.handle.clone();
            let task = self.sup.rt.spawn(async move {
                if let Ok(Ok(s)) = j.await {
                    let _ = tokio::task::spawn_blocking(move || h.unsubscribe(s.id)).await;
                }
            });
            drop(task);
        }
    }
}

/// Starts a verification read of `sha` with the short deadline.
fn start_verify(
    shared: &Arc<Shared>,
    sha: [u8; 32],
    cfg: &AttachConfig,
    generation: u64,
) -> Verifying {
    let deadline = shared.cfg.clock.now() + shared.cfg.short_deadline;
    #[cfg(not(test))]
    let _ = cfg;
    Verifying {
        handle: spawn_read(
            shared,
            deadline,
            verify_read(
                sha,
                #[cfg(test)]
                cfg.hooks.clone(),
            ),
        ),
        deadline,
        generation,
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
    /// The nudge count when it started: an older read does not answer a newer nudge.
    generation: u64,
}

/// A resize's answer: the PTY's size after it and whether every leg succeeded.
type ResizeOut = (u16, u16, bool);

/// A resize in flight.
type Resizing = Pin<Box<dyn Future<Output = Result<ResizeOut, tokio::task::JoinError>> + Send>>;

/// What woke the loop.
enum Ev {
    Shutdown,
    Replaced,
    Dropped,
    Nudge,
    Tick,
    /// A tick the test asked for.
    TestTick,
    Verified(usize, Joined<bool>),
    Sent(Sink, Result<(), axum::Error>),
    Inbound(Option<Result<Message, axum::Error>>),
    Output,
    /// The terminal close is due: every frame before it was sent.
    Terminal,
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
    la: Arc<LaunchAttach>,
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
        let la = self.la.clone();
        #[cfg(test)]
        let hook = crate::supervisor::lock(&self.cfg.hooks.submit).take();
        Box::pin(async move {
            // Runs on the socket's task: the socket handles nothing else meanwhile.
            #[cfg(test)]
            if let Some(f) = hook {
                f();
            }
            let now = sup.now();
            let not_after = now + wait;
            loop {
                // A raw unit waits until every earlier raw unit of the launch settled, by
                // its own dispatch deadline.
                if !paste {
                    let mut settled = pin!(la.settled.notified());
                    settled.as_mut().enable();
                    if crate::supervisor::lock(&la.marks).unsettled > 0 {
                        tokio::select! {
                            () = settled => {}
                            () = socket_timer(sup.timer(not_after)) => {
                                return Err(Refused::NotWritten(AttachReason::Expired));
                            }
                        }
                        continue;
                    }
                }
                let (tx, rx) = oneshot::channel();
                let unit = Submit {
                    sl: sl.clone(),
                    la: la.clone(),
                    fence: fence.clone(),
                    bytes: bytes.clone(),
                    paste,
                    needs_2004,
                    not_after: sup.real(not_after),
                    report: tx,
                };
                let r = entry
                    .with_session(&sup, now + READ_WAIT, move |s| unit.run(s))
                    .await;
                return match r {
                    Ok(Ok(Some(ticket))) => Ok((ticket, rx)),
                    Ok(Ok(None)) => continue,
                    Ok(Err(e)) => Err(e),
                    Err(SessionError::Busy) => Err(Refused::NotWritten(AttachReason::Busy)),
                    Err(SessionError::Gone) => Err(Refused::NotWritten(AttachReason::NotWritten)),
                };
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

/// One unit's submission inside `with_session`.
struct Submit {
    sl: Arc<SendLaunch>,
    la: Arc<LaunchAttach>,
    fence: Fence,
    bytes: Vec<u8>,
    paste: bool,
    needs_2004: bool,
    not_after: Instant,
    report: oneshot::Sender<KeyReport>,
}

impl Submit {
    /// The ticket, or `None` when an earlier raw unit is unsettled again (another socket
    /// submitted one meanwhile): the caller waits and retries.
    fn run(self, s: &chaldea_pty_runtime::Session) -> Result<Option<WriteTicket>, Refused> {
        let modes = s.modes();
        if self.needs_2004 && !modes.modes.bracketed_paste {
            return Err(Refused::Rejected(AttachReason::BracketedPasteOff));
        }
        let guard = Guard {
            not_after: self.not_after,
            basis: None,
            bracketed_paste: self.needs_2004.then_some(modes.bracketed_paste_epoch),
            recheck: None,
            fence: Some(self.fence),
        };
        let unit = WriteUnit::bytes(WriteOrigin::Attach, self.bytes.clone());
        if self.paste {
            return self
                .sl
                .submit_human(s, unit, guard, Some(self.report))
                .map(Some)
                .map_err(|e| Refused::NotWritten(submit_reason(e)));
        }
        // The marker rule's final decision (R-6): every earlier raw unit settled, so the
        // tail is what the PTY received before this unit.
        {
            let mut m = crate::supervisor::lock(&self.la.marks);
            if m.unsettled > 0 {
                return Ok(None);
            }
            if completes_marker(&m.tail, &self.bytes) {
                return Err(Refused::NotWritten(AttachReason::PasteMarker));
            }
            m.unsettled += 1;
        }
        let (la, bytes) = (self.la.clone(), self.bytes);
        let settled: RecordHook = Box::new(move |r| la.raw_settled(&bytes, r.bytes_written));
        self.sl
            .submit_human_then(s, unit, guard, Some(self.report), Some(settled))
            .map(Some)
            .map_err(|e| {
                self.la.raw_unqueued();
                Refused::NotWritten(submit_reason(e))
            })
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
            #[cfg(test)]
            sl.lease_waits
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
    #[cfg(test)]
    let hook = crate::supervisor::lock(&cfg.hooks.subscribed).take();
    let subscribing = sup.rt.spawn_blocking(move || {
        let r = h2.subscribe(sink2, sub_deadline, |m| grid_budget(thread, launch, m, max));
        #[cfg(test)]
        if let Some(f) = hook {
            f();
        }
        r
    });
    let mut cand = Candidacy {
        sup: sup.clone(),
        handle: handle.clone(),
        buf: buf.clone(),
        pending: Some(subscribing),
        sub: None,
    };
    // Verification stays active through startup (R-7): a nudge after the startup read starts
    // a fresh read at once, and the candidate takes over only once the read started after
    // the latest nudge answered that the token is valid.
    let mut fresh: Option<Verifying> = None;
    let mut joined = None;
    let subscribed = loop {
        if fresh.is_none()
            && let Some(j) = joined.take()
        {
            break j;
        }
        let fresh_by = fresh.as_ref().map(|v| shared.timer(v.deadline));
        let ev = tokio::select! {
            biased;
            () = shutdown.wait() => {
                return close(&shared, out, &mut stream, code::AWAY, "shutting_down").await;
            }
            () = pending_opt(fresh_by.map(socket_timer)) => return,
            j = pending_opt(fresh.as_mut().map(|v| &mut v.handle)) => Some(j),
            _ = nudge.changed() => {
                fresh = Some(start_verify(&shared, sha, &cfg, 0));
                None
            }
            j = pending_opt(cand.pending.as_mut()), if joined.is_none() => {
                cand.pending = None;
                match j {
                    Ok(Ok(s)) => {
                        cand.sub = Some(s.id);
                        joined = Some(s);
                    }
                    Ok(Err(SubscribeError::Closed)) => {
                        return close(&shared, out, &mut stream, no_live, "no_live_launch").await;
                    }
                    Ok(Err(SubscribeError::VtUnavailable | SubscribeError::TooManySubscribers))
                    | Err(_) => {
                        return close(&shared, out, &mut stream, close_code::BUSY, "busy").await;
                    }
                }
                None
            }
        };
        if let Some(j) = ev {
            let Some(v) = fresh.take() else { return };
            match take_read(&shared, v.deadline, j) {
                Ok(true) => {}
                Ok(false) => {
                    return close(&shared, out, &mut stream, code::POLICY, "token_revoked").await;
                }
                Err(_) => return,
            }
        }
    };
    let first = attached(
        thread,
        launch,
        &subscribed.meta,
        subscribed.lines.as_deref(),
        cfg.attached_max,
    );
    // The takeover: the candidate subscribed, so it becomes current.
    cand.sub = None;
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
        la: la.clone(),
        entry: entry.clone(),
        sl: sl.clone(),
        fence: fence.clone(),
        cfg: cfg.clone(),
        agent: entry.agent,
        host_os: sup.host_os,
        paste_on_2004: entry.caps.pty_encoding() == InputEncoding::BracketedPaste,
    };

    // Set when the revocation is detected: a frame not yet handed to the transport is not
    // sent after it.
    let cut = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut out = match out {
        Out::Free(sink) => Out::Sending(
            send_unless(
                &shared,
                sink,
                vec![Message::Text(first.into())],
                Some(cut.clone()),
            ),
            tokio::time::Instant::now() + wcfg.send_timeout,
        ),
        o @ Out::Sending(..) => o,
    };
    let mut ctl: VecDeque<AttachServerMessage> = VecDeque::new();
    let mut pending = Pending::default();
    let mut held: Vec<u8> = Vec::new();
    // The tail the socket's own accepted raw input will follow (the receipt check).
    let mut proj: Vec<u8> = Vec::new();
    // Timers on the supervisor's clock (`sup.timer` each iteration), so a test clock
    // drives them.
    let mut hold_until: Option<Instant> = None;
    let mut outstanding: Option<Outstanding> = None;
    let size_wanted: Arc<Mutex<Option<PtySize>>> = Arc::new(Mutex::new(None));
    let mut resizing: Option<Resizing> = None;
    let mut resized_once = false;
    let mut verifying: [Option<Verifying>; 2] = [None, None];
    let mut generation: u64 = 0;
    let mut verified_generation: u64 = 0;
    // Set when a nudge found both read slots taken: the socket drops at this instant unless
    // a read started after the nudge answers first.
    let mut verify_by: Option<Instant> = None;
    let mut verify_due = false;
    let mut ending: Option<(Ending, Instant)> = None;
    let mut ended_sent = false;
    let mut ping_due = false;
    let mut idle_at: Option<tokio::time::Instant> = None;
    #[cfg(test)]
    let tick_every = cfg.tick_every.unwrap_or(wcfg.heads_interval);
    #[cfg(not(test))]
    let tick_every = wcfg.heads_interval;
    let mut next_tick = tokio::time::Instant::now() + tick_every;
    #[cfg(test)]
    let mut ticks_seen = cfg.hooks.ticks.load(std::sync::atomic::Ordering::SeqCst);
    #[cfg(not(test))]
    let mut ticks_seen = 0;
    let mut next_gone = sup.now() + cfg.gone_poll;

    loop {
        let mut terminal: Option<(u16, &str)> = None;
        // Latch a terminal event (the first wins): the sink stops, the grant is revoked,
        // and one deadline covers the rest.
        if ending.is_none() {
            let latched = buf.ending();
            #[cfg(test)]
            if let Some(f) = crate::supervisor::lock(&cfg.hooks.sampled).take() {
                f();
            }
            if let Some(e) = latched {
                la.revoke(me);
                ending = Some((e, sup.now() + cfg.drain));
                #[cfg(test)]
                cfg.hooks
                    .latched
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let drain_at = ending.map(|(_, at)| sup.timer(at));
        // A fresh verification on a nudge or a tick, even with one read in flight.
        if verify_due && let Some(slot) = verifying.iter_mut().find(|v| v.is_none()) {
            verify_due = false;
            *slot = Some(start_verify(&shared, sha, &cfg, generation));
        } else if verify_due && verify_by.is_none() {
            verify_by = verifying.iter().flatten().map(|v| v.deadline).min();
            #[cfg(test)]
            cfg.hooks
                .saturated
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
                    // The terminal close is decided in the select below, after shutdown,
                    // revocation, and the deadlines.
                    match e {
                        Ending::Lagged => terminal = Some((close_code::LAGGED, "lagged")),
                        Ending::Ended(_) if ended_sent => {
                            terminal = Some((close_code::LAUNCH_ENDED, "launch_ended"));
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
                    Out::Sending(send_unless(&shared, sink, msgs, Some(cut.clone())), at)
                }
            }
            sending @ Out::Sending(..) => sending,
        };
        let drain_timer = drain_at.filter(|_| matches!(out, Out::Free(_)));
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
        let verify_timer = verifying_min(v0.as_ref(), v1.as_ref()).map(|d| shared.timer(d));
        let ev = tokio::select! {
            biased;
            () = shutdown.wait() => Ev::Shutdown,
            () = pending_opt(verify_by.map(|d| socket_timer(shared.timer(d)))) => Ev::Dropped,
            () = pending_opt(verify_timer.map(socket_timer)) => Ev::Dropped,
            j = pending_opt(v0.as_mut().map(|v| &mut v.handle)) => Ev::Verified(0, j),
            j = pending_opt(v1.as_mut().map(|v| &mut v.handle)) => Ev::Verified(1, j),
            _ = nudge.changed() => Ev::Nudge,
            () = replaced.wait() => Ev::Replaced,
            () = pending_opt(send_deadline.map(socket_timer)) => Ev::Dropped,
            () = pending_opt(drain_timer.map(socket_timer)) => Ev::Dropped,
            () = pending_opt(idle_at.map(socket_timer)) => Ev::Dropped,
            () = socket_timer(next_tick) => Ev::Tick,
            n = test_tick(&cfg, ticks_seen) => {
                ticks_seen = n;
                Ev::TestTick
            }
            sent = pending_opt(sending.zip(send_deadline).map(|(f, d)| before(d, f))) => match sent {
                Some((sink, r)) => Ev::Sent(sink, r),
                None => Ev::Dropped,
            },
            () = socket_timer(sup.timer(next_gone)) => Ev::GonePoll,
            () = poked(&cfg) => continue,
            () = buf.wake.notified(), if ending.is_none() => Ev::Output,
            r = pending_opt(submitting) => Ev::Submitted(r),
            r = pending_opt(report_rx) => Ev::Report(r),
            () = pending_opt(report_by.map(socket_timer)) => Ev::ReportLate,
            r = pending_opt(resizing.as_mut()) => Ev::Resized(r),
            () = pending_opt(hold_until.map(|at| socket_timer(sup.timer(at)))) => Ev::HoldEnd,
            () = std::future::ready(()), if terminal.is_some() => Ev::Terminal,
            m = stream.next(), if reading => Ev::Inbound(m),
        };
        let refusal: (u16, &str) = match ev {
            Ev::Shutdown => (code::AWAY, "shutting_down"),
            Ev::Dropped => return,
            Ev::Terminal => {
                let Some(((code, why), (_, at))) = terminal.zip(ending) else {
                    return;
                };
                // The close frame too is sent only before the drain deadline (R-8).
                let mut closing = pin!(close(&shared, out, &mut stream, code, why));
                loop {
                    tokio::select! {
                        biased;
                        () = shutdown.wait() => return,
                        () = socket_timer(sup.timer(at)) => return,
                        () = poked(&cfg) => {}
                        () = &mut closing => return,
                    }
                }
            }
            Ev::Replaced => (close_code::REPLACED, "replaced"),
            Ev::Nudge => {
                generation += 1;
                verify_due = true;
                continue;
            }
            Ev::Tick => {
                next_tick += tick_every;
                generation += 1;
                verify_due = true;
                ping_due = true;
                continue;
            }
            Ev::TestTick => {
                generation += 1;
                verify_due = true;
                ping_due = true;
                #[cfg(test)]
                cfg.hooks
                    .ticked
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
                        cut.store(true, std::sync::atomic::Ordering::Release);
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
                next_gone = sup.now() + cfg.gone_poll;
                #[cfg(test)]
                cfg.hooks
                    .gone_polls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if sup.live(thread, launch).is_none() {
                    // Latched with the sink's events: the first one wins.
                    buf.latch(Ending::Ended(EndReason::Gone));
                }
                continue;
            }
            Ev::HoldEnd => {
                hold_until = None;
                let h = std::mem::take(&mut held);
                advance_tail(&mut proj, &h);
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
                        // With none of its own raw input in flight, the socket's view of
                        // the tail is the launch's written tail; the final decision is made
                        // at submission, in write order.
                        let own = pending.has_raw()
                            || !held.is_empty()
                            || matches!(&outstanding, Some(o) if matches!(o.unit, Unit::Raw(_)));
                        if !own {
                            proj.clone_from(&crate::supervisor::lock(&la.marks).tail);
                        }
                        match filter(&proj, &held, &b) {
                            Verdict::Refused => {
                                ctl.push_back(AttachServerMessage::InputDropped {
                                    bytes: b.len() as u64,
                                    reason: AttachReason::PasteMarker,
                                });
                            }
                            Verdict::Accepted { write, held: h } => {
                                advance_tail(&mut proj, &write);
                                pending.raw(&write);
                                held = h;
                                hold_until = (!held.is_empty()).then(|| sup.now() + cfg.hold);
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
                                    advance_tail(&mut proj, &h);
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

/// A test's poke: the loop recomputes its timers after the test clock moved. Never
/// resolves outside tests.
async fn poked(cfg: &AttachConfig) {
    #[cfg(test)]
    cfg.hooks.poke.notified().await;
    #[cfg(not(test))]
    {
        let _ = cfg;
        std::future::pending::<()>().await;
    }
}

/// Resolves with the test's tick count once it is past `seen`. Never resolves outside
/// tests.
async fn test_tick(cfg: &AttachConfig, seen: u64) -> u64 {
    #[cfg(test)]
    loop {
        let mut n = pin!(cfg.hooks.tick_now.notified());
        n.as_mut().enable();
        let c = cfg.hooks.ticks.load(std::sync::atomic::Ordering::SeqCst);
        if c > seen {
            return c;
        }
        n.await;
    }
    #[cfg(not(test))]
    {
        let _ = (cfg, seen);
        std::future::pending().await
    }
}

fn verifying_min(a: Option<&Verifying>, b: Option<&Verifying>) -> Option<Instant> {
    [a, b].into_iter().flatten().map(|v| v.deadline).min()
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
