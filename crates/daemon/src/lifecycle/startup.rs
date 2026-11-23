//! The startup task (design §2.7): steps in a fixed order, each checked against the gate at
//! its boundary; it owns everything it creates and hands it to the coordinator only by
//! returning it. It is never aborted: a stop takes effect at the next step boundary, and a
//! failing step returns at once without deciding the shutdown itself.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use chaldea_pty_runtime::host::{self, StopListener};

use super::files::{
    self, Discovery, DiscoveryRecord, Evidence, InstanceLock, LockError, RecordState, ReplaceStage,
    TokenFile, TokenFileError,
};
use super::gate::{GateHandle, GateSink, ShutdownGate, StopCause};
use super::log::{LogFile, LogTarget, Logger};
use super::ready::{ReadyWriter, ready_line};
use super::{
    Budgets, BuildCtx, Components, FailReason, LOCK_RETRY, LOG_CAP, PORT_RANGE, RunMode,
    ShutdownParticipant, names, state_dir,
};
use crate::http::{
    AuthFailure, HttpConfig, HttpServer, LifecycleSink, ServeHandle, StoreFatal, verify_secret_in,
};
use crate::store::{
    Clock, RandomIds, Store, StoreConfig, SubmitError, SystemClock, TokenKind, WriteError,
};

/// A reconciler's admission deadline (design §2.7 step 9).
const RECONCILER_ADMISSION: Duration = Duration::from_secs(30);
/// The owner token's issue deadline and its verifying read's wait (design §2.13).
const TOKEN_DEADLINE: Duration = Duration::from_secs(5);

/// How the startup task ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Serving.
    Ok,
    Failed(Failure),
    /// The gate fired before it became ready.
    Stopped,
}

/// A failing step's reason and its message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failure {
    pub(crate) reason: FailReason,
    pub(crate) message: String,
}

/// Everything the startup task created, returned to the coordinator.
pub(crate) struct Started {
    pub(crate) outcome: Outcome,
    pub(crate) ready: Option<Box<dyn ReadyWriter>>,
    pub(crate) lock: Option<InstanceLock>,
    pub(crate) store: Option<Store>,
    pub(crate) server: Option<HttpServer>,
    /// Drains HTTP's registry: from step 11 (the server exists); serving from step 13.
    pub(crate) serve: Option<ServeHandle>,
    pub(crate) participants: Vec<Box<dyn ShutdownParticipant>>,
    /// The `port` rename of step 13 committed this port.
    pub(crate) port_saved: Option<u16>,
    /// The log target is `daemon.log`.
    pub(crate) log_file: bool,
    pub(crate) stop_listener: Option<StopListener>,
}

impl Started {
    pub(crate) fn empty() -> Started {
        Started {
            outcome: Outcome::Stopped,
            ready: None,
            lock: None,
            store: None,
            server: None,
            serve: None,
            participants: Vec::new(),
            port_saved: None,
            log_file: false,
            stop_listener: None,
        }
    }

    fn failed(mut self, reason: FailReason, message: impl Into<String>) -> Started {
        self.outcome = Outcome::Failed(Failure {
            reason,
            message: message.into(),
        });
        self
    }
}

/// What the coordinator learns without the task's return (it may have panicked).
#[derive(Default)]
pub(crate) struct Shared {
    /// Step 5 began: the lock may be held by the task.
    pub(crate) lock_attempted: AtomicBool,
    /// The canonical state directory, once step 4 accepted it.
    pub(crate) dir: OnceLock<PathBuf>,
    /// A record of this run was renamed into place: step 5 of the shutdown may remove it.
    pub(crate) record_published: AtomicBool,
}

pub(crate) struct StartupCtx {
    pub(crate) state_dir: PathBuf,
    pub(crate) port: Option<u16>,
    pub(crate) mode: RunMode,
    pub(crate) budgets: Budgets,
    pub(crate) components: Box<dyn Components>,
    pub(crate) gate: Arc<ShutdownGate>,
    pub(crate) instance: String,
    pub(crate) log: Logger,
    pub(crate) shared: Arc<Shared>,
    /// Step 2's evidence, recorded in `daemon.json`.
    pub(crate) evidence: std::sync::Mutex<Evidence>,
    /// A process run (`run`, a helper's `run_process_with`): step 1 may leave the session and
    /// step 2 reads the process's job; an isolated run does neither.
    pub(crate) process: bool,
    #[cfg(test)]
    pub(crate) faults: Arc<super::faults::Faults>,
}

