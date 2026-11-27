//! The fault entry of the `harness = false` binaries: with [`AS_CHALDEA`] set, the binary is
//! `chaldea` (`chaldea_cli::dispatch_with`) with the seams its environment selects. The
//! variables reach the child `daemon start` launches (std's spawn inherits them; the launchd
//! job carries the starter's environment), so a starter's faults are its child's too.
//!
//! # Gates
//!
//! Every pause is a [`Gate`]: a directory shared by the test and the paused process (the
//! waiter), with one implementation on each side, [`Gate`] for the test and [`wait_at`] for the
//! waiter. The waiter creates `<dir>/waiting` and `<dir>/reached`, polls `<dir>/decided`, and
//! removes `waiting` once it has seen a decision. `decided` holds the gate's one decision: each
//! side writes its outcome to a file of its own and links it to `decided`, so exactly one
//! outcome wins, `released` (the test's release), `dropped` (the test's guard), or `expired`
//! (the waiter's bound). A gate guarantees, as a test tool and not a product contract:
//!
//! - G1: every wait is bounded. The waiter's bound, [`GATE_MAX`], is longer than the longest
//!   sequence of watchdog-bounded steps a test may take between creating a gate and releasing
//!   it: each test-side wait (`Gate::reached`, `wait_for`, `Proc::serving`,
//!   `Proc::wait_within` with its output drain) adds its watchdog to its thread's total
//!   (`support::spend`), and `Gate::release` fails unless that total since the gate's creation,
//!   plus a margin of `WAIT` for the steps without a watchdog (spawns, file operations), is
//!   within the bound. The longest sequence in these tests is 130 s
//!   (`stopping_winner_is_not_reported_running`'s held shutdown), so the bound is 4 min.
//!   `tests/gates.rs` tests G1–G3.
//! - G2: the waiter continues only on the test's explicit release. Expiry, a missing gate
//!   directory, a `dropped` decision, or any other gate I/O error ends the waiter with exit code
//!   [`GATE_FAILED`] and a message on stderr, and the test fails on it: on the waiter's exit
//!   code or output, and in `Gate::release`, which fails on a gate decided otherwise.
//! - G3: every gate is its own drop guard: dropping one that was not released decides it
//!   `dropped` without panicking and keeps its directory until a waiter has seen that (at most
//!   `WAIT`), so a panicking test never leaves a waiter behind.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chaldea_cli::{Seams, Stall};
use chaldea_daemon::lifecycle::{
    BuildCtx, Built, Components, Discovery, MvpComponents, ParticipantEnd, Reconciler,
    ShutdownParticipant,
};
use chaldea_daemon::store::Store;
use chaldea_pty_runtime::host::{self, LaunchctlOutput};
use tempfile::TempDir;

