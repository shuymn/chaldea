//! The OS boundary. Every OS-specific PTY/process/console call lives under
//! this module; the rest of the crate is OS-agnostic and has no `cfg`.

use serde::Serialize;

mod drain;
pub use drain::*;

/// The line prefix FakeCli `spawn-tree --emit-output` writes (F-2c
/// `after-spawn` fires only once it is seen in the held output).
pub const EMIT_OUTPUT_MARKER: &str = "OUTPUT-";

/// How long the `after-spawn` injection waits for [`EMIT_OUTPUT_MARKER`].
pub const AFTER_SPAWN_WAIT_SECS: u64 = 5;

/// The `after-spawn` injection's error: the injected failure once the
/// fixture's output line was seen, else the missed milestone.
pub fn after_spawn_error(marker_seen: bool) -> String {
    if marker_seen {
        "injected failure after spawn".into()
    } else {
        format!(
            "milestone not reached: {EMIT_OUTPUT_MARKER} marker not seen in \
             {AFTER_SPAWN_WAIT_SECS} s"
        )
    }
}

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

/// `WindowObserver::stop`: every `EVENT_OBJECT_SHOW` recorded, and the
/// observer's health (an error means the events may be incomplete).
pub type ObserverStop = (Vec<WindowEvent>, Result<(), String>);

/// One `EVENT_OBJECT_SHOW`, recorded at callback time (C-5, F-3a). No
/// visibility filter: a flash already hidden still counts.
#[derive(Debug, Clone, Default, Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct WindowEvent {
    /// Milliseconds since the observer started.
    pub time_ms: u64,
    pub hwnd: u64,
    pub id_object: i32,
    pub id_child: i32,
    pub class: String,
    pub owner_pid: u32,
    pub image: Option<String>,
    pub ppid: Option<u32>,
    pub start_time: Option<u64>,
    /// The owner was in the observation job at callback time.
    pub in_job: Option<bool>,
    /// The event's own `dwmsEventTime` and `idEventThread`.
    #[serde(default)]
    pub event_ms: u32,
    #[serde(default)]
    pub event_thread: u32,
    /// The process of `idEventThread`, when it could be resolved.
    #[serde(default)]
    pub event_pid: Option<u32>,
    /// The hwnd's owning thread at callback time.
    #[serde(default)]
    pub owner_thread: u32,
}

/// `OBJID_WINDOW` / `CHILDID_SELF`: the only events classified.
pub const OBJID_WINDOW: i32 = 0;
pub const CHILDID_SELF: i32 = 0;

impl WindowEvent {
    pub fn is_window(&self) -> bool {
        self.id_object == OBJID_WINDOW && self.id_child == CHILDID_SELF
    }

    /// The hwnd resolved at callback time still belongs to the process
    /// (or, when that cannot be resolved, the thread) that raised the event.
    pub fn owner_consistent(&self) -> bool {
        match self.event_pid {
            Some(p) => p == self.owner_pid,
            None => self.event_thread != 0 && self.owner_thread == self.event_thread,
        }
    }
}

/// One window observer at a time (C-5): an observer whose thread never
/// exited (abandoned) keeps its slot, so a later launch cannot be observed
/// with that thread still able to record events.
pub struct ObserverSlot(std::sync::atomic::AtomicBool);

impl Default for ObserverSlot {
    fn default() -> ObserverSlot {
        ObserverSlot::new()
    }
}

/// Held by a running observer thread; frees the slot when dropped.
pub struct SlotGuard<'a>(&'a ObserverSlot);

impl ObserverSlot {
    pub const fn new() -> ObserverSlot {
        ObserverSlot(std::sync::atomic::AtomicBool::new(false))
    }

    /// Claims the slot; refused while another observer thread holds it.
    pub fn arm(&self) -> Result<SlotGuard<'_>, String> {
        if self.0.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Err("a previous window observer is still active".into());
        }
        Ok(SlotGuard(self))
    }
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        self.0.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// One console `KEY_EVENT` record as `ReadConsoleInputW` returned it
/// (FakeCli `read-keys`, §4.5). `uchar` is one UTF-16 unit; `ms` is the
/// time since the read loop started.
#[derive(Debug, Clone, Copy, Default, Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct KeyRecord {
    pub down: bool,
    pub vk: u16,
    pub scan: u16,
    pub uchar: u16,
    pub control: u32,
    pub repeat: u16,
    pub ms: u64,
}

