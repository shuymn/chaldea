//! Session guard and watchdog (§4.6, §4.7).
//!
//! The guard exists from the instant `spawn_command` returns and owns the kill
//! handles. Normal teardown and the watchdog share one idempotent kill path:
//! whichever takes the kill lock first kills and runs the fallback, and the
//! tree is marked done only after both completed. The watchdog kills every
//! registered tree at the scenario cutoff (deadline − 10 s), and at the hard
//! deadline, on Ctrl-C, or on panic it also runs cleanups, writes
//! `report.json`, and exits.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use portable_pty::{Child, ChildKiller, CommandBuilder, PtySize, native_pty_system};
use serde::Serialize;
use serde_json::{Value, json};

use crate::os::{self, ProcInfo};
use crate::pty::PtyIo;
use crate::report::Sink;

pub const KILL_GRACE: Duration = Duration::from_secs(2);
pub const VERIFY: Duration = Duration::from_secs(5);

/// Fault injection for B-10.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inject {
    SetupFail,
}

/// Environment variable names removed from every launch (§4.3, prior-art 6c).
pub fn scrub_env_name(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    n == "CLAUDECODE"
        || n.starts_with("CLAUDE_CODE_")
        || n == "CODEX_THREAD_ID"
        || n == "CODEX_SESSION_ID"
        || (n.starts_with("CODEX_") && (n.contains("SESSION") || n.contains("THREAD")))
        || n.starts_with("CHALDEA_")
}

/// Removes every scrubbed variable named in `keys` (from `std::env::vars_os`,
/// so a non-UTF-8 value cannot hide its key).
pub fn apply_scrub(cmd: &mut CommandBuilder, keys: impl Iterator<Item = OsString>) {
    for k in keys {
        if scrub_env_name(&k.to_string_lossy()) {
            cmd.env_remove(&k);
        }
    }
}

/// Kill handles shared with the watchdog.
pub struct Owned {
    pub child_pid: u32,
    pub child_start: u64,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    tree: Mutex<Option<os::Tree>>,
    tree_error: Mutex<Option<String>>,
    recorded: Mutex<BTreeMap<(u32, u64), ProcInfo>>,
    /// Serializes the kill path between teardown and the watchdog.
    kill_lock: Mutex<()>,
    /// Set only after kill + fallback completed.
    done: AtomicBool,
    /// B-10 test hook: runs inside teardown, after it started and before the kill.
    pub before_kill: Mutex<Option<BeforeKill>>,
}

pub type BeforeKill = Box<dyn Fn() + Send + Sync>;

#[derive(Debug, Clone, Default, Serialize)]
pub struct KillResult {
    pub primary_steps: Vec<String>,
    pub escaped_primary: Vec<ProcInfo>,
    pub killed_by_fallback: Vec<ProcInfo>,
    /// Another path (watchdog or guard drop) had already killed this tree.
    pub already_done: bool,
}

impl Owned {
    pub fn recorded(&self) -> Vec<ProcInfo> {
        self.recorded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Records the child's descendants with verified lineage (ppid walk from
    /// identities already recorded) and, on Unix, its pgid/sid members.
    pub fn record_scan(&self) {
        let Ok(procs) = os::list_processes() else {
            return;
        };
        let mut roots: Vec<(u32, u64)> = vec![(self.child_pid, self.child_start)];
        roots.extend(
            self.recorded
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .keys()
                .copied(),
        );
        let mut found = owned_by(self.child_pid, self.child_start, &roots, &procs);
        if let Some(t) = self.tree.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            for pid in t.members() {
                if let Some(p) = procs.iter().find(|p| p.pid == pid) {
                    found.push(p.clone());
                }
            }
        }
        let mut rec = self.recorded.lock().unwrap_or_else(|e| e.into_inner());
        for p in found {
            rec.entry((p.pid, p.start_time)).or_insert(p);
        }
    }