/// Set: this process is `chaldea` with test seams.
pub const AS_CHALDEA: &str = "CHALDEA_TEST_AS_CHALDEA";
/// The supervisor's agent program for every agent, a JSON array of the program and its
/// leading arguments: `daemon run` builds `SupervisorComponents` with it.
pub const AGENT_PROGRAM: &str = "CHALDEA_TEST_AGENT_PROGRAM";
/// With [`AGENT_PROGRAM`]: the supervisor holds at this pause point (`after_reserve`,
/// `after_spawn`, `prober_start`) on the gate at [`SUPERVISOR_GATE`].
pub const SUPERVISOR_PAUSE: &str = "CHALDEA_TEST_SUPERVISOR_PAUSE";
pub const SUPERVISOR_GATE: &str = "CHALDEA_TEST_SUPERVISOR_GATE";
/// Windows: the native runtime's first spawn takes `RejectJobList` with a gate; at its
/// suspended create the child's pid is written to `pid` in this test gate's directory, which
/// is then waited at (`docs/design/supervisor-recovery.md` §5.2 PR10a2-CL-10).
pub const A29_GATE: &str = "CHALDEA_TEST_A29_GATE";
/// Windows, `assign_terminate_denied`: the native runtime's first spawn takes
/// `WindowsFault::AssignTerminateDenied`, so it fails with its suspended child alive outside
/// any launch job and recorded in `failed_start`.
pub const WINDOWS_FAULT: &str = "CHALDEA_TEST_WINDOWS_FAULT";
/// With [`AGENT_PROGRAM`], set: the supervisor runs on `FakePty`, its first child's waiter
/// held, so a close of it abandons `pty-wait` and its token stays counted.
pub const FAKE_PTY_HOLD_WAITER: &str = "CHALDEA_TEST_FAKE_PTY_HOLD_WAITER";
/// With [`AGENT_PROGRAM`]: the Claude decoder panics on a `UserPromptSubmit`, `in_parse` (in
/// its payload parser, the recovered region) or `after_parse` (after the parser returned).
pub const CLAUDE_DECODER_PANIC: &str = "CHALDEA_TEST_CLAUDE_DECODER_PANIC";
/// The launchd label prefix (every test start on a GUI host uses its own).
pub const LABEL_PREFIX: &str = "CHALDEA_TEST_LABEL_PREFIX";
/// A `launchctl` fault: see [`launchctl_fault`].
pub const LAUNCHCTL: &str = "CHALDEA_TEST_LAUNCHCTL";
/// A gate directory: an extra reconciler (step 9) waits at it.
pub const PARK_RECONCILER: &str = "CHALDEA_TEST_PARK_RECONCILER";
/// A gate directory: the marker's publication waits at it between its temporary and its link.
pub const MARKER_PAUSE: &str = "CHALDEA_TEST_MARKER_PAUSE";
/// A gate directory: `daemon stop` waits at it after sending its request.
pub const STOP_PAUSE: &str = "CHALDEA_TEST_STOP_PAUSE";
/// Milliseconds a shutdown participant takes (bounded by its end).
pub const SLOW_SHUTDOWN_MS: &str = "CHALDEA_TEST_SLOW_SHUTDOWN_MS";
/// Milliseconds a store reader, started at step 9, stays open: the shutdown's store report,
/// and so the lock decision, waits that long.
pub const HOLD_READER_MS: &str = "CHALDEA_TEST_HOLD_READER_MS";
/// Set: a component logs lines with secret markers at step 11.
pub const LOG_MARKERS: &str = "CHALDEA_TEST_LOG_MARKERS";
/// Milliseconds after which a fake detached child closes its readiness channel without a line.
pub const EOF_CHILD_MS: &str = "CHALDEA_TEST_EOF_CHILD_MS";
/// A gate directory: a detached child waits at it at step 1, before it opens its readiness
/// writer.
pub const READY_PAUSE: &str = "CHALDEA_TEST_READY_PAUSE";
/// A gate directory: `daemon start` waits at it once it read a verdict line.
pub const VERDICT_PAUSE: &str = "CHALDEA_TEST_VERDICT_PAUSE";
/// A gate directory: `daemon start` waits at it once its serving check got its 200, before
/// its last discovery.
pub const SERVE_PAUSE: &str = "CHALDEA_TEST_SERVE_PAUSE";
/// A gate directory: once its serving check got its 200, `daemon start` marks the gate reached
/// and brings its cutoff forward to that moment, so its last discovery comes after the cutoff
/// (nothing waits for a release).
pub const EXPIRE_AFTER_ANSWER: &str = "CHALDEA_TEST_EXPIRE_AFTER_ANSWER";
/// A gate directory: once it read the end of its readiness channel, `daemon start` waits at the
/// gate, then sets its follow-up's cutoff to the instant it returns from the gate (at or after
/// the release), so the follow-up starts after the cutoff.
pub const EXPIRE_AFTER_EOF: &str = "CHALDEA_TEST_EXPIRE_AFTER_EOF";
/// A directory: each discovery of `daemon start`'s serving check leaves a file named for what
/// it found (`starting`, `serving`, `stopping`, `not_running`, `unknown`).
pub const DISCOVERY_MARKS: &str = "CHALDEA_TEST_DISCOVERY_MARKS";
/// A gate directory: once the discovery after its serving check's 200 returned, `daemon start`
/// writes what it found to `<dir>/seen` (`<state> <instance>`, or the state alone) and marks the
/// gate reached (nothing waits for a release).
pub const RECHECK_MARK: &str = "CHALDEA_TEST_RECHECK_MARK";
/// A gate directory: a shutdown participant waits at it, so the daemon stays stopping (its
/// HTTP surface shut down, the lock held, and its record `stopping`; after a logged failure or
/// timeout of that rewrite the record may stay `serving`, as a rename already issued at the
/// timeout can still land) until the test releases it.
pub const SHUTDOWN_GATE: &str = "CHALDEA_TEST_SHUTDOWN_GATE";
/// Milliseconds: the deadline of `daemon start` and `daemon stop` instead of 15 s.
pub const DEADLINE_MS: &str = "CHALDEA_TEST_DEADLINE_MS";
/// A directory: a detached child writes its exit code to `<dir>/daemon-exit` just before it
/// exits.
pub const EXIT_RECORD: &str = "CHALDEA_TEST_EXIT_RECORD";