/// The step boundary: parks (tests), then returns `Stopped` if the gate fired or a console end
/// arrived (which does not fire the gate; the coordinator is exiting).
macro_rules! boundary {
    ($cx:expr, $st:ident, $n:expr) => {
        #[cfg(test)]
        if let Some(p) = $cx.faults.parked(super::faults::Point::Before($n)) {
            p.hold().await;
        }
        if $cx.gate.fired() || $cx.gate.crashed().is_some() {
            $st.outcome = Outcome::Stopped;
            return $st;
        }
    };
}

/// Unwraps a blocking task's result, resuming its panic in this task.
fn joined<T>(r: Result<T, tokio::task::JoinError>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => std::panic::resume_unwind(e.into_panic()),
    }
}

impl StartupCtx {
    fn replace(&self, dir: &Path, name: &str, bytes: &[u8]) -> Result<files::Replaced, String> {
        #[cfg(test)]
        let faults = self.faults.clone();
        let hook = move |n: &str, s: ReplaceStage| -> std::io::Result<()> {
            #[cfg(test)]
            {
                faults.replace_hook(n, s)
            }
            #[cfg(not(test))]
            {
                files::no_hook(n, s)
            }
        };
        match files::write_replace(dir, name, bytes, &self.instance, &hook) {
            Ok(r) => {
                if let Some(w) = &r.durability_warning {
                    self.log.warn(w);
                }
                Ok(r)
            }
            Err(e) => Err(format!(
                "{} cannot be written: {e}",
                dir.join(name).display()
            )),
        }
    }

    fn record(&self, state: RecordState, port: Option<u16>) -> Result<DiscoveryRecord, String> {
        let me = host::current_identity().map_err(|e| format!("own identity: {e}"))?;
        let detached = match &self.mode {
            RunMode::Foreground => None,
            RunMode::DetachedChild { via, .. } => Some(via.as_str().to_owned()),
        };
        Ok(DiscoveryRecord {
            version: files::RECORD_VERSION,
            instance: self.instance.clone(),
            pid: me.pid,
            start_time: me.start_time,
            state,
            port,
            url: port.map(url),
            started_at: SystemClock.now().1.0,
            detached,
            evidence: self.evidence.lock().map(|e| e.clone()).unwrap_or_default(),
        })
    }
}

/// Step 2's job state: the process's for a process run; a test's override, else in no job,
/// for an isolated run.
fn job_state(cx: &StartupCtx) -> std::io::Result<host::JobState> {
    #[cfg(test)]
    if let Some(j) = cx.faults.job_state.lock().ok().and_then(|j| *j) {
        return Ok(j);
    }
    if cx.process {
        host::job_state()
    } else {
        Ok(host::JobState::NONE)
    }
}

/// `evidence.job` of `daemon.json`.
fn job_evidence(job: &host::JobState) -> serde_json::Value {
    serde_json::json!({
        "in_job": job.in_job,
        "immediate": job.immediate.map(|l| serde_json::json!({
            "kill_on_close": l.kill_on_close,
            "breakaway_ok": l.breakaway_ok,
            "silent_breakaway_ok": l.silent_breakaway_ok,
        })),
    })
}

fn job_text(job: &host::JobState) -> String {
    match job.immediate {
        Some(l) => format!(
            "immediate job: kill_on_close={}, breakaway_ok={}, silent_breakaway_ok={}",
            l.kill_on_close, l.breakaway_ok, l.silent_breakaway_ok
        ),
        None => "its limits are not visible".to_owned(),
    }
}

fn url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

fn already_running_message(dir: &Path) -> String {
    let who = match files::discover(dir) {
        Discovery::Serving(r) => format!(
            "pid {}, {}",
            r.pid,
            r.url.unwrap_or_else(|| "starting".to_owned())
        ),
        Discovery::Starting(r) => format!("pid {}, starting", r.pid),
        Discovery::NotRunning | Discovery::Unknown => "starting".to_owned(),
    };
    format!("a daemon already holds {} ({who})", dir.display())
}