/// The code points key-down records delivered: each nonzero `uchar`
/// (repeated `repeat` times, at least once) with surrogate units joined;
/// a lone surrogate unit is kept as its own value so a loss is visible.
pub fn delivered_code_points(events: &[KeyRecord]) -> Vec<u32> {
    let mut out = Vec::new();
    let mut high: Option<u32> = None;
    for e in events.iter().filter(|e| e.down && e.uchar != 0) {
        for _ in 0..e.repeat.max(1) {
            let u = e.uchar as u32;
            match (high.take(), u) {
                (Some(h), 0xDC00..=0xDFFF) => {
                    out.push(0x10000 + ((h - 0xD800) << 10) + (u - 0xDC00));
                }
                (h, _) => {
                    out.extend(h);
                    if (0xD800..0xDC00).contains(&u) {
                        high = Some(u);
                    } else {
                        out.push(u);
                    }
                }
            }
        }
    }
    out.extend(high);
    out
}

/// F-2c fault injections for the raw backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawInject {
    BeforeSpawn,
    AfterSpawn,
    ReaderStart,
    ForceSuspended,
    ResumeFail,
}

impl RawInject {
    pub fn parse(s: &str) -> Option<RawInject> {
        Some(match s {
            "before-spawn" => RawInject::BeforeSpawn,
            "after-spawn" => RawInject::AfterSpawn,
            "reader-start" => RawInject::ReaderStart,
            "force-suspended" => RawInject::ForceSuspended,
            "resume-fail" => RawInject::ResumeFail,
            _ => return None,
        })
    }
}

/// PTY backend of a launch. `Portable` is today's portable-pty path (and
/// the only one off Windows); `Raw` is the spike's own ConPTY launch with
/// `PROC_THREAD_ATTRIBUTE_JOB_LIST` (§4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    #[default]
    Portable,
    Raw {
        conpty_flags: u32,
        inject: Option<RawInject>,
    },
}

/// A raw ConPTY launch, plugged into `Session` through portable-pty's own
/// traits. `branch` is `job_list` or `suspended`; `job_list_error` is why
/// the JOB_LIST attempt failed, when it did.
pub struct RawSpawn {
    pub master: Box<dyn portable_pty::MasterPty + Send>,
    pub child: Box<dyn portable_pty::Child + Send + Sync>,
    pub tree: Tree,
    pub branch: String,
    pub job_list_error: Option<String>,
    /// `CreateProcessW` with the job list itself failed (not an attribute
    /// setup step or an injection).
    pub job_list_rejected: bool,
    /// Each creation attempt: branch, stage, and Win32 result.
    pub attempts: Vec<serde_json::Value>,
    pub conpty_module: Option<String>,
}

/// A job member opened by pid for the kill-proof census (`Tree::open_member`).
pub enum MemberOpen {
    /// The pid no longer names a process, or now names one outside the job
    /// (the member exited and its pid was reused): it cannot survive the stop.
    Exited,
    /// Creation time read from a handle on the member, which is retained.
    Opened(u64, Retained),
    Failed(String),
}

/// One row of the process table. `pgid`/`sid` are Unix-only.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: Option<u32>,
    pub sid: Option<u32>,
    /// Opaque, monotonic per boot; used with `pid` to detect pid reuse.
    pub start_time: u64,
    pub name: String,
    pub zombie: bool,
}

/// True when `pid` still names the process that started at `start_time`
/// and has not exited (zombies count as dead).
pub fn is_alive(pid: u32, start_time: u64) -> bool {
    match proc_info(pid) {
        Some(p) => p.start_time == start_time && !p.zombie,
        None => false,
    }
}

/// Ancestry chain starting at `pid`: `(pid, name)` for `pid` and up to `depth` ancestors.
pub fn ancestry(pid: u32, depth: usize) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let mut cur = proc_info(pid);
    while let Some(p) = cur {
        out.push((p.pid, p.name.clone()));
        if out.len() > depth || p.ppid == 0 || p.ppid == p.pid {
            break;
        }
        cur = proc_info(p.ppid);
    }
    out
}

/// Roles FakeCli `spawn-tree` starts on this OS (§4.5).
pub fn spawn_tree_roles() -> &'static [&'static str] {
    SPAWN_TREE_ROLES
}

pub fn os_name() -> &'static str {
    std::env::consts::OS
}

/// A spawned child's identity: pid 0 or start time 0 (a failed lookup) is
/// none.
pub fn verified_identity(pid: u32, start_time: u64) -> Result<(u32, u64), String> {
    if pid == 0 || start_time == 0 {
        return Err(format!(
            "child identity unknown (pid {pid}, start time {start_time})"
        ));
    }
    Ok((pid, start_time))
}

/// The spawned child's verified identity, its start time read from the
/// process handle the launch holds (Windows) or the process table (Unix).
pub fn child_identity(child: &dyn portable_pty::Child) -> Result<(u32, u64), String> {
    let pid = child.process_id().unwrap_or(0);
    verified_identity(pid, child_start_time(child, pid))
}