/// How long the fake EOF child keeps the state directory's lock after closing its channel.
const EOF_CHILD_LINGER: Duration = Duration::from_secs(5);

/// Set to a gate directory: this process only waits at it (G1–G3) and exits 0 once released.
pub const GATE_WAITER: &str = "CHALDEA_TEST_GATE_WAITER";
/// Milliseconds: a [`GATE_WAITER`]'s bound instead of [`GATE_MAX`], for the gate's own tests.
pub const GATE_BOUND_MS: &str = "CHALDEA_TEST_GATE_BOUND_MS";

/// The waiter's bound (G1).
pub const GATE_MAX: Duration = Duration::from_secs(240);

/// The exit code of a waiter whose gate was not released (G2).
pub const GATE_FAILED: i32 = 86;

/// A gate's files and the outcomes its decision can hold.
const DECIDED: &str = "decided";
const WAITING: &str = "waiting";
const RELEASED: &str = "released";
const DROPPED: &str = "dropped";
const EXPIRED: &str = "expired";

/// The secrets the log-marker component writes (redacted in every diagnostic).
pub const TOKEN_MSG: &str = "chd_MSGMSGMSGMSGMSGMSGMSGMSGMSGMSGMSGMSGMSGMSGM";
pub const TOKEN_FIELD: &str = "chdt_FIELDFIELDFIELDFIELDFIELDFIELDFIELDFIELDF";
pub const TOKEN_SPAN: &str = "chdp_SPANSPANSPANSPANSPANSPANSPANSPANSPANSPANS";

/// The Claude payload parser, panicking on a `UserPromptSubmit`.
fn panic_in_parse(
    event: &str,
    json: &serde_json::Value,
) -> chaldea_daemon::supervisor::send::claude::Parsed {
    assert!(event != "UserPromptSubmit", "injected parser panic");
    chaldea_daemon::supervisor::send::claude::parse(event, json)
}

/// The Claude decoder, panicking on a `UserPromptSubmit` once its parser returned.
struct PanicAfterParse;

impl chaldea_daemon::supervisor::send::HookDecoder for PanicAfterParse {
    fn decode(
        &self,
        tx: &chaldea_daemon::store::ReadTx<'_>,
        hook: &chaldea_daemon::hook::AcceptedHook<'_>,
    ) -> Result<
        (
            chaldea_daemon::supervisor::send::HookFact,
            Vec<chaldea_daemon::hook::HookEvent>,
        ),
        chaldea_daemon::store::StoreError,
    > {
        let out = chaldea_daemon::supervisor::send::ClaudeDecoder::default().decode(tx, hook)?;
        assert!(
            hook.event_name != "UserPromptSubmit",
            "injected panic after the parser"
        );
        Ok(out)
    }
}

/// A pause point shared by a test and its child processes (G1–G3 in the module doc).
pub struct Gate {
    dir: TempDir,
    /// The watchdogs this thread had spent when the gate was created.
    created: Duration,
}

