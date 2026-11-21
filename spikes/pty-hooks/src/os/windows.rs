//! Windows side of the OS boundary: Toolhelp enumeration, Job Objects
//! (assign-after and the C-3 job-first launch), console modes, and the C-5
//! window observer. Minimum needed for sets B and C (§0, §4.8); not tuned.

use std::ffi::{OsStr, OsString, c_void};
use std::io::{self, Read};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_NO_MORE_FILES, FILETIME, HANDLE, HWND,
    INVALID_HANDLE_VALUE, LPARAM, LRESULT, STILL_ACTIVE, WPARAM,
};
use windows_sys::Win32::System::Console::{
    COORD, CTRL_BREAK_EVENT, ClosePseudoConsole, CreatePseudoConsole, ENABLE_ECHO_INPUT,
    ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING, GenerateConsoleCtrlEvent, GetConsoleCP, GetConsoleMode,
    GetConsoleWindow, GetStdHandle, HPCON, ReadConsoleW, ResizePseudoConsole, STD_INPUT_HANDLE,
    STD_OUTPUT_HANDLE, SetConsoleMode,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
    JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, QueryInformationJobObject,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW,
};
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DETACHED_PROCESS, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetCurrentThreadId, GetExitCodeProcess,
    GetProcessHandleCount, GetProcessIdOfThread, GetProcessTimes, INFINITE,
    InitializeProcThreadAttributeList, OpenProcess, OpenThread, PROC_THREAD_ATTRIBUTE_JOB_LIST,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    THREAD_QUERY_LIMITED_INFORMATION, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject,
};
use windows_sys::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateCaret, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    EVENT_OBJECT_SHOW, GetClassNameW, GetMessageW, GetWindowThreadProcessId, MSG, PM_NOREMOVE,
    PM_REMOVE, PeekMessageW, PostThreadMessageW, RegisterClassW, SW_SHOW, ShowCaret, ShowWindow,
    TranslateMessage, WINEVENT_OUTOFCONTEXT, WM_QUIT, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};

use super::{Drain, ProcInfo, close_bounded};

pub const SPAWN_TREE_ROLES: &[&str] = &["plain", "breakaway"];

pub fn default_root() -> PathBuf {
    PathBuf::from(r"C:\chs")
}

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Owned kernel handle, closed on drop. Stored as usize so it is Send + Sync.
#[derive(Debug)]
struct Handle(usize);

impl Handle {
    fn new(h: HANDLE) -> Option<Handle> {
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            None
        } else {
            Some(Handle(h as usize))
        }
    }
    fn raw(&self) -> HANDLE {
        self.0 as HANDLE
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: we own this handle.
        unsafe {
            CloseHandle(self.raw());
        }
    }
}

fn filetime_u64(ft: &FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

/// (creation time, still running) for `pid`, if it can be opened; the
/// running flag is `None` when the exit code cannot be read.
fn process_times(pid: u32) -> Option<(u64, Option<bool>)> {
    // SAFETY: plain Win32 calls on a handle we own for the duration.
    unsafe {
        let h = Handle::new(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid))?;
        let mut c: FILETIME = std::mem::zeroed();
        let mut e: FILETIME = std::mem::zeroed();
        let mut k: FILETIME = std::mem::zeroed();
        let mut u: FILETIME = std::mem::zeroed();
        if GetProcessTimes(h.raw(), &mut c, &mut e, &mut k, &mut u) == 0 {
            return None;
        }
        let mut code = 0u32;
        let running =
            (GetExitCodeProcess(h.raw(), &mut code) != 0).then_some(code == STILL_ACTIVE as u32);
        Some((filetime_u64(&c), running))
    }
}

pub fn list_processes() -> io::Result<Vec<ProcInfo>> {
    // SAFETY: Toolhelp snapshot walk with a correctly sized entry.
    unsafe {
        let snap = Handle::new(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0))
            .ok_or_else(io::Error::last_os_error)?;
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut out = Vec::new();
        if Process32FirstW(snap.raw(), &mut e) == 0 {
            return Err(io::Error::last_os_error());
        }
        loop {
            let len = e
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(e.szExeFile.len());
            let name = String::from_utf16_lossy(&e.szExeFile[..len]);
            // An unreadable exit code leaves the identity unknown (start time
            // 0, so `liveness` reports `None`), never dead.
            let (start_time, running) = match process_times(e.th32ProcessID) {
                Some((st, Some(r))) => (st, r),
                _ => (0, true),
            };
            out.push(ProcInfo {
                pid: e.th32ProcessID,
                ppid: e.th32ParentProcessID,
                pgid: None,
                sid: None,
                start_time,
                name,
                zombie: !running,
            });
            if Process32NextW(snap.raw(), &mut e) == 0 {
                // Only ERROR_NO_MORE_FILES ends the walk; anything else is a
                // failed enumeration, never a short process table.
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                    break;
                }
                return Err(err);
            }
        }
        Ok(out)
    }
}

/// A handle retained on a live process (identity verified by creation
/// time) for positive death evidence after the stop.
pub struct Retained(Handle);

impl Retained {
    pub fn open(pid: u32, start: u64) -> Option<Retained> {
        // SAFETY: handle owned by the wrapper; identity checked before use.
        let h = Handle::new(unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            )
        })?;
        (creation_time(h.raw()) == start).then_some(Retained(h))
    }

    /// `Some(true)` once the process has exited (the handle is signalled),
    /// `Some(false)` if it still runs at `until`, `None` if the wait failed.
    pub fn wait_dead(&self, until: Instant) -> Option<bool> {
        let ms = until.saturating_duration_since(Instant::now()).as_millis() as u32;
        match wait_exit(self.0.raw(), ms) {
            "exited" => Some(true),
            "timeout" => Some(false),
            _ => None,
        }
    }
}

pub fn proc_info(pid: u32) -> Option<ProcInfo> {
    list_processes().ok()?.into_iter().find(|p| p.pid == pid)
}

pub fn force_kill(pid: u32, start_time: u64) -> bool {
    // SAFETY: handle is owned; identity rechecked via creation time before terminating.
    unsafe {
        let Some(h) = Handle::new(OpenProcess(
            PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )) else {
            return false;
        };
        let mut c: FILETIME = std::mem::zeroed();
        let mut e: FILETIME = std::mem::zeroed();
        let mut k: FILETIME = std::mem::zeroed();
        let mut u: FILETIME = std::mem::zeroed();
        if GetProcessTimes(h.raw(), &mut c, &mut e, &mut k, &mut u) == 0
            || filetime_u64(&c) != start_time
        {
            return false;
        }
        TerminateProcess(h.raw(), 1) != 0
    }
}

