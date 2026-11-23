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
) -> ExitReport {
    let log = env.log.clone();
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
    let coordinator = Coordinator {
        gate: gate.clone(),
        log: log.clone(),
        budgets,
        instance: uuid::Uuid::new_v4().to_string(),
        shared: Arc::new(startup::Shared::default()),
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
            ExitReport::failed(FailReason::Panic, LockDisposition::HeldUntilExit)
        });
    // Blocking tasks (an abandoned `Store::shutdown` or `Store::open`) keep running until the
    // process exits; neither a return nor an unwind waits for them.
    ManuallyDrop::into_inner(rt).shutdown_background();
    report
}

/// A console end was observed: no further step starts.
struct Crashed;

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
        self.finish(ExitReport::failed(FailReason::ConsoleEnd, lock))
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
                task = Some(tokio::spawn(startup::run(StartupCtx {
                    state_dir: cfg.state_dir,
                    port: cfg.port,
                    mode: cfg.mode,
                    budgets: cfg.budgets,
                    components,
                    gate: self.gate.clone(),
                    instance: self.instance.clone(),
                    log: self.log.clone(),
                    shared: self.shared.clone(),
                    #[cfg(test)]
                    faults: env.faults.clone(),
                })));
            }
        }

        // Wait for the first request, taking the startup task's return meanwhile.
        loop {
            if self.gate.crashed().is_some() {
                return self.crash_exit(LockDisposition::HeldUntilExit);
            }
            if self.gate.fired() {
                break;
            }
            let gate = self.gate.clone();
            match task.as_mut() {
                Some(t) => {
                    tokio::select! {
                        r = t => {
                            task = None;
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
            match self
                .guarded(tokio::time::timeout_at(
                    tokio::time::Instant::from_std(end),
                    t,
                ))
                .await
            {
                Err(Crashed) => return self.crash_exit(LockDisposition::HeldUntilExit),
                Ok(Err(_elapsed)) => {
                    self.log.error(
                        "the startup did not reach a step boundary in time: exiting with the instance lock held",
                    );
                    return self.finish(ExitReport::failed(
                        FailReason::StartupUnjoined,
                        LockDisposition::HeldUntilExit,
                    ));
                }
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

        // 2. HTTP until C − 5 s, awaited to its return.
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

        // 3. The participants in order, until C − 3 s.
        let participants = std::mem::take(&mut st.participants);
        let end = cutoff - b.store_before;
        for p in &participants {
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
            self.mark("store", Some(cutoff));
            #[cfg(test)]
            let faults = self.faults.clone();
            let h = tokio::task::spawn_blocking(move || {
                #[cfg(test)]
                let _ = faults
                    .store_shutdown_thread
                    .set(std::thread::current().id());
                store.shutdown(cutoff)
            });
            let until = tokio::time::Instant::from_std(cutoff + b.report_grace);
            match self.guarded(tokio::time::timeout_at(until, h)).await {
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

        // 5. Remove our record, never another instance's.
        if let Some(dir) = self.shared.dir.get()
            && let Ok(Some(r)) = files::read_record(dir)
            && r.instance == self.instance
        {
            let _ = std::fs::remove_file(dir.join(names::RECORD));
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
                    let after = env
                        .test_log
                        .clone()
                        .map_or(LogTarget::Discard, LogTarget::Test);
                    #[cfg(not(test))]
                    let after = LogTarget::Discard;
                    if !self.log.switch(after, b.log_flush) {
                        why.push("the log thread did not acknowledge leaving daemon.log");
                    }
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
        if self.gate.crashed().is_some() {
            return self.crash_exit(if lock == LockDisposition::Released {
                LockDisposition::Released
            } else {
                LockDisposition::HeldUntilExit
            });
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