    fn kill_primary(&self, grace: Duration) -> Vec<String> {
        let tree = self.tree.lock().unwrap_or_else(|e| e.into_inner());
        let mut steps = match tree.as_ref() {
            Some(t) => t.kill(grace),
            None => Vec::new(),
        };
        // The retained child handle, always: covers a failed or delayed tree setup.
        let r = self.killer.lock().unwrap_or_else(|e| e.into_inner()).kill();
        steps.push(format!("child_killer.kill={}", r.is_ok()));
        steps
    }

    /// SIGKILL / TerminateProcess every recorded pid still alive (pid+start rechecked).
    fn fallback(&self) -> Vec<ProcInfo> {
        self.recorded()
            .into_iter()
            .filter(|p| os::force_kill(p.pid, p.start_time))
            .collect()
    }

    pub fn alive_recorded(&self) -> Vec<ProcInfo> {
        self.recorded()
            .into_iter()
            .filter(|p| os::is_alive(p.pid, p.start_time))
            .collect()
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }

    /// The one kill path: scan → primary → check → fallback, then `done`.
    /// Idempotent: a second caller waits for the first and returns `already_done`.
    pub fn kill_and_fallback(&self, grace: Duration) -> KillResult {
        let _g = self.kill_lock.lock().unwrap_or_else(|e| e.into_inner());
        if self.done.load(Ordering::SeqCst) {
            return KillResult {
                already_done: true,
                ..Default::default()
            };
        }
        self.record_scan();
        let primary_steps = self.kill_primary(grace);
        // Give the primary a moment to land before judging escapees.
        let settle = Instant::now() + Duration::from_millis(300);
        while !self.alive_recorded().is_empty() && Instant::now() < settle {
            std::thread::sleep(Duration::from_millis(20));
        }
        let escaped_primary = self.alive_recorded();
        let killed_by_fallback = self.fallback();
        self.done.store(true, Ordering::SeqCst);
        KillResult {
            primary_steps,
            escaped_primary,
            killed_by_fallback,
            already_done: false,
        }
    }

    /// Watchdog / guard-drop path: no grace period and no verification wait.
    pub fn emergency_kill(&self) {
        self.kill_and_fallback(Duration::ZERO);
    }
}

/// Owned processes with verified lineage: descendants by ppid walk from the
/// `roots` whose (pid, start_time) identity is present in `procs` (a root
/// whose pid now names another process seeds nothing), plus Unix pgid/sid
/// members of the launched child. Children never predate their parent.
pub fn owned_by(
    child: u32,
    child_start: u64,
    roots: &[(u32, u64)],
    procs: &[ProcInfo],
) -> Vec<ProcInfo> {
    let mut out: Vec<ProcInfo> = Vec::new();
    let mut frontier: Vec<(u32, u64)> = Vec::new();
    for &(pid, start) in roots {
        if let Some(r) = procs.iter().find(|p| p.pid == pid && p.start_time == start) {
            if !out.iter().any(|o| o.pid == r.pid) {
                out.push(r.clone());
            }
            frontier.push((pid, start));
        }
    }
    while let Some((pid, start)) = frontier.pop() {
        for p in procs {
            if p.ppid == pid
                && p.pid != pid
                && p.start_time >= start
                && !out.iter().any(|o| o.pid == p.pid)
            {
                out.push(p.clone());
                frontier.push((p.pid, p.start_time));
            }
        }
    }
    // A live process now holding the child's pid under another start time
    // means the group / session id was reused: it is not ours.
    let reused = procs
        .iter()
        .any(|p| p.pid == child && p.start_time != child_start);
    for p in procs.iter().filter(|_| !reused) {
        let member = p.pgid == Some(child) || p.sid == Some(child);
        if member && p.start_time >= child_start && !out.iter().any(|o| o.pid == p.pid) {
            out.push(p.clone());
        }
    }
    out
}

