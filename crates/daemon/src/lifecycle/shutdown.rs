//! The coordinator and the shutdown sequence (design §2.8): the only caller of
//! `ServeHandle::shutdown`, `Store::shutdown`, and `InstanceLock::release`. It joins the
//! startup task, then shuts down HTTP, the participants, and the store within one budget
//! measured back from the first request's cutoff, removes our record, decides the lock, and
//! only then sends a detached child's verdict. A Windows console end makes it return at once.

use std::future::Future;
use std::mem::ManuallyDrop;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use futures_util::FutureExt;
use tokio::task::JoinHandle;

use super::files;
use super::gate::{self, ShutdownGate, StopCause};
use super::log::{LogTarget, Logger};
use super::panic::RUNTIME_THREAD;
use super::ready::fail_line;
use super::startup::{self, Failure, Outcome, Started, StartupCtx};
use super::{
    Budgets, Components, DaemonConfig, ExitCode, ExitReport, FailReason, LockDisposition, RunEnv,
    RunMode, names, signals,
};
use crate::store::ShutdownReport;

/// The runtime, the gate, and the coordinator of one daemon (`run` and `run_isolated`).
pub(crate) fn run_daemon(
    cfg: DaemonConfig,
    components: Box<dyn Components>,
    env: RunEnv,
    shared: Arc<startup::Shared>,
) -> ExitReport {
    let log = env.log.clone();
    #[cfg(test)]
    assert!(
        !env.faults.panic_before_runtime.load(Ordering::SeqCst),
        "injected panic before the runtime"
    );
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name(RUNTIME_THREAD)
        .build()
    {
        Ok(rt) => ManuallyDrop::new(rt),
        Err(e) => {
            log.error(&format!("the runtime cannot start: {e}"));
            log.error("exiting: code 1 (internal)");
            log.flush(cfg.budgets.log_flush);
            return ExitReport::failed(FailReason::Internal, LockDisposition::NeverTaken);
        }
    };
    let gate = Arc::new(ShutdownGate::new(cfg.budgets.total, log.clone()));
    if env.process {
        gate::register(&gate);
    }
    #[cfg(test)]
    {
        let _ = env.faults.gate.set(gate.clone());
        let workers = rt.metrics().num_workers();
        let _ = env.faults.runtime.set((rt.handle().clone(), workers));
    }
    let budgets = cfg.budgets;
    let seen = shared.clone();
    let coordinator = Coordinator {
        gate: gate.clone(),
        log: log.clone(),
        budgets,
        instance: uuid::Uuid::new_v4().to_string(),
        shared,
        #[cfg(test)]
        probe: env.faults.probe.lock().ok().and_then(|mut p| p.take()),
        #[cfg(not(test))]
        probe: (),
        #[cfg(test)]
        faults: env.faults.clone(),
    };
    let report = rt
        .block_on(AssertUnwindSafe(coordinator.run(cfg, components, env)).catch_unwind())
        .unwrap_or_else(|_| {
            log.error("exiting: code 1 (panic in the coordinator)");
            log.flush(budgets.log_flush);
            let lock = if seen.lock_attempted.load(Ordering::SeqCst) {
                LockDisposition::HeldUntilExit
            } else {
                LockDisposition::NeverTaken
            };
            ExitReport::failed(FailReason::Panic, lock)
        });
    // Blocking tasks (an abandoned `Store::shutdown` or `Store::open`) keep running until the
    // process exits; neither a return nor an unwind waits for them.
    ManuallyDrop::into_inner(rt).shutdown_background();
    report
}

/// How long step 5 retries the removal of our record, and how often.
const RECORD_REMOVAL: std::time::Duration = std::time::Duration::from_millis(500);
const RECORD_RETRY: std::time::Duration = std::time::Duration::from_millis(50);

/// The most step 2's `stopping` rewrite of our record may take, out of HTTP's budget.
const STOPPING_REWRITE: std::time::Duration = std::time::Duration::from_millis(500);

/// A console end was observed: no further step starts.
struct Crashed;

