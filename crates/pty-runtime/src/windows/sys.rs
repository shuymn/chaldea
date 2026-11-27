//! The only Windows module allowed `unsafe` (`docs/design/pty-windows.md` §2.7). Each
//! function is safe to call: handles are borrowed `OwnedHandle`s or std pipe ends, every
//! returned handle is wrapped exactly once and only on success, and each `unsafe` block
//! holds one operation. It also holds the process-level calls of `host`
//! (`docs/design/daemon-lifecycle.md` §2.2): security descriptors, known folders, and events.

use std::cmp::Ordering;
use std::ffi::c_void;
use std::io;
use std::marker::PhantomData;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    FILETIME, HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Globalization::{
    CSTR_EQUAL, CSTR_GREATER_THAN, CSTR_LESS_THAN, CompareStringOrdinal,
};
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_PROCESS_ID_LIST,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
    JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, QueryInformationJobObject,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, GetProcessTimes,
    InitializeProcThreadAttributeList, OpenProcess, OpenThread, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
    ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, THREAD_SUSPEND_RESUME, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

use crate::backend::PtySize;
use crate::spawn::SpawnGuard;

/// `ERROR_MORE_DATA`: the pid list did not fit.
const ERROR_MORE_DATA: i32 = 234;
/// `FACILITY_WIN32` of an HRESULT.
const FACILITY_WIN32: u32 = 7;
/// The job access right `QueryInformationJobObject` needs (`winnt.h`); a duplicate with only
/// this right cannot terminate the job.
#[cfg(any(test, feature = "fake"))]
pub(crate) const JOB_OBJECT_QUERY: u32 = 0x4;

fn raw(h: &impl AsRawHandle) -> HANDLE {
    h.as_raw_handle()
}

/// An `io::Error` from an HRESULT: the Win32 code when its facility is `FACILITY_WIN32`.
fn hresult_error(hr: i32) -> io::Error {
    let bits = hr.cast_unsigned();
    if (bits >> 16) & 0x1fff == FACILITY_WIN32 {
        io::Error::from_raw_os_error((bits & 0xffff).cast_signed())
    } else {
        io::Error::other(format!("HRESULT {bits:#010x}"))
    }
}

fn coord(size: PtySize) -> COORD {
    // `PtySize` keeps both dimensions at or below 4095, so they fit in `i16`.
    COORD {
        X: i16::try_from(size.cols()).unwrap_or(i16::MAX),
        Y: i16::try_from(size.rows()).unwrap_or(i16::MAX),
    }
}

fn len_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Wraps a handle a call returned, or the last error when it is null or invalid.
fn owned(h: HANDLE) -> io::Result<OwnedHandle> {
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `h` is a handle the caller's API call just returned to this process and
    // nothing else owns; it is wrapped exactly once.
    Ok(unsafe { OwnedHandle::from_raw_handle(h) })
}

// ---- Pseudoconsole ----

/// A pseudoconsole. Not a kernel handle: closed only by [`Pcon::close`]; dropping it does
/// nothing (the close worker owns its close, `docs/design/pty-windows.md` §2.3).
#[derive(Debug)]
pub(crate) struct Pcon(HPCON);

/// `CreatePseudoConsole(size, input, output, 0)`. The guard proves the spawn lock is held:
/// the call holds inheritable duplicates of both pipe ends while it starts conhost.
pub(crate) fn create_pseudo_console(
    _held: &SpawnGuard,
    size: PtySize,
    input: &std::io::PipeReader,
    output: &std::io::PipeWriter,
) -> io::Result<Pcon> {
    let mut hpc: HPCON = 0;
    #[cfg(any(test, feature = "fake"))]
    super::hooks::native_boundary("create_pseudo_console");
    // SAFETY: both handles are borrowed and valid for the call; `hpc` is a local the call
    // writes on success.
    let hr = unsafe { CreatePseudoConsole(coord(size), raw(input), raw(output), 0, &raw mut hpc) };
    if hr < 0 {
        return Err(hresult_error(hr));
    }
    Ok(Pcon(hpc))
}

impl Pcon {
    /// `ClosePseudoConsole`. Consuming: the pseudoconsole is closed once, and no `resize`
    /// borrow can exist.
    pub(crate) fn close(self) {
        // SAFETY: `self.0` came from a successful `CreatePseudoConsole` and is closed only
        // here, once, since `close` consumes the only owner.
        unsafe { ClosePseudoConsole(self.0) }
    }

    pub(crate) fn resize(&self, size: PtySize) -> io::Result<()> {
        // SAFETY: `self.0` is a live pseudoconsole: `close` consumes the `Pcon`.
        let hr = unsafe { ResizePseudoConsole(self.0, coord(size)) };
        if hr < 0 {
            return Err(hresult_error(hr));
        }
        Ok(())
    }

    /// The value `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` takes.
    fn value(&self) -> HPCON {
        self.0
    }
}

// ---- Job ----

/// `CreateJobObjectW(NULL, NULL)`: unnamed and not inheritable.
pub(crate) fn create_job() -> io::Result<OwnedHandle> {
    // SAFETY: null attributes and name are allowed; the result is checked before wrapping.
    let h = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    owned(h)
}