pub struct SpawnOpts {
    pub argv: Vec<OsString>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
    pub responder: bool,
    pub out_file: PathBuf,
    /// Delay tree attachment until `attach_tree` is called (`--hold-assign-until-child`).
    pub hold_assign: bool,
    pub inject: Option<Inject>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StopReport {
    pub primary_steps: Vec<String>,
    pub recorded: Vec<ProcInfo>,
    pub escaped_primary: Vec<ProcInfo>,
    pub killed_by_fallback: Vec<ProcInfo>,
    pub survivors: Vec<ProcInfo>,
    pub killed_by_watchdog: bool,
    pub tree: Value,
    pub threads: Value,
    pub teardown_ms: u64,
}

static SPAWNED: Mutex<Vec<(u32, u64)>> = Mutex::new(Vec::new());

/// `(pid, start_time)` of every child this process spawned in a PTY.
pub fn spawned_children() -> Vec<(u32, u64)> {
    SPAWNED.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub struct Session {
    pub owned: Arc<Owned>,
    pub io: Option<PtyIo>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    recorder: Option<(JoinHandle<()>, Arc<AtomicBool>)>,
    pub t0: Instant,
    stopped: bool,
}

impl Session {
    pub fn spawn(opts: SpawnOpts) -> Result<Session, String> {
        let t0 = Instant::now();
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: opts.rows,
                cols: opts.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("openpty: {e}"))?;
        let mut cmd = CommandBuilder::from_argv(opts.argv.clone());
        apply_scrub(&mut cmd, std::env::vars_os().map(|(k, _)| k));
        for (k, v) in &opts.env {
            cmd.env(k, v);
        }
        cmd.cwd(&opts.cwd);
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("spawn {:?}: {e}", opts.argv))?;
        // --- guard exists from here on; every early return below kills the child ---
        drop(pair.slave);
        let pid = child.process_id().unwrap_or(0);
        let start = os::proc_info(pid).map(|p| p.start_time).unwrap_or(0);
        let owned = Arc::new(Owned {
            child_pid: pid,
            child_start: start,
            killer: Mutex::new(child.clone_killer()),
            tree: Mutex::new(None),
            tree_error: Mutex::new(None),
            recorded: Mutex::new(BTreeMap::new()),
            kill_lock: Mutex::new(()),
            done: AtomicBool::new(false),
            before_kill: Mutex::new(None),
        });
        watchdog_register(&owned);
        SPAWNED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((pid, start));
        let mut s = Session {
            owned,
            io: None,
            child: Mutex::new(child),
            recorder: None,
            t0,
            stopped: false,
        };
        if !opts.hold_assign {
            s.attach_tree();
        }
        if opts.inject == Some(Inject::SetupFail) {
            return Err("injected post-spawn setup failure".into());
        }
        let o = s.owned.clone();
        s.io = Some(PtyIo::start(
            pair.master,
            &opts.out_file,
            opts.cols,
            opts.rows,
            opts.responder,
            t0,
            &move || o.emergency_kill(),
        )?);
        let stop = Arc::new(AtomicBool::new(false));
        let (o, st) = (s.owned.clone(), stop.clone());
        let join = std::thread::Builder::new()
            .name("recorder".into())
            .spawn(move || {
                while !st.load(Ordering::SeqCst) {
                    o.record_scan();
                    std::thread::sleep(Duration::from_millis(200));
                }
            })
            .map_err(|e| e.to_string())?;
        s.recorder = Some((join, stop));
        Ok(s)
    }

    /// Establishes harness ownership of the tree (pgid check / job assignment).
    pub fn attach_tree(&self) -> Value {
        let child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        let res = os::Tree::attach(child.as_ref());
        drop(child);
        match res {
            Ok(t) => {
                let d = t.describe();
                *self.owned.tree.lock().unwrap_or_else(|e| e.into_inner()) = Some(t);
                d
            }
            Err(e) => {
                *self
                    .owned
                    .tree_error
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(e.to_string());
                json!({"error": e.to_string()})
            }
        }
    }