/// Simulated Ctrl-C for B-10: CTRL_BREAK to a child started with `interruptible`.
pub fn send_interrupt(pid: u32) -> io::Result<()> {
    // SAFETY: plain call.
    if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) } != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

pub fn interruptible(cmd: &mut Command) {
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

fn new_kill_on_close_job() -> io::Result<Handle> {
    // SAFETY: job creation and limit setting with a correctly sized struct.
    unsafe {
        let job = Handle::new(CreateJobObjectW(std::ptr::null(), std::ptr::null()))
            .ok_or_else(io::Error::last_os_error)?;
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job.raw(),
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }
}

fn job_members(job: HANDLE) -> io::Result<Vec<u32>> {
    const N: usize = 1024;
    let off = std::mem::offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList);
    let bytes = off + N * std::mem::size_of::<usize>();
    let mut buf = vec![0usize; bytes.div_ceil(std::mem::size_of::<usize>())];
    // SAFETY: buffer is usize-aligned and `bytes` long; we read only the reported count.
    unsafe {
        if QueryInformationJobObject(
            job,
            JobObjectBasicProcessIdList,
            buf.as_mut_ptr() as *mut c_void,
            bytes as u32,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let list = &*(buf.as_ptr() as *const JOBOBJECT_BASIC_PROCESS_ID_LIST);
        if list.NumberOfAssignedProcesses as usize > N {
            return Err(io::Error::other(
                "job has more processes than the list holds",
            ));
        }
        let n = list.NumberOfProcessIdsInList as usize;
        let ids = (buf.as_ptr() as *const u8).add(off) as *const usize;
        Ok((0..n).map(|i| *ids.add(i) as u32).collect())
    }
}

/// Harness-established ownership: a `KILL_ON_JOB_CLOSE` job, assigned after
/// spawn (portable-pty) or given at creation (raw backend).
pub struct Tree {
    job: Handle,
    /// `after_spawn`, `job_list`, or `suspended`.
    assign: &'static str,
}

impl Tree {
    pub fn attach(child: &dyn portable_pty::Child) -> io::Result<Tree> {
        let h = child
            .as_raw_handle()
            .ok_or_else(|| io::Error::other("child has no handle"))?;
        let job = new_kill_on_close_job()?;
        // SAFETY: both handles are valid for the call.
        if unsafe { AssignProcessToJobObject(job.raw(), h as HANDLE) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Tree {
            job,
            assign: "after_spawn",
        })
    }

    pub fn describe(&self) -> serde_json::Value {
        serde_json::json!({"mechanism": "job_object", "kill_on_job_close": true, "assign": self.assign})
    }

    /// `TerminateJobObject`; also whether it succeeded.
    pub fn kill(&self, _grace: Duration) -> (Vec<String>, bool) {
        // SAFETY: valid job handle.
        let ok = unsafe { TerminateJobObject(self.job.raw(), 1) } != 0;
        let step = if ok {
            "TerminateJobObject=ok".to_string()
        } else {
            format!("TerminateJobObject failed: {}", io::Error::last_os_error())
        };
        (vec![step], ok)
    }

    /// Best-effort membership for the kill scan (the lineage scan backs it up).
    pub fn members(&self) -> Vec<u32> {
        job_members(self.job.raw()).unwrap_or_default()
    }

    /// Positive job membership backs the §4.8 `backend` kill proof.
    pub fn proves_membership(&self) -> bool {
        true
    }

    /// Membership; an enumeration failure is an error, never an empty job.
    pub fn members_checked(&self) -> Result<Vec<u32>, String> {
        job_members(self.job.raw()).map_err(|e| e.to_string())
    }
}

/// Creates a new file, failing if it exists (the run dir is private).
pub fn create_private_file(path: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Non-interactive harness launch: no console window (C-5).
pub fn no_window(cmd: &mut Command) {
    cmd.creation_flags(CREATE_NO_WINDOW);
}

pub fn spawn_escapee(cmd: &mut Command) -> io::Result<(Child, &'static str)> {
    cmd.creation_flags(CREATE_BREAKAWAY_FROM_JOB);
    match cmd.spawn() {
        Ok(c) => Ok((c, "breakaway")),
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            cmd.creation_flags(0);
            cmd.spawn().map(|c| (c, "breakaway_denied"))
        }
        Err(e) => Err(e),
    }
}

pub fn ignore_term_signals() {}

pub struct RawMode {
    input: (usize, u32),
    output: Option<(usize, u32)>,
}

pub fn enable_raw_mode() -> io::Result<RawMode> {
    // SAFETY: console mode get/set on std handles.
    unsafe {
        let hin = GetStdHandle(STD_INPUT_HANDLE);
        let mut mode = 0u32;
        if GetConsoleMode(hin, &mut mode) == 0 {
            return Err(io::Error::last_os_error());
        }
        let raw = (mode & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
            | ENABLE_VIRTUAL_TERMINAL_INPUT;
        if SetConsoleMode(hin, raw) == 0 {
            return Err(io::Error::last_os_error());
        }
        let hout = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut omode = 0u32;
        let output = if GetConsoleMode(hout, &mut omode) != 0 {
            SetConsoleMode(hout, omode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
            Some((hout as usize, omode))
        } else {
            None
        };
        Ok(RawMode {
            input: (hin as usize, mode),
            output,
        })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restores modes read in enable_raw_mode.
        unsafe {
            SetConsoleMode(self.input.0 as HANDLE, self.input.1);
            if let Some((h, m)) = self.output {
                SetConsoleMode(h as HANDLE, m);
            }
        }
    }
}

/// `(cols, rows)` of the console window on stdout.
pub fn console_size() -> Option<(u16, u16)> {
    // SAFETY: plain query into a zeroed struct.
    unsafe {
        let mut i: windows_sys::Win32::System::Console::CONSOLE_SCREEN_BUFFER_INFO =
            std::mem::zeroed();
        if windows_sys::Win32::System::Console::GetConsoleScreenBufferInfo(
            GetStdHandle(STD_OUTPUT_HANDLE),
            &mut i,
        ) == 0
        {
            return None;
        }
        let w = i.srWindow;
        Some(((w.Right - w.Left + 1) as u16, (w.Bottom - w.Top + 1) as u16))
    }
}

pub fn enable_vt_output() {
    // SAFETY: console mode get/set on stdout.
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut m = 0u32;
        if GetConsoleMode(h, &mut m) != 0 {
            SetConsoleMode(h, m | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}

/// `ReadConsoleW` → UTF-8, keeping a dangling high surrogate across reads.
struct ConsoleReader {
    handle: usize,
    pending: Vec<u8>,
    high: Option<u16>,
}

impl Read for ConsoleReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pending.is_empty() {
            let mut buf = [0u16; 1024];
            let mut n = 0u32;
            // SAFETY: buffer length matches the char count passed.
            let ok = unsafe {
                ReadConsoleW(
                    self.handle as HANDLE,
                    buf.as_mut_ptr() as *mut c_void,
                    buf.len() as u32,
                    &mut n,
                    std::ptr::null(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            if n == 0 {
                return Ok(0);
            }
            let mut units: Vec<u16> = self.high.take().into_iter().collect();
            units.extend_from_slice(&buf[..n as usize]);
            if let Some(last) = units.last()
                && (0xD800..0xDC00).contains(last)
            {
                self.high = units.pop();
            }
            for c in char::decode_utf16(units) {
                let c = c.unwrap_or('\u{FFFD}');
                let mut tmp = [0u8; 4];
                self.pending
                    .extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
            }
        }
        let n = out.len().min(self.pending.len());
        out[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

pub fn stdin_reader() -> Box<dyn Read + Send> {
    // SAFETY: probing the console mode of stdin.
    let h = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let mut m = 0u32;
    if unsafe { GetConsoleMode(h, &mut m) } != 0 {
        Box::new(ConsoleReader {
            handle: h as usize,
            pending: Vec::new(),
            high: None,
        })
    } else {
        Box::new(io::stdin())
    }
}

pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)
}

pub fn set_private_file(_path: &Path) -> io::Result<()> {
    Ok(())
}

pub fn make_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// ACL review is out of the spike's scope on Windows (§4.3 checks Unix modes only).
pub fn check_secure_path(path: &Path) -> Result<(), String> {
    std::fs::symlink_metadata(path)
        .map(|_| ())
        .map_err(|e| format!("{}: {e}", path.display()))
}

pub fn os_version() -> String {
    // `ver` would need a shell; the report records the kernel32 file path instead.
    std::env::var("OS").unwrap_or_else(|_| "Windows".into())
}

fn module_path(name: &str) -> Option<String> {
    // SAFETY: module lookup without loading; buffer length passed.
    unsafe {
        let m = GetModuleHandleW(wide(OsStr::new(name)).as_ptr());
        if m.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let n = GetModuleFileNameW(m, buf.as_mut_ptr(), buf.len() as u32);
        if n == 0 {
            None
        } else {
            Some(String::from_utf16_lossy(&buf[..n as usize]))
        }
    }
}

/// Which ConPTY provider is loaded: a `conpty.dll` path, else kernel32's path.
pub fn conpty_module() -> Option<String> {
    module_path("conpty.dll").or_else(|| module_path("kernel32.dll"))
}

pub fn self_job_info() -> serde_json::Value {
    // SAFETY: queries about the current process; NULL job = the caller's job.
    unsafe {
        let mut in_job = 0;
        if IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) == 0 {
            return serde_json::json!({"in_job": null, "error": io::Error::last_os_error().to_string()});
        }
        if in_job == 0 {
            return serde_json::json!({"in_job": false});
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        let ok = QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            std::ptr::null_mut(),
        );
        if ok == 0 {
            return serde_json::json!({"in_job": true, "limits": null, "error": io::Error::last_os_error().to_string()});
        }
        let f = info.BasicLimitInformation.LimitFlags;
        serde_json::json!({
            "in_job": true,
            "limit_flags": format!("{f:#x}"),
            "kill_on_job_close": f & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0,
            "breakaway_ok": f & JOB_OBJECT_LIMIT_BREAKAWAY_OK != 0,
            "silent_breakaway_ok": f & JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK != 0,
        })
    }
}

pub fn console_window_present() -> Option<bool> {
    // SAFETY: plain call.
    Some(!unsafe { GetConsoleWindow() }.is_null())
}

pub fn console_code_page() -> Option<u32> {
    // SAFETY: plain call.
    Some(unsafe { GetConsoleCP() })
}

/// C-5 launch: `DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB | CREATE_NEW_PROCESS_GROUP`.
/// Returns whether breakaway was permitted.
pub fn spawn_detached(cmd: &mut Command) -> Result<(Child, bool), String> {
    cmd.creation_flags(DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB | CREATE_NEW_PROCESS_GROUP);
    match cmd.spawn() {
        Ok(c) => Ok((c, true)),
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
            cmd.spawn().map(|c| (c, false)).map_err(|e| e.to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// One observer's state, owned by that observer (never global): a thread
/// abandoned by a stop that timed out can only write into its own state,
/// never into a later launch's evidence.
struct ObsState {
    t0: Instant,
    /// The observation job, owned for the observer's lifetime.
    job: Option<Arc<Handle>>,
    events: Mutex<Vec<super::WindowEvent>>,
}

thread_local! {
    /// The state of the observer whose message loop runs on this thread
    /// (out-of-context WinEvent callbacks run on the hooking thread).
    static OBS: std::cell::RefCell<Option<Arc<ObsState>>> = const { std::cell::RefCell::new(None) };
}

/// At most one observer thread alive at a time.
static OBSERVER_SLOT: super::ObserverSlot = super::ObserverSlot::new();

fn image_of(h: HANDLE) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut n = buf.len() as u32;
    // SAFETY: buffer length passed in `n`.
    if unsafe { QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut n) } == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..n as usize]))
}

/// The process owning thread `tid`, if it can still be opened.
fn thread_pid(tid: u32) -> Option<u32> {
    // SAFETY: handle owned for the duration of the query.
    unsafe {
        let h = Handle::new(OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, tid))?;
        let pid = GetProcessIdOfThread(h.raw());
        (pid != 0).then_some(pid)
    }
}

unsafe extern "system" fn on_show(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    event_thread: u32,
    event_ms: u32,
) {
    let Some(state) = OBS.with(|o| o.borrow().clone()) else {
        return;
    };
    let time_ms = state.t0.elapsed().as_millis() as u64;
    let mut owner = 0u32;
    let mut owner_thread = 0u32;
    let mut class = String::new();
    if !hwnd.is_null() {
        let mut buf = [0u16; 256];
        // SAFETY: hwnd comes from the system for this callback; buffer length passed.
        unsafe {
            owner_thread = GetWindowThreadProcessId(hwnd, &mut owner);
            let n = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
            if n > 0 {
                class = String::from_utf16_lossy(&buf[..n as usize]);
            }
        }
    }
    let (mut image, mut in_job) = (None, None);
    if owner != 0 {
        // SAFETY: handle owned for the duration; the job handle is owned by this observer.
        unsafe {
            if let Some(h) = Handle::new(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, owner)) {
                image = image_of(h.raw());
                if let Some(job) = &state.job {
                    let mut r = 0;
                    if IsProcessInJob(h.raw(), job.raw(), &mut r) != 0 {
                        in_job = Some(r != 0);
                    }
                }
            }
        }
    }
    let info = (owner != 0).then(|| proc_info(owner)).flatten();
    let ev = super::WindowEvent {
        time_ms,
        hwnd: hwnd as usize as u64,
        id_object,
        id_child,
        class,
        owner_pid: owner,
        image,
        ppid: info.as_ref().map(|p| p.ppid),
        start_time: info.as_ref().map(|p| p.start_time).filter(|t| *t != 0),
        in_job,
        event_ms,
        event_thread,
        event_pid: thread_pid(event_thread),
        owner_thread,
    };
    state
        .events
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(ev);
}

/// After the observed process ended, the observer keeps pumping this long so
/// callbacks already queued are delivered (a flash at the end of a launch).
pub const OBSERVER_DRAIN: Duration = Duration::from_millis(500);

/// Out-of-context `EVENT_OBJECT_SHOW` observer on its own message-loop thread.
pub struct WindowObserver {
    thread_id: u32,
    state: Arc<ObsState>,
    /// The loop thread returns whether its message loop ended cleanly (WM_QUIT).
    join: std::thread::JoinHandle<bool>,
}

impl WindowObserver {
    /// Arms the observer; `job` is the observation job whose membership each
    /// event records. Call before the observed process is created. Refused
    /// while an earlier observer's thread is still alive.
    pub fn start(job: Option<&ObservationJob>) -> Result<WindowObserver, String> {
        let state = Arc::new(ObsState {
            t0: Instant::now(),
            job: job.map(|j| j.job.clone()),
            events: Mutex::new(Vec::new()),
        });
        let slot = OBSERVER_SLOT.arm()?;
        let (tx, rx) = std::sync::mpsc::channel();
        let st = state.clone();
        let join = std::thread::Builder::new()
            .name("window-observer".into())
            .spawn(move || {
                // The slot stays claimed until this thread exits.
                let _slot = slot;
                OBS.with(|o| *o.borrow_mut() = Some(st));
                // SAFETY: hook + message loop on this thread; unhooked before exit.
                unsafe {
                    // Create the thread's message queue before reporting the id.
                    let mut msg: MSG = std::mem::zeroed();
                    PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
                    let hook = SetWinEventHook(
                        EVENT_OBJECT_SHOW,
                        EVENT_OBJECT_SHOW,
                        std::ptr::null_mut(),
                        Some(on_show),
                        0,
                        0,
                        WINEVENT_OUTOFCONTEXT,
                    );
                    let ready = tx.send((GetCurrentThreadId(), !hook.is_null())).is_ok();
                    if hook.is_null() {
                        return false;
                    }
                    if !ready {
                        // The starter gave up waiting: never observe unclaimed.
                        UnhookWinEvent(hook);
                        return false;
                    }
                    let clean = loop {
                        match GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) {
                            0 => break true,
                            -1 => break false,
                            _ => {
                                TranslateMessage(&msg);
                                DispatchMessageW(&msg);
                            }
                        }
                    };
                    UnhookWinEvent(hook);
                    clean
                }
            })
            .map_err(|e| format!("observer thread: {e}"))?;
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok((tid, true)) => Ok(WindowObserver {
                thread_id: tid,
                state,
                join,
            }),
            Ok((_, false)) => Err("SetWinEventHook failed".into()),
            // Dropping `rx` makes the thread's readiness send fail: it unhooks.
            Err(_) => Err("observer thread did not start".into()),
        }
    }

    /// Stops the loop after the drain interval (bounded: a thread that does
    /// not quit is abandoned and keeps the observer slot, so no later
    /// observer arms). Returns this observer's events and its health: a
    /// failed `WM_QUIT` post, a thread that did not exit, or a message loop
    /// that ended on an error means the events may be incomplete.
    pub fn stop(self) -> super::ObserverStop {
        std::thread::sleep(OBSERVER_DRAIN);
        // SAFETY: posting WM_QUIT to the observer thread.
        let posted = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) } != 0;
        let health = if !posted {
            Err(format!(
                "WM_QUIT post failed: {}",
                io::Error::last_os_error()
            ))
        } else {
            let end = Instant::now() + Duration::from_secs(2);
            while !self.join.is_finished() && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(10));
            }
            if !self.join.is_finished() {
                Err("observer thread did not exit".into())
            } else {
                match self.join.join() {
                    Ok(true) => Ok(()),
                    Ok(false) => Err("observer message loop ended on an error".into()),
                    Err(_) => Err("observer thread panicked".into()),
                }
            }
        };
        let events = self
            .state
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        (events, health)
    }
}