/// The deadline passed before the awaited future completed.
struct Elapsed;

/// Awaits `fut` until `end`, deadline first: the deadline is checked before every poll, so a
/// completion first observable after `end` never wins over it (design §2.8).
async fn deadline_first<F: Future>(end: Instant, fut: F) -> Result<F::Output, Elapsed> {
    let mut fut = std::pin::pin!(fut);
    let mut sleep = std::pin::pin!(tokio::time::sleep_until(tokio::time::Instant::from_std(
        end
    )));
    std::future::poll_fn(|cx| {
        if Instant::now() >= end {
            return std::task::Poll::Ready(Err(Elapsed));
        }
        if let std::task::Poll::Ready(v) = fut.as_mut().poll(cx) {
            return std::task::Poll::Ready(Ok(v));
        }
        if sleep.as_mut().poll(cx).is_ready() {
            return std::task::Poll::Ready(Err(Elapsed));
        }
        std::task::Poll::Pending
    })
    .await
}

#[cfg(test)]
type Probe = Option<tokio::sync::mpsc::UnboundedReceiver<std::sync::mpsc::Sender<()>>>;
#[cfg(not(test))]
type Probe = ();

/// Answers one probe (tests); pending forever otherwise.
async fn next_probe(probe: &mut Probe) {
    #[cfg(test)]
    if let Some(rx) = probe
        && let Some(reply) = rx.recv().await
    {
        let _ = reply.send(());
        return;
    }
    let _ = probe;
    std::future::pending::<()>().await;
}

struct Coordinator {
    gate: Arc<ShutdownGate>,
    log: Logger,
    budgets: Budgets,
    instance: String,
    shared: Arc<startup::Shared>,
    probe: Probe,
    #[cfg(test)]
    faults: Arc<super::faults::Faults>,
}

/// What the coordinator observed of the startup task.
#[derive(Default)]
struct Observed {
    started: Option<Started>,
    failure: Option<Failure>,
    panicked: bool,
}

impl Observed {
    fn take(&mut self, gate: &ShutdownGate, r: Result<Started, tokio::task::JoinError>) {
        match r {
            Ok(s) => {
                if let Outcome::Failed(f) = &s.outcome {
                    gate.request(StopCause::StartupFailed(f.reason));
                    self.failure = Some(f.clone());
                }
                self.started = Some(s);
            }
            Err(_) => {
                gate.request(StopCause::Panic);
                self.panicked = true;
            }
        }
    }
}

impl Coordinator {
    #[cfg(test)]
    fn mark(&self, what: &str, end: Option<Instant>) {
        self.faults.mark(what, end);
    }

    #[cfg(not(test))]
    fn mark(&self, _what: &str, _end: Option<Instant>) {}

    /// Awaits `fut`, unless a console end is observed first; answers test probes meanwhile.
    async fn guarded<F: Future>(&mut self, fut: F) -> Result<F::Output, Crashed> {
        let mut fut = std::pin::pin!(fut);
        loop {
            if self.gate.crashed().is_some() {
                return Err(Crashed);
            }
            tokio::select! {
                biased;
                () = self.gate.woken() => {}
                out = &mut fut => {
                    if self.gate.crashed().is_some() {
                        return Err(Crashed);
                    }
                    return Ok(out);
                }
                () = next_probe(&mut self.probe) => {}
            }
        }
    }

    fn finish(&self, report: ExitReport) -> ExitReport {
        let reason = report.reason.map_or("clean", FailReason::as_str);
        self.log
            .info(&format!("exiting: code {} ({reason})", report.code.code()));
        self.log.flush(self.budgets.log_flush);
        report
    }

    fn crash_exit(&self, lock: LockDisposition) -> ExitReport {
        #[cfg(test)]
        if let Some(p) = self.faults.parked(super::faults::Point::CrashExit) {
            p.hold_blocking();
        }
        self.finish(ExitReport::failed(FailReason::ConsoleEnd, lock))
    }