impl Gate {
    #[must_use]
    pub fn new() -> Gate {
        Gate {
            dir: tempfile::tempdir().unwrap(),
            created: super::spent(),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Waits (bounded) until the paused side reached the gate.
    #[must_use]
    pub fn reached(&self, within: Duration) -> bool {
        super::spend(within);
        let end = Instant::now() + within;
        loop {
            if self.dir.path().join("reached").exists() {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Lets the waiter go on, the only way it does (G2). Panics when the watchdogs spent since
    /// the gate's creation leave no margin within the waiter's bound (G1), or when the gate was
    /// decided otherwise.
    pub fn release(&self) {
        let spent = super::spent().saturating_sub(self.created);
        assert!(
            spent + super::WAIT <= GATE_MAX,
            "{spent:?} of watchdogs before this release, plus a margin of {:?}, exceed the \
             waiter's bound of {GATE_MAX:?}",
            super::WAIT
        );
        if let Err(e) = decide(self.path(), RELEASED) {
            panic!(
                "the gate {} could not be released ({e}): {}",
                self.path().display(),
                decision(self.path())
            );
        }
    }

    /// The test killed the waiting process: its `waiting` marker goes with it, so dropping
    /// the gate does not wait for a waiter that is gone.
    pub fn waiter_killed(&self) {
        let _ = std::fs::remove_file(self.path().join(WAITING));
    }

    /// Ends the waiter without letting it go on, unless the gate was already decided: the
    /// directory then stays until a waiter has seen the decision, at most `WAIT` (G3).
    pub fn abandon(&self) {
        if decide(self.path(), DROPPED).is_ok() {
            let end = Instant::now() + super::WAIT;
            while self.path().join(WAITING).exists() && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        self.abandon();
    }
}

/// Writes `outcome` to a file of its own and links it to the gate's decision: `AlreadyExists`
/// when the other side decided first.
fn decide(dir: &Path, outcome: &str) -> std::io::Result<()> {
    let own = dir.join(outcome);
    std::fs::write(&own, outcome)?;
    let linked = std::fs::hard_link(&own, dir.join(DECIDED));
    let _ = std::fs::remove_file(&own);
    linked
}

/// What the gate's decision says, for a message.
fn decision(dir: &Path) -> String {
    match std::fs::read_to_string(dir.join(DECIDED)) {
        Ok(d) => format!("it was decided {d}"),
        Err(e) => format!("its decision cannot be read: {e}"),
    }
}

impl Default for Gate {
    fn default() -> Self {
        Gate::new()
    }
}

/// The waiter's side of a [`Gate`], every pause's one implementation: returns only on the
/// test's release; ends this process with [`GATE_FAILED`] and a message on anything else (G1,
/// G2).
fn wait_at(dir: &Path) {
    let outcome = await_release(dir);
    let _ = std::fs::remove_file(dir.join(WAITING));
    if let Err(why) = outcome {
        eprintln!("the test gate {} {why}", dir.display());
        std::process::exit(GATE_FAILED);
    }
}

/// Waits at `dir` until the gate is decided, its bound included: `Err` says why it was not
/// released.
fn await_release(dir: &Path) -> Result<(), String> {
    for name in [WAITING, "reached"] {
        std::fs::write(dir.join(name), b"").map_err(|e| format!("cannot be entered: {e}"))?;
    }
    let end = Instant::now() + env_ms(GATE_BOUND_MS).unwrap_or(GATE_MAX);
    loop {
        match std::fs::read_to_string(dir.join(DECIDED)) {
            Ok(d) => return verdict(&d),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if !dir.is_dir() {
                    return Err("is gone".to_owned());
                }
                if Instant::now() >= end {
                    match decide(dir, EXPIRED) {
                        Ok(()) => return Err("expired before the test released it".to_owned()),
                        // Decided meanwhile, or an entry that reads as missing: one read
                        // settles it, so no path returns to the loop after a collision.
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                            return match std::fs::read_to_string(dir.join(DECIDED)) {
                                Ok(d) => verdict(&d),
                                Err(e) => Err(format!("cannot be read: {e}")),
                            };
                        }
                        Err(e) => return Err(format!("cannot be decided: {e}")),
                    }
                }
            }
            Err(e) => return Err(format!("cannot be read: {e}")),
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// What a gate's decision `d` lets the waiter do.
fn verdict(d: &str) -> Result<(), String> {
    if d == RELEASED {
        Ok(())
    } else {
        Err(format!("was decided {d}, not released"))
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

fn env_ms(name: &str) -> Option<Duration> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
}

/// Acts as `chaldea` (never returns) when [`AS_CHALDEA`] is set, or as a bare waiter when
/// [`GATE_WAITER`] is.
pub fn enter_if_chaldea() {
    if let Some(dir) = env_path(GATE_WAITER) {
        wait_at(&dir);
        std::process::exit(0);
    }
    if std::env::var_os(AS_CHALDEA).is_none() {
        return;
    }
    let args: Vec<OsString> = std::env::args_os().collect();
    let detached_child = args.iter().any(|a| a == "--detached-child");
    if detached_child && let Some(after) = env_ms(EOF_CHILD_MS) {
        eof_child(&args, after);
    }
    let mut seams = Seams {
        label_prefix: std::env::var(LABEL_PREFIX).ok(),
        deadline: env_ms(DEADLINE_MS),
        ..Seams::default()
    };
    if let Ok(f) = std::env::var(LAUNCHCTL) {
        seams.launchctl = Some(launchctl_fault(f));
    }
    let park = env_path(PARK_RECONCILER);
    let slow = env_ms(SLOW_SHUTDOWN_MS);
    let markers = std::env::var_os(LOG_MARKERS).is_some();
    let hold_reader = env_ms(HOLD_READER_MS);
    let shutdown_gate = env_path(SHUTDOWN_GATE);
    if park.is_some()
        || slow.is_some()
        || markers
        || hold_reader.is_some()
        || shutdown_gate.is_some()
    {
        seams.components = Some(Box::new(move || {
            Box::new(FaultComponents {
                park,
                slow,
                markers,
                hold_reader,
                shutdown_gate,
            }) as Box<dyn Components>
        }));
    }
    if let Ok(json) = std::env::var(AGENT_PROGRAM) {
        let argv: Vec<OsString> = serde_json::from_str::<Vec<String>>(&json)
            .expect("CHALDEA_TEST_AGENT_PROGRAM is a JSON array of strings")
            .into_iter()
            .map(OsString::from)
            .collect();
        seams.components = Some(Box::new(move || {
            let host = chaldea_core::adapter::HostOs::from_os_str(std::env::consts::OS)
                .expect("a supported host");
            let mut cfg = chaldea_daemon::supervisor::SupervisorConfig::default();
            cfg.program = Some(std::sync::Arc::new(move |_| Some(argv.clone())));
            if let (Ok(at), Some(dir)) =
                (std::env::var(SUPERVISOR_PAUSE), env_path(SUPERVISOR_GATE))
            {
                let at = match at.as_str() {
                    "after_reserve" => chaldea_daemon::supervisor::Pause::AfterReserve,
                    "after_spawn" => chaldea_daemon::supervisor::Pause::AfterSpawn,
                    "prober_start" => chaldea_daemon::supervisor::Pause::ProberStart,
                    other => panic!("unknown supervisor pause {other}"),
                };
                cfg.pauses = std::sync::Arc::new(GatePause { at, dir });
            }
            if std::env::var_os(FAKE_PTY_HOLD_WAITER).is_some() {
                let pty = chaldea_pty_runtime::fake::FakePty::new();
                pty.script_next(chaldea_pty_runtime::fake::FakeScript {
                    hold_waiter: true,
                    ..chaldea_pty_runtime::fake::FakeScript::default()
                });
                cfg.runtime = std::sync::Arc::new(pty);
            }
            match std::env::var(CLAUDE_DECODER_PANIC).as_deref() {
                Ok("in_parse") => {
                    cfg.decoders.claude = std::sync::Arc::new(
                        chaldea_daemon::supervisor::send::ClaudeDecoder::with_parse(panic_in_parse),
                    );
                }
                Ok("after_parse") => cfg.decoders.claude = std::sync::Arc::new(PanicAfterParse),
                Ok(other) => panic!("unknown decoder panic {other}"),
                Err(_) => {}
            }
            #[cfg(windows)]
            if std::env::var(WINDOWS_FAULT).as_deref() == Ok("assign_terminate_denied") {
                use chaldea_pty_runtime::fake::{WindowsFault, with_windows_fault};
                cfg.runtime =
                    with_windows_fault(cfg.runtime.clone(), WindowsFault::AssignTerminateDenied);
            }
            // One gate holds the spawning thread until the test kills the helper: the
            // backend's gate waits as long as the test gate's waiter does.
            #[cfg(windows)]
            if let Some(dir) = env_path(A29_GATE) {
                use chaldea_pty_runtime::fake::{WindowsFault, gate_within, with_windows_fault};
                let (g, control) = gate_within(GATE_MAX);
                cfg.runtime =
                    with_windows_fault(cfg.runtime.clone(), WindowsFault::RejectJobList(Some(g)));
                std::thread::spawn(move || {
                    if let Some(pid) = control.wait_reached_pid(GATE_MAX) {
                        std::fs::write(dir.join("pid"), pid.to_string()).unwrap();
                        wait_at(&dir);
                    }
                    drop(control);
                });
            }
            Box::new(chaldea_daemon::supervisor::SupervisorComponents::new(
                host, cfg,
            )) as Box<dyn Components>
        }));
    }
    if let Some(dir) = env_path(MARKER_PAUSE) {
        chaldea_daemon::lifecycle::set_marker_pause(Box::new(move || wait_at(&dir)));
    }
    if let Some(dir) = env_path(STOP_PAUSE) {
        seams.after_stop_sent = Some(Box::new(move || wait_at(&dir)));
    }
    if let Some(dir) = env_path(READY_PAUSE) {
        seams.before_ready = Some(Box::new(move || wait_at(&dir)));
    }
    if let Some(dir) = env_path(VERDICT_PAUSE) {
        seams.after_verdict = Some(Box::new(move || wait_at(&dir)));
    }
    if let Some(dir) = env_path(SERVE_PAUSE) {
        seams.after_answer = Some(Box::new(move |cutoff| {
            wait_at(&dir);
            cutoff
        }));
    }
    if let Some(dir) = env_path(EXPIRE_AFTER_ANSWER) {
        seams.after_answer = Some(Box::new(move |_| expire(&dir)));
    }
    if let Some(dir) = env_path(EXPIRE_AFTER_EOF) {
        seams.after_eof = Some(Box::new(move |_| {
            wait_at(&dir);
            Instant::now()
        }));
    }
    if let Some(dir) = env_path(DISCOVERY_MARKS) {
        seams.on_discovery = Some(Box::new(move |d| {
            let _ = std::fs::write(dir.join(state_name(d)), b"");
        }));
    }
    if let Some(dir) = env_path(RECHECK_MARK) {
        seams.after_recheck = Some(Box::new(move |d| {
            let seen = match d {
                Discovery::Starting(r) | Discovery::Serving(r) | Discovery::Stopping(r) => {
                    format!("{} {}", state_name(d), r.instance)
                }
                Discovery::NotRunning | Discovery::Unknown => state_name(d).to_owned(),
            };
            let _ = std::fs::write(dir.join("seen"), seen);
            mark(&dir);
        }));
    }
    if detached_child && let Some(dir) = env_path(EXIT_RECORD) {
        seams.on_exit = Some(Box::new(move |code| {
            let _ = std::fs::write(dir.join("daemon-exit"), code.to_string());
        }));
    }
    chaldea_cli::dispatch_with(&args, Stall::Off, seams)
}

/// Marks `gate` reached and returns a cutoff that has come: the caller's next observation is
/// after its cutoff because the test brought it forward, whatever the host's speed.
fn expire(gate: &Path) -> Instant {
    mark(gate);
    Instant::now()
}

/// The name of a discovery's state.
fn state_name(d: &Discovery) -> &'static str {
    match d {
        Discovery::Starting(_) => "starting",
        Discovery::Serving(_) => "serving",
        Discovery::Stopping(_) => "stopping",
        Discovery::NotRunning => "not_running",
        Discovery::Unknown => "unknown",
    }
}

/// Marks `gate` reached without waiting for a release.
fn mark(gate: &Path) {
    let _ = std::fs::write(gate.join("reached"), b"");
}

/// The value after `flag` in `args`.
fn arg_after<'a>(args: &'a [OsString], flag: &str) -> Option<&'a OsString> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
}

/// A detached child that opens its readiness channel and takes the state directory's lock,
/// closes the channel after `after` without a line, keeps running with the lock for 5 s more,
/// and exits 0.
fn eof_child(args: &[OsString], after: Duration) -> ! {
    let ready = arg_after(args, "--ready").expect("--ready");
    let w = host::ready_writer(&host::ReadyArg::parse(ready).unwrap()).unwrap();
    let dir = PathBuf::from(arg_after(args, "--state-dir").expect("--state-dir"));
    let _ = std::fs::create_dir_all(&dir);
    let lock = std::fs::File::create(dir.join(chaldea_daemon::lifecycle::names::LOCK)).unwrap();
    lock.try_lock().unwrap();
    std::thread::sleep(after);
    w.close();
    std::thread::sleep(EOF_CHILD_LINGER);
    drop(lock);
    std::process::exit(0)
}

struct FaultComponents {
    park: Option<PathBuf>,
    slow: Option<Duration>,
    markers: bool,
    hold_reader: Option<Duration>,
    shutdown_gate: Option<PathBuf>,
}

impl Components for FaultComponents {
    fn reconcilers(&mut self, store: &Store) -> Vec<Reconciler> {
        if let Some(d) = self.hold_reader {
            let (tx, rx) = std::sync::mpsc::channel();
            let store = store.clone();
            std::thread::spawn(move || {
                let _ = store.read(
                    Instant::now() + GATE_MAX,
                    move |_r| -> Result<(), chaldea_daemon::store::StoreError> {
                        let _ = tx.send(());
                        std::thread::sleep(d);
                        Ok(())
                    },
                );
            });
            let _ = rx.recv_timeout(GATE_MAX);
        }
        let mut v = MvpComponents.reconcilers(store);
        if let Some(dir) = self.park.clone() {
            v.push(Reconciler {
                name: "test_park",
                job: Box::new(move |_| {
                    wait_at(&dir);
                    Ok(0)
                }),
            });
        }
        v
    }

    fn build(&mut self, ctx: &BuildCtx) -> Built {
        if self.markers {
            let span = tracing::info_span!("test_span", secret = TOKEN_SPAN);
            let _g = span.enter();
            tracing::info!(secret = TOKEN_FIELD, "LOG-MARKER {TOKEN_MSG}");
        }
        let mut b = MvpComponents.build(ctx);
        if let Some(d) = self.slow {
            b.participants.push(Box::new(Slow(d)));
        }
        if let Some(dir) = self.shutdown_gate.clone() {
            b.participants.push(Box::new(HeldShutdown(dir)));
        }
        b
    }
}

/// A shutdown participant that takes `0` (bounded by its end).
struct Slow(Duration);

impl ShutdownParticipant for Slow {
    fn name(&self) -> &'static str {
        "test_slow"
    }

    fn shutdown(
        &self,
        end: Instant,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ParticipantEnd> + Send + '_>> {
        let d = self.0.min(end.saturating_duration_since(Instant::now()));
        Box::pin(async move {
            std::thread::sleep(d);
            ParticipantEnd::Clean
        })
    }
}

/// A shutdown participant that blocks the shutdown at a gate until the test releases it, past
/// its end: the daemon stays stopping meanwhile.
struct HeldShutdown(PathBuf);

impl ShutdownParticipant for HeldShutdown {
    fn name(&self) -> &'static str {
        "test_held"
    }

    fn shutdown(
        &self,
        _end: Instant,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ParticipantEnd> + Send + '_>> {
        Box::pin(async move {
            wait_at(&self.0);
            ParticipantEnd::Clean
        })
    }
}

pub type Launchctl = Box<dyn Fn(&[&OsStr], Instant) -> std::io::Result<LaunchctlOutput>>;

fn timed_out() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::TimedOut, "injected timeout")
}

fn sleep_until(t: Instant) {
    std::thread::sleep(t.saturating_duration_since(Instant::now()));
}

/// `print <domain>/<label>`: a service lookup (not the domain probe or listing).
fn is_service_print(args: &[&OsStr]) -> bool {
    args.len() == 2 && args[0] == "print" && args[1].to_string_lossy().matches('/').count() >= 2
}

/// The `launchctl` faults (stderr notes each service lookup as `fault: print <service> -> <code>`):
///
/// - `probe-timeout`: `print gui/<uid>` waits for its deadline and times out.
/// - `bootstrap-refused`: `bootstrap` exits 5 without registering anything.
/// - `bootstrap-late`: the real `bootstrap`, then a wait for its deadline and a timeout.
/// - `bootstrap-timeout-register-later`: `bootstrap` times out at once without registering
///   anything; the first service lookup after it that answers absent (113) runs the real
///   `bootstrap` within that lookup's deadline, so the next lookup finds the job. The order is
///   set by the lookups, not by time, and the start keeps nearly its whole deadline for the
///   daemon's readiness; a lookup that does not answer absent registers nothing.
/// - `all-timeout`: `bootstrap` and every service lookup wait for their deadline and time out.
pub fn launchctl_fault(kind: String) -> Launchctl {
    launchctl_fault_over(kind, Box::new(host::launchctl))
}

/// [`launchctl_fault`] over `real` instead of `host::launchctl` (a test's recording stub).
pub fn launchctl_fault_over(kind: String, real: Launchctl) -> Launchctl {
    // The arguments of a `bootstrap` that `bootstrap-timeout-register-later` has yet to run.
    let deferred: std::cell::Cell<Option<Vec<OsString>>> = std::cell::Cell::new(None);
    Box::new(move |args: &[&OsStr], deadline: Instant| {
        let op = args.first().and_then(|a| a.to_str());
        let service_print = is_service_print(args);
        let result = match (kind.as_str(), op) {
            ("probe-timeout", Some("print")) if args.len() == 2 && !service_print => {
                sleep_until(deadline);
                Err(timed_out())
            }
            ("bootstrap-refused", Some("bootstrap")) => Ok(LaunchctlOutput {
                code: Some(5),
                stdout: String::new(),
                stderr: "Bootstrap failed: 5: Input/output error (injected)".to_owned(),
            }),
            ("bootstrap-late", Some("bootstrap")) => {
                let r = real(args, deadline);
                eprintln!("fault: real bootstrap -> {:?}", r.as_ref().map(|o| o.code));
                sleep_until(deadline);
                Err(timed_out())
            }
            ("bootstrap-timeout-register-later", Some("bootstrap")) => {
                deferred.set(Some(args.iter().map(|a| (*a).to_owned()).collect()));
                Err(timed_out())
            }
            ("bootstrap-timeout-register-later", Some("print")) if service_print => {
                let lookup = real(args, deadline);
                // 113: `launchctl print` found no such service.
                let absent = lookup.as_ref().is_ok_and(|o| o.code == Some(113));
                if absent && let Some(owned) = deferred.take() {
                    let refs: Vec<&OsStr> = owned.iter().map(OsString::as_os_str).collect();
                    let r = real(&refs, deadline);
                    eprintln!("fault: late bootstrap -> {:?}", r.as_ref().map(|o| o.code));
                }
                lookup
            }
            ("all-timeout", Some("bootstrap")) => {
                sleep_until(deadline);
                Err(timed_out())
            }
            ("all-timeout", Some("print")) if service_print => {
                sleep_until(deadline);
                Err(timed_out())
            }
            _ => real(args, deadline),
        };
        if service_print {
            eprintln!(
                "fault: print {} -> {}",
                args[1].to_string_lossy(),
                match &result {
                    Ok(o) => format!("{:?}", o.code),
                    Err(e) => e.to_string(),
                }
            );
        }
        result
    })
}

/// `chaldea` with test seams: this binary with [`AS_CHALDEA`] and the label prefix of
/// `labels`, `CHALDEA_STATE_DIR` removed, stdin null, stdout and stderr piped.
#[must_use]
pub fn chaldea(labels: &Labels, args: &[&OsStr]) -> std::process::Command {
    let mut c = std::process::Command::new(std::env::current_exe().unwrap());
    c.args(args)
        .env(AS_CHALDEA, "1")
        .env(LABEL_PREFIX, &labels.prefix)
        .env_remove(chaldea_daemon::lifecycle::STATE_DIR_ENV)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    c
}

/// `chaldea --state-dir <dir> <args…>` (this binary with test seams), `env` set after the
/// fault entry's own variables. `daemon start` and `daemon run` without `--port` get
/// `super::own_port(dir)`: no test of these binaries is about the choice from the range, so
/// their daemons stay off it, where `daemon_run`'s tests of that choice expect a port to be
/// free, and a restart on the saved port finds its port still the test's.
pub fn cmd(
    labels: &Labels,
    dir: &Path,
    args: &[&str],
    env: &[(&str, &OsStr)],
) -> std::process::Command {
    let mut c = chaldea(labels, &[OsStr::new("--state-dir"), dir.as_os_str()]);
    c.args(args);
    let starts = matches!(args, ["daemon", "start" | "run", ..]);
    if starts
        && !args
            .iter()
            .any(|a| *a == "--port" || a.starts_with("--port="))
        && let Some(p) = super::own_port(dir)
    {
        c.args(["--port", &p.to_string()]);
    }
    for (k, v) in env {
        c.env(k, v);
    }
    c
}

/// A test's own launchd label prefix; dropping it boots out every job with that prefix still
/// in the GUI domain (macOS), so no test job outlives its test.
pub struct Labels {
    pub prefix: String,
}

impl Labels {
    #[must_use]
    pub fn new() -> Labels {
        Labels {
            prefix: format!(
                "dev.chaldea.test.{}.",
                &uuid::Uuid::new_v4().simple().to_string()[..12]
            ),
        }
    }