    pub fn tree_desc(&self) -> Value {
        match self
            .owned
            .tree
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            Some(t) => t.describe(),
            None => {
                json!({"error": self.owned.tree_error.lock().unwrap_or_else(|e| e.into_inner()).clone()})
            }
        }
    }

    pub fn io(&self) -> &PtyIo {
        self.io.as_ref().expect("io started")
    }

    pub fn child_exited(&self) -> Option<u32> {
        let mut c = self.child.lock().unwrap_or_else(|e| e.into_inner());
        c.try_wait().ok().flatten().map(|s| s.exit_code())
    }

    /// Waits for the child to exit by `deadline`; returns its exit code.
    pub fn wait_exit(&self, deadline: Instant) -> Option<u32> {
        loop {
            if let Some(c) = self.child_exited() {
                return Some(c);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Teardown (§4.7): kill tree → check → fallback → verify → drop writer →
    /// close master while draining → join threads with a bound.
    pub fn stop(&mut self) -> StopReport {
        let t = Instant::now();
        self.stopped = true;
        if let Some((j, st)) = self.recorder.take() {
            st.store(true, Ordering::SeqCst);
            let _ = j.join();
        }
        let hook = self
            .owned
            .before_kill
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(h) = hook {
            h();
        }
        let k = self.owned.kill_and_fallback(KILL_GRACE);
        let end = Instant::now() + VERIFY;
        let _ = self.child_exited();
        let mut survivors = self.owned.alive_recorded();
        while !survivors.is_empty() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(50));
            let _ = self.child_exited();
            survivors = self.owned.alive_recorded();
        }
        let threads = match self.io.as_mut() {
            Some(io) => {
                io.close_writer();
                io.close_master();
                io.join_bounded(Duration::from_secs(2))
            }
            None => json!({}),
        };
        StopReport {
            primary_steps: k.primary_steps,
            recorded: self.owned.recorded(),
            escaped_primary: k.escaped_primary,
            killed_by_fallback: k.killed_by_fallback,
            survivors,
            killed_by_watchdog: k.already_done,
            tree: self.tree_desc(),
            threads,
            teardown_ms: t.elapsed().as_millis() as u64,
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.stopped {
            if let Some((_, st)) = &self.recorder {
                st.store(true, Ordering::SeqCst);
            }
            // Kill before any live PTY handle (the Unix writer) can drop.
            self.owned.emergency_kill();
            let _ = self.child_exited();
            if let Some(io) = self.io.as_mut() {
                io.close_writer();
            }
        }
    }
}

// ---------------- watchdog ----------------

type Cleanup = Box<dyn Fn() -> String + Send + Sync>;

struct Wd {
    sink: Mutex<Option<Sink>>,
    owned: Mutex<Vec<Weak<Owned>>>,
    cleanups: Mutex<Vec<(String, Cleanup)>>,
    interrupted: Arc<AtomicBool>,
    finished: AtomicBool,
    fired: AtomicBool,
}

static WD: OnceLock<Wd> = OnceLock::new();

fn wd() -> &'static Wd {
    WD.get_or_init(|| Wd {
        sink: Mutex::new(None),
        owned: Mutex::new(Vec::new()),
        cleanups: Mutex::new(Vec::new()),
        interrupted: Arc::new(AtomicBool::new(false)),
        finished: AtomicBool::new(false),
        fired: AtomicBool::new(false),
    })
}

pub fn watchdog_register(o: &Arc<Owned>) {
    let mut v = wd().owned.lock().unwrap_or_else(|e| e.into_inner());
    v.retain(|w| w.strong_count() > 0);
    v.push(Arc::downgrade(o));
}

/// Registers a cleanup that runs on every watchdog exit path (e.g. run-dir removal).
pub fn watchdog_cleanup(name: &str, f: Cleanup) {
    wd().cleanups
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((name.to_string(), f));
}

/// Runs every registered cleanup (watchdog exit paths).
fn run_cleanups() -> Vec<String> {
    wd().cleanups
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(n, f)| format!("{n}: {}", f()))
        .collect()
}

pub fn interrupted() -> bool {
    wd().interrupted.load(Ordering::SeqCst)
}

fn live_owned() -> Vec<Arc<Owned>> {
    wd().owned
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter_map(Weak::upgrade)
        .collect()
}

