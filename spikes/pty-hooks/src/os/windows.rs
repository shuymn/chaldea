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
use std::sync::Mutex;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, FILETIME, HANDLE, INVALID_HANDLE_VALUE, STILL_ACTIVE,
};
use windows_sys::Win32::System::Console::{
    COORD, CTRL_BREAK_EVENT, ClosePseudoConsole, CreatePseudoConsole, ENABLE_ECHO_INPUT,
    ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING, GenerateConsoleCtrlEvent, GetConsoleCP, GetConsoleMode,
    GetConsoleWindow, GetStdHandle, HPCON, ReadConsoleW, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    SetConsoleMode,
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
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DETACHED_PROCESS, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetCurrentThreadId, GetExitCodeProcess,
    GetProcessTimes, InitializeProcThreadAttributeList, OpenProcess,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, ResumeThread, STARTF_USESTDHANDLES,
    STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
};
use windows_sys::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EVENT_OBJECT_SHOW, GetMessageW, GetWindowThreadProcessId, MSG, OBJID_WINDOW,
    PM_NOREMOVE, PeekMessageW, PostThreadMessageW, TranslateMessage, WINEVENT_OUTOFCONTEXT,
    WM_QUIT,
};

use super::ProcInfo;

pub const SPAWN_TREE_ROLES: &[&str] = &["plain", "breakaway"];

pub fn default_root() -> PathBuf {
    PathBuf::from(r"C:\chs")
}

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Owned kernel handle, closed on drop. Stored as usize so it is Send + Sync.
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

/// (creation time, still running) for `pid`, if it can be opened.
fn process_times(pid: u32) -> Option<(u64, bool)> {
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
        let running = GetExitCodeProcess(h.raw(), &mut code) != 0 && code == STILL_ACTIVE as u32;
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
            let (start_time, running) = process_times(e.th32ProcessID).unwrap_or((0, true));
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
                break;
            }
        }
        Ok(out)
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

fn job_members(job: HANDLE) -> Vec<u32> {
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
            return Vec::new();
        }
        let list = &*(buf.as_ptr() as *const JOBOBJECT_BASIC_PROCESS_ID_LIST);
        let n = (list.NumberOfProcessIdsInList as usize).min(N);
        let ids = (buf.as_ptr() as *const u8).add(off) as *const usize;
        (0..n).map(|i| *ids.add(i) as u32).collect()
    }
}

/// Harness-established ownership: a `KILL_ON_JOB_CLOSE` job assigned after spawn.
pub struct Tree {
    job: Handle,
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
        Ok(Tree { job })
    }

    pub fn describe(&self) -> serde_json::Value {
        serde_json::json!({"mechanism": "job_object", "kill_on_job_close": true, "assign": "after_spawn"})
    }

    pub fn kill(&self, _grace: Duration) -> Vec<String> {
        // SAFETY: valid job handle.
        let r = unsafe { TerminateJobObject(self.job.raw(), 1) };
        vec![format!("TerminateJobObject={r}")]
    }

    pub fn members(&self) -> Vec<u32> {
        job_members(self.job.raw())
    }
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

pub fn file_mode(_path: &Path) -> Option<u32> {
    None
}

pub fn set_file_mode(_path: &Path, _mode: Option<u32>) -> io::Result<()> {
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

/// `(pid, ppid at event time)` per shown window, recorded inside the callback
/// so a short-lived owner is still attributable after it exits.
static SHOWN: Mutex<Vec<(u32, Option<u32>)>> = Mutex::new(Vec::new());

unsafe extern "system" fn on_show(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: windows_sys::Win32::Foundation::HWND,
    id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if id_object != OBJID_WINDOW || hwnd.is_null() {
        return;
    }
    // No IsWindowVisible filter: a flash may already be hidden by now.
    let mut pid = 0u32;
    // SAFETY: hwnd comes from the system for this callback.
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
    }
    let ppid = proc_info(pid).map(|p| p.ppid);
    if let Ok(mut v) = SHOWN.lock() {
        v.push((pid, ppid));
    }
}

/// Out-of-context `EVENT_OBJECT_SHOW` observer on its own message-loop thread.
pub struct WindowObserver {
    thread_id: u32,
    join: std::thread::JoinHandle<()>,
}

