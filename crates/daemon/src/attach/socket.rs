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
    Fence, Guard, ModeView, PtySize, ScreenMeta, SessionHandle, SubscribeError, Subscribed,
    SubscriptionId, WriteOrigin, WriteTicket, WriteUnit, grid_rows,
};
use futures_util::StreamExt;
use tokio::sync::oneshot;

use super::input::{Item, Pending, Verdict, decide_frames, filter};
use super::output::{Ending, OutBuf, Taken};
use super::registry::{Candidate, LaunchAttach, Removed, Replaced};
use super::{Attach, AttachConfig, paste_reason, refusal_reason, submit_reason};
use crate::http::ws::{
    Out, Sink, bad_utf8, before, close, code, pending_opt, send, socket_timer, too_big,
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
            hooks
                .verified
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
}

impl Drop for Exit {
    fn drop(&mut self) {
        self.buf.stop();
        unsubscribe(&self.sup, self.la.end(self.socket));
        self.a.reg.retire(|l| self.sup.entry(l).is_some());
        #[cfg(test)]
        self.a
            .cfg
            .hooks
            .registry_len
            .store(self.a.reg.len(), std::sync::atomic::Ordering::SeqCst);
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
type Submitted = Result<Submission, Refused>;

/// A queued unit.
struct Submission {
    /// `None` when nothing was queued: the unit's accepted bytes were all held back.
    ticket: Option<WriteTicket>,
    report: oneshot::Receiver<KeyReport>,
    /// The unit's dispatch deadline (supervisor clock), taken before the submission.
    not_after: Instant,
    /// The bytes queued.
    len: usize,
    /// The bytes of raw frames the marker rule refused at the submission.
    refused: usize,
    /// An accepted deferred frame's trailing partial marker, held for the next frame.
    held: Vec<u8>,
}

/// A unit's decision inside `with_session`.
struct Decided {
    ticket: Option<WriteTicket>,
    len: usize,
    refused: usize,
    held: Vec<u8>,
}

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
pub(super) type Resizing =
    Pin<Box<dyn Future<Output = Result<ResizeOut, tokio::task::JoinError>> + Send>>;

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

/// A unit's input as submitted.
enum Input {
    /// Raw frames, each kept whole for the marker rule's final decision; the flag: the
    /// last frame was deferred, so its partial-marker hold applies once it is accepted.
    Raw(Vec<Vec<u8>>, bool),
    /// A paste's encoded bytes and the mode sampled before its encoding.
    Paste(Vec<u8>, ModeView),
}

/// The bytes one submission attempt writes.
enum Payload<'a> {
    /// Borrowed: a raw unit is decided again on every attempt.
    Raw(&'a [Vec<u8>], bool),
    /// Moved into the unit's one part: a paste is submitted once.
    Paste(Vec<u8>),
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
    /// Submits `input` as one attach unit (R-4): its guard carries the socket's grant, a
    /// 30 s dispatch deadline, and for a paste on a bracketed-paste row the mode epoch
    /// sampled before its encoding (D5).
    fn submit(&self, mut input: Input) -> Pin<Box<dyn Future<Output = Submitted> + Send>> {
        let (sup, entry, sl, fence) = (
            self.sup.clone(),
            self.entry.clone(),
            self.sl.clone(),
            self.fence.clone(),
        );
        let wait = self.cfg.unit_wait;
        let needs_2004 = match &input {
            Input::Paste(_, m) if self.paste_on_2004 => Some(*m),
            _ => None,
        };
        let la = self.la.clone();
        #[cfg(test)]
        let hook = crate::supervisor::lock(&self.cfg.hooks.submit).take();
        Box::pin(async move {
            let now = sup.now();
            let not_after = now + wait;
            // After the cutoff is taken; on the socket's task: the socket handles nothing
            // else meanwhile.
            #[cfg(test)]
            if let Some(f) = hook {
                f();
            }
            loop {
                if sup.now() >= not_after {
                    return Err(Refused::NotWritten(AttachReason::Expired));
                }
                // A raw unit waits until every earlier raw unit of the launch settled, by
                // its own dispatch deadline.
                if matches!(input, Input::Raw(..)) {
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
                    payload: match &mut input {
                        Input::Raw(frames, hold_last) => Payload::Raw(frames, *hold_last),
                        Input::Paste(bytes, _) => Payload::Paste(std::mem::take(bytes)),
                    },
                    needs_2004,
                    not_after: sup.real(not_after),
                    report: tx,
                };
                // The lock budget starts when the lock is tried (after any wait above),
                // within the dispatch deadline.
                let by = (sup.now() + READ_WAIT).min(not_after);
                let r = entry.with_session(&sup, by, move |s| unit.run(s)).await;
                return match r {
                    Ok(Ok(Some(d))) => Ok(Submission {
                        ticket: d.ticket,
                        report: rx,
                        not_after,
                        len: d.len,
                        refused: d.refused,
                        held: d.held,
                    }),
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

/// One unit's submission attempt inside `with_session`.
struct Submit<'a> {
    sl: Arc<SendLaunch>,
    la: Arc<LaunchAttach>,
    fence: Fence,
    payload: Payload<'a>,
    /// The mode at encoding, for a paste on a bracketed-paste row.
    needs_2004: Option<ModeView>,
    not_after: Instant,
    report: oneshot::Sender<KeyReport>,
}

impl Submit<'_> {
    /// The ticket with the bytes queued and the raw bytes refused, or `None` when an
    /// earlier raw unit is unsettled again (another socket submitted one meanwhile): the
    /// caller waits and retries.
    fn run(self, s: &chaldea_pty_runtime::Session) -> Result<Option<Decided>, Refused> {
        if self.needs_2004.is_some_and(|m| !m.modes.bracketed_paste) {
            return Err(Refused::Rejected(AttachReason::BracketedPasteOff));
        }
        let guard = Guard {
            not_after: self.not_after,
            basis: None,
            bracketed_paste: self.needs_2004.map(|m| m.bracketed_paste_epoch),
            recheck: None,
            fence: Some(self.fence),
        };
        let (frames, hold_last) = match self.payload {
            Payload::Raw(frames, hold_last) => (frames, hold_last),
            Payload::Paste(bytes) => {
                let len = bytes.len();
                return self
                    .sl
                    .submit_human(
                        s,
                        WriteUnit::bytes(WriteOrigin::Attach, bytes),
                        guard,
                        Some(self.report),
                    )
                    .map(|t| {
                        Some(Decided {
                            ticket: Some(t),
                            len,
                            refused: 0,
                            held: Vec::new(),
                        })
                    })
                    .map_err(|e| Refused::NotWritten(submit_reason(e)));
            }
        };
        // The marker rule's final decision (R-6): every earlier raw unit settled, so the
        // tail is what the PTY received before this unit; a frame that would complete a
        // marker is refused whole, the others are written.
        let (bytes, refused, held) = {
            let mut m = crate::supervisor::lock(&self.la.marks);
            if m.unsettled > 0 {
                return Ok(None);
            }
            let (bytes, refused, held) = decide_frames(&m.tail, frames, hold_last);
            if bytes.is_empty() {
                if held.is_empty() {
                    return Err(Refused::NotWritten(AttachReason::PasteMarker));
                }
                // Only a held partial: nothing is queued.
                return Ok(Some(Decided {
                    ticket: None,
                    len: 0,
                    refused,
                    held,
                }));
            }
            m.unsettled += 1;
            (bytes, refused, held)
        };
        let len = bytes.len();
        let unit = WriteUnit::bytes(WriteOrigin::Attach, bytes.clone());
        let la = self.la.clone();
        let settled: RecordHook = Box::new(move |r| la.raw_settled(&bytes, r.bytes_written));
        self.sl
            .submit_human_then(s, unit, guard, Some(self.report), Some(settled))
            .map(|t| {
                Some(Decided {
                    ticket: Some(t),
                    len,
                    refused,
                    held,
                })
            })
            .map_err(|e| {
                self.la.raw_unqueued();
                Refused::NotWritten(submit_reason(e))
            })
    }
}

/// Runs one resize attempt to `size` under the launch lease (D7): waits for the lease, then
/// on the blocking pool decides the legs from the size it finds (at an equal size, the
/// nudge on a socket's `first` resize, else nothing) and runs each under the socket's
/// fence; it reads the result size, then releases the lease by its id, even after the
/// socket ended.
pub(super) fn resize(
    sl: Arc<SendLaunch>,
    handle: SessionHandle,
    fence: Fence,
    socket: u64,
    size: PtySize,
    first: bool,
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
            // Released by its id also if a leg panics.
            let lease = LeaseGuard { sl: &sl, id };
            let legs = match (size == handle.size(), first) {
                (false, _) => vec![size],
                (true, true) => nudge_legs(size),
                (true, false) => Vec::new(),
            };
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
            // The result is the size this resize left, read while it still holds the lease.
            let s = handle.size();
            drop(lease);
            #[cfg(test)]
            crate::supervisor::send::run_hook(&sl.hooks.after_resize_release);
            (s.cols(), s.rows(), ok)
        })
        .await
    })
}

/// A resize's lease, released by its id when dropped (unwinding included).
struct LeaseGuard<'a> {
    sl: &'a SendLaunch,
    id: u64,
}

impl Drop for LeaseGuard<'_> {
    fn drop(&mut self) {
        self.sl.release_lease(&mut self.sl.state(), self.id);
    }
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
    // The nudge generation the startup read verifies.
    let mut seen = *nudge.borrow_and_update();
    let (sink, mut stream) = socket.split();
    let out = Out::Free(sink);
    let sha = grant.token_sha256;
    let Some(sup) = a.sup.upgrade() else {
        return close(&shared, out, &mut stream, code::AWAY, "shutting_down").await;
    };
    // The candidacy's one deadline from the upgrade: no subscription delay or nudge
    // extends it.
    let candidacy_end = sup.now() + cfg.candidacy;

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
    let la = a.reg.launch(launch, |l| sup.entry(l).is_some());
    #[cfg(test)]
    cfg.hooks
        .registry_len
        .store(a.reg.len(), std::sync::atomic::Ordering::SeqCst);
    let me = a.reg.socket_id();
    let buf = OutBuf::new(cfg.out_max);
    let replaced = Arc::new(Replaced::default());
    let _exit = Exit {
        sup: sup.clone(),
        a: a.clone(),
        la: la.clone(),
        buf: buf.clone(),
        socket: me,
    };
    let sub_deadline = sup.real(sup.now() + cfg.subscribe_wait);
    let (h2, sink2, max) = (handle.clone(), buf.sink(), cfg.attached_max);
    #[cfg(test)]
    let hook = crate::supervisor::lock(&cfg.hooks.subscribed)
        .take()
        .or_else(|| crate::supervisor::lock(&cfg.hooks.subscribes).pop());
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
    // Verification stays active through startup (R-7). Every nudge counts one generation;
    // a read verifies the generation it started at, at most two reads run at once, and a
    // nudge that finds both taken drops the socket by their deadline unless a later read
    // answers first. The takeover is the point of effect: it happens only with the latest
    // generation verified, compared while the nudge cannot be published (under its lock).
    let mut nudges: u64 = 0;
    let mut verified: u64 = 0;
    let mut reads: [Option<Verifying>; 2] = [None, None];
    let mut due = false;
    let mut verify_by: Option<Instant> = None;
    let mut joined: Option<Subscribed> = None;
    let mut before_swap_ran = false;
    let (subscribed, granted, removed) = loop {
        if due {
            if let Some(slot) = reads.iter_mut().find(|r| r.is_none()) {
                due = false;
                *slot = Some(start_verify(&shared, sha, &cfg, nudges));
            } else if verify_by.is_none() {
                verify_by = reads.iter().flatten().map(|v| v.deadline).min();
                #[cfg(test)]
                cfg.hooks
                    .saturated
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        if joined.is_some() && verified == nudges && !due {
            #[cfg(test)]
            if !before_swap_ran {
                before_swap_ran = true;
                if let Some(f) = crate::supervisor::lock(&cfg.hooks.before_swap).take() {
                    f();
                }
            }
            #[cfg(not(test))]
            let _ = &mut before_swap_ran;
            // The candidacy's deadline holds at the takeover too, before the subscription
            // and the grant change hands.
            if sup.now() >= candidacy_end {
                return close(&shared, out, &mut stream, close_code::BUSY, "busy").await;
            }
            let published = nudge.borrow();
            if *published == seen {
                #[cfg(test)]
                if let Some(f) = crate::supervisor::lock(&cfg.hooks.compared).take() {
                    f();
                }
                let s = joined.take().unwrap_or_else(|| unreachable!());
                cand.sub = None;
                let (granted, removed) = la.swap(Candidate {
                    socket: me,
                    sub: s.id,
                    active: buf.active.clone(),
                    handle: handle.clone(),
                    replaced: replaced.clone(),
                });
                #[cfg(test)]
                cfg.hooks
                    .swaps
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                drop(published);
                break (s, granted, removed);
            }
            // A nudge was published since: the select below takes it.
        }
        let [r0, r1] = &mut reads;
        let read_by = verifying_min(r0.as_ref(), r1.as_ref()).map(|d| shared.timer(d));
        let ev = tokio::select! {
            biased;
            () = shutdown.wait() => {
                return close(&shared, out, &mut stream, code::AWAY, "shutting_down").await;
            }
            () = socket_timer(sup.timer(candidacy_end)) => {
                return close(&shared, out, &mut stream, close_code::BUSY, "busy").await;
            }
            () = pending_opt(verify_by.map(|d| socket_timer(shared.timer(d)))) => return,
            () = pending_opt(read_by.map(socket_timer)) => return,
            _ = nudge.changed() => {
                seen = *nudge.borrow_and_update();
                nudges += 1;
                due = true;
                None
            }
            j = pending_opt(r0.as_mut().map(|v| &mut v.handle)) => Some((0, j)),
            j = pending_opt(r1.as_mut().map(|v| &mut v.handle)) => Some((1, j)),
            () = poked(&cfg) => None,
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
        if let Some((i, j)) = ev {
            let Some(v) = reads[i].take() else { return };
            match take_read(&shared, v.deadline, j) {
                Ok(true) => {
                    verified = verified.max(v.generation);
                    if verified == nudges {
                        verify_by = None;
                    }
                }
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
    unsubscribe(&sup, removed);
    let fence = la.fence(granted);
    {
        let (la, sup) = (Arc::downgrade(&la), Arc::downgrade(&sup));
        buf.on_latch(Box::new(move || {
            if let Some(la) = la.upgrade() {
                la.revoke(me);
            }
            sup.upgrade().map(|s| s.now())
        }));
    }
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

    let mut out = match out {
        Out::Free(sink) => Out::Sending(
            send(&shared, sink, vec![Message::Text(first.into())]),
            tokio::time::Instant::now() + wcfg.send_timeout,
        ),
        o @ Out::Sending(..) => o,
    };
    let mut ctl = Ctl::default();
    let mut pending = Pending::default();
    let mut held: Vec<u8> = Vec::new();
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
    // On the supervisor's clock, as the other socket timers.
    let mut idle_at: Option<Instant> = None;
    #[cfg(test)]
    let tick_every = cfg.tick_every.unwrap_or(wcfg.heads_interval);
    #[cfg(not(test))]
    let tick_every = wcfg.heads_interval;
    let mut next_tick = tokio::time::Instant::now() + tick_every;
    #[cfg(test)]
    let mut ticks_seen = cfg.hooks.ticks.load(std::sync::atomic::Ordering::SeqCst);
    #[cfg(not(test))]
    let mut ticks_seen = 0;
    #[cfg(test)]
    let mut gones_seen = cfg
        .hooks
        .gone_requests
        .load(std::sync::atomic::Ordering::SeqCst);
    #[cfg(not(test))]
    let mut gones_seen = 0;
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
            if let Some((e, at)) = latched {
                la.revoke(me);
                // The deadline counts from the latch itself.
                ending = Some((e, at.unwrap_or_else(|| sup.now()) + cfg.drain));
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
                Some(Item::Raw(b, hold_last)) => Some(Outstanding {
                    unit: Unit::Raw(b.iter().map(Vec::len).sum()),
                    phase: Phase::Submitting(writer.submit(Input::Raw(b, hold_last))),
                }),
                Some(Item::Paste { id, text }) => {
                    // The mode is sampled before the encoding; the writer refuses the unit
                    // if 2004 changed after it.
                    let modes = handle.modes();
                    match writer.paste_bytes(&text) {
                        Ok(b) => Some(Outstanding {
                            unit: Unit::Paste(id),
                            phase: Phase::Submitting(writer.submit(Input::Paste(b, modes))),
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
                    }
                }
                None => None,
            };
        }
        // The next resize: one in flight, the latest size wins; what it does is decided
        // under the lease.
        if resizing.is_none() && ending.is_none() {
            let want = crate::supervisor::lock(&size_wanted).take();
            if let Some(size) = want {
                let first = !resized_once;
                resized_once = true;
                resizing = Some(resize(
                    sl.clone(),
                    handle.clone(),
                    fence.clone(),
                    me,
                    size,
                    first,
                    size_wanted.clone(),
                ));
            }
        }
        // The next frame, when the sink is free.
        out = match out {
            Out::Free(sink) => {
                let mut msgs = Vec::new();
                if let Some(t) = ctl.pop_front().filter(|_| ending.is_none()) {
                    msgs.push(Message::Text(t.into()));
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
                    Out::Sending(send(&shared, sink, msgs), at)
                }
            }
            sending @ Out::Sending(..) => sending,
        };
        let drain_timer = drain_at.filter(|_| matches!(out, Out::Free(_)));
        let (sending, send_deadline) = match &mut out {
            Out::Sending(f, d) => (Some(f), Some(drain_at.map_or(*d, |e| e.min(*d)))),
            Out::Free(_) => (None, None),
        };
        // Reads pause at the watermark (pongs too): pending input and queued responses.
        let reading = pending.bytes() + ctl.bytes < cfg.watermark;
        #[cfg(test)]
        {
            use std::sync::atomic::Ordering::SeqCst;
            if reading {
                let retained = (pending.retained() + ctl.bytes) as u64;
                cfg.hooks.retained_max.fetch_max(retained, SeqCst);
            } else {
                cfg.hooks.paused.fetch_add(1, SeqCst);
            }
        }
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
        #[cfg(test)]
        if terminal.is_some()
            && let Some(f) = crate::supervisor::lock(&cfg.hooks.before_terminal).take()
        {
            f();
            // The reads the test released have finished, so their answers are ready at
            // this turn's select together with the terminal close.
            let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
            tokio::task::block_in_place(|| {
                let pending = || {
                    [v0.as_ref(), v1.as_ref()]
                        .into_iter()
                        .flatten()
                        .any(|v| !v.handle.is_finished())
                };
                while pending() {
                    assert!(
                        std::time::Instant::now() < until,
                        "a released verification read did not finish"
                    );
                    std::thread::yield_now();
                }
            });
        }
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
            () = pending_opt(idle_at.map(|at| socket_timer(sup.timer(at)))) => Ev::Dropped,
            () = socket_timer(next_tick) => Ev::Tick,
            n = test_tick(&cfg, false, ticks_seen) => {
                ticks_seen = n;
                Ev::TestTick
            }
            sent = pending_opt(sending.zip(send_deadline).map(|(f, d)| before(d, f))) => match sent {
                Some((sink, r)) => Ev::Sent(sink, r),
                None => Ev::Dropped,
            },
            () = socket_timer(sup.timer(next_gone)) => Ev::GonePoll,
            n = test_tick(&cfg, true, gones_seen) => {
                gones_seen = n;
                Ev::GonePoll
            }
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
                // The close frame too is sent only before the drain deadline (R-8), and
                // verification stays polled until the socket ends: a revocation found
                // meanwhile drops the connection (the close frame may be staged).
                let mut closing = pin!(close(&shared, out, &mut stream, code, why));
                loop {
                    if verify_due && let Some(slot) = verifying.iter_mut().find(|v| v.is_none()) {
                        verify_due = false;
                        *slot = Some(start_verify(&shared, sha, &cfg, generation));
                    }
                    let [v0, v1] = &mut verifying;
                    let read_by = verifying_min(v0.as_ref(), v1.as_ref()).map(|d| shared.timer(d));
                    let read = tokio::select! {
                        biased;
                        () = shutdown.wait() => return,
                        () = pending_opt(read_by.map(socket_timer)) => return,
                        j = pending_opt(v0.as_mut().map(|v| &mut v.handle)) => Some((0, j)),
                        j = pending_opt(v1.as_mut().map(|v| &mut v.handle)) => Some((1, j)),
                        _ = nudge.changed() => {
                            generation += 1;
                            verify_due = true;
                            None
                        }
                        () = socket_timer(sup.timer(at)) => return,
                        () = poked(&cfg) => None,
                        () = &mut closing => return,
                    };
                    if let Some((i, j)) = read {
                        let Some(v) = verifying[i].take() else { return };
                        if !matches!(take_read(&shared, v.deadline, j), Ok(true)) {
                            return;
                        }
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
                        la.revoke(me);
                        // A send in flight may have staged an application frame in the
                        // sink: the connection is dropped rather than closed through it.
                        if matches!(out, Out::Sending(..)) {
                            return;
                        }
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
                    idle_at = Some(sup.now() + wcfg.idle_timeout);
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
                pending.raw(&std::mem::take(&mut held));
                continue;
            }
            Ev::Submitted(r) => {
                let Some(o) = outstanding.take() else {
                    continue;
                };
                match r {
                    Ok(Submission {
                        ticket,
                        report: rx,
                        not_after,
                        len,
                        refused,
                        held: back,
                    }) => {
                        let by = not_after + READ_WAIT;
                        if refused > 0 {
                            ctl.push_back(AttachServerMessage::InputDropped {
                                bytes: refused as u64,
                                reason: AttachReason::PasteMarker,
                            });
                        }
                        // A held partial goes ahead of what arrived meanwhile; with nothing
                        // pending it waits for the next frame or the hold's end.
                        if !back.is_empty() {
                            if pending.bytes() > 0 || !held.is_empty() {
                                pending.raw_front(back);
                            } else {
                                held = back;
                                hold_until = Some(sup.now() + cfg.hold);
                            }
                        }
                        let Some(ticket) = ticket else {
                            continue;
                        };
                        let unit = match o.unit {
                            Unit::Raw(_) => Unit::Raw(len),
                            u @ Unit::Paste(_) => u,
                        };
                        #[cfg(test)]
                        cfg.hooks
                            .waiting
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        outstanding = Some(Outstanding {
                            unit,
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
                    // The deadline decision of `key`: a report at or after `by` is late, so
                    // the unit is `pending` (it is settled: nothing stays outstanding).
                    (Phase::Waiting { ticket, by, .. }, Ok(_)) if sup.now() >= by => {
                        let cancelled = ticket.try_cancel() == Some(true);
                        ctl.push_back(pending_frame(&o.unit, cancelled));
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
                    idle_at = Some(sup.now() + wcfg.idle_timeout);
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
                        // With no raw input of the launch in flight (none of this socket's
                        // pending or outstanding, no unit unsettled), the written tail is
                        // known and the rule applies here, final. Otherwise the whole frame
                        // is left to the submission, decided in write order.
                        let own = pending.has_raw()
                            || matches!(&outstanding, Some(o) if matches!(o.unit, Unit::Raw(_)));
                        let tail = if own {
                            None
                        } else {
                            let m = crate::supervisor::lock(&la.marks);
                            (m.unsettled == 0).then(|| m.tail.clone())
                        };
                        let Some(tail) = tail else {
                            // Deferred: the frame stays whole (no held split) until its
                            // decision at submission, after any held partial in arrival
                            // order; once it is accepted there, the partial marker the write
                            // ends with is held back, as at receipt.
                            if !held.is_empty() {
                                hold_until = None;
                                pending.raw(&std::mem::take(&mut held));
                            }
                            pending.raw_deferred(&b);
                            continue;
                        };
                        match filter(&tail, &held, &b) {
                            Verdict::Refused => {
                                ctl.push_back(AttachServerMessage::InputDropped {
                                    bytes: b.len() as u64,
                                    reason: AttachReason::PasteMarker,
                                });
                            }
                            Verdict::Accepted { write, held: h } => {
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
                                    hold_until = None;
                                    pending.raw(&std::mem::take(&mut held));
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

/// Responses queued for the socket, serialized, and their bytes (counted against the
/// watermark with the pending input).
#[derive(Default)]
struct Ctl {
    q: VecDeque<String>,
    bytes: usize,
}

impl Ctl {
    #[allow(
        clippy::needless_pass_by_value,
        reason = "callers build the frame in place"
    )]
    fn push_back(&mut self, m: AttachServerMessage) {
        if let Ok(t) = serde_json::to_string(&m) {
            self.bytes += t.len();
            self.q.push_back(t);
        }
    }

    fn extend(&mut self, m: Option<AttachServerMessage>) {
        if let Some(m) = m {
            self.push_back(m);
        }
    }

    fn pop_front(&mut self) -> Option<String> {
        let t = self.q.pop_front()?;
        self.bytes -= t.len();
        Some(t)
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
async fn test_tick(cfg: &AttachConfig, gone: bool, seen: u64) -> u64 {
    #[cfg(test)]
    loop {
        let mut n = pin!(cfg.hooks.tick_now.notified());
        n.as_mut().enable();
        let counter = if gone {
            &cfg.hooks.gone_requests
        } else {
            &cfg.hooks.ticks
        };
        let c = counter.load(std::sync::atomic::Ordering::SeqCst);
        if c > seen {
            return c;
        }
        n.await;
    }
    #[cfg(not(test))]
    {
        let _ = (cfg, gone, seen);
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
fn settle_lost(unit: Unit, ticket: &WriteTicket, ctl: &mut Ctl) -> Option<Outstanding> {
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
    ctl: &mut Ctl,
) -> Option<Outstanding> {
    let cancelled = ticket.try_cancel() == Some(true);
    ctl.push_back(pending_frame(&unit, cancelled));
    (!cancelled).then_some(Outstanding {
        unit,
        phase: Phase::Stuck(Some(rx)),
    })
}