    /// The wait between removal attempts, never past `end` (a test may lengthen it and observe
    /// its first poll).
    fn record_wait(&self, end: Instant) -> impl Future<Output = ()> + use<> {
        #[cfg(test)]
        let (wait, faults) = (
            self.faults
                .record_retry
                .lock()
                .ok()
                .and_then(|w| *w)
                .unwrap_or(RECORD_RETRY),
            self.faults.clone(),
        );
        #[cfg(not(test))]
        let wait = RECORD_RETRY;
        let wait = wait.min(end.saturating_duration_since(Instant::now()));
        async move {
            #[cfg(test)]
            faults.record_sleeping.store(true, Ordering::SeqCst);
            tokio::time::sleep(wait).await;
        }
    }

    /// Whether the startup join's end (`C − startup_join_before`) has passed.
    fn join_ended(&self) -> bool {
        self.gate
            .stopping()
            .is_some_and(|s| Instant::now() >= s.cutoff - self.budgets.startup_join_before)
    }

    /// The bare exit of a startup task not joined by its end (design §2.8 step 1).
    fn unjoined(&self) -> ExitReport {
        self.log.error(
            "the startup did not reach a step boundary in time: exiting with the instance lock held",
        );
        self.finish(ExitReport::failed(
            FailReason::StartupUnjoined,
            LockDisposition::HeldUntilExit,
        ))
    }

    /// The lock as a crash exit before the lock decision leaves it: held when startup tried
    /// to take it, otherwise never taken.
    fn unreleased(&self) -> LockDisposition {
        if self.shared.lock_attempted.load(Ordering::SeqCst) {
            LockDisposition::HeldUntilExit
        } else {
            LockDisposition::NeverTaken
        }
    }