impl WindowObserver {
    pub fn start() -> Result<WindowObserver, String> {
        if let Ok(mut v) = SHOWN.lock() {
            v.clear();
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let join = std::thread::spawn(move || {
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
                let _ = tx.send((GetCurrentThreadId(), !hook.is_null()));
                if hook.is_null() {
                    return;
                }
                while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                UnhookWinEvent(hook);
            }
        });
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok((tid, true)) => Ok(WindowObserver {
                thread_id: tid,
                join,
            }),
            Ok((_, false)) => Err("SetWinEventHook failed".into()),
            Err(_) => Err("observer thread did not start".into()),
        }
    }

    /// Stops the loop (bounded: a thread that does not quit is abandoned).
    pub fn stop(self) -> Vec<(u32, Option<u32>)> {
        // SAFETY: posting WM_QUIT to the observer thread.
        let posted = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) } != 0;
        if posted {
            let end = std::time::Instant::now() + Duration::from_secs(2);
            while !self.join.is_finished() && std::time::Instant::now() < end {
                std::thread::sleep(Duration::from_millis(10));
            }
            if self.join.is_finished() {
                let _ = self.join.join();
            }
        }
        SHOWN.lock().map(|v| v.clone()).unwrap_or_default()
    }
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

/// C-3 job-first launch: raw ConPTY + `CreateProcessW` with
/// `PROC_THREAD_ATTRIBUTE_JOB_LIST`; the `CREATE_SUSPENDED` + assign +
/// `ResumeThread` branch runs only if JOB_LIST fails.
pub struct JobFirst {
    hpc: HPCON,
    job: Handle,
    process: Handle,
    pid: u32,
    start: u64,
    _input: std::fs::File,
    drain: Option<std::thread::JoinHandle<()>>,
    terminated: bool,
    closed: bool,
}

impl JobFirst {
    pub fn launch(
        argv: &[OsString],
        env: &[(String, String)],
        remove: &[String],
        cwd: &Path,
    ) -> Result<(JobFirst, String), String> {
        // SAFETY: standard ConPTY setup per Microsoft's "Creating a Pseudoconsole session";
        // every handle is owned by a Handle/File or closed on the error path.
        unsafe {
            let mut in_r: HANDLE = std::ptr::null_mut();
            let mut in_w: HANDLE = std::ptr::null_mut();
            let mut out_r: HANDLE = std::ptr::null_mut();
            let mut out_w: HANDLE = std::ptr::null_mut();
            if windows_sys::Win32::System::Pipes::CreatePipe(
                &mut in_r,
                &mut in_w,
                std::ptr::null(),
                0,
            ) == 0
            {
                return Err(format!("CreatePipe: {}", io::Error::last_os_error()));
            }
            let in_r = Handle(in_r as usize);
            let input = std::fs::File::from_raw_handle(in_w);
            if windows_sys::Win32::System::Pipes::CreatePipe(
                &mut out_r,
                &mut out_w,
                std::ptr::null(),
                0,
            ) == 0
            {
                return Err(format!("CreatePipe: {}", io::Error::last_os_error()));
            }
            let out_w = Handle(out_w as usize);
            let mut output = std::fs::File::from_raw_handle(out_r);
            let mut hpc: HPCON = 0;
            let hr = CreatePseudoConsole(
                COORD { X: 120, Y: 40 },
                in_r.raw(),
                out_w.raw(),
                0,
                &mut hpc,
            );
            if hr < 0 {
                return Err(format!("CreatePseudoConsole hr={hr:#x}"));
            }
            drop(in_r);
            drop(out_w);
            let drain = std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while matches!(output.read(&mut buf), Ok(n) if n > 0) {}
            });
            let job = match new_kill_on_close_job() {
                Ok(j) => j,
                Err(e) => {
                    ClosePseudoConsole(hpc);
                    return Err(format!("job: {e}"));
                }
            };
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

