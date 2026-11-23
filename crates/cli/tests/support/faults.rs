//! The fault entry of the `harness = false` binaries: with [`AS_CHALDEA`] set, the binary is
//! `chaldea` (`chaldea_cli::dispatch_with`) with the seams its environment selects. The
//! variables reach the child `daemon start` launches (std's spawn inherits them; the launchd
//! job carries the starter's environment), so a starter's faults are its child's too.
//!
//! Every pause is a [`Gate`]: the paused side creates `<dir>/reached` and waits until
//! `<dir>/release` exists, for at most 60 s.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chaldea_cli::{Seams, Stall};
use chaldea_daemon::lifecycle::{
    BuildCtx, Built, Components, MvpComponents, Reconciler, ShutdownParticipant,
};
use chaldea_daemon::store::Store;
use chaldea_pty_runtime::host::{self, LaunchctlOutput};
use tempfile::TempDir;

/// Set: this process is `chaldea` with test seams.
pub const AS_CHALDEA: &str = "CHALDEA_TEST_AS_CHALDEA";
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
/// A directory: a detached child writes its exit code to `<dir>/daemon-exit` just before it
/// exits.
pub const EXIT_RECORD: &str = "CHALDEA_TEST_EXIT_RECORD";

/// How long the fake EOF child keeps the state directory's lock after closing its channel.
const EOF_CHILD_LINGER: Duration = Duration::from_secs(5);

/// The longest a gate waits.
const GATE_MAX: Duration = Duration::from_secs(60);

/// The secrets the log-marker component writes (redacted in every diagnostic).
pub const TOKEN_MSG: &str = "chd_MSGMSGMSGMSGMSGMSGMSGMSGMSGMSGMSGMSGMSGMSGM";
pub const TOKEN_FIELD: &str = "chdt_FIELDFIELDFIELDFIELDFIELDFIELDFIELDFIELDF";
pub const TOKEN_SPAN: &str = "chdp_SPANSPANSPANSPANSPANSPANSPANSPANSPANSPANS";

/// A pause point shared by a test and its child processes.
pub struct Gate {
    dir: TempDir,
}

impl Gate {
    #[must_use]
    pub fn new() -> Gate {
        Gate {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Waits (bounded) until the paused side reached the gate.
    #[must_use]
    pub fn reached(&self, within: Duration) -> bool {
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

    pub fn release(&self) {
        std::fs::write(self.dir.path().join("release"), b"").unwrap();
    }
}

impl Default for Gate {
    fn default() -> Self {
        Gate::new()
    }
}

/// The paused side of a [`Gate`].
fn wait_at(dir: &Path) {
    let _ = std::fs::write(dir.join("reached"), b"");
    let end = Instant::now() + GATE_MAX;
    while Instant::now() < end && !dir.join("release").exists() {
        std::thread::sleep(Duration::from_millis(20));
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

/// Acts as `chaldea` (never returns) when [`AS_CHALDEA`] is set.
pub fn enter_if_chaldea() {
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
        ..Seams::default()
    };
    if let Ok(f) = std::env::var(LAUNCHCTL) {
        seams.launchctl = Some(launchctl_fault(f));
    }
    let park = env_path(PARK_RECONCILER);
    let slow = env_ms(SLOW_SHUTDOWN_MS);
    let markers = std::env::var_os(LOG_MARKERS).is_some();
    let hold_reader = env_ms(HOLD_READER_MS);
    if park.is_some() || slow.is_some() || markers || hold_reader.is_some() {
        seams.components = Some(Box::new(move || {
            Box::new(FaultComponents {
                park,
                slow,
                markers,
                hold_reader,
            }) as Box<dyn Components>
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
        seams.after_answer = Some(Box::new(move || wait_at(&dir)));
    }
    if detached_child && let Some(dir) = env_path(EXIT_RECORD) {
        seams.on_exit = Some(Box::new(move |code| {
            let _ = std::fs::write(dir.join("daemon-exit"), code.to_string());
        }));
    }
    chaldea_cli::dispatch_with(&args, Stall::Off, seams)
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
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let d = self.0.min(end.saturating_duration_since(Instant::now()));
        Box::pin(async move { std::thread::sleep(d) })
    }
}

type Launchctl = Box<dyn Fn(&[&OsStr], Instant) -> std::io::Result<LaunchctlOutput>>;

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
/// - `bootstrap-timeout-register-later`: `bootstrap` waits for its deadline and times out; the
///   real one runs 1 s later.
/// - `all-timeout`: `bootstrap` and every service lookup wait for their deadline and time out.
fn launchctl_fault(kind: String) -> Launchctl {
    Box::new(move |args: &[&OsStr], deadline: Instant| {
        let op = args.first().map(|a| a.to_string_lossy().into_owned());
        let service_print = is_service_print(args);
        let result = match (kind.as_str(), op.as_deref()) {
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
                let r = host::launchctl(args, deadline);
                eprintln!("fault: real bootstrap -> {:?}", r.as_ref().map(|o| o.code));
                sleep_until(deadline);
                Err(timed_out())
            }
            ("bootstrap-timeout-register-later", Some("bootstrap")) => {
                let owned: Vec<OsString> = args.iter().map(|a| (*a).to_owned()).collect();
                std::thread::spawn(move || {
                    sleep_until(deadline + Duration::from_secs(1));
                    let refs: Vec<&OsStr> = owned.iter().map(OsString::as_os_str).collect();
                    let r = host::launchctl(&refs, Instant::now() + Duration::from_secs(10));
                    eprintln!("fault: late bootstrap -> {:?}", r.as_ref().map(|o| o.code));
                });
                sleep_until(deadline);
                Err(timed_out())
            }
            ("all-timeout", Some("bootstrap")) => {
                sleep_until(deadline);
                Err(timed_out())
            }
            ("all-timeout", Some("print")) if service_print => {
                sleep_until(deadline);
                Err(timed_out())
            }
            _ => host::launchctl(args, deadline),
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