    /// Removes our record, retrying for a bounded time (Windows refuses the removal while
    /// another process holds the file without delete sharing); a record left behind is named
    /// in the log. Every attempt first re-reads the record and removes it only while it still
    /// names this run; a re-read that fails for now is retried the same way, and a console end
    /// ends the retries at once.
    async fn remove_record(&mut self, dir: &std::path::Path) -> Result<(), Crashed> {
        let path = dir.join(names::RECORD);
        let path = path.as_path();
        let end = Instant::now() + RECORD_REMOVAL;
        // Why the last attempt failed; once the budget is spent it is the warning.
        let mut failed: Option<String> = None;
        loop {
            if self.gate.crashed().is_some() {
                return Err(Crashed);
            }
            if let Some(why) = &failed
                && Instant::now() >= end
            {
                self.log.warn(why);
                return Ok(());
            }
            #[cfg(test)]
            let read = self
                .faults
                .record_verify()
                .and_then(|()| files::read_record(dir));
            #[cfg(not(test))]
            let read = files::read_record(dir);
            match read {
                Ok(Some(r)) if r.instance == self.instance => {}
                Ok(_) => return Ok(()),
                // Not a record of ours to judge: malformed, of another version, or not a
                // regular file.
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::InvalidData | std::io::ErrorKind::InvalidInput
                    ) =>
                {
                    return Ok(());
                }
                // Unreadable for now (a Windows sharing violation): retried like a removal.
                Err(e) => {
                    failed = Some(format!(
                        "the discovery record {} could not be verified for removal: {e}",
                        path.display()
                    ));
                    self.guarded(self.record_wait(end)).await?;
                    continue;
                }
            }
            #[cfg(test)]
            if let Some(p) = self.faults.parked(super::faults::Point::AfterRecordVerify) {
                p.hold().await;
            }
            // The last boundary: a console end observed by now issues no deletion, nor does
            // the end of the budget.
            if self.gate.crashed().is_some() {
                return Err(Crashed);
            }
            if Instant::now() >= end {
                self.log.warn(&format!(
                    "the discovery record {} was left in place: its removal did not start within {} ms",
                    path.display(),
                    RECORD_REMOVAL.as_millis()
                ));
                return Ok(());
            }
            #[cfg(test)]
            let removed = self
                .faults
                .record_remove()
                .and_then(|()| std::fs::remove_file(path));
            #[cfg(not(test))]
            let removed = std::fs::remove_file(path);
            match removed {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => {
                    failed = Some(format!(
                        "the discovery record {} could not be removed: {e}",
                        path.display()
                    ));
                    #[cfg(test)]
                    if let Some(p) = self.faults.parked(super::faults::Point::RecordRetry) {
                        p.hold().await;
                    }
                    self.guarded(self.record_wait(end)).await?;
                }
            }
        }
    }

    /// Rewrites our record with the `stopping` state on a blocking thread, awaited until `end`
    /// and guarded: a console end observed by now starts no rewrite, and one observed during it
    /// ends the wait. A failed or unfinished rewrite is logged and shutdown goes on; the thread
    /// itself stops at `end` ([`mark_stopping`]).
    async fn publish_stopping(
        &mut self,
        dir: std::path::PathBuf,
        end: Instant,
    ) -> Result<(), Crashed> {
        #[cfg(test)]
        if let Some(p) = self.faults.parked(super::faults::Point::BeforeStopping) {
            p.hold().await;
        }
        if self.gate.crashed().is_some() {
            return Err(Crashed);
        }
        self.mark("stopping", Some(end));
        let instance = self.instance.clone();
        #[cfg(test)]
        let faults = self.faults.clone();
        let rewrite = tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            if let Some(p) = faults.parked(super::faults::Point::InsideStopping) {
                p.hold_blocking();
            }
            let hook = move |n: &str, s: files::ReplaceStage| -> std::io::Result<()> {
                #[cfg(test)]
                {
                    faults.replace_hook(n, s)
                }
                #[cfg(not(test))]
                {
                    files::no_hook(n, s)
                }
            };
            mark_stopping(&dir, &instance, end, &|| {}, &hook)
        });
        match self.guarded(deadline_first(end, rewrite)).await? {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(why))) => self.log.warn(&why),
            Ok(Err(e)) => self.log.warn(&format!(
                "the discovery record could not be marked stopping: {e}"
            )),
            Err(Elapsed) => self.log.warn(&format!(
                "the discovery record was not marked stopping within {} ms",
                STOPPING_REWRITE.as_millis()
            )),
        }
        Ok(())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the shutdown sequence of design §2.8 in order, one place"
    )]
    async fn run(
        mut self,
        cfg: DaemonConfig,
        components: Box<dyn Components>,
        env: RunEnv,
    ) -> ExitReport {
        let detached = matches!(cfg.mode, RunMode::DetachedChild { .. });
        let mut seen = Observed::default();
        let mut task: Option<JoinHandle<Started>> = None;
        // Step 3: the signal listeners (process-wide, `run` only).
        let listeners = if env.process {
            signals::start(&self.gate)
        } else {
            Ok(())
        };
        match listeners {
            Err(e) => {
                let mut s = Started::empty();
                let f = Failure {
                    reason: FailReason::Internal,
                    message: format!("the signal listeners cannot start: {e}"),
                };
                s.outcome = Outcome::Failed(f.clone());
                self.log.error(&f.message);
                self.gate.request(StopCause::StartupFailed(f.reason));
                seen.failure = Some(f);
                seen.started = Some(s);
            }
            Ok(()) => {
                if env.process && detached {
                    // Nothing to write to until step 6 opens `daemon.log`.
                    let _ = self.log.switch(LogTarget::Discard, self.budgets.log_flush);
                }
                #[cfg(test)]
                let returned = env.faults.clone();
                let startup = startup::run(StartupCtx {
                    state_dir: cfg.state_dir,
                    port: cfg.port,
                    mode: cfg.mode,
                    budgets: cfg.budgets,
                    components,
                    gate: self.gate.clone(),
                    instance: self.instance.clone(),
                    log: self.log.clone(),
                    shared: self.shared.clone(),
                    evidence: files::Evidence::default(),
                    process: env.process,
                    #[cfg(test)]
                    faults: env.faults.clone(),
                });
                task = Some(tokio::spawn(async move {
                    let st = startup.await;
                    #[cfg(test)]
                    let _ = returned.startup_returned.set(Instant::now());
                    st
                }));
            }
        }

        // Wait for the first request, taking the startup task's return meanwhile.
        loop {
            if self.gate.crashed().is_some() {
                return self.crash_exit(self.unreleased());
            }
            if self.gate.fired() {
                break;
            }
            let gate = self.gate.clone();
            match task.as_mut() {
                Some(t) => {
                    #[cfg(test)]
                    if let Some(p) = self.faults.parked(super::faults::Point::CoordinatorWaiting) {
                        p.hold_blocking();
                        // The test released the startup task first: wait until its join is
                        // ready, so the select sees both branches.
                        let end = Instant::now() + std::time::Duration::from_secs(10);
                        while !t.is_finished() && Instant::now() < end {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                    }
                    tokio::select! {
                        biased;
                        r = t => {
                            task = None;
                            #[cfg(test)]
                            self.faults.first_wait_took_startup.store(true, Ordering::SeqCst);
                            // As in step 1: a console end first, then the join's end (a stop
                            // may have fired while this loop was not polled).
                            if self.gate.crashed().is_some() {
                                return self.crash_exit(self.unreleased());
                            }
                            if self.join_ended() {
                                return self.unjoined();
                            }
                            seen.take(&gate, r);
                        }
                        () = gate.woken() => {}
                    }
                }
                None => gate.woken().await,
            }
        }
        let cutoff = self
            .gate
            .stopping()
            .map_or_else(|| Instant::now() + self.budgets.total, |s| s.cutoff);
        let b = self.budgets;

        // 1. Join the startup task until C − 6 s; a missed join is a bare exit.
        if let Some(t) = task.take() {
            let end = cutoff - b.startup_join_before;
            self.mark("startup_join", Some(end));
            #[cfg(test)]
            if let Some(p) = self
                .faults
                .parked(super::faults::Point::CoordinatorBeforeJoin)
            {
                p.hold_blocking();
            }
            match self.guarded(deadline_first(end, t)).await {
                Err(Crashed) => return self.crash_exit(self.unreleased()),
                Ok(Err(Elapsed)) => return self.unjoined(),
                Ok(Ok(r)) => seen.take(&self.gate, r),
            }
        }
        let mut st = seen.started.take().unwrap_or_else(Started::empty);
        let lost_lock = seen.panicked && self.shared.lock_attempted.load(Ordering::SeqCst);
        let held_now = move |st: &Started| {
            if st.lock.is_some() || lost_lock {
                LockDisposition::HeldUntilExit
            } else {
                LockDisposition::NeverTaken
            }
        };

        // 2. The record says `stopping` before HTTP shuts down, so no client takes this run
        // for one that serves once its surface may be gone; the rewrite takes at most 500 ms
        // of HTTP's budget and never runs past it. Then HTTP until C − 5 s, awaited
        // to its return; also a server that never served, whose registry can hold startup's
        // own writes (the owner token's observer).
        if self.shared.record_published.load(Ordering::SeqCst)
            && let Some(dir) = self.shared.dir.get().cloned()
        {
            let end = (Instant::now() + STOPPING_REWRITE).min(cutoff - b.http_before);
            if self.publish_stopping(dir, end).await.is_err() {
                return self.crash_exit(held_now(&st));
            }
        }
        let mut http_aborted = false;
        if let Some(serve) = st.serve.take() {
            let end = cutoff - b.http_before;
            self.mark("http", Some(end));
            match self.guarded(serve.shutdown(end)).await {
                Err(Crashed) => return self.crash_exit(held_now(&st)),
                Ok(r) => {
                    http_aborted = r.abort_requested.total() > 0;
                    self.log.info(&format!(
                        "http shutdown: {} exited, {} abort requested",
                        r.exited.total(),
                        r.abort_requested.total()
                    ));
                }
            }
        }

        #[cfg(test)]
        if let Some(p) = self.faults.parked(super::faults::Point::AfterHttp) {
            p.hold().await;
        }

        // 3. The participants in order, until C − 3 s.
        let participants = std::mem::take(&mut st.participants);
        let end = cutoff - b.store_before;
        for p in &participants {
            if self.gate.crashed().is_some() {
                return self.crash_exit(held_now(&st));
            }
            self.mark(&format!("participant:{}", p.name()), Some(end));
            let fut = tokio::time::timeout_at(tokio::time::Instant::from_std(end), p.shutdown(end));
            if self.guarded(fut).await.is_err() {
                return self.crash_exit(held_now(&st));
            }
        }
        drop(participants);

        // 4. The store on a blocking thread with the cutoff, observed until C + 250 ms.
        let mut store_report: Option<ShutdownReport> = None;
        let store_opened = st.store.is_some();
        if let Some(store) = st.store.take() {
            #[cfg(test)]
            if let Some(p) = self.faults.parked(super::faults::Point::BeforeStore) {
                p.hold().await;
            }
            if self.gate.crashed().is_some() {
                return self.crash_exit(held_now(&st));
            }
            self.mark("store", Some(cutoff));
            #[cfg(test)]
            let faults = self.faults.clone();
            let h = tokio::task::spawn_blocking(move || {
                #[cfg(test)]
                let _ = faults
                    .store_shutdown_thread
                    .set(std::thread::current().id());
                let r = store.shutdown(cutoff);
                #[cfg(test)]
                let _ = faults.store_shutdown_done.set(Instant::now());
                r
            });
            #[cfg(test)]
            if let Some(p) = self
                .faults
                .parked(super::faults::Point::CoordinatorBeforeStoreWait)
            {
                p.hold_blocking();
            }
            match self
                .guarded(deadline_first(cutoff + b.report_grace, h))
                .await
            {
                Err(Crashed) => return self.crash_exit(held_now(&st)),
                Ok(Ok(Ok(r))) => {
                    self.log.info(&format!(
                        "store shutdown: readers_returned {}, writer_exited {}, checkpoint {:?}",
                        r.readers_returned, r.writer_exited, r.checkpoint
                    ));
                    store_report = Some(r);
                }
                Ok(_) => self
                    .log
                    .warn("the store's shutdown report was not seen by its deadline"),
            }
        }
        // The server and its store clone go only after the store's shutdown.
        drop(st.server.take());
        if self.gate.crashed().is_some() {
            return self.crash_exit(held_now(&st));
        }

        // 5. Remove our record, never another instance's, and only when this run published one.
        if self.shared.record_published.load(Ordering::SeqCst)
            && let Some(dir) = self.shared.dir.get().cloned()
            && self.remove_record(&dir).await.is_err()
        {
            return self.crash_exit(held_now(&st));
        }

        // 6. The lock rule, read once.
        let lock = match st.lock.take() {
            None => held_now(&st),
            Some(l) => {
                let mut why = Vec::new();
                if http_aborted {
                    why.push("an HTTP task did not end");
                }
                if store_opened
                    && !store_report
                        .as_ref()
                        .is_some_and(|r| r.readers_returned && r.writer_exited)
                {
                    why.push("the store did not report its readers returned and its writer exited");
                }
                if st.log_file {
                    #[cfg(test)]
                    if let Some(p) = self.faults.parked(super::faults::Point::BeforeLogSwitch) {
                        p.hold().await;
                    }
                    if self.gate.crashed().is_some() {
                        drop(l);
                        return self.crash_exit(LockDisposition::HeldUntilExit);
                    }
                    #[cfg(test)]
                    self.faults.log_switch_queued.store(true, Ordering::SeqCst);
                    #[cfg(test)]
                    let after = env
                        .test_log
                        .clone()
                        .map_or(LogTarget::Discard, LogTarget::Test);
                    #[cfg(not(test))]
                    let after = LogTarget::Discard;
                    if !self.log.switch(after, b.log_flush) {
                        why.push("the log thread did not acknowledge leaving daemon.log");
                    }
                    #[cfg(test)]
                    if let Some(p) = self.faults.parked(super::faults::Point::AfterLogSwitch) {
                        p.hold().await;
                    }
                }
                if self.gate.crashed().is_some() {
                    why.push("a console end arrived before the release");
                }
                if why.is_empty() {
                    l.release();
                    LockDisposition::Released
                } else {
                    self.log.warn(&format!(
                        "exiting with the instance lock held ({})",
                        why.join("; ")
                    ));
                    drop(l);
                    LockDisposition::HeldUntilExit
                }
            }
        };
        #[cfg(test)]
        if let Some(p) = self.faults.parked(super::faults::Point::AfterLockDecision) {
            p.hold().await;
        }
        if self.gate.crashed().is_some() {
            return self.crash_exit(lock);
        }

        // 7. The outcome; a detached child that never sent `ready` gets its verdict now.
        let cause = self.gate.stopping().map(|s| s.cause.clone());
        let worst = self.gate.worst();
        let reason = if worst >= 2 || seen.panicked || matches!(cause, Some(StopCause::Panic)) {
            Some(FailReason::Panic)
        } else if worst >= 1 || matches!(cause, Some(StopCause::StoreFatal(_))) {
            Some(FailReason::StoreFatal)
        } else if let Some(f) = &seen.failure {
            Some(f.reason)
        } else if lock == LockDisposition::HeldUntilExit {
            Some(FailReason::LockHeldAtExit)
        } else {
            None
        };
        if let Some(f) = &seen.failure {
            self.log.error(&f.message);
        }
        let saved_notice = st.port_saved.zip(self.shared.dir.get()).map(|(p, d)| {
            format!(
                "port {p} was saved for {} before the start ended; the next start uses it, so re-pair with `chaldea open`",
                d.display()
            )
        });
        if st.outcome != Outcome::Ok {
            if let Some(n) = &saved_notice {
                self.log.warn(n);
            }
            if let Some(mut w) = st.ready.take() {
                let token = reason.map_or("stopped", FailReason::as_str);
                let mut message = match (&seen.failure, reason) {
                    (Some(f), _) => f.message.clone(),
                    (None, None) => "stopped before it became ready".to_owned(),
                    (None, Some(r)) => format!("the daemon ended: {r}"),
                };
                if let Some(n) = &saved_notice {
                    message = format!("{n}. {message}");
                }
                let _ = w.write_line(&fail_line(token, st.port_saved, &message));
                w.close();
            }
        }
        let code = match reason {
            None => ExitCode::Ok,
            Some(FailReason::AlreadyRunning) => ExitCode::AlreadyRunning,
            Some(_) => ExitCode::Failed,
        };
        drop(st.stop_listener.take());
        self.finish(ExitReport { code, reason, lock })
    }
}