/// Starts the watchdog thread for this process (once). At `cutoff` it kills
/// every registered tree (idempotent with normal teardown); at
/// `hard_deadline` it fires.
pub fn watchdog_start(sink: Sink, cutoff: Instant, hard_deadline: Instant) {
    let w = wd();
    *w.sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink);
    let flag = w.interrupted.clone();
    let _ = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst));
    let _ = std::thread::Builder::new()
        .name("watchdog".into())
        .spawn(move || {
            let mut cut = false;
            loop {
                if wd().finished.load(Ordering::SeqCst) {
                    return;
                }
                if wd().interrupted.load(Ordering::SeqCst) {
                    watchdog_fire("error", "interrupted (Ctrl-C)", 130);
                }
                if !cut && Instant::now() >= cutoff {
                    cut = true;
                    for o in live_owned() {
                        o.emergency_kill();
                    }
                }
                if Instant::now() >= hard_deadline {
                    watchdog_fire(
                        "timeout",
                        "hard deadline reached; tree killed by watchdog",
                        124,
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
}

/// Marks normal completion so the watchdog never fires afterwards.
pub fn watchdog_finish() {
    wd().finished.store(true, Ordering::SeqCst);
}

/// Kills every registered tree, runs cleanups, writes the report, exits.
pub fn watchdog_fire(status: &str, why: &str, code: i32) -> ! {
    let w = wd();
    if w.fired.swap(true, Ordering::SeqCst) {
        // Another path is already firing; let it finish.
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    let mut killed = Vec::new();
    for o in live_owned() {
        o.emergency_kill();
        killed.push(json!({"child_pid": o.child_pid, "recorded": o.recorded(),
                           "done": o.is_done(), "alive_after": o.alive_recorded()}));
    }
    let cleanups = run_cleanups();
    if let Some(sink) = w.sink.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        sink.with(|r| {
            r.status = status.to_string();
            r.errors.push(why.to_string());
            r.detail("watchdog", json!({"killed": killed, "cleanups": cleanups}));
        });
        let _ = sink.write();
    }
    std::process::exit(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, ppid: u32, start: u64) -> ProcInfo {
        ProcInfo {
            pid,
            ppid,
            pgid: Some(pid),
            sid: Some(pid),
            start_time: start,
            name: "x".into(),
            zombie: false,
        }
    }

    /// M6: a reused root pid (different start time) seeds no fallback targets.
    #[test]
    fn reused_root_pid_seeds_nothing() {
        let procs = vec![p(100, 1, 99), p(200, 100, 120)];
        assert!(owned_by(100, 10, &[(100, 10)], &procs).is_empty());
        // With the genuine root identity the child is owned.
        let procs = vec![p(100, 1, 10), p(200, 100, 120)];
        let got: Vec<u32> = owned_by(100, 10, &[(100, 10)], &procs)
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(got, vec![100, 200]);
    }

    /// M6: lineage continues from an already-recorded identity after the root exited.
    #[test]
    fn recorded_identity_extends_lineage_after_root_exit() {
        let procs = vec![p(200, 1, 20), p(300, 200, 30)];
        let got: Vec<u32> = owned_by(100, 10, &[(100, 10), (200, 20)], &procs)
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(got, vec![200, 300]);
    }

    /// S7: a scrubbed key whose value is not UTF-8 is still removed.
    #[test]
    fn scrub_removes_non_utf8_valued_keys() {
        let bad = bad_os_value();
        let mut cmd = CommandBuilder::new("x");
        cmd.env("CLAUDE_CODE_SPIKE_TEST", &bad);
        cmd.env("KEEP_ME", "1");
        apply_scrub(
            &mut cmd,
            [
                OsString::from("CLAUDE_CODE_SPIKE_TEST"),
                OsString::from("KEEP_ME"),
            ]
            .into_iter(),
        );
        assert!(cmd.get_env("CLAUDE_CODE_SPIKE_TEST").is_none());
        assert!(cmd.get_env("KEEP_ME").is_some());
    }

    fn bad_os_value() -> OsString {
        let v = os::non_utf8_os_string();
        assert!(v.to_str().is_none());
        v
    }
}
