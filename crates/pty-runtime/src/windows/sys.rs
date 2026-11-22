//! The only Windows module allowed `unsafe` (`docs/design/pty-windows.md` §2.7). Each
//! function is safe to call: handles are borrowed `OwnedHandle`s or std pipe ends, every
//! returned handle is wrapped exactly once and only on success, and each `unsafe` block
//! holds one operation.

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
    InitializeProcThreadAttributeList, OpenProcess, PROC_THREAD_ATTRIBUTE_JOB_LIST,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES,
    STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
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

pub(crate) fn assign(job: &OwnedHandle, process: &OwnedHandle) -> io::Result<()> {
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

/// The current-process pseudo-handle (needs no close).
#[cfg(any(test, feature = "fake"))]
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
#[cfg(test)]
pub(crate) fn make_inheritable(h: &impl AsRawHandle) -> io::Result<()> {
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
    // SAFETY: the handle is borrowed and valid.
    if unsafe { SetHandleInformation(raw(h), HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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