/// Rewrites the record in `dir` with the `stopping` state if it is `instance`'s; a record that
/// is missing or another instance's is left alone. `Err`: why it was not rewritten.
///
/// Nothing starts at or after `end`: not the read, not the temporary file, not a rename
/// attempt or its retry. A call already issued at `end` may finish after it. `after_read`
/// runs between the read and the creation of the temporary file.
pub(super) fn mark_stopping(
    dir: &std::path::Path,
    instance: &str,
    end: Instant,
    after_read: &dyn Fn(),
    hook: files::ReplaceHook<'_>,
) -> Result<(), String> {
    let late = || {
        format!(
            "the discovery record was not marked stopping: its {} ms ended",
            STOPPING_REWRITE.as_millis()
        )
    };
    if Instant::now() >= end {
        return Err(late());
    }
    let mut r = match files::read_record(dir) {
        Ok(Some(r)) if r.instance == instance => r,
        Ok(_) => return Ok(()),
        Err(e) => {
            return Err(format!(
                "the discovery record could not be read to mark it stopping: {e}"
            ));
        }
    };
    after_read();
    if Instant::now() >= end {
        return Err(late());
    }
    r.state = files::RecordState::Stopping;
    files::write_replace(dir, names::RECORD, &r.to_bytes(), instance, Some(end), hook)
        .map(drop)
        .map_err(|e| format!("the discovery record could not be marked stopping: {e}"))
}
