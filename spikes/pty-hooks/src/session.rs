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
use crate::verdict::Outcome;

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
    /// §4.8 `backend` kill proof (job trees only; see `kill_proof_outcome`).
    pub kill_proof: Option<Value>,
}

/// How long the live set may take to die after `TerminateJobObject`.
pub const JOB_DEATH_WAIT: Duration = Duration::from_secs(2);

/// A live identity before the stop and the handle retained on it, if one
/// could be opened with its start time verified.
type Retained = (u32, u64, Option<os::Retained>);

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
    /// identities already recorded) and, on Unix, its pgid/sid members. A
    /// failed process enumeration is returned (the kill path's final scan
    /// records it in the kill proof).
    pub fn record_scan(&self) -> Result<(), String> {
        let procs = os::list_processes().map_err(|e| format!("process enumeration: {e}"))?;
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
        Ok(())
    }

    /// Pre-stop half of the kill proof (job trees only): the historical
    /// descendant list, each identity's liveness, one membership snapshot,
    /// and a retained handle on every live identity (positive death evidence
    /// after the stop). The census gaps are recorded: the final scan's error,
    /// job members whose current identity (pid + start time) is not a
    /// recorded one, and live identities whose
    /// handle could not be retained.
    fn proof_before(&self, scan: Result<(), String>) -> Option<(Value, Vec<Retained>)> {
        let tree = self.tree.lock().unwrap_or_else(|e| e.into_inner());
        let t = tree.as_ref().filter(|t| t.proves_membership())?;
        let membership = t.members_checked();
        let mut retained = Vec::new();
        let pre: Vec<Value> = self
            .recorded()
            .iter()
            .map(|p| {
                let live = os::liveness(p.pid, p.start_time);
                if live == Some(true) {
                    retained.push((p.pid, p.start_time, os::Retained::open(p.pid, p.start_time)));
                }
                json!({"pid": p.pid, "start_time": p.start_time, "name": p.name, "live": live})
            })
            .collect();
        let ids: Vec<(u32, u64)> = self
            .recorded()
            .iter()
            .map(|p| (p.pid, p.start_time))
            .collect();
        // A fresh lookup of each member's identity; a failed read leaves
        // every member unverified.
        let procs = os::list_processes().unwrap_or_default();
        let now = |pid: u32| procs.iter().find(|p| p.pid == pid).map(|p| p.start_time);
        let open_failed: Vec<u32> = retained
            .iter()
            .filter(|(_, _, h)| h.is_none())
            .map(|(pid, _, _)| *pid)
            .collect();
        let mut proof = json!({"pre_stop": pre, "membership": match &membership {
            Ok(m) => json!(m), Err(e) => json!({"error": e}) },
            "scan_error": scan.err(),
            "unreconciled_members": membership.as_ref().map(|m| unreconciled(m, now, &ids)).unwrap_or_default(),
            "retained_open_failed": open_failed});
        if t.describe()["assign"] == json!("after_spawn") {
            proof["known_limit"] = json!(
                "assign-after: a grandchild created by a child that exits before the job \
                 assignment, between two recorder scans, is never recorded; this proof \
                 cannot see it (job-first launches are not affected)"
            );
        }
        Some((proof, retained))
    }

    /// After `TerminateJobObject`: waits (bounded) for every identity that
    /// was live before the stop to be positively dead, before the
    /// retained-child kill, any pid fallback, or the ConPTY close. Death is
    /// read from the handle retained before the stop where one was opened,
    /// else from the process table; an unknown result stays unknown.
    fn proof_after(proof: &mut Value, retained: &[Retained]) {
        let end = Instant::now() + JOB_DEATH_WAIT;
        let after: Vec<Value> = retained
            .iter()
            .map(|(pid, st, h)| {
                let (live, evidence) = match h {
                    Some(h) => (h.wait_dead(end).map(|dead| !dead), "retained_handle"),
                    None => {
                        let live = loop {
                            let l = os::liveness(*pid, *st);
                            if l != Some(true) || Instant::now() >= end {
                                break l;
                            }
                            std::thread::sleep(Duration::from_millis(50));
                        };
                        (live, "process_table")
                    }
                };
                json!({"pid": pid, "live": live, "evidence": evidence})
            })
            .collect();
        let dead = after.iter().all(|a| a["live"] == json!(false));
        proof["after_job"] = json!(after);
        proof["live_set_dead_before_fallback"] = json!(dead);
    }

    fn kill_primary(
        &self,
        grace: Duration,
        proof: &mut Option<(Value, Vec<Retained>)>,
    ) -> Vec<String> {
        let tree = self.tree.lock().unwrap_or_else(|e| e.into_inner());
        let (mut steps, ok) = match tree.as_ref() {
            Some(t) => t.kill(grace),
            None => (Vec::new(), false),
        };
        drop(tree);
        if let Some((p, retained)) = proof.as_mut() {
            p["terminate_job_ok"] = json!(ok);
            Self::proof_after(p, retained);
        }
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
        let scan = self.record_scan();
        let mut kill_proof = self.proof_before(scan);
        let primary_steps = self.kill_primary(grace, &mut kill_proof);
        let kill_proof = kill_proof.map(|(p, _)| p);
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
            kill_proof,
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

/// Job members with no recorded identity before the stop: a member
/// reconciles only when `now` (a fresh lookup of its pid's start time)
/// yields a nonzero start time recorded with that pid, so a reused pid or
/// one without a verified current identity stays unreconciled.
pub fn unreconciled(
    members: &[u32],
    now: impl Fn(u32) -> Option<u64>,
    recorded: &[(u32, u64)],
) -> Vec<u32> {
    members
        .iter()
        .copied()
        .filter(|&m| !now(m).is_some_and(|st| st != 0 && recorded.contains(&(m, st))))
        .collect()
}

/// The §4.8 `backend` kill proof as a case outcome. Every descendant live
/// before the stop must be positively in the job (one outside it is an
/// escapee); the MCP witness `(pid, start)` must be live and in the job;
/// identities that already exited need no membership; an unknown liveness or
/// a failed membership query is a harness failure; `TerminateJobObject`
/// must itself have succeeded (else harness failure); and every member of
/// the live set must be positively dead after it and before the
/// retained-child kill, any pid fallback, or the ConPTY close.
pub fn kill_proof_outcome(proof: &Value, witness: Option<(u32, u64)>) -> Outcome {
    let Some(members) = proof["membership"].as_array() else {
        return Outcome::harness(format!("membership query failed: {}", proof["membership"]));
    };
    let members: Vec<u64> = members.iter().filter_map(Value::as_u64).collect();
    // An incomplete census proves nothing either way.
    if let Some(e) = proof["scan_error"].as_str() {
        return Outcome::harness(format!("final process scan failed: {e}"));
    }
    let listed = |k: &str| proof[k].as_array().is_some_and(|a| !a.is_empty());
    if listed("unreconciled_members") {
        return Outcome::harness(format!(
            "job members with no recorded identity: {}",
            proof["unreconciled_members"]
        ));
    }
    if listed("retained_open_failed") {
        return Outcome::harness(format!(
            "no handle retained on live identities: {}",
            proof["retained_open_failed"]
        ));
    }
    let pre: Vec<&Value> = proof["pre_stop"].as_array().into_iter().flatten().collect();
    if pre.iter().any(|p| p["live"].is_null()) {
        return Outcome::harness("pre-stop liveness unknown");
    }
    let live = |p: &Value| p["live"] == json!(true);
    let outside: Vec<u64> = pre
        .iter()
        .filter(|p| live(p))
        .filter_map(|p| p["pid"].as_u64())
        .filter(|pid| !members.contains(pid))
        .collect();
    if !outside.is_empty() {
        return Outcome::platform(format!("live descendants outside the job: {outside:?}"));
    }
    let witness_ok = witness.is_some_and(|(wp, ws)| {
        pre.iter().any(|p| {
            live(p)
                && p["pid"].as_u64() == Some(wp as u64)
                && p["start_time"].as_u64() == Some(ws)
                && members.contains(&(wp as u64))
        })
    });
    if !witness_ok {
        return Outcome::not_exercised(
            "the MCP witness was not live and in the job before the stop",
        );
    }
    if proof["terminate_job_ok"] != json!(true) {
        return Outcome::harness("TerminateJobObject did not succeed");
    }
    let after: Vec<&Value> = proof["after_job"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    if after.iter().any(|p| p["live"].is_null()) {
        return Outcome::harness("liveness after TerminateJobObject unknown");
    }
    if proof["live_set_dead_before_fallback"] != json!(true) {
        return Outcome::platform("the live set survived TerminateJobObject");
    }
    Outcome::Pass
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
    /// `Portable` (default, today's path) or the Windows raw backend.
    pub backend: os::Backend,
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
    /// Backend, launch branch, and loaded ConPTY module of this launch.
    pub launch: Value,
    pub kill_proof: Option<Value>,
}

static SPAWNED: Mutex<Vec<(u32, u64)>> = Mutex::new(Vec::new());

/// Launch records of raw launches that never produced a session.
static FAILED_LAUNCHES: Mutex<Vec<Value>> = Mutex::new(Vec::new());

fn record_failed_launch(launch: Value) {
    FAILED_LAUNCHES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(launch);
}

/// Every failed raw launch's record (`{"launch": …}` each), for the report.
pub fn failed_launches() -> Vec<Value> {
    FAILED_LAUNCHES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|l| json!({"launch": l}))
        .collect()
}

/// `(pid, start_time)` of every child this process spawned in a PTY.
pub fn spawned_children() -> Vec<(u32, u64)> {
    SPAWNED.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub struct Session {
    pub owned: Arc<Owned>,
    pub io: Option<PtyIo>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    recorder: Option<(JoinHandle<()>, Arc<AtomicBool>)>,
    stopped: bool,
    /// Backend, branch, and ConPTY module (raw: recorded before I/O starts).
    pub launch: Value,
}

impl Session {
    pub fn spawn(opts: SpawnOpts) -> Result<Session, String> {
        let t0 = Instant::now();
        // The output sink is prepared before anything is spawned.
        let out = std::fs::File::create(&opts.out_file)
            .map_err(|e| format!("{}: {e}", opts.out_file.display()))?;
        let (master, child, raw_tree, launch) = match opts.backend {
            os::Backend::Portable => {
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
                let launch = json!({"backend": "portable", "branch": "assign_after",
                                    "conpty_module": os::conpty_module()});
                (pair.master, child, None, launch)
            }
            os::Backend::Raw {
                conpty_flags,
                inject,
            } => {
                if opts.hold_assign {
                    return Err(
                        "--hold-assign-until-child does not apply to the raw backend".into(),
                    );
                }
                let remove: Vec<String> = std::env::vars_os()
                    .map(|(k, _)| k.to_string_lossy().into_owned())
                    .filter(|k| scrub_env_name(k))
                    .collect();
                let rs = match os::raw_spawn(
                    &opts.argv,
                    &opts.env,
                    &remove,
                    &opts.cwd,
                    opts.cols,
                    opts.rows,
                    conpty_flags,
                    inject,
                ) {
                    Ok(rs) => rs,
                    Err(e) => {
                        // Every attempt failed: keep the launch record anyway.
                        let a = os::raw_last_launch().unwrap_or(Value::Null);
                        record_failed_launch(json!({"backend": "raw",
                            "conpty_flags": conpty_flags, "branch": "none", "error": e,
                            "attempts": a["attempts"],
                            "job_list_rejected": a["job_list_rejected"] == json!(true),
                            "teardown": os::raw_last_teardown()}));
                        return Err(e);
                    }
                };
                // --- guard exists from here on; the tree is the job from creation ---
                let launch = json!({"backend": "raw", "conpty_flags": conpty_flags,
                                    "branch": rs.branch, "job_list_error": rs.job_list_error,
                                    "job_list_rejected": rs.job_list_rejected,
                                    "attempts": rs.attempts,
                                    "conpty_module": rs.conpty_module});
                (rs.master, rs.child, Some(rs.tree), launch)
            }
        };
        let identity = os::child_identity(child.as_ref());
        let (pid, start) = match &identity {
            Ok(id) => *id,
            Err(_) => (child.process_id().unwrap_or(0), 0),
        };
        let owned = Arc::new(Owned {
            child_pid: pid,
            child_start: start,
            killer: Mutex::new(child.clone_killer()),
            tree: Mutex::new(raw_tree),
            tree_error: Mutex::new(None),
            recorded: Mutex::new(BTreeMap::new()),
            kill_lock: Mutex::new(()),
            done: AtomicBool::new(false),
            before_kill: Mutex::new(None),
        });
        watchdog_register(&owned);
        if identity.is_ok() {
            SPAWNED
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((pid, start));
        }
        let mut s = Session {
            owned,
            io: None,
            child: Mutex::new(child),
            recorder: None,
            stopped: false,
            launch,
        };
        if !opts.hold_assign && opts.backend == os::Backend::Portable {
            s.attach_tree();
        }
        // The guard exists: dropping the session kills the unidentified child.
        identity?;
        if opts.inject == Some(Inject::SetupFail) {
            return Err("injected post-spawn setup failure".into());
        }
        let o = s.owned.clone();
        s.io = Some(PtyIo::start(
            master,
            out,
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
                    let _ = o.record_scan();
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

    /// The launch record; a raw launch also carries its master's teardown
    /// (close result and how the output drain ended).
    fn launch_with_teardown(&self) -> Value {
        let mut l = self.launch.clone();
        if l["backend"] == json!("raw") {
            l["teardown"] = os::raw_last_teardown().unwrap_or(Value::Null);
        }
        l
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
            launch: self.launch_with_teardown(),
            kill_proof: k.kill_proof,
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
    /// Run-dir removal, registered by `Ctx::new` unless `--keep-temp`.
    run_dir_cleanup: Mutex<Option<Cleanup>>,
    interrupted: Arc<AtomicBool>,
    finished: AtomicBool,
    fired: AtomicBool,
    /// Set before the watchdog kills anything (cutoff or fire).
    tearing_down: AtomicBool,
}

static WD: OnceLock<Wd> = OnceLock::new();

fn wd() -> &'static Wd {
    WD.get_or_init(|| Wd {
        sink: Mutex::new(None),
        owned: Mutex::new(Vec::new()),
        run_dir_cleanup: Mutex::new(None),
        interrupted: Arc::new(AtomicBool::new(false)),
        finished: AtomicBool::new(false),
        fired: AtomicBool::new(false),
        tearing_down: AtomicBool::new(false),
    })
}

pub fn watchdog_register(o: &Arc<Owned>) {
    let mut v = wd().owned.lock().unwrap_or_else(|e| e.into_inner());
    v.retain(|w| w.strong_count() > 0);
    v.push(Arc::downgrade(o));
}

/// Registers the run-dir removal that runs on every watchdog exit path.
pub fn watchdog_run_dir_cleanup(f: Cleanup) {
    *wd()
        .run_dir_cleanup
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(f);
}

/// Runs the registered cleanup (watchdog exit paths); the report lists it.
fn run_cleanups() -> Vec<String> {
    wd().run_dir_cleanup
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|f| format!("run dir: {}", f()))
        .collect()
}

pub fn interrupted() -> bool {
    wd().interrupted.load(Ordering::SeqCst)
}

/// True once the harness itself started tearing trees down (cutoff,
/// interrupt, or fire): a death seen after this is not platform evidence.
pub fn teardown_in_progress() -> bool {
    let w = wd();
    w.tearing_down.load(Ordering::SeqCst)
        || w.fired.load(Ordering::SeqCst)
        || w.interrupted.load(Ordering::SeqCst)
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
                    wd().tearing_down.store(true, Ordering::SeqCst);
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
    w.tearing_down.store(true, Ordering::SeqCst);
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

    fn proof(pre: Value, members: Value, after: Value, dead: bool) -> Value {
        json!({"pre_stop": pre, "membership": members, "after_job": after,
               "live_set_dead_before_fallback": dead, "terminate_job_ok": true})
    }

    /// M3: the kill proof credits job termination only when
    /// `TerminateJobObject` itself succeeded.
    #[test]
    fn kill_proof_needs_terminate_success() {
        let mut p = json!({"pre_stop": [{"pid": 10, "start_time": 1, "live": true}],
            "membership": [10], "after_job": [{"pid": 10, "live": false}],
            "live_set_dead_before_fallback": true, "terminate_job_ok": false});
        let o = kill_proof_outcome(&p, Some((10, 1)));
        assert!(matches!(o, Outcome::HarnessFail { .. }), "{o:?}");
        p["terminate_job_ok"] = json!(true);
        assert_eq!(kill_proof_outcome(&p, Some((10, 1))), Outcome::Pass);
    }

    /// M1: a failed final scan, a job member missing from the recorded
    /// identities, or a live identity whose handle could not be retained
    /// leaves the proof incomplete: harness failure, never a pass.
    #[test]
    fn kill_proof_needs_complete_census() {
        let base = || {
            json!({"pre_stop": [{"pid": 10, "start_time": 1, "live": true}],
                "membership": [10], "after_job": [{"pid": 10, "live": false}],
                "live_set_dead_before_fallback": true, "terminate_job_ok": true})
        };
        assert_eq!(kill_proof_outcome(&base(), Some((10, 1))), Outcome::Pass);
        for (k, v) in [
            ("scan_error", json!("Process32NextW: access denied")),
            ("unreconciled_members", json!([11])),
            ("retained_open_failed", json!([10])),
        ] {
            let mut p = base();
            p[k] = v;
            let o = kill_proof_outcome(&p, Some((10, 1)));
            assert!(matches!(o, Outcome::HarnessFail { .. }), "{k}: {o:?}");
        }
    }

    /// M1 (r3): a job member reconciles only by identity: its pid's fresh
    /// start time must match a recorded identity. A reused pid (recorded
    /// with another start time), a pid with no current identity, and a
    /// start time of 0 stay unreconciled.
    #[test]
    fn job_census_reconciles_by_identity() {
        let now = |pid: u32| match pid {
            10 => Some(1),
            11 => Some(9),
            13 => Some(0),
            _ => None,
        };
        let recorded = [(10, 1), (11, 5), (12, 3), (13, 0)];
        assert_eq!(
            unreconciled(&[10, 11, 12, 13], now, &recorded),
            vec![11, 12, 13]
        );
        assert!(unreconciled(&[10], now, &recorded).is_empty());
    }

    /// §4.8 `backend` kill proof as a case outcome.
    #[test]
    fn kill_proof_rules() {
        let pre = json!([
            {"pid": 10, "start_time": 1, "live": true},
            {"pid": 11, "start_time": 1, "live": true},
            {"pid": 12, "start_time": 1, "live": false}
        ]);
        let after = json!([{"pid": 10, "live": false}, {"pid": 11, "live": false}]);
        let w = Some((11, 1));
        let ok = proof(pre.clone(), json!([10, 11]), after.clone(), true);
        assert_eq!(kill_proof_outcome(&ok, w), Outcome::Pass);
        // An exited identity needs no membership; a live one outside the job is an escapee.
        let esc = proof(pre.clone(), json!([10]), after.clone(), true);
        assert!(matches!(
            kill_proof_outcome(&esc, w),
            Outcome::PlatformFail { .. }
        ));
        // The witness must be live and in the job (same start time).
        assert!(matches!(
            kill_proof_outcome(&ok, Some((11, 2))),
            Outcome::NotExercised { .. }
        ));
        assert!(matches!(
            kill_proof_outcome(&ok, None),
            Outcome::NotExercised { .. }
        ));
        // A failed membership query or unknown liveness is a harness failure.
        let bad = proof(pre.clone(), json!({"error": "denied"}), after.clone(), true);
        assert!(matches!(
            kill_proof_outcome(&bad, w),
            Outcome::HarnessFail { .. }
        ));
        let unk = proof(
            json!([{"pid": 10, "start_time": 1, "live": null}]),
            json!([10]),
            after.clone(),
            true,
        );
        assert!(matches!(
            kill_proof_outcome(&unk, w),
            Outcome::HarnessFail { .. }
        ));
        // The live set must be dead before the later steps.
        let late = proof(
            pre,
            json!([10, 11]),
            json!([{"pid": 10, "live": true}]),
            false,
        );
        assert!(matches!(
            kill_proof_outcome(&late, w),
            Outcome::PlatformFail { .. }
        ));
    }
}