    /// The labels with this prefix in `gui/<uid>` (macOS; empty elsewhere).
    #[must_use]
    pub fn present(&self) -> Vec<String> {
        if !cfg!(target_os = "macos") {
            return Vec::new();
        }
        let target = host::gui_domain_target();
        let Ok(o) = host::launchctl(
            &[OsStr::new("print"), OsStr::new(&target)],
            Instant::now() + Duration::from_secs(10),
        ) else {
            return Vec::new();
        };
        let mut v: Vec<String> = o
            .stdout
            .split_whitespace()
            .filter(|t| t.starts_with(&self.prefix))
            .map(str::to_owned)
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// Boots out `label` (macOS).
    pub fn bootout(label: &str) -> Option<i32> {
        let service = format!("{}/{label}", host::gui_domain_target());
        host::launchctl(
            &[OsStr::new("bootout"), OsStr::new(&service)],
            Instant::now() + Duration::from_secs(10),
        )
        .ok()
        .and_then(|o| o.code)
    }
}

impl Default for Labels {
    fn default() -> Self {
        Labels::new()
    }
}

impl Drop for Labels {
    fn drop(&mut self) {
        for l in self.present() {
            let code = Labels::bootout(&l);
            eprintln!("test cleanup: booted out {l} ({code:?})");
        }
    }
}

/// Holds the supervisor at one pause point on a test gate.
struct GatePause {
    at: chaldea_daemon::supervisor::Pause,
    dir: PathBuf,
}

impl chaldea_daemon::supervisor::Pauses for GatePause {
    fn at(
        &self,
        p: chaldea_daemon::supervisor::Pause,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        if p != self.at {
            return Box::pin(async {});
        }
        let dir = self.dir.clone();
        Box::pin(async move {
            let _ = tokio::task::spawn_blocking(move || wait_at(&dir)).await;
        })
    }
}
