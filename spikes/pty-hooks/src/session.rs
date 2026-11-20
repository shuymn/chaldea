//! Session guard and watchdog (§4.6, §4.7).
//!
//! The guard exists from the instant `spawn_command` returns and owns the kill
//! handles. The watchdog has an independent stop path: at the hard deadline,
//! on Ctrl-C, or on panic it kills every registered tree directly, runs
//! registered cleanups, writes `report.json`, and exits.

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
pub const WATCHDOG_LEAD: Duration = Duration::from_millis(500);

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

/// Kill handles shared with the watchdog.
pub struct Owned {
    pub child_pid: u32,
    pub child_start: u64,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    tree: Mutex<Option<os::Tree>>,
    tree_error: Mutex<Option<String>>,
    recorded: Mutex<BTreeMap<(u32, u64), ProcInfo>>,
    done: AtomicBool,
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

    /// Records the child's descendants (ppid walk) and, on Unix, its pgid/sid members.
    pub fn record_scan(&self) {
        let Ok(procs) = os::list_processes() else {
            return;
        };
        let mut found = owned_by(self.child_pid, self.child_start, &procs);
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

    /// Primary mechanism: the tree (pgid / job), else the retained child handle.
    pub fn kill_primary(&self, grace: Duration) -> Vec<String> {
        let tree = self.tree.lock().unwrap_or_else(|e| e.into_inner());
        match tree.as_ref() {
            Some(t) => t.kill(grace),
            None => {
                let r = self.killer.lock().unwrap_or_else(|e| e.into_inner()).kill();
                vec![format!("child_killer.kill={r:?}")]
            }
        }
    }

    /// SIGKILL / TerminateProcess every recorded pid still alive (pid+start rechecked).
    pub fn fallback(&self) -> Vec<ProcInfo> {
        let mut killed = Vec::new();
        for p in self.recorded() {
            if os::force_kill(p.pid, p.start_time) {
                killed.push(p);
            }
        }
        killed
    }

    pub fn alive_recorded(&self) -> Vec<ProcInfo> {
        self.recorded()
            .into_iter()
            .filter(|p| os::is_alive(p.pid, p.start_time))
            .collect()
    }

    /// Watchdog / guard-drop path: no grace period and no verification wait.
    pub fn emergency_kill(&self) {
        if self.done.swap(true, Ordering::SeqCst) {
            return;
        }
        self.record_scan();
        self.kill_primary(Duration::ZERO);
        // Retained child handle as well, in case tree setup was partial.
        let _ = self.killer.lock().unwrap_or_else(|e| e.into_inner()).kill();
        self.fallback();
    }
}

/// Descendants of `root` by ppid walk (start times must not precede the parent's,
/// guarding against pid reuse), plus Unix pgid/sid members of `root`.
pub fn owned_by(root: u32, root_start: u64, procs: &[ProcInfo]) -> Vec<ProcInfo> {
    let mut out: Vec<ProcInfo> = Vec::new();
    if let Some(r) = procs
        .iter()
        .find(|p| p.pid == root && p.start_time == root_start)
    {
        out.push(r.clone());
    }
    let mut frontier = vec![(root, root_start)];
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
    for p in procs {
        let member = p.pgid == Some(root) || p.sid == Some(root);
        if member && p.start_time >= root_start && !out.iter().any(|o| o.pid == p.pid) {
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
        let remove: Vec<String> = cmd
            .iter_full_env_as_str()
            .map(|(k, _)| k.to_string())
            .filter(|k| scrub_env_name(k))
            .collect();
        for k in remove {
            cmd.env_remove(k);
        }
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
            done: AtomicBool::new(false),
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
        s.io = Some(PtyIo::start(
            pair.master,
            &opts.out_file,
            opts.cols,
            opts.rows,
            opts.responder,
            t0,
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

    /// Teardown (§4.7): kill tree → check → fallback → drop writer → close
    /// master while draining → join threads with a bound.
    pub fn stop(&mut self) -> StopReport {
        let t = Instant::now();
        self.stopped = true;
        self.owned.done.store(true, Ordering::SeqCst);
        if let Some((j, st)) = self.recorder.take() {
            st.store(true, Ordering::SeqCst);
            let _ = j.join();
        }
        self.owned.record_scan();
        let primary_steps = self.owned.kill_primary(KILL_GRACE);
        // Reap our direct child so it does not linger as a zombie.
        let reap_end = Instant::now() + Duration::from_millis(500);
        while self.child_exited().is_none() && Instant::now() < reap_end {
            std::thread::sleep(Duration::from_millis(10));
        }
        let escaped_primary = self.owned.alive_recorded();
        let killed_by_fallback = self.owned.fallback();
        let _ = self.child_exited();
        let end = Instant::now() + VERIFY;
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
            primary_steps,
            recorded: self.owned.recorded(),
            escaped_primary,
            killed_by_fallback,
            survivors,
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

/// Registers a cleanup that runs on every watchdog exit path (e.g. hooks.json restore).
pub fn watchdog_cleanup(name: &str, f: Cleanup) {
    wd().cleanups
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((name.to_string(), f));
}

pub fn watchdog_remove_cleanup(name: &str) {
    wd().cleanups
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(n, _)| n != name);
}

pub fn interrupted() -> bool {
    wd().interrupted.load(Ordering::SeqCst)
}

/// Starts the watchdog thread for this process (once).
pub fn watchdog_start(sink: Sink, hard_deadline: Instant) {
    let w = wd();
    *w.sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink);
    let flag = w.interrupted.clone();
    let _ = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst));
    let _ = std::thread::Builder::new()
        .name("watchdog".into())
        .spawn(move || {
            loop {
                if wd().finished.load(Ordering::SeqCst) {
                    return;
                }
                if wd().interrupted.load(Ordering::SeqCst) {
                    watchdog_fire("error", "interrupted (Ctrl-C)", 130);
                }
                // Fire just before the hard deadline so kill + report fit inside it.
                if Instant::now() + WATCHDOG_LEAD >= hard_deadline {
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
    let owned: Vec<Arc<Owned>> = w
        .owned
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter_map(Weak::upgrade)
        .collect();
    let mut killed = Vec::new();
    for o in &owned {
        o.emergency_kill();
        killed.push(json!({"child_pid": o.child_pid, "recorded": o.recorded(), "alive_after": o.alive_recorded()}));
    }
    let cleanups: Vec<String> = w
        .cleanups
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(n, f)| format!("{n}: {}", f()))
        .collect();
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