/// Positive liveness for recovery decisions: `Some(false)` only when a full
/// process-table read shows `pid` absent, reused, or a zombie; `None` when
/// the table or the start time cannot be read (callers must refuse).
pub fn liveness(pid: u32, start_time: u64) -> Option<bool> {
    let procs = list_processes().ok()?;
    match procs.iter().find(|p| p.pid == pid) {
        None => Some(false),
        Some(p) if p.start_time == 0 => None,
        Some(p) => Some(p.start_time == start_time && !p.zombie),
    }
}

/// An `OsString` that is not valid UTF-8 on this platform (test fixture).
pub fn non_utf8_os_string() -> std::ffi::OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(vec![b'a', 0xff, b'b'])
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        std::ffi::OsString::from_wide(&[0x61, 0xD800, 0x62])
    }
}

/// A one-shot handoff from a worker thread to the thread that started it
/// (the window observer's readiness report). A zero-capacity channel: the
/// offer completes only when the starter receives it, and fails once the
/// starter has given up and dropped its side, so both agree on whether
/// the handoff happened.
pub fn rendezvous<T>() -> (Offer<T>, Claim<T>) {
    let (tx, rx) = std::sync::mpsc::sync_channel(0);
    (Offer(tx), Claim(rx))
}

/// The worker's side: `offer` is true only when the starter took the value.
pub struct Offer<T>(std::sync::mpsc::SyncSender<T>);

impl<T> Offer<T> {
    pub fn offer(self, v: T) -> bool {
        self.0.send(v).is_ok()
    }
}

/// The starter's side.
pub struct Claim<T>(std::sync::mpsc::Receiver<T>);

impl<T> Claim<T> {
    /// The offered value, if it came within `timeout`.
    pub fn claim(&self, timeout: std::time::Duration) -> Option<T> {
        self.0.recv_timeout(timeout).ok()
    }
}

#[cfg(test)]
mod tests {
    /// `read-keys`: key-down units are joined into code points (surrogates
    /// paired, a lone one kept), key-up records and zero units ignored.
    #[test]
    fn delivered_code_points_join_surrogates() {
        let k = |down, uchar| super::KeyRecord {
            down,
            uchar,
            repeat: 1,
            ..Default::default()
        };
        let evs = [
            k(true, 0xD83D),
            k(false, 0xD83D),
            k(true, 0xDC68),
            k(true, 0x200D),
            k(false, 0x200D),
            k(true, 0),
            k(true, 0x31F7),
            k(true, 0xD83D),
            k(true, 0x309A),
        ];
        assert_eq!(
            super::delivered_code_points(&evs),
            [0x1F468, 0x200D, 0x31F7, 0xD83D, 0x309A]
        );
        let mut twice = k(true, b'a' as u16);
        twice.repeat = 2;
        assert_eq!(super::delivered_code_points(&[twice]), [0x61, 0x61]);
    }

    /// M3 (r3): the `after-spawn` injection reports the injected failure
    /// only once the fixture's `OUTPUT-` line was seen; otherwise the
    /// milestone was not reached (a distinct error the oracle rejects).
    #[test]
    fn after_spawn_error_names_the_missed_milestone() {
        assert_eq!(
            super::after_spawn_error(true),
            "injected failure after spawn"
        );
        assert_eq!(
            super::after_spawn_error(false),
            "milestone not reached: OUTPUT- marker not seen in 5 s"
        );
    }

    /// M5: a readiness report offered after the starter stopped waiting
    /// is never taken as claimed (the worker then unhooks and exits), even
    /// while the starter has not dropped its side yet; one offered in time
    /// is claimed on both sides.
    #[test]
    fn late_ready_is_not_claimed() {
        use std::time::Duration;
        let (offer, claim) = super::rendezvous::<u32>();
        assert_eq!(claim.claim(Duration::from_millis(20)), None);
        let worker = std::thread::spawn(move || offer.offer(7));
        std::thread::sleep(Duration::from_millis(100));
        drop(claim);
        assert!(
            !worker.join().unwrap(),
            "a late offer must not count as claimed"
        );
        let (offer, claim) = super::rendezvous::<u32>();
        let worker = std::thread::spawn(move || offer.offer(7));
        assert_eq!(claim.claim(Duration::from_secs(5)), Some(7));
        assert!(worker.join().unwrap());
    }

    /// M5: a second observer cannot arm while the first one's thread still
    /// holds the slot; it can once that thread has exited.
    #[test]
    fn observer_slot_refuses_a_second_arm() {
        let slot = super::ObserverSlot::new();
        let g = slot.arm().unwrap();
        assert!(slot.arm().is_err());
        drop(g);
        assert!(slot.arm().is_ok());
    }

    /// A process is named after its executable image: the test binary's
    /// full name (longer than the 16-byte `pbi_name`) comes back intact.
    #[test]
    fn process_name_is_the_executable() {
        let exe = std::env::current_exe().unwrap();
        let want = exe.file_name().unwrap().to_string_lossy().into_owned();
        let got = super::proc_info(std::process::id()).unwrap().name;
        assert_eq!(got, want);
    }
}