/// Step 10: the port to bind and the listener.
fn bind(
    requested: Option<u16>,
    saved: Option<u16>,
    dir: &Path,
    #[cfg(test)] faults: &super::faults::Faults,
) -> Result<(std::net::TcpListener, u16), String> {
    let try_bind = |p: u16| -> std::io::Result<std::net::TcpListener> {
        #[cfg(test)]
        faults.bind_attempts.fetch_add(1, Ordering::SeqCst);
        #[cfg(test)]
        if faults.bind_fails() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                "injected bind failure",
            ));
        }
        let l = std::net::TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, p))?;
        l.set_nonblocking(true)?;
        Ok(l)
    };
    if requested == Some(0) {
        return Err("port 0 is not a valid port; pass `--port <n>` with n in 1–65535".to_owned());
    }
    if let Some(p) = requested {
        return try_bind(p)
            .map(|l| (l, p))
            .map_err(|e| format!("port {p} is unavailable: {e}"));
    }
    if let Some(p) = saved {
        return try_bind(p).map(|l| (l, p)).map_err(|e| {
            format!(
                "port {p} (saved for {}) is unavailable: {e}; `--port <m>` starts on another port, which changes the pairing origin (re-run `chaldea open` after it)",
                dir.display()
            )
        });
    }
    let mut last = None;
    for p in PORT_RANGE {
        match try_bind(p) {
            Ok(l) => return Ok((l, p)),
            Err(e) => last = Some(e),
        }
    }
    Err(format!(
        "no free port in {}–{} on 127.0.0.1 (last error: {}); pass `--port <n>`",
        PORT_RANGE.start(),
        PORT_RANGE.end(),
        last.map_or_else(|| "none".to_owned(), |e| e.to_string())
    ))
}

/// Maps a reconciler's or a token write's store outcome: fatal ones also request the gate.
fn fatal(gate: &ShutdownGate, f: StoreFatal) -> FailReason {
    gate.request(StopCause::StoreFatal(f));
    FailReason::StoreFatal
}