/// A `KILL_ON_JOB_CLOSE` job each observed launch runs in (C-5): the child
/// waits for `go` on stdin until it has been assigned.
pub struct ObservationJob {
    job: Arc<Handle>,
}

impl ObservationJob {
    pub fn new() -> io::Result<ObservationJob> {
        Ok(ObservationJob {
            job: Arc::new(new_kill_on_close_job()?),
        })
    }
    pub fn assign(&self, child: &Child) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: both handles are valid for the call.
        if unsafe { AssignProcessToJobObject(self.job.raw(), child.as_raw_handle() as HANDLE) } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

unsafe extern "system" fn calib_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // SAFETY: default handling for our own window class.
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

/// Pumps this thread's messages for `d`.
fn pump(d: Duration) {
    let end = Instant::now() + d;
    // SAFETY: plain message pump on the calling thread.
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while Instant::now() < end {
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// C-5 calibration helper body (`chs-fake-cli --mode window-calibrate`): a
/// caret first (a non-window control event), then one visible top-level
/// window of its own (so the event's owner pid is this process), shown for
/// 1 s, or only in the last 50 ms with `at_exit`. Returns `(pid, start
/// time, hwnd)`; the caller reports them before the window shows.
pub fn show_calibration_window(
    at_exit: bool,
    report: &dyn Fn(u32, u64, u64),
) -> Result<(u32, u64, u64), String> {
    let pid = std::process::id();
    let start = proc_info(pid).map(|p| p.start_time).unwrap_or(0);
    let class = wide(OsStr::new("ChsCalibrationWindow"));
    let title = wide(OsStr::new("chs calibration"));
    // SAFETY: class registration and window creation for our own process.
    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(calib_proc);
        wc.hInstance = hinst;
        wc.lpszClassName = class.as_ptr();
        if RegisterClassW(&wc) == 0 {
            return Err(format!("RegisterClassW: {}", io::Error::last_os_error()));
        }
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            100,
            100,
            320,
            120,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return Err(format!("CreateWindowExW: {}", io::Error::last_os_error()));
        }
        let h = hwnd as usize as u64;
        report(pid, start, h);
        // The caret control: a show event with OBJID_CARET from the same pid.
        CreateCaret(hwnd, std::ptr::null_mut(), 2, 16);
        ShowCaret(hwnd);
        if at_exit {
            pump(Duration::from_millis(300));
        }
        ShowWindow(hwnd, SW_SHOW);
        pump(if at_exit {
            Duration::from_millis(50)
        } else {
            Duration::from_secs(1)
        });
        DestroyWindow(hwnd);
        Ok((pid, start, h))
    }
}

/// `GetProcessHandleCount` of this process (F-2c).
pub fn process_handle_count() -> Option<u32> {
    let mut n = 0u32;
    // SAFETY: plain call on the current-process pseudo handle.
    (unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut n) } != 0).then_some(n)
}