            let try_create =
                |job_list: bool, cmdline: &mut Vec<u16>| -> Result<PROCESS_INFORMATION, String> {
                    // Declared before the attribute list so it outlives the list.
                    let jobs = [job.raw()];
                    let mut attrs =
                        AttrList::new(if job_list { 2 } else { 1 }).map_err(|e| e.to_string())?;
                    attrs
                        .set(
                            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                            hpc as *const c_void,
                            std::mem::size_of::<HPCON>(),
                        )
                        .map_err(|e| format!("attr pseudoconsole: {e}"))?;
                    if job_list {
                        attrs
                            .set(
                                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                                jobs.as_ptr() as *const c_void,
                                std::mem::size_of::<HANDLE>(),
                            )
                            .map_err(|e| format!("attr job_list: {e}"))?;
                    }
                    let mut si: STARTUPINFOEXW = std::mem::zeroed();
                    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
                    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
                    si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
                    si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
                    si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
                    si.lpAttributeList = attrs.ptr();
                    let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
                    let flags = EXTENDED_STARTUPINFO_PRESENT
                        | CREATE_UNICODE_ENVIRONMENT
                        | if job_list { 0 } else { CREATE_SUSPENDED };
                    if CreateProcessW(
                        std::ptr::null(),
                        cmdline.as_mut_ptr(),
                        std::ptr::null(),
                        std::ptr::null(),
                        0,
                        flags,
                        envb.as_ptr() as *const c_void,
                        cwdw.as_ptr(),
                        &si.StartupInfo,
                        &mut pi,
                    ) == 0
                    {
                        return Err(format!("CreateProcessW: {}", io::Error::last_os_error()));
                    }
                    Ok(pi)
                };

            let (pi, branch) = match try_create(true, &mut cmdline.clone()) {
                Ok(pi) => (pi, "job_list".to_string()),
                Err(job_list_err) => match try_create(false, &mut cmdline) {
                    Ok(pi) => {
                        if AssignProcessToJobObject(job.raw(), pi.hProcess) == 0 {
                            let e = io::Error::last_os_error();
                            TerminateProcess(pi.hProcess, 1);
                            CloseHandle(pi.hThread);
                            CloseHandle(pi.hProcess);
                            ClosePseudoConsole(hpc);
                            return Err(format!(
                                "job_list failed ({job_list_err}); suspended assign failed: {e}"
                            ));
                        }
                        // The thread was created suspended once: anything but 1 means
                        // it did not start (or started early), so this branch is void.
                        let prev = ResumeThread(pi.hThread);
                        if prev != 1 {
                            let e = io::Error::last_os_error();
                            TerminateJobObject(job.raw(), 1);
                            TerminateProcess(pi.hProcess, 1);
                            CloseHandle(pi.hThread);
                            CloseHandle(pi.hProcess);
                            ClosePseudoConsole(hpc);
                            return Err(format!(
                                "job_list failed ({job_list_err}); ResumeThread returned {prev} ({e})"
                            ));
                        }
                        (pi, format!("suspended (job_list failed: {job_list_err})"))
                    }
                    Err(e) => {
                        ClosePseudoConsole(hpc);
                        return Err(format!("job_list: {job_list_err}; suspended: {e}"));
                    }
                },
            };
            CloseHandle(pi.hThread);
            let process = Handle(pi.hProcess as usize);
            let mut c: FILETIME = std::mem::zeroed();
            let mut e: FILETIME = std::mem::zeroed();
            let mut k: FILETIME = std::mem::zeroed();
            let mut u: FILETIME = std::mem::zeroed();
            let start = if GetProcessTimes(process.raw(), &mut c, &mut e, &mut k, &mut u) != 0 {
                filetime_u64(&c)
            } else {
                0
            };
            Ok((
                JobFirst {
                    hpc,
                    job,
                    process,
                    pid: pi.dwProcessId,
                    start,
                    _input: input,
                    drain: Some(drain),
                    terminated: false,
                    closed: false,
                },
                branch,
            ))
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn members(&self) -> Vec<u32> {
        job_members(self.job.raw())
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
        // SAFETY: owned handles.
        unsafe {
            vec![
                format!(
                    "TerminateJobObject={}",
                    TerminateJobObject(self.job.raw(), 1) != 0
                ),
                format!(
                    "TerminateProcess(retained)={}",
                    TerminateProcess(self.process.raw(), 1) != 0
                ),
            ]
        }
    }

    pub fn close_conpty(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        // SAFETY: hpc is owned and closed once.
        unsafe {
            ClosePseudoConsole(self.hpc);
        }
        if let Some(d) = self.drain.take() {
            let end = std::time::Instant::now() + Duration::from_secs(2);
            while !d.is_finished() && std::time::Instant::now() < end {
                std::thread::sleep(Duration::from_millis(20));
            }
            if d.is_finished() {
                let _ = d.join();
            }
        }
    }
}

impl Drop for JobFirst {
    fn drop(&mut self) {
        self.terminate_job();
        self.close_conpty();
    }
}