/// The startup task.
#[allow(
    clippy::too_many_lines,
    reason = "the steps of design §2.7 in order, one place"
)]
pub(crate) async fn run(mut cx: StartupCtx) -> Started {
    let mut st = Started::empty();
    let detached = matches!(cx.mode, RunMode::DetachedChild { .. });
    // Step 1: a detached child holds its readiness writer before anything else.
    if let RunMode::DetachedChild { ready, via } =
        std::mem::replace(&mut cx.mode, RunMode::Foreground)
    {
        match ready() {
            Ok(w) => st.ready = Some(w),
            Err(e) => {
                return st.failed(
                    FailReason::Internal,
                    format!("the readiness channel cannot be opened: {e}"),
                );
            }
        }
        // Keep the mode (for the record's `detached`) without the consumed opener.
        cx.mode = RunMode::DetachedChild {
            ready: Box::new(|| Err(std::io::Error::other("already opened"))),
            via,
        };
        // Both macOS paths leave the starter's (or the job's) session here, after the
        // readiness writer exists to report a failure.
        if cfg!(target_os = "macos")
            && cx.process
            && let Err(e) = host::detach_session()
        {
            return st.failed(
                FailReason::Internal,
                format!("the daemon cannot leave the terminal's session (setsid): {e}"),
            );
        }
    }

    // Step 2: the parent job (Windows, design §2.12). The child decides: a detached daemon
    // must be in no job; a foreground one in a job warns and serves.
    boundary!(cx, st, 2);
    match job_state(&cx) {
        Ok(job) => {
            if let Ok(mut e) = cx.evidence.lock() {
                if cfg!(windows) || job.in_job {
                    e.job = Some(job_evidence(&job));
                }
                e.console = host::has_console();
            }
            if job.in_job {
                if detached {
                    return st.failed(
                        FailReason::ParentJob,
                        format!(
                            "this daemon runs inside a job ({}), which could kill it when the job closes; run `chaldea daemon run` in a window you keep open, or start Chaldea from a terminal outside such a job",
                            job_text(&job)
                        ),
                    );
                }
                cx.log.warn(&format!(
                    "this daemon runs inside a job ({}) and may be killed with it, with its sessions",
                    job_text(&job)
                ));
            }
        }
        Err(e) if detached => {
            return st.failed(
                FailReason::ParentJob,
                format!("the daemon's job state cannot be read, so it cannot prove it runs in no job: {e}"),
            );
        }
        Err(e) => cx.log.warn(&format!("the job state cannot be read: {e}")),
    }

    // Step 4: the state directory.
    boundary!(cx, st, 4);
    let dir = match state_dir::prepare(&cx.state_dir, &cx.instance) {
        Ok(d) => d,
        Err(r) => return st.failed(FailReason::StateDir, r.0),
    };
    let _ = cx.shared.dir.set(dir.clone());
    #[cfg(test)]
    let _ = cx.faults.dir.set(dir.clone());

    // Step 5: the single-instance lock.
    boundary!(cx, st, 5);
    cx.shared.lock_attempted.store(true, Ordering::SeqCst);
    let d = dir.clone();
    match joined(tokio::task::spawn_blocking(move || InstanceLock::acquire(&d, LOCK_RETRY)).await) {
        Ok(l) => st.lock = Some(l),
        Err(LockError::Held) => {
            return st.failed(FailReason::AlreadyRunning, already_running_message(&dir));
        }
        Err(LockError::Io(e)) => {
            return st.failed(
                FailReason::LockIo,
                format!("{} cannot be locked: {e}", dir.join(names::LOCK).display()),
            );
        }
    }

    // Step 6: every check before a further mutation; then a detached daemon's log.
    boundary!(cx, st, 6);
    let token = match files::check_token_file(&dir) {
        Ok(t) => t,
        Err(TokenFileError::NotPrivate(p)) => {
            return st.failed(
                FailReason::TokenFile,
                format!(
                    "owner.token is readable by others ({}); its token may be exposed. Removing the file does not revoke the token: revoke it, then remove the file",
                    p.describe()
                ),
            );
        }
        Err(TokenFileError::BadForm) => {
            return st.failed(
                FailReason::TokenFile,
                "owner.token is not a Chaldea token file; move it away",
            );
        }
        Err(TokenFileError::Io(e)) => {
            return st.failed(
                FailReason::TokenFile,
                format!("owner.token cannot be read: {e}"),
            );
        }
    };
    let saved = match files::read_port(&dir) {
        Ok(p) => p,
        Err(why) => {
            return st.failed(
                FailReason::PortFile,
                format!(
                    "{} is not a port number ({why}); move it away and start again to choose a new port",
                    dir.join(names::PORT).display()
                ),
            );
        }
    };
    for f in names::DB_FILES {
        match host::is_plain_file_or_absent(&dir.join(f)) {
            Ok(true) => match host::hard_links(&dir.join(f)) {
                Ok(n) if n > 1 => {
                    return st.failed(
                        FailReason::StoreOpen,
                        format!(
                            "{} has {n} hard links; the database must have one: with every writer of it stopped, make a consistent copy with SQLite's backup (`sqlite3 <db> \".backup <new>\"`), then move the copy into place as `chaldea.db` and remove the old names",
                            dir.join(f).display()
                        ),
                    );
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return st.failed(
                        FailReason::StoreOpen,
                        format!("{} cannot be inspected: {e}", dir.join(f).display()),
                    );
                }
            },
            Ok(false) => {
                return st.failed(
                    FailReason::StoreOpen,
                    format!(
                        "a link where the database should be: {}",
                        dir.join(f).display()
                    ),
                );
            }
            Err(e) => {
                return st.failed(
                    FailReason::StoreOpen,
                    format!("{} cannot be inspected: {e}", dir.join(f).display()),
                );
            }
        }
    }
    if detached {
        #[cfg(test)]
        let cap = cx
            .faults
            .log_cap
            .lock()
            .ok()
            .and_then(|c| *c)
            .unwrap_or(LOG_CAP);
        #[cfg(not(test))]
        let cap = LOG_CAP;
        match LogFile::open(dir.clone(), cap) {
            Ok(f) => {
                // Handed to the log thread from here on, acknowledged or not: the shutdown
                // must see the switch away acknowledged before it releases the lock.
                st.log_file = true;
                if !cx.log.switch(LogTarget::File(f), cx.budgets.log_flush) {
                    return st.failed(
                        FailReason::Internal,
                        "the log thread did not switch to daemon.log",
                    );
                }
            }
            Err(e) => {
                return st.failed(
                    FailReason::Internal,
                    format!("{} cannot be opened: {e}", dir.join(names::LOG).display()),
                );
            }
        }
    }

    // Step 7: the stop listener, then the `starting` record.
    boundary!(cx, st, 7);
    let stop_gate = cx.gate.clone();
    match host::listen_for_stop(
        &cx.instance,
        Box::new(move || {
            stop_gate.request(StopCause::StopRequest);
        }),
    ) {
        Ok(l) => st.stop_listener = Some(l),
        Err(e) => {
            return st.failed(
                FailReason::Internal,
                format!("the stop listener cannot start: {e}"),
            );
        }
    }
    let written = cx
        .record(RecordState::Starting, None)
        .and_then(|r| cx.replace(&dir, names::RECORD, &r.to_bytes()));
    if let Err(e) = written {
        return st.failed(FailReason::Internal, e);
    }
    cx.shared.record_published.store(true, Ordering::SeqCst);

    // Step 8: the store, awaited to its return (an open can rebuild projections).
    boundary!(cx, st, 8);
    let db = dir.join(names::DB);
    #[cfg(test)]
    let open_faults = (
        cx.faults.parked(super::faults::Point::Inside(8)),
        cx.faults.store_faults.lock().ok().and_then(|f| f.clone()),
    );
    let opened = tokio::task::spawn_blocking(move || {
        let cfg = StoreConfig {
            path: db,
            readers: 4,
            writer_queue: 256,
            busy_timeout: Duration::from_secs(5),
            clock: Arc::new(SystemClock),
            ids: Arc::new(RandomIds),
        };
        #[cfg(test)]
        {
            let (park, faults) = open_faults;
            if let Some(p) = park {
                p.hold_blocking();
            }
            if let Some(f) = faults {
                return crate::store::test_faults::open_with_shared(cfg, f);
            }
        }
        Store::open(cfg)
    })
    .await;
    let store = match joined(opened) {
        Ok(s) => s,
        Err(e) => {
            return st.failed(
                FailReason::StoreOpen,
                format!("the store cannot be opened: {e}"),
            );
        }
    };
    st.store = Some(store.clone());
    #[cfg(test)]
    let _ = cx.faults.store.set(store.clone());

    // Step 9: restart reconciliation, before the listener exists.
    boundary!(cx, st, 9);
    for r in cx.components.reconcilers(&store) {
        let name = r.name;
        let ticket = match store.submit(Instant::now() + RECONCILER_ADMISSION, r.job) {
            Ok(t) => t,
            Err(SubmitError::Failed) => {
                let reason = fatal(&cx.gate, StoreFatal::Failed);
                return st.failed(
                    reason,
                    format!("reconciler {name}: the store writer failed"),
                );
            }
            Err(e) => {
                return st.failed(FailReason::StoreOpen, format!("reconciler {name}: {e}"));
            }
        };
        match ticket.await {
            Ok(n) => cx.log.info(&format!("reconciled {name}: {n} rows")),
            Err(e) => {
                let reason = match &e {
                    WriteError::CommitUnknown(_) => fatal(&cx.gate, StoreFatal::CommitUnknown),
                    WriteError::WriterGone => fatal(&cx.gate, StoreFatal::WriterGone),
                    WriteError::Failed(_) => fatal(&cx.gate, StoreFatal::Failed),
                    _ => FailReason::StoreOpen,
                };
                return st.failed(reason, format!("reconciler {name}: {e}"));
            }
        }
    }

    // Step 10: choose and bind the port.
    boundary!(cx, st, 10);
    let (std_listener, port) = match bind(
        cx.port,
        saved,
        &dir,
        #[cfg(test)]
        &cx.faults,
    ) {
        Ok(v) => v,
        Err(m) => return st.failed(FailReason::PortUnavailable, m),
    };
    let listener = match tokio::net::TcpListener::from_std(std_listener) {
        Ok(l) => l,
        Err(e) => return st.failed(FailReason::PortUnavailable, format!("port {port}: {e}")),
    };
    #[cfg(test)]
    let _ = cx.faults.port.set(port);

    // Step 11: the components and the server.
    boundary!(cx, st, 11);
    let sink: Arc<dyn LifecycleSink> = Arc::new(GateSink(cx.gate.clone()));
    let built = cx.components.build(&BuildCtx {
        store: store.clone(),
        gate: GateHandle(cx.gate.clone()),
        sink: sink.clone(),
        port,
        state_dir: dir.clone(),
    });
    st.participants = built.participants;
    let server = HttpServer::new(
        HttpConfig::new(port),
        store.clone(),
        built.commands,
        sink,
        built.mounts,
    );
    // From here the registry can hold startup's own work (the owner token's write observer):
    // the coordinator drains it whether or not step 13 serves.
    st.serve = Some(server.shutdown_handle());
    #[cfg(test)]
    let _ = cx
        .faults
        .http_registry
        .set(server.shared().registry.clone());

    // Step 12: the token service, then the owner token file.
    boundary!(cx, st, 12);
    cx.components.attach(&server);
    let keep = match token {
        TokenFile::Absent => false,
        TokenFile::Present(secret) => {
            let s = store.clone();
            #[cfg(test)]
            let read_fails = cx.faults.token_read_fails.load(Ordering::SeqCst);
            let verified = tokio::task::spawn_blocking(move || {
                #[cfg(test)]
                if read_fails {
                    return Err("injected read failure".to_owned());
                }
                s.read(Instant::now() + TOKEN_DEADLINE, |tx| {
                    Ok(verify_secret_in(tx, &secret))
                })
                .map_err(|e| e.to_string())
            })
            .await;
            match joined(verified) {
                Ok(Ok(a)) if a.kind == TokenKind::Owner => true,
                Ok(Ok(_) | Err(AuthFailure::Invalid)) => false,
                Ok(Err(AuthFailure::Store(e))) => {
                    return st.failed(
                        FailReason::StoreOpen,
                        format!("the owner token cannot be verified: {e}"),
                    );
                }
                Err(e) => {
                    return st.failed(
                        FailReason::StoreOpen,
                        format!("the owner token cannot be verified: {e}"),
                    );
                }
            }
        }
    };
    if !keep {
        let secret = match server
            .tokens()
            .issue_owner(None, Instant::now() + TOKEN_DEADLINE)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                return st.failed(
                    FailReason::StoreOpen,
                    format!("the owner token cannot be issued: {:?}", e.code()),
                );
            }
        };
        let mut bytes = secret.expose().as_bytes().to_vec();
        bytes.push(b'\n');
        if let Err(e) = cx.replace(&dir, names::OWNER_TOKEN, &bytes) {
            return st.failed(FailReason::TokenFile, e);
        }
        cx.log.info("issued a new owner token");
    }

    // Step 13: serve, then the port, then the `serving` record.
    boundary!(cx, st, 13);
    st.serve = Some(server.serve(listener));
    st.server = Some(server);
    #[cfg(test)]
    if let Some(p) = cx.faults.parked(super::faults::Point::Inside(13)) {
        p.hold().await;
    }
    if saved != Some(port) {
        if let Err(e) = cx.replace(&dir, names::PORT, format!("{port}\n").as_bytes()) {
            return st.failed(FailReason::Internal, e);
        }
        st.port_saved = Some(port);
        if let Some(old) = saved {
            cx.log.warn(&format!(
                "port {port} replaces the saved port {old} for {}; this changes the pairing origin: re-pair with `chaldea open`",
                dir.display()
            ));
        }
    }
    #[cfg(test)]
    if let Some(p) = cx.faults.parked(super::faults::Point::AfterPortRename) {
        p.hold().await;
    }
    let written = cx
        .record(RecordState::Serving, Some(port))
        .and_then(|r| cx.replace(&dir, names::RECORD, &r.to_bytes()));
    if let Err(e) = written {
        return st.failed(FailReason::Internal, e);
    }

    // Step 14: the verdict, only if the gate has not fired.
    boundary!(cx, st, 14);
    #[cfg(test)]
    if let Some(p) = cx.faults.parked(super::faults::Point::Inside(14)) {
        p.hold().await;
    }
    if cx.gate.fired() || cx.gate.crashed().is_some() {
        st.outcome = Outcome::Stopped;
        return st;
    }
    match st.ready.take() {
        Some(mut w) => {
            let _ = w.write_line(&ready_line(port, std::process::id()));
            w.close();
        }
        None => cx.log.info(&format!("listening on {}", url(port))),
    }
    st.outcome = Outcome::Ok;
    st
}