fn reg_sz(root: HKEY, sub: &str, value: Option<&str>) -> Option<String> {
    let sub = wide(OsStr::new(sub));
    let val = value.map(|v| wide(OsStr::new(v)));
    let mut buf = vec![0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: buffer size passed in bytes.
    let r = unsafe {
        RegGetValueW(
            root,
            sub.as_ptr(),
            val.as_ref().map_or(std::ptr::null(), |v| v.as_ptr()),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut c_void,
            &mut len,
        )
    };
    if r != 0 {
        return None;
    }
    let n = (len as usize / 2).min(buf.len());
    let s = String::from_utf16_lossy(&buf[..n]);
    Some(s.trim_end_matches('\0').to_string())
}

/// The host's default-terminal setting (`HKCU\Console\%%Startup`) as raw
/// CLSID values, each resolved to an executable only through a plain
/// `HKCR\CLSID\{…}\LocalServer32` default value, else `unresolved`.
pub fn default_terminal_delegation() -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for name in ["DelegationConsole", "DelegationTerminal"] {
        let raw = reg_sz(HKEY_CURRENT_USER, r"Console\%%Startup", Some(name));
        let exe = raw.as_ref().and_then(|c| {
            reg_sz(
                HKEY_CLASSES_ROOT,
                &format!(r"CLSID\{c}\LocalServer32"),
                None,
            )
        });
        out.insert(
            name.into(),
            serde_json::json!({"clsid": raw, "exe": exe.unwrap_or_else(|| "unresolved".into())}),
        );
    }
    serde_json::Value::Object(out)
}