/// `SetInformationJobObject(JobObjectExtendedLimitInformation)` with `limit` as the only
/// limit flag (the backend passes `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`).
pub(crate) fn set_job_limit(job: &OwnedHandle, limit: u32) -> io::Result<()> {
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = limit;
    // SAFETY: the job handle is borrowed and valid; `info` is a zero-initialised struct of
    // integers whose exact size is passed.
    let ok = unsafe {
        SetInformationJobObject(
            raw(job),
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            len_u32(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The limit the backend sets on every session job.
pub(crate) const KILL_ON_JOB_CLOSE: u32 = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

/// Assigns `process` (owned here or by another owner, such as a `std::process::Child`) to `job`.
pub(crate) fn assign(job: &OwnedHandle, process: &impl AsRawHandle) -> io::Result<()> {
    // SAFETY: both handles are borrowed and valid.
    if unsafe { AssignProcessToJobObject(raw(job), raw(process)) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn terminate_job(job: &OwnedHandle, code: u32) -> io::Result<()> {
    // SAFETY: the job handle is borrowed and valid.
    if unsafe { TerminateJobObject(raw(job), code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn active_processes(job: &OwnedHandle) -> io::Result<u32> {
    let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    // SAFETY: the job handle is borrowed and valid; `info` is a zeroed struct of integers
    // whose size is passed.
    let ok = unsafe {
        QueryInformationJobObject(
            raw(job),
            JobObjectBasicAccountingInformation,
            (&raw mut info).cast(),
            len_u32(size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>()),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.ActiveProcesses)
}

/// One `QueryInformationJobObject(JobObjectBasicProcessIdList)` with room for `capacity`
/// pids; `TooSmall` on `ERROR_MORE_DATA`. The caller drives the retries.
pub(crate) fn job_pids(job: &OwnedHandle, capacity: usize) -> io::Result<super::kill::PidList> {
    let header = std::mem::offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList);
    let entries = size_of::<usize>();
    let bytes = header + capacity * entries;
    let mut buf = vec![0usize; bytes.div_ceil(entries)];
    // SAFETY: the job handle is borrowed and valid; `buf` is pointer-aligned and at least
    // `bytes` long, and `bytes` is what is passed.
    let ok = unsafe {
        QueryInformationJobObject(
            raw(job),
            JobObjectBasicProcessIdList,
            buf.as_mut_ptr().cast(),
            len_u32(bytes),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(ERROR_MORE_DATA) {
            return Ok(super::kill::PidList::TooSmall);
        }
        return Err(e);
    }
    // Read through bytes: the header is two `u32` counts, then `usize` entries.
    let bytes_of: Vec<u8> = buf.iter().flat_map(|w| w.to_ne_bytes()).collect();
    let u32_at = |off: usize| {
        bytes_of
            .get(off..off + 4)
            .and_then(|b| b.try_into().ok())
            .map_or(0, u32::from_ne_bytes)
    };
    let listed = usize::try_from(u32_at(4)).unwrap_or(0).min(capacity);
    let pids = (0..listed)
        .filter_map(|i| {
            let off = header + i * entries;
            let b = bytes_of.get(off..off + entries)?;
            let mut w = [0u8; size_of::<usize>()];
            w.copy_from_slice(b);
            u32::try_from(usize::from_ne_bytes(w)).ok()
        })
        .collect();
    Ok(super::kill::PidList::Complete(pids))
}

pub(crate) fn in_job(process: &OwnedHandle, job: &OwnedHandle) -> io::Result<bool> {
    let mut result = 0;
    // SAFETY: both handles are borrowed and valid; `result` is a local.
    if unsafe { IsProcessInJob(raw(process), raw(job), &raw mut result) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(result != 0)
}

// ---- Attribute list and process creation ----

/// A `PROC_THREAD_ATTRIBUTE_LIST`. The lifetime ties it to the values it points to (the
/// pseudoconsole and the job array), so they outlive every use in `CreateProcessW`.
pub(crate) struct AttrList<'a> {
    buf: Vec<usize>,
    _values: PhantomData<&'a ()>,
}

impl<'a> AttrList<'a> {
    pub(crate) fn new(count: u32) -> io::Result<AttrList<'a>> {
        let mut size = 0usize;
        // SAFETY: the size query: a null list with a valid size out-pointer; it fails with
        // `ERROR_INSUFFICIENT_BUFFER` by design and only writes `size`.
        let _ = unsafe {
            InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &raw mut size)
        };
        if size == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0usize; size.div_ceil(size_of::<usize>())];
        // SAFETY: `buf` is pointer-aligned and at least `size` bytes long.
        let ok = unsafe {
            InitializeProcThreadAttributeList(buf.as_mut_ptr().cast(), count, 0, &raw mut size)
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        super::tr("attr_init", true, 0, Ok(u64::from(count)));
        Ok(AttrList {
            buf,
            _values: PhantomData,
        })
    }

    fn ptr(&mut self) -> *mut c_void {
        self.buf.as_mut_ptr().cast()
    }

    fn update(&mut self, attr: u32, value: *const c_void, size: usize) -> io::Result<()> {
        let list = self.ptr();
        // SAFETY: the list was initialised in `new`; `value` points to (or is) data that
        // lives for `'a`, as each caller's signature requires.
        let ok = unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                attr as usize,
                value,
                size,
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(crate) fn set_pseudoconsole(&mut self, pcon: &'a Pcon) -> io::Result<()> {
        // The attribute's value is the HPCON itself.
        self.update(
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
            std::ptr::without_provenance(pcon.value().cast_unsigned()),
            size_of::<HPCON>(),
        )
    }

    pub(crate) fn set_job_list(&mut self, jobs: &'a [HANDLE; 1]) -> io::Result<()> {
        self.update(
            PROC_THREAD_ATTRIBUTE_JOB_LIST,
            jobs.as_ptr().cast(),
            size_of::<HANDLE>(),
        )
    }

    /// The only handles the child inherits (each must be inheritable, none repeated).
    fn set_handle_list(&mut self, handles: &'a [HANDLE]) -> io::Result<()> {
        self.update(
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            handles.as_ptr().cast(),
            size_of_val(handles),
        )
    }
}

impl Drop for AttrList<'_> {
    fn drop(&mut self) {
        let list = self.ptr();
        // SAFETY: the list was initialised in `new` (an `AttrList` exists only then) and is
        // deleted once, here.
        unsafe { DeleteProcThreadAttributeList(list) }
        super::tr("attr_delete", true, 0, Ok(0));
    }
}

/// The raw job handle for [`AttrList::set_job_list`].
pub(crate) fn job_array(job: &OwnedHandle) -> [HANDLE; 1] {
    [raw(job)]
}

/// What `CreateProcessW` returned.
pub(crate) struct Created {
    pub(crate) process: OwnedHandle,
    pub(crate) thread: OwnedHandle,
    pub(crate) pid: u32,
}

/// The creation flags of one attempt.
pub(crate) fn creation_flags(suspended: bool) -> u32 {
    EXTENDED_STARTUPINFO_PRESENT
        | CREATE_UNICODE_ENVIRONMENT
        | if suspended { CREATE_SUSPENDED } else { 0 }
}

/// `CREATE_SUSPENDED`, for trace checks.
#[cfg(test)]
pub(crate) const SUSPENDED: u32 = CREATE_SUSPENDED;

/// `CreateProcessW(app, cmdline, NULL, NULL, FALSE, flags, env, cwd, &si, &pi)` with the
/// pseudoconsole's standard handles. `cmdline` is this attempt's fresh mutable copy
/// (`CreateProcessW` may write to it); `app`, `env`, and `cwd` are NUL-terminated (the
/// environment block double-NUL), as `cmdline::Buffers` guarantees. `created` sees the
/// new process's handle at the success boundary, before this returns (test and `fake`
/// builds record its job membership there).
#[allow(
    clippy::too_many_arguments,
    reason = "one argument per CreateProcessW input"
)]
pub(crate) fn create_process(
    _held: &SpawnGuard,
    app: &[u16],
    cmdline: &mut [u16],
    env: &[u16],
    cwd: &[u16],
    attrs: &mut AttrList<'_>,
    suspended: bool,
    created: &dyn Fn(&OwnedHandle),
) -> io::Result<Created> {
    debug_assert!(app.ends_with(&[0]) && cmdline.ends_with(&[0]) && cwd.ends_with(&[0]));
    debug_assert!(env.ends_with(&[0, 0]));
    let mut si = STARTUPINFOEXW::default();
    si.StartupInfo.cb = len_u32(size_of::<STARTUPINFOEXW>());
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
    si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
    si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
    si.lpAttributeList = attrs.ptr();
    let mut pi = PROCESS_INFORMATION::default();
    #[cfg(any(test, feature = "fake"))]
    super::hooks::native_boundary("create_process");
    // SAFETY: every string is NUL-terminated and borrowed for the call; `cmdline` is a
    // mutable buffer of this attempt; `si` is zeroed with `cb` set and points to an
    // initialised attribute list; `bInheritHandles` is FALSE; `pi` is a local.
    let ok = unsafe {
        CreateProcessW(
            app.as_ptr(),
            cmdline.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            creation_flags(suspended),
            env.as_ptr().cast(),
            cwd.as_ptr(),
            &raw const si.StartupInfo,
            &raw mut pi,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let process = owned(pi.hProcess)?;
    let thread = owned(pi.hThread)?;
    created(&process);
    Ok(Created {
        process,
        thread,
        pid: pi.dwProcessId,
    })
}

/// Sets or clears `HANDLE_FLAG_INHERIT` on a handle.
fn set_inherit(h: HANDLE, inherit: bool) -> io::Result<()> {
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
    let value = if inherit { HANDLE_FLAG_INHERIT } else { 0 };
    // SAFETY: the caller passes a handle it borrows for the call.
    if unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, value) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `CreateProcessW(app, cmdline, NULL, NULL, TRUE, flags | EXTENDED_STARTUPINFO_PRESENT, NULL,
/// cwd, &si, &pi)` of a detached child (docs/design/daemon-lifecycle.md §2.10): this
/// process's environment, the standard handles `stdio` (input, output, error), and
/// `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` naming exactly those, so nothing else this process
/// holds is inherited. The handles are made inheritable for the call (under the spawn lock,
/// which every spawn of this process takes) and not inheritable again after it.
pub(crate) fn create_detached(
    _held: &SpawnGuard,
    app: &[u16],
    cmdline: &mut [u16],
    cwd: &[u16],
    flags: u32,
    stdio: [BorrowedHandle<'_>; 3],
) -> io::Result<Created> {
    debug_assert!(app.ends_with(&[0]) && cmdline.ends_with(&[0]) && cwd.ends_with(&[0]));
    let [input, output, error] = stdio.map(|h| raw(&h));
    let mut list: Vec<HANDLE> = Vec::with_capacity(3);
    for h in [input, output, error] {
        if !list.contains(&h) {
            list.push(h);
        }
    }
    for h in &list {
        set_inherit(*h, true)?;
    }
    let created = (|| {
        let mut attrs = AttrList::new(1)?;
        attrs.set_handle_list(&list)?;
        let mut si = STARTUPINFOEXW::default();
        si.StartupInfo.cb = len_u32(size_of::<STARTUPINFOEXW>());
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = input;
        si.StartupInfo.hStdOutput = output;
        si.StartupInfo.hStdError = error;
        si.lpAttributeList = attrs.ptr();
        let mut pi = PROCESS_INFORMATION::default();
        // SAFETY: every string is NUL-terminated and borrowed for the call; `cmdline` is a
        // mutable buffer of this call; `si` is zeroed with `cb` set and points to an
        // initialised attribute list whose handle list outlives the call; the environment is
        // NULL (inherited); `pi` is a local.
        let ok = unsafe {
            CreateProcessW(
                app.as_ptr(),
                cmdline.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                flags | EXTENDED_STARTUPINFO_PRESENT,
                std::ptr::null(),
                cwd.as_ptr(),
                &raw const si.StartupInfo,
                &raw mut pi,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Created {
            process: owned(pi.hProcess)?,
            thread: owned(pi.hThread)?,
            pid: pi.dwProcessId,
        })
    })();
    for h in &list {
        let _ = set_inherit(*h, false);
    }
    created
}

/// `ResumeThread`: the previous suspend count; `u32::MAX` is mapped to the last error.
pub(crate) fn resume(thread: &OwnedHandle) -> io::Result<u32> {
    // SAFETY: the thread handle is borrowed and valid.
    let n = unsafe { ResumeThread(raw(thread)) };
    if n == u32::MAX {
        return Err(io::Error::last_os_error());
    }
    Ok(n)
}

pub(crate) fn terminate_process(process: &OwnedHandle, code: u32) -> io::Result<()> {
    // SAFETY: the process handle is borrowed and valid.
    if unsafe { TerminateProcess(raw(process), code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ---- Waits and process queries ----

/// `WaitForSingleObject` for `ms` whole milliseconds (below `INFINITE`; zero polls): true
/// when the handle is signalled.
pub(crate) fn wait(h: BorrowedHandle<'_>, ms: u32) -> io::Result<bool> {
    let ms = ms.min(u32::MAX - 1);
    // SAFETY: the handle is borrowed and valid for the call.
    match unsafe { WaitForSingleObject(raw(&h), ms) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        WAIT_FAILED => Err(io::Error::last_os_error()),
        other => Err(io::Error::other(format!(
            "WaitForSingleObject returned {other:#x}"
        ))),
    }
}

/// [`wait`] for a `Duration`, floored to whole milliseconds (a sub-millisecond wait polls).
pub(crate) fn wait_floor(h: BorrowedHandle<'_>, d: Duration) -> io::Result<bool> {
    wait(h, u32::try_from(d.as_millis()).unwrap_or(u32::MAX - 1))
}

pub(crate) fn exit_code(process: &OwnedHandle) -> io::Result<u32> {
    let mut code = 0u32;
    // SAFETY: the process handle is borrowed and valid; `code` is a local.
    if unsafe { GetExitCodeProcess(raw(process), &raw mut code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(code)
}

/// The process creation time on that handle, in 100-ns units since 1601-01-01 UTC.
pub(crate) fn creation_time(process: BorrowedHandle<'_>) -> io::Result<u64> {
    let mut times = [FILETIME::default(); 4];
    let [c, e, k, u] = &mut times;
    // SAFETY: the handle is borrowed and valid; the four out-parameters are locals.
    let ok = unsafe { GetProcessTimes(raw(&process), c, e, k, u) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((u64::from(times[0].dwHighDateTime) << 32) | u64::from(times[0].dwLowDateTime))
}

/// `OpenProcess(access, FALSE, pid)`.
pub(crate) fn open_process(pid: u32, access: u32) -> io::Result<OwnedHandle> {
    // SAFETY: plain call; `bInheritHandle` is FALSE; the result is checked before wrapping.
    let h = unsafe { OpenProcess(access, 0, pid) };
    owned(h)
}

/// One entry of a Toolhelp process snapshot.
pub(crate) struct SnapshotEntry {
    pub(crate) pid: u32,
    /// `th32ParentProcessID`: the creator's pid, never updated after the creator exits.
    pub(crate) ppid: u32,
    /// `szExeFile`: the image file name, lossily decoded.
    pub(crate) exe: String,
}

/// `ERROR_NO_MORE_FILES`: the end of a Toolhelp enumeration.
const ERROR_NO_MORE_FILES: i32 = 18;

/// Every process of one `CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS)`, through
/// `Process32FirstW` / `Process32NextW`; the snapshot handle is closed on return.
pub(crate) fn process_snapshot() -> io::Result<Vec<SnapshotEntry>> {
    // SAFETY: plain call; the result is checked (INVALID_HANDLE_VALUE) before wrapping.
    let snap = owned(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) })?;
    let mut e = PROCESSENTRY32W {
        dwSize: len_u32(size_of::<PROCESSENTRY32W>()),
        ..PROCESSENTRY32W::default()
    };
    let mut out = Vec::new();
    // SAFETY: the snapshot handle is borrowed and valid; `e` is a local with `dwSize` set.
    let mut ok = unsafe { Process32FirstW(raw(&snap), &raw mut e) };
    while ok != 0 {
        let len = e
            .szExeFile
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(e.szExeFile.len());
        out.push(SnapshotEntry {
            pid: e.th32ProcessID,
            ppid: e.th32ParentProcessID,
            exe: String::from_utf16_lossy(&e.szExeFile[..len]),
        });
        // SAFETY: as for `Process32FirstW`.
        ok = unsafe { Process32NextW(raw(&snap), &raw mut e) };
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(ERROR_NO_MORE_FILES) {
        Ok(out)
    } else {
        Err(err)
    }
}

// ---- Evidence and helpers ----

/// The file path of a loaded module, without taking a reference count.
pub(crate) fn module_path(name: &str) -> Option<PathBuf> {
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    // SAFETY: `wide` is NUL-terminated; `GetModuleHandleW` takes no reference.
    let module = unsafe { GetModuleHandleW(wide.as_ptr()) };
    if module.is_null() {
        return None;
    }
    let mut buf = vec![0u16; 1024];
    // SAFETY: `module` is loaded (the process imports it); the buffer length is passed.
    let n = unsafe { GetModuleFileNameW(module, buf.as_mut_ptr(), len_u32(buf.len())) };
    let n = usize::try_from(n).ok()?;
    if n == 0 || n >= buf.len() {
        return None;
    }
    buf.truncate(n);
    Some(PathBuf::from(std::ffi::OsString::from_wide(&buf)))
}

/// `CompareStringOrdinal(a, b, TRUE)`; a failure (only for invalid parameters) falls back to
/// an ordinal comparison of the raw units.
pub(crate) fn compare_ordinal_ignore_case(a: &[u16], b: &[u16]) -> Ordering {
    let (Ok(la), Ok(lb)) = (i32::try_from(a.len()), i32::try_from(b.len())) else {
        return a.cmp(b);
    };
    // SAFETY: both pointers come with their lengths; the call reads only those units.
    let r = unsafe { CompareStringOrdinal(a.as_ptr(), la, b.as_ptr(), lb, 1) };
    match r {
        CSTR_LESS_THAN => Ordering::Less,
        CSTR_EQUAL => Ordering::Equal,
        CSTR_GREATER_THAN => Ordering::Greater,
        _ => a.cmp(b),
    }
}

/// `TerminateProcess(GetCurrentProcess(), code)`: ends this process without exit handlers or
/// DLL detach; it returns only when refused.
pub(crate) fn terminate_current(code: u32) -> io::Result<()> {
    // SAFETY: the pseudo-handle of the current process is always valid.
    if unsafe { TerminateProcess(current_process(), code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The current-process pseudo-handle (needs no close).
fn current_process() -> HANDLE {
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: no arguments; returns a constant pseudo-handle.
    unsafe { GetCurrentProcess() }
}

/// `GetProcessHandleCount(GetCurrentProcess())`.
#[cfg(any(test, feature = "fake"))]
pub(crate) fn process_handle_count() -> io::Result<u32> {
    use windows_sys::Win32::System::Threading::GetProcessHandleCount;
    let me = current_process();
    let mut n = 0u32;
    // SAFETY: the current-process pseudo-handle needs no close; `n` is a local.
    if unsafe { GetProcessHandleCount(me, &raw mut n) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n)
}

/// A duplicate of `h` with exactly `access` (the `…TerminateDenied` faults).
#[cfg(any(test, feature = "fake"))]
pub(crate) fn duplicate_with_access(h: &OwnedHandle, access: u32) -> io::Result<OwnedHandle> {
    use windows_sys::Win32::Foundation::DuplicateHandle;
    let me = current_process();
    let mut out: HANDLE = std::ptr::null_mut();
    // SAFETY: source and target are the current process (a pseudo-handle); `h` is borrowed
    // and valid; `out` is a local; `bInheritHandle` is FALSE.
    let ok = unsafe { DuplicateHandle(me, raw(h), me, &raw mut out, access, 0, 0) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    owned(out)
}

/// Makes a handle inheritable (CL-21).
#[cfg(any(test, feature = "fake"))]
pub(crate) fn make_inheritable(h: &impl AsRawHandle) -> io::Result<()> {
    set_inherit(raw(h), true)
}

/// Makes a handle not inheritable again (CL-21).
#[cfg(test)]
pub(crate) fn clear_inheritable(h: &impl AsRawHandle) -> io::Result<()> {
    set_inherit(raw(h), false)
}

/// Whether a pipe's write end is still open anywhere (CL-21): `PeekNamedPipe` on the read
/// end, `ERROR_BROKEN_PIPE` meaning no writer.
#[cfg(test)]
pub(crate) fn pipe_has_writer(r: &std::io::PipeReader) -> io::Result<bool> {
    use windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE;
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;
    let mut avail = 0u32;
    // SAFETY: the read end is borrowed and valid; no buffer is passed (size 0, null), and
    // the only out-pointer is a local.
    let ok = unsafe {
        PeekNamedPipe(
            raw(r),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &raw mut avail,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(ERROR_BROKEN_PIPE.cast_signed()) {
            return Ok(false);
        }
        return Err(e);
    }
    Ok(true)
}

// ---- Process-level primitives of `host` (docs/design/daemon-lifecycle.md §2.2) ----

/// A buffer an API call allocated with `LocalAlloc`; freed once on drop.
struct Local(*mut c_void);

impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the buffer came from an API that documents `LocalFree`, and nothing
            // else frees it.
            let _ = unsafe { windows_sys::Win32::Foundation::LocalFree(self.0) };
        }
    }
}

fn wide_str(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// `p` made absolute (normalized by `GetFullPathNameW`) in the `\\?\` form, so a create
/// accepts paths longer than `MAX_PATH` as std's own opens do; NUL-terminated.
fn long_path(p: &std::path::Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    let abs: Vec<u16> = std::path::absolute(p)?.as_os_str().encode_wide().collect();
    let units = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
    let mut out = if abs.starts_with(&units(r"\\?\")) || abs.starts_with(&units(r"\\.\")) {
        abs
    } else if abs.starts_with(&units(r"\\")) {
        let mut v = units(r"\\?\UNC\");
        v.extend_from_slice(&abs[2..]);
        v
    } else {
        let mut v = units(r"\\?\");
        v.extend_from_slice(&abs);
        v
    };
    out.push(0);
    Ok(out)
}

/// The units of a NUL-terminated UTF-16 string an API returned.
fn wide_units<'a>(p: *const u16) -> &'a [u16] {
    if p.is_null() {
        return &[];
    }
    // SAFETY: `p` is a NUL-terminated string the API returned and the caller keeps alive.
    let n = unsafe { windows_sys::Win32::Globalization::lstrlenW(p) };
    let n = usize::try_from(n).unwrap_or(0);
    // SAFETY: `p` points to `n` initialized units (its length without the NUL).
    unsafe { std::slice::from_raw_parts(p, n) }
}

/// The string form (`S-1-…`) of `sid`.
fn sid_string(sid: windows_sys::Win32::Security::PSID) -> io::Result<String> {
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    let mut out: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: `sid` is a valid SID for the duration of the call; `out` is a local.
    if unsafe { ConvertSidToStringSidW(sid, &raw mut out) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let free = Local(out.cast());
    let s = String::from_utf16_lossy(wide_units(out));
    drop(free);
    Ok(s)
}

/// The current user's SID in string form.
pub(crate) fn user_sid() -> io::Result<String> {
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::OpenProcessToken;
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: the current-process pseudo-handle needs no close; `token` is a local.
    if unsafe { OpenProcessToken(current_process(), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = owned(token)?;
    let mut needed = 0u32;
    // SAFETY: a null buffer of size 0 asks for the size only; `needed` is a local.
    let _ = unsafe {
        GetTokenInformation(
            raw(&token),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &raw mut needed,
        )
    };
    let bytes = usize::try_from(needed)
        .unwrap_or(0)
        .max(size_of::<TOKEN_USER>());
    let mut buf = vec![0usize; bytes.div_ceil(size_of::<usize>())];
    // SAFETY: `buf` is pointer-aligned and at least `needed` bytes long, which is passed.
    let ok = unsafe {
        GetTokenInformation(
            raw(&token),
            TokenUser,
            buf.as_mut_ptr().cast(),
            len_u32(buf.len() * size_of::<usize>()),
            &raw mut needed,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call wrote a `TOKEN_USER` at the start of the pointer-aligned `buf`.
    let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
    sid_string(user.User.Sid)
}

/// A security descriptor converted from SDDL; freed on drop.
fn descriptor(sddl: &str) -> io::Result<Local> {
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    let w = wide_str(sddl);
    let mut sd: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `w` is NUL-terminated; `sd` is a local; the size is not requested.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w.as_ptr(),
            SDDL_REVISION_1,
            &raw mut sd,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Local(sd))
}

fn attributes(sd: &Local) -> windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
    windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        nLength: len_u32(size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>()),
        lpSecurityDescriptor: sd.0,
        bInheritHandle: 0,
    }
}

/// `CreateDirectoryW` with the descriptor `sddl` (its last component only).
pub(crate) fn create_dir_with(path: &std::path::Path, sddl: &str) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
    let sd = descriptor(sddl)?;
    let sa = attributes(&sd);
    let w = long_path(path)?;
    // SAFETY: `w` is NUL-terminated; `sa` and the descriptor it points to outlive the call.
    if unsafe { CreateDirectoryW(w.as_ptr(), &raw const sa) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `CreateFileW(CREATE_NEW)` for reading and writing with the descriptor `sddl`, never
/// following a reparse point.
pub(crate) fn create_new_file_with(
    path: &std::path::Path,
    sddl: &str,
) -> io::Result<std::fs::File> {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let sd = descriptor(sddl)?;
    let sa = attributes(&sd);
    let w = long_path(path)?;
    // SAFETY: `w` is NUL-terminated; `sa` and its descriptor outlive the call; no template.
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &raw const sa,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    Ok(std::fs::File::from(owned(h)?))
}

/// One ACE of a DACL: its type and flags, and its SID for the allowed and denied types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AceInfo {
    pub(crate) ace_type: u8,
    pub(crate) flags: u8,
    pub(crate) sid: Option<String>,
}

/// The owner and DACL of an object (`None`: a NULL DACL, which grants everyone).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SecurityInfo {
    pub(crate) owner: String,
    pub(crate) dacl: Option<Vec<AceInfo>>,
}

/// `ACCESS_ALLOWED_ACE_TYPE` and `ACCESS_DENIED_ACE_TYPE` (`winnt.h`), which share a layout.
pub(crate) const ACE_ALLOWED: u8 = 0;
pub(crate) const ACE_DENIED: u8 = 1;

/// `GetSecurityInfo` (owner and DACL) of the file or directory open as `h`; the handle needs
/// `READ_CONTROL`.
pub(crate) fn security_of(h: BorrowedHandle<'_>) -> io::Result<SecurityInfo> {
    security_of_object(
        h,
        windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT,
    )
}

/// `GetSecurityInfo` (owner and DACL) of an object of `object_type` open as `h` with
/// `READ_CONTROL`.
pub(crate) fn security_of_object(
    h: BorrowedHandle<'_>,
    object_type: windows_sys::Win32::Security::Authorization::SE_OBJECT_TYPE,
) -> io::Result<SecurityInfo> {
    use windows_sys::Win32::Security::Authorization::GetSecurityInfo;
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, GetAce,
        OWNER_SECURITY_INFORMATION,
    };
    let mut owner: windows_sys::Win32::Security::PSID = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: the handle is borrowed and valid; the out-pointers are locals; group and SACL
    // are not requested.
    let rc = unsafe {
        GetSecurityInfo(
            raw(&h),
            object_type,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &raw mut owner,
            std::ptr::null_mut(),
            &raw mut dacl,
            std::ptr::null_mut(),
            &raw mut sd,
        )
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc.cast_signed()));
    }
    // Owner and DACL point into `sd`, which lives until the end of this function.
    let sd = Local(sd);
    let owner = sid_string(owner)?;
    if dacl.is_null() {
        drop(sd);
        return Ok(SecurityInfo { owner, dacl: None });
    }
    // SAFETY: `dacl` points to the ACL inside the live descriptor.
    let count = unsafe { (*dacl).AceCount };
    let mut aces = Vec::with_capacity(usize::from(count));
    for i in 0..u32::from(count) {
        let mut ace: *mut c_void = std::ptr::null_mut();
        // SAFETY: `dacl` is live and `i` is below its count; `ace` is a local.
        if unsafe { GetAce(dacl, i, &raw mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: every ACE begins with an `ACE_HEADER`, and ACEs are DWORD-aligned.
        let header = unsafe { ace.cast::<ACE_HEADER>().read() };
        let sid = if header.AceType == ACE_ALLOWED || header.AceType == ACE_DENIED {
            // SAFETY: allowed and denied ACEs have the `ACCESS_ALLOWED_ACE` layout, whose
            // `SidStart` is the first DWORD of the SID.
            let start = unsafe { &raw mut (*ace.cast::<ACCESS_ALLOWED_ACE>()).SidStart };
            Some(sid_string(start.cast())?)
        } else {
            None
        };
        aces.push(AceInfo {
            ace_type: header.AceType,
            flags: header.AceFlags,
            sid,
        });
    }
    drop(sd);
    Ok(SecurityInfo {
        owner,
        dacl: Some(aces),
    })
}

/// `SHGetKnownFolderPath(id)` for the current user.
pub(crate) fn known_folder(id: &windows_sys::core::GUID) -> io::Result<PathBuf> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::SHGetKnownFolderPath;
    let mut p: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: `id` is a valid GUID; flags 0 (`KF_FLAG_DEFAULT`); no token (the current
    // user); `p` is a local.
    let hr = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &raw mut p) };
    let out = if hr >= 0 {
        Ok(PathBuf::from(std::ffi::OsString::from_wide(wide_units(p))))
    } else {
        Err(hresult_error(hr))
    };
    // SAFETY: the buffer (null on some failures, which is allowed) is freed exactly once.
    unsafe { CoTaskMemFree(p.cast()) };
    out
}

/// `FOLDERID_LocalAppData`.
pub(crate) fn local_app_data() -> io::Result<PathBuf> {
    known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_LocalAppData)
}

/// `FOLDERID_Public` (tests).
#[cfg(feature = "fake")]
pub(crate) fn public_folder() -> io::Result<PathBuf> {
    known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_Public)
}

/// A named auto-reset event created with the descriptor `sddl`; `AlreadyExists` when the
/// name was taken (the existing object's handle is closed).
pub(crate) fn create_named_event(name: &str, sddl: &str) -> io::Result<OwnedHandle> {
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, SetLastError};
    use windows_sys::Win32::System::Threading::CreateEventW;
    let sd = descriptor(sddl)?;
    let sa = attributes(&sd);
    let w = wide_str(name);
    // SAFETY: plain call; clears the thread's last error so the next read is this call's.
    unsafe { SetLastError(0) };
    // SAFETY: `w` is NUL-terminated; `sa` and its descriptor outlive the call; auto-reset,
    // initially non-signalled.
    let h = unsafe { CreateEventW(&raw const sa, 0, 0, w.as_ptr()) };
    // SAFETY: plain call, right after `CreateEventW` on this thread.
    let last = unsafe { GetLastError() };
    let h = owned(h)?;
    if last == ERROR_ALREADY_EXISTS {
        drop(h);
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("the event {name} already exists"),
        ));
    }
    Ok(h)
}

/// An unnamed manual-reset event, initially non-signalled.
pub(crate) fn create_event() -> io::Result<OwnedHandle> {
    use windows_sys::Win32::System::Threading::CreateEventW;
    // SAFETY: null attributes and name are allowed; manual-reset, non-signalled.
    let h = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    owned(h)
}

/// `SetEvent`.
pub(crate) fn set_event(h: &OwnedHandle) -> io::Result<()> {
    use windows_sys::Win32::System::Threading::SetEvent;
    // SAFETY: the handle is borrowed and valid.
    if unsafe { SetEvent(raw(h)) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Waits without a timeout until one of `a`, `b` is signalled; 0 for `a`, 1 for `b`.
pub(crate) fn wait_either(a: &OwnedHandle, b: &OwnedHandle) -> io::Result<usize> {
    use windows_sys::Win32::System::Threading::WaitForMultipleObjects;
    let handles = [raw(a), raw(b)];
    // SAFETY: both handles are borrowed and valid for the wait; the array length is passed.
    let r = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, u32::MAX) };
    match r {
        WAIT_FAILED => Err(io::Error::last_os_error()),
        r if r == WAIT_OBJECT_0 => Ok(0),
        r if r == WAIT_OBJECT_0 + 1 => Ok(1),
        r => Err(io::Error::other(format!("unexpected wait result {r:#x}"))),
    }
}

/// `OpenEventW(EVENT_MODIFY_STATE, name)`.
pub(crate) fn open_event_modify(name: &str) -> io::Result<OwnedHandle> {
    use windows_sys::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW};
    let w = wide_str(name);
    // SAFETY: `w` is NUL-terminated; not inheritable.
    let h = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, w.as_ptr()) };
    owned(h)
}

/// Opens the named event with `READ_CONTROL` only (tests read its descriptor back).
#[cfg(feature = "fake")]
pub(crate) fn open_event_read_control(name: &str) -> io::Result<OwnedHandle> {
    use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
    use windows_sys::Win32::System::Threading::OpenEventW;
    let w = wide_str(name);
    // SAFETY: `w` is NUL-terminated; not inheritable.
    let h = unsafe { OpenEventW(READ_CONTROL, 0, w.as_ptr()) };
    owned(h)
}

/// `GenerateConsoleCtrlEvent(event, group)` (tests).
#[cfg(feature = "fake")]
pub(crate) fn generate_ctrl_event(event: u32, group: u32) -> io::Result<()> {
    use windows_sys::Win32::System::Console::GenerateConsoleCtrlEvent;
    // SAFETY: plain call.
    if unsafe { GenerateConsoleCtrlEvent(event, group) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `SetConsoleCtrlHandler(NULL, TRUE)`: this process ignores Ctrl-C (tests).
#[cfg(feature = "fake")]
pub(crate) fn ignore_ctrl_c() -> io::Result<()> {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    // SAFETY: a null handler with TRUE sets the process's ignore attribute.
    if unsafe { SetConsoleCtrlHandler(None, 1) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Clears an inherited "ignore Ctrl-C" attribute (`SetConsoleCtrlHandler(NULL, FALSE)`) when
/// the process has a console; without one there is nothing to deliver.
///
/// `GetConsoleProcessList` returns 0 both when the process has no console and when the call
/// itself fails; the two are not told apart here. Either way the reset is skipped: a process
/// with a console that the probe failed for keeps an inherited ignore, and still stops on
/// Ctrl-Break and on the stop event.
pub(crate) fn enable_ctrl_c_if_console() -> io::Result<()> {
    use windows_sys::Win32::System::Console::{GetConsoleProcessList, SetConsoleCtrlHandler};
    let mut one = [0u32; 1];
    // SAFETY: the buffer holds one entry, which is passed.
    if unsafe { GetConsoleProcessList(one.as_mut_ptr(), 1) } == 0 {
        return Ok(());
    }
    // SAFETY: a null handler with FALSE clears the process's ignore attribute.
    if unsafe { SetConsoleCtrlHandler(None, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The number of hard links of the file open as `h` (`GetFileInformationByHandle`).
pub(crate) fn file_links(h: BorrowedHandle<'_>) -> io::Result<u32> {
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    // SAFETY: a plain-data struct the call fills.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is borrowed and valid; `info` is a local.
    if unsafe { GetFileInformationByHandle(raw(&h), &raw mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.nNumberOfLinks)
}

/// The pids attached to this process's console (tests).
#[cfg(feature = "fake")]
pub(crate) fn console_processes() -> io::Result<Vec<u32>> {
    use windows_sys::Win32::System::Console::GetConsoleProcessList;
    let mut pids = vec![0u32; 64];
    // SAFETY: the buffer holds `pids.len()` entries, which is passed.
    let n = unsafe { GetConsoleProcessList(pids.as_mut_ptr(), len_u32(pids.len())) };
    if n == 0 {
        return Err(io::Error::last_os_error());
    }
    pids.truncate(usize::try_from(n).unwrap_or(0).min(64));
    Ok(pids)
}

// ---- Parent job, console, standard handles, browser (`host`, daemon-lifecycle §2.10–§2.13) ----

/// Whether `process` is in any job (`IsProcessInJob(process, NULL)`).
pub(crate) fn in_any_job(process: HANDLE) -> io::Result<bool> {
    let mut result = 0;
    // SAFETY: `process` is a valid handle (or the current-process pseudo-handle) for the call;
    // a null job handle asks about any job; `result` is a local.
    if unsafe { IsProcessInJob(process, std::ptr::null_mut(), &raw mut result) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(result != 0)
}

/// Whether this process is in any job.
#[cfg(test)]
pub(crate) fn in_any_job_now() -> io::Result<bool> {
    in_any_job(current_process())
}

/// Whether `process` (an open handle) is in any job.
#[cfg(feature = "fake")]
pub(crate) fn handle_in_any_job(process: &OwnedHandle) -> io::Result<bool> {
    in_any_job(raw(process))
}

/// This process's job state: `None` in no job, else the immediate job's `LimitFlags`
/// (`QueryInformationJobObject(NULL, JobObjectExtendedLimitInformation)`, L13).
pub(crate) fn own_job_flags() -> io::Result<Option<u32>> {
    if !in_any_job(current_process())? {
        return Ok(None);
    }
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    // SAFETY: a null job handle queries the immediate job of the calling process; `info` is a
    // zeroed struct of integers whose size is passed.
    let ok = unsafe {
        QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            (&raw mut info).cast(),
            len_u32(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(info.BasicLimitInformation.LimitFlags))
}

/// `GetConsoleWindow() != NULL`.
pub(crate) fn has_console_window() -> bool {
    use windows_sys::Win32::System::Console::GetConsoleWindow;
    // SAFETY: no arguments.
    !unsafe { GetConsoleWindow() }.is_null()
}

/// Takes ownership of this process's standard output handle and sets the process's standard
/// output to NULL, so std's `Stdout` writes nowhere and nothing else closes the handle.
pub(crate) fn take_std_output() -> io::Result<OwnedHandle> {
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_OUTPUT_HANDLE, SetStdHandle};
    // SAFETY: plain call.
    let h = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "this process has no standard output handle",
        ));
    }
    // SAFETY: plain call; NULL is accepted as the new standard handle.
    if unsafe { SetStdHandle(STD_OUTPUT_HANDLE, std::ptr::null_mut()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `h` was this process's standard output handle; once replaced by NULL no other
    // code of this process reaches it through `GetStdHandle`, so it is wrapped exactly once.
    Ok(unsafe { OwnedHandle::from_raw_handle(h) })
}

/// Resumes every thread of the process `pid` (a process created suspended by a caller that
/// holds no thread handle): one `TH32CS_SNAPTHREAD` snapshot, `OpenThread` with
/// `THREAD_SUSPEND_RESUME`, and `ResumeThread` on each of its threads. The number resumed.
pub(crate) fn resume_process_threads(pid: u32) -> io::Result<usize> {
    // SAFETY: plain call; the result is checked (INVALID_HANDLE_VALUE) before wrapping.
    let snap = owned(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) })?;
    let mut e = THREADENTRY32 {
        dwSize: len_u32(size_of::<THREADENTRY32>()),
        ..THREADENTRY32::default()
    };
    let mut resumed = 0;
    // SAFETY: the snapshot handle is borrowed and valid; `e` is a local with `dwSize` set.
    let mut ok = unsafe { Thread32First(raw(&snap), &raw mut e) };
    while ok != 0 {
        if e.th32OwnerProcessID == pid {
            // SAFETY: plain call; the result is checked before wrapping.
            let t = owned(unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, e.th32ThreadID) })?;
            resume(&t)?;
            resumed += 1;
        }
        // SAFETY: as for `Thread32First`.
        ok = unsafe { Thread32Next(raw(&snap), &raw mut e) };
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(ERROR_NO_MORE_FILES) {
        Ok(resumed)
    } else {
        Err(err)
    }
}

/// The full Win32 image path of the process `pid` (`QueryFullProcessImageNameW`).
#[cfg(feature = "fake")]
pub(crate) fn image_path(pid: u32) -> io::Result<PathBuf> {
    use windows_sys::Win32::System::Threading::{
        PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    let p = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut buf = vec![0u16; 32_768];
    let mut n = len_u32(buf.len());
    // SAFETY: `p` is an open process handle; `n` holds the buffer's length in units and
    // receives the written length.
    let ok = unsafe {
        QueryFullProcessImageNameW(
            p.as_raw_handle(),
            PROCESS_NAME_WIN32,
            buf.as_mut_ptr(),
            &raw mut n,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(usize::try_from(n).unwrap_or(0));
    Ok(PathBuf::from(std::ffi::OsString::from_wide(&buf)))
}

/// The smallest previous suspend count over the threads of the process `pid`, each thread
/// suspended once and resumed at once (`SuspendThread`, `ResumeThread`): at least 1 when
/// every thread was suspended. An error when the process has no thread.
#[cfg(feature = "fake")]
pub(crate) fn suspend_count(pid: u32) -> io::Result<u32> {
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, SuspendThread, THREAD_SUSPEND_RESUME};
    // SAFETY: plain call; the result is checked (INVALID_HANDLE_VALUE) before wrapping.
    let snap = owned(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) })?;
    let mut e = THREADENTRY32 {
        dwSize: len_u32(size_of::<THREADENTRY32>()),
        ..THREADENTRY32::default()
    };
    let mut least: Option<u32> = None;
    // SAFETY: the snapshot handle is borrowed and valid; `e` is a local with `dwSize` set.
    let mut ok = unsafe { Thread32First(raw(&snap), &raw mut e) };
    while ok != 0 {
        if e.th32OwnerProcessID == pid {
            // SAFETY: plain call; the result is checked before wrapping.
            let t = owned(unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, e.th32ThreadID) })?;
            // SAFETY: the thread handle is borrowed and valid.
            let n = unsafe { SuspendThread(raw(&t)) };
            if n == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            resume(&t)?;
            least = Some(least.map_or(n, |l| l.min(n)));
        }
        // SAFETY: as for `Thread32First`.
        ok = unsafe { Thread32Next(raw(&snap), &raw mut e) };
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() != Some(ERROR_NO_MORE_FILES) {
        return Err(err);
    }
    least.ok_or_else(|| io::Error::other("the process has no thread"))
}

/// `ShellExecuteExW` with the `open` verb on `url`, `SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI`
/// (L21), after `CoInitializeEx` as the shell requires.
pub(crate) fn shell_open(url: &str) -> io::Result<()> {
    use windows_sys::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx,
    };
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    /// `SW_SHOWNORMAL` (`winuser.h`).
    const SW_SHOWNORMAL: i32 = 1;
    // SAFETY: reserved pointer null; a failure (already initialized another way) is fine for
    // the shell call below.
    let _ = unsafe {
        CoInitializeEx(
            std::ptr::null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).cast_unsigned(),
        )
    };
    let verb = wide_str("open");
    let file = wide_str(url);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: len_u32(size_of::<SHELLEXECUTEINFOW>()),
        fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        nShow: SW_SHOWNORMAL,
        ..SHELLEXECUTEINFOW::default()
    };
    // SAFETY: `info` is fully initialized with its size; `verb` and `file` are NUL-terminated
    // and outlive the call.
    if unsafe { ShellExecuteExW(&raw mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