fn quote_arg(arg: &OsStr, out: &mut Vec<u16>) {
    let s: Vec<u16> = arg.encode_wide().collect();
    let needs = s.is_empty()
        || s.iter()
            .any(|c| *c == b' ' as u16 || *c == b'\t' as u16 || *c == b'"' as u16);
    if !needs {
        out.extend_from_slice(&s);
        return;
    }
    out.push(b'"' as u16);
    let mut backslashes = 0;
    for &c in &s {
        if c == b'\\' as u16 {
            backslashes += 1;
        } else if c == b'"' as u16 {
            out.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
            out.push(c);
            backslashes = 0;
        } else {
            out.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
            out.push(c);
            backslashes = 0;
        }
    }
    out.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
    out.push(b'"' as u16);
}

fn env_block(set: &[(String, String)], remove: &[String]) -> Vec<u16> {
    let mut vars: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(k, _)| {
            let k = k.to_string_lossy().to_ascii_uppercase();
            !remove.iter().any(|r| r.eq_ignore_ascii_case(&k))
                && !set.iter().any(|(s, _)| s.eq_ignore_ascii_case(&k))
        })
        .collect();
    for (k, v) in set {
        vars.push((k.into(), v.into()));
    }
    vars.sort_by_key(|a| a.0.to_string_lossy().to_ascii_uppercase());
    let mut block = Vec::new();
    for (k, v) in vars {
        block.extend(k.encode_wide());
        block.push(b'=' as u16);
        block.extend(v.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

struct AttrList(Vec<u8>);

impl AttrList {
    fn new(count: u32) -> io::Result<AttrList> {
        let mut size = 0usize;
        // SAFETY: first call only reports the size; second initializes our buffer.
        unsafe {
            InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &mut size);
            let mut buf = vec![0u8; size];
            if InitializeProcThreadAttributeList(
                buf.as_mut_ptr() as *mut c_void,
                count,
                0,
                &mut size,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(AttrList(buf))
        }
    }
    fn ptr(&mut self) -> *mut c_void {
        self.0.as_mut_ptr() as *mut c_void
    }
    fn set(&mut self, attr: usize, value: *const c_void, size: usize) -> io::Result<()> {
        // SAFETY: value outlives the list's use in CreateProcessW (caller keeps it alive).
        if unsafe {
            UpdateProcThreadAttribute(
                self.ptr(),
                0,
                attr,
                value,
                size,
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for AttrList {
    fn drop(&mut self) {
        // SAFETY: list was initialized in new().
        unsafe { DeleteProcThreadAttributeList(self.ptr()) }
    }
}

/// The last raw launch's attempts (each with its stage and Win32 result),
/// kept so a launch whose branches all failed still leaves a record.
static RAW_LAUNCH: Mutex<Option<serde_json::Value>> = Mutex::new(None);

/// The last raw launch's attempts and whether JOB_LIST was rejected.
pub fn raw_last_launch() -> Option<serde_json::Value> {
    RAW_LAUNCH.lock().ok().and_then(|l| l.clone())
}

/// Records of raw-backend teardowns (F-2c evidence), newest last.
static RAW_TEARDOWNS: Mutex<Vec<serde_json::Value>> = Mutex::new(Vec::new());

fn record_teardown(v: serde_json::Value) {
    if let Ok(mut t) = RAW_TEARDOWNS.lock() {
        t.push(v);
    }
}

/// The last raw-backend teardown record.
pub fn raw_last_teardown() -> Option<serde_json::Value> {
    RAW_TEARDOWNS.lock().ok().and_then(|t| t.last().cloned())
}

/// Bound on `ClosePseudoConsole` (run on a helper thread).
const CLOSE_BOUND: Duration = Duration::from_secs(3);

/// Owned pseudoconsole, closed on drop.
struct Pcon(HPCON);

impl Drop for Pcon {
    fn drop(&mut self) {
        // SAFETY: hpc is owned and closed once.
        unsafe { ClosePseudoConsole(self.0) }
    }
}

/// Everything a raw launch acquired, each held from the moment it is
/// created. Dropping it (every early return) releases in a fixed order:
/// terminate the job and process if one exists, close the pseudoconsole
/// (the drain keeps reading, or the read end is already closed), then the
/// pipe ends; the drain is waited for, bounded, and the teardown recorded.
#[derive(Default)]
struct Setup {
    job: Option<Handle>,
    process: Option<Handle>,
    thread: Option<Handle>,
    pcon: Option<Pcon>,
    input: Option<std::fs::File>,
    in_r: Option<Handle>,
    out_w: Option<Handle>,
    drain: Option<Drain>,
    pid: u32,
    start: u64,
    step: &'static str,
    /// `after-spawn`: output bytes seen, and whether the child still ran,
    /// when the injection fired.
    output_pending: Option<u64>,
    running_at_injection: Option<bool>,
}

/// Bound on waiting for a terminated process to exit (retained handle).
const PROCESS_EXIT_WAIT_MS: u32 = 2000;

/// Positive exit evidence through a retained process handle: `exited`,
/// `timeout`, or `wait_failed`.
fn wait_exit(h: HANDLE, ms: u32) -> &'static str {
    // SAFETY: plain wait on a handle the caller owns.
    match unsafe { WaitForSingleObject(h, ms) } {
        0 => "exited",
        0x102 => "timeout",
        _ => "wait_failed",
    }
}

fn still_active(h: HANDLE) -> bool {
    let mut code = 0u32;
    // SAFETY: plain query on a handle the caller owns.
    unsafe { GetExitCodeProcess(h, &mut code) != 0 && code == STILL_ACTIVE as u32 }
}

impl Drop for Setup {
    fn drop(&mut self) {
        if self.pcon.is_none()
            && self.process.is_none()
            && self.job.is_none()
            && self.drain.is_none()
        {
            return;
        }
        let t = Instant::now();
        let mut steps = Vec::new();
        // SAFETY: owned handles.
        unsafe {
            if let Some(j) = &self.job {
                steps.push(format!(
                    "TerminateJobObject={}",
                    TerminateJobObject(j.raw(), 1) != 0
                ));
            }
            if let Some(p) = &self.process {
                steps.push(format!(
                    "TerminateProcess={}",
                    TerminateProcess(p.raw(), 1) != 0
                ));
            }
        }
        // The process handle is released only after its exit is confirmed.
        let process_exit = self
            .process
            .as_ref()
            .map(|p| wait_exit(p.raw(), PROCESS_EXIT_WAIT_MS));
        self.thread.take();
        self.process.take();
        let read_end_closed = self.drain.is_none();
        let had_pcon = self.pcon.is_some();
        let close = self
            .pcon
            .take()
            .and_then(|p| close_bounded(move || drop(p), CLOSE_BOUND));
        self.input.take();
        self.in_r.take();
        self.out_w.take();
        let drain_end = self.drain.take().and_then(|d| {
            d.wait(Duration::from_secs(2));
            d.end()
        });
        self.job.take();
        record_teardown(serde_json::json!({
            "path": "setup_error", "failed_at": self.step, "child_pid": (self.pid != 0).then_some(self.pid),
            "child_start": (self.start != 0).then_some(self.start), "steps": steps,
            "process_exit": process_exit, "output_pending_bytes": self.output_pending,
            "child_running_at_injection": self.running_at_injection,
            "close_pseudoconsole_ms": close, "close_hung": had_pcon && close.is_none(),
            "drain_reached_eof": drain_end.as_deref() == Some("eof"), "drain_end": drain_end,
            "host_read_end_closed": read_end_closed, "teardown_ms": t.elapsed().as_millis() as u64,
        }));
    }
}

/// `MasterPty` over the raw pseudoconsole. Drop closes the pseudoconsole
/// while the drain keeps reading, so `PtyIo::close_master` keeps its order.
struct RawMaster {
    pcon: Mutex<Option<Pcon>>,
    input: Mutex<Option<std::fs::File>>,
    drain: Mutex<Option<Drain>>,
    size: Mutex<portable_pty::PtySize>,
    fail_reader: AtomicBool,
}

impl portable_pty::MasterPty for RawMaster {
    fn resize(&self, size: portable_pty::PtySize) -> Result<(), anyhow::Error> {
        let p = self.pcon.lock().unwrap_or_else(|e| e.into_inner());
        let Some(p) = p.as_ref() else {
            anyhow::bail!("pseudoconsole closed");
        };
        // SAFETY: owned pseudoconsole.
        let hr = unsafe {
            ResizePseudoConsole(
                p.0,
                COORD {
                    X: size.cols as i16,
                    Y: size.rows as i16,
                },
            )
        };
        if hr < 0 {
            anyhow::bail!("ResizePseudoConsole hr={hr:#x}");
        }
        *self.size.lock().unwrap_or_else(|e| e.into_inner()) = size;
        Ok(())
    }

    fn get_size(&self) -> Result<portable_pty::PtySize, anyhow::Error> {
        Ok(*self.size.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>, anyhow::Error> {
        if self.fail_reader.swap(false, Ordering::SeqCst) {
            anyhow::bail!("injected reader-start failure");
        }
        let d = self.drain.lock().unwrap_or_else(|e| e.into_inner());
        let Some(d) = d.as_ref() else {
            anyhow::bail!("no output drain");
        };
        Ok(Box::new(d.attach().map_err(anyhow::Error::msg)?))
    }

    fn take_writer(&self) -> Result<Box<dyn std::io::Write + Send>, anyhow::Error> {
        match self.input.lock().unwrap_or_else(|e| e.into_inner()).take() {
            Some(f) => Ok(Box::new(f)),
            None => anyhow::bail!("writer already taken"),
        }
    }
}

impl Drop for RawMaster {
    fn drop(&mut self) {
        let t = Instant::now();
        let pcon = self.pcon.lock().unwrap_or_else(|e| e.into_inner()).take();
        let had_pcon = pcon.is_some();
        let close = pcon.and_then(|p| close_bounded(move || drop(p), CLOSE_BOUND));
        self.input.lock().unwrap_or_else(|e| e.into_inner()).take();
        let drain_end = self
            .drain
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .and_then(|d| d.finish(Duration::from_secs(2)));
        record_teardown(serde_json::json!({
            "path": "master_drop", "close_pseudoconsole_ms": close,
            "close_hung": had_pcon && close.is_none(),
            "drain_reached_eof": drain_end.as_deref() == Some("eof"), "drain_end": drain_end,
            "teardown_ms": t.elapsed().as_millis() as u64,
        }));
    }
}

/// `Child` over the raw launch's retained process handle. Unlike
/// portable-pty's Windows killer, `kill` returns `Ok` only when
/// `TerminateProcess` succeeds.
#[derive(Debug, Clone)]
struct RawChild {
    process: Arc<Handle>,
    pid: u32,
}

impl portable_pty::ChildKiller for RawChild {
    fn kill(&mut self) -> io::Result<()> {
        // SAFETY: owned handle.
        if unsafe { TerminateProcess(self.process.raw(), 1) } != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
        Box::new(self.clone())
    }
}

impl portable_pty::Child for RawChild {
    fn try_wait(&mut self) -> io::Result<Option<portable_pty::ExitStatus>> {
        let mut code = 0u32;
        // SAFETY: owned handle.
        if unsafe { GetExitCodeProcess(self.process.raw(), &mut code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((code != STILL_ACTIVE as u32).then(|| portable_pty::ExitStatus::with_exit_code(code)))
    }
    fn wait(&mut self) -> io::Result<portable_pty::ExitStatus> {
        // SAFETY: owned handle.
        unsafe { WaitForSingleObject(self.process.raw(), INFINITE) };
        loop {
            if let Some(s) = self.try_wait()? {
                return Ok(s);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn process_id(&self) -> Option<u32> {
        Some(self.pid)
    }
    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        Some(self.process.raw() as std::os::windows::io::RawHandle)
    }
}

fn creation_time(h: HANDLE) -> u64 {
    // SAFETY: plain query on a valid handle.
    unsafe {
        let mut c: FILETIME = std::mem::zeroed();
        let mut e: FILETIME = std::mem::zeroed();
        let mut k: FILETIME = std::mem::zeroed();
        let mut u: FILETIME = std::mem::zeroed();
        if GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) != 0 {
            filetime_u64(&c)
        } else {
            0
        }
    }
}

/// Raw ConPTY launch (§4.6): own `CreatePseudoConsole` (`flags`) and
/// `CreateProcessW` with one attribute list holding
/// `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` and `PROC_THREAD_ATTRIBUTE_JOB_LIST`,
/// so the child is a job member from creation; the `CREATE_SUSPENDED` +
/// assign + `ResumeThread == 1` branch runs only if JOB_LIST fails. The
/// output drain starts right after `CreatePseudoConsole`. `inject` fails the
/// launch at the named step (F-2c).
#[allow(clippy::too_many_arguments)]
pub fn raw_spawn(
    argv: &[OsString],
    env: &[(String, String)],
    remove: &[String],
    cwd: &Path,
    cols: u16,
    rows: u16,
    flags: u32,
    inject: Option<super::RawInject>,
) -> Result<super::RawSpawn, String> {
    use super::RawInject as I;
    if let Ok(mut l) = RAW_LAUNCH.lock() {
        *l = None;
    }
    let mut st = Setup::default();
    st.step = "pipes";
    // SAFETY: Microsoft's "Creating a Pseudoconsole session"; every handle is
    // owned by `st` from the moment it exists, and `st`'s drop releases them.
    unsafe {
        let (mut in_r, mut in_w, mut out_r, mut out_w): (HANDLE, HANDLE, HANDLE, HANDLE) = (
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        if CreatePipe(&mut in_r, &mut in_w, std::ptr::null(), 0) == 0 {
            return Err(format!("CreatePipe: {}", io::Error::last_os_error()));
        }
        st.in_r = Handle::new(in_r);
        st.input = Some(std::fs::File::from_raw_handle(in_w));
        if CreatePipe(&mut out_r, &mut out_w, std::ptr::null(), 0) == 0 {
            return Err(format!("CreatePipe: {}", io::Error::last_os_error()));
        }
        st.out_w = Handle::new(out_w);
        let output = std::fs::File::from_raw_handle(out_r);
        st.step = "pseudoconsole";
        let mut hpc: HPCON = 0;
        let hr = CreatePseudoConsole(
            COORD {
                X: cols as i16,
                Y: rows as i16,
            },
            in_r,
            out_w,
            flags,
            &mut hpc,
        );
        if hr < 0 {
            drop(output);
            return Err(format!("CreatePseudoConsole hr={hr:#x}"));
        }
        st.pcon = Some(Pcon(hpc));
        // The pseudoconsole holds its own copies of its pipe ends.
        st.in_r.take();
        st.out_w.take();
        st.step = "drain";
        st.drain = Some(
            Drain::start(output)
                .map_err(|e| format!("drain thread: {e} (host read end closed)"))?,
        );
        st.step = "job";
        let job = new_kill_on_close_job().map_err(|e| format!("job: {e}"))?;
        let job_raw = job.raw();
        st.job = Some(job);
        st.step = "before_spawn";
        if inject == Some(I::BeforeSpawn) {
            return Err("injected failure before spawn".into());
        }
        let mut cmdline = Vec::new();
        for (i, a) in argv.iter().enumerate() {
            if i > 0 {
                cmdline.push(b' ' as u16);
            }
            quote_arg(a, &mut cmdline);
        }
        cmdline.push(0);
        let envb = env_block(env, remove);
        let cwdw = wide(cwd.as_os_str());
        // Each attempt records its stage and Win32 result; only a failed
        // `CreateProcessW` with the job list is a JOB_LIST rejection.
        let try_create = |job_list: bool,
                          cmdline: &mut Vec<u16>|
         -> Result<PROCESS_INFORMATION, (&'static str, String)> {
            // Declared before the attribute list so it outlives the list.
            let jobs = [job_raw];
            let mut attrs = AttrList::new(if job_list { 2 } else { 1 })
                .map_err(|e| ("attr_list", e.to_string()))?;
            attrs
                .set(
                    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                    hpc as *const c_void,
                    std::mem::size_of::<HPCON>(),
                )
                .map_err(|e| ("attr_pseudoconsole", e.to_string()))?;
            if job_list {
                attrs
                    .set(
                        PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                        jobs.as_ptr() as *const c_void,
                        std::mem::size_of::<HANDLE>(),
                    )
                    .map_err(|e| ("attr_job_list", e.to_string()))?;
            }
            let mut si: STARTUPINFOEXW = std::mem::zeroed();
            si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
            si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
            si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
            si.lpAttributeList = attrs.ptr();
            let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
            let cflags = EXTENDED_STARTUPINFO_PRESENT
                | CREATE_UNICODE_ENVIRONMENT
                | if job_list { 0 } else { CREATE_SUSPENDED };
            if CreateProcessW(
                std::ptr::null(),
                cmdline.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                cflags,
                envb.as_ptr() as *const c_void,
                cwdw.as_ptr(),
                &si.StartupInfo,
                &mut pi,
            ) == 0
            {
                return Err(("create_process", io::Error::last_os_error().to_string()));
            }
            Ok(pi)
        };
        let attempt = |branch: &str, r: &Result<PROCESS_INFORMATION, (&str, String)>| match r {
            Ok(_) => serde_json::json!({"branch": branch, "stage": "created"}),
            Err((stage, e)) => serde_json::json!({"branch": branch, "stage": stage, "error": e}),
        };
        st.step = "create_process";
        let force_suspended = matches!(inject, Some(I::ForceSuspended | I::ResumeFail));
        let first = if force_suspended {
            Err(("injected", "injected JOB_LIST failure".to_string()))
        } else {
            try_create(true, &mut cmdline.clone())
        };
        let mut attempts = vec![attempt("job_list", &first)];
        let job_list_rejected = matches!(&first, Err(("create_process", _)));
        let record = |attempts: &[serde_json::Value]| {
            if let Ok(mut l) = RAW_LAUNCH.lock() {
                *l = Some(serde_json::json!({"attempts": attempts,
                                             "job_list_rejected": job_list_rejected}));
            }
        };
        record(&attempts);
        let (branch, job_list_error) = match first {
            Ok(pi) => {
                st.process = Handle::new(pi.hProcess);
                st.thread = Handle::new(pi.hThread);
                st.pid = pi.dwProcessId;
                st.start = st.process.as_ref().map_or(0, |p| creation_time(p.raw()));
                ("job_list".to_string(), None)
            }
            Err((stage, e)) => {
                let job_list_err = format!("{stage}: {e}");
                let second = try_create(false, &mut cmdline);
                attempts.push(attempt("suspended", &second));
                record(&attempts);
                let pi = second.map_err(|(stage, e)| {
                    format!("job_list: {job_list_err}; suspended: {stage}: {e}")
                })?;
                st.process = Handle::new(pi.hProcess);
                st.thread = Handle::new(pi.hThread);
                st.pid = pi.dwProcessId;
                // Identity recorded at acquisition, before any later step can fail.
                st.start = st.process.as_ref().map_or(0, |p| creation_time(p.raw()));
                st.step = "suspended_assign";
                let (Some(p), Some(t)) = (st.process.as_ref(), st.thread.as_ref()) else {
                    return Err("CreateProcessW returned no handles".into());
                };
                if AssignProcessToJobObject(job_raw, p.raw()) == 0 {
                    return Err(format!(
                        "job_list failed ({job_list_err}); suspended assign failed: {}",
                        io::Error::last_os_error()
                    ));
                }
                st.step = "resume";
                // The thread was created suspended once: anything but 1 means
                // it did not start (or started early), so this branch is void.
                if inject == Some(I::ResumeFail) {
                    return Err("injected ResumeThread failure".into());
                }
                let prev = ResumeThread(t.raw());
                if prev != 1 {
                    return Err(format!(
                        "job_list failed ({job_list_err}); ResumeThread returned {prev} ({})",
                        io::Error::last_os_error()
                    ));
                }
                ("suspended".to_string(), Some(job_list_err))
            }
        };
        st.step = "after_spawn";
        if inject == Some(I::AfterSpawn) {
            // Fail with the child's output pending: wait (bounded) until its
            // output has been seen while it still runs (the fixture writes
            // continuously), and record both.
            let end = Instant::now() + Duration::from_secs(5);
            loop {
                let seen = st.drain.as_ref().map_or(0, |d| d.bytes());
                let running = st.process.as_ref().is_some_and(|p| still_active(p.raw()));
                if (seen > 0 && running) || Instant::now() >= end {
                    st.output_pending = Some(seen);
                    st.running_at_injection = Some(running);
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            return Err("injected failure after spawn".into());
        }
        st.thread.take();
        let process = Arc::new(st.process.take().ok_or("no process handle")?);
        let child = RawChild {
            process,
            pid: st.pid,
        };
        let master = RawMaster {
            pcon: Mutex::new(st.pcon.take()),
            input: Mutex::new(st.input.take()),
            drain: Mutex::new(st.drain.take()),
            size: Mutex::new(portable_pty::PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            }),
            fail_reader: AtomicBool::new(inject == Some(I::ReaderStart)),
        };
        let tree = Tree {
            job: st.job.take().ok_or("no job")?,
            assign: if branch == "job_list" {
                "job_list"
            } else {
                "suspended"
            },
        };
        Ok(super::RawSpawn {
            master: Box::new(master),
            child: Box::new(child),
            tree,
            branch,
            job_list_error,
            job_list_rejected,
            attempts,
            conpty_module: conpty_module(),
        })
    }
}

/// C-3 job-first launch: a thin wrapper over `raw_spawn` (flags 0) that
/// keeps the pseudoconsole open until `close_conpty`.
pub struct JobFirst {
    spawn: Option<super::RawSpawn>,
    pid: u32,
    start: u64,
    terminated: bool,
}

impl JobFirst {
    pub fn launch(
        argv: &[OsString],
        env: &[(String, String)],
        remove: &[String],
        cwd: &Path,
    ) -> Result<(JobFirst, String), String> {
        let rs = raw_spawn(argv, env, remove, cwd, 120, 40, 0, None)?;
        let pid = rs.child.process_id().unwrap_or(0);
        let start = rs
            .child
            .as_raw_handle()
            .map_or(0, |h| creation_time(h as HANDLE));
        let branch = match &rs.job_list_error {
            None => rs.branch.clone(),
            Some(e) => format!("{} (job_list failed: {e})", rs.branch),
        };
        Ok((
            JobFirst {
                spawn: Some(rs),
                pid,
                start,
                terminated: false,
            },
            branch,
        ))
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Job membership; an enumeration failure is an error, never an empty job.
    pub fn members(&self) -> Result<Vec<u32>, String> {
        match &self.spawn {
            Some(rs) => rs.tree.members_checked(),
            None => Err("closed".into()),
        }
    }

    pub fn start_time(&self) -> u64 {
        self.start
    }

    /// Kills through the harness's own authority only: the job and the
    /// retained process handle. ConPTY stays open (closing it can itself kill
    /// console clients and hide survivors); call `close_conpty` after the
    /// caller has verified and run its fallback.
    pub fn terminate_job(&mut self) -> Vec<String> {
        if self.terminated {
            return vec!["already terminated".into()];
        }
        self.terminated = true;
        let Some(rs) = self.spawn.as_mut() else {
            return vec!["closed".into()];
        };
        let (mut steps, _) = rs.tree.kill(Duration::ZERO);
        steps.push(format!(
            "TerminateProcess(retained)={}",
            rs.child.kill().is_ok()
        ));
        steps
    }

    pub fn close_conpty(&mut self) {
        // Dropping the master closes the pseudoconsole while the drain reads.
        self.spawn.take();
    }
}

impl Drop for JobFirst {
    fn drop(&mut self) {
        self.terminate_job();
        self.close_conpty();
    }
}
