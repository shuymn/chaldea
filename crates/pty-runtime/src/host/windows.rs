//! `host` on Windows: std for opens (with `FILE_FLAG_OPEN_REPARSE_POINT`), attributes, and
//! creation flags, `windows::sys` for security descriptors, known folders, events, the job
//! state, the standard handles, and `ShellExecuteExW`.

use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::Instant;

use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, READ_CONTROL,
};

use super::{
    Access, Breakaway, EntryKind, JobLimits, JobState, LaunchctlOutput, Privacy, ProcessIdentity,
    ReadyArg, ReadyRead, SigchldDisposition, SignalState, StopTarget,
};
use crate::windows::sys::{self, ACE_ALLOWED, ACE_DENIED, AceInfo, SecurityInfo};

/// `ERROR_INVALID_PARAMETER`: `OpenProcess` of a pid no process has.
const ERROR_INVALID_PARAMETER: i32 = 87;

/// The well-known SIDs of the private trustees (`docs/design/daemon-lifecycle.md` §2.4).
const SYSTEM: &str = "S-1-5-18";
const ADMINISTRATORS: &str = "S-1-5-32-544";
const CREATOR_OWNER: &str = "S-1-3-0";
const OWNER_RIGHTS: &str = "S-1-3-4";

pub(super) fn exit_now(code: i32) -> ! {
    // Termination of the current process does not return once it began; the wait covers an
    // asynchronous start. Only a refused call falls back to the normal exit.
    if sys::terminate_current(code.cast_unsigned()).is_ok() {
        loop {
            std::thread::park();
        }
    }
    std::process::exit(code)
}

pub(super) fn process_identity(pid: u32) -> io::Result<Option<ProcessIdentity>> {
    let h = match sys::open_process(pid, crate::windows::QUERY_SYNC) {
        Ok(h) => h,
        Err(e) if e.raw_os_error() == Some(ERROR_INVALID_PARAMETER) => return Ok(None),
        Err(e) => return Err(e),
    };
    if sys::wait(h.as_handle(), 0)? {
        return Ok(None);
    }
    let start = sys::creation_time(h.as_handle())?;
    Ok(Some(ProcessIdentity {
        pid,
        start_time: start.to_string(),
        group: String::new(),
    }))
}

/// Windows has no inherited signal mask; the one inherited state that keeps a console
/// program from stopping is an "ignore Ctrl-C" attribute (for example from `start /b`).
pub(super) fn reset_process_signals() -> io::Result<()> {
    sys::enable_ctrl_c_if_console()
}

pub(super) fn signal_state() -> io::Result<SignalState> {
    Ok(SignalState {
        sigchld: SigchldDisposition::Default,
        no_cldwait: false,
        sigchld_flags: 0,
        blocked: Vec::new(),
    })
}

pub(super) fn default_state_base() -> io::Result<PathBuf> {
    sys::local_app_data()
}

/// The owner-only descriptor of a new directory: owner the user, a protected DACL with full
/// access for the user and `SYSTEM`, inherited by files and subdirectories.
fn private_dir_sddl(user: &str) -> String {
    format!("O:{user}D:P(A;OICI;FA;;;{user})(A;OICI;FA;;;SY)")
}

/// The owner-only descriptor of a new secret file.
fn private_file_sddl(user: &str) -> String {
    format!("O:{user}D:P(A;;FA;;;{user})(A;;FA;;;SY)")
}

pub(super) fn create_private_dir(path: &Path) -> io::Result<()> {
    let user = sys::user_sid()?;
    sys::create_dir_with(path, &private_dir_sddl(&user))
}

pub(super) fn create_new_private(path: &Path) -> io::Result<File> {
    let user = sys::user_sid()?;
    sys::create_new_file_with(path, &private_file_sddl(&user))
}

/// What an entry's attributes say it is, against what it must be (unit-tested over
/// synthetic inputs: a reparse point is a link whatever its tag, so a file symlink needs no
/// symlink privilege to be covered).
#[must_use]
pub fn classify_entry(attributes: u32, _reparse_tag: u32, kind: EntryKind) -> Option<Privacy> {
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Some(Privacy::Link);
    }
    let is_dir = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    let of_kind = match kind {
        EntryKind::File => !is_dir,
        EntryKind::Directory => is_dir,
    };
    (!of_kind).then_some(Privacy::NotOfKind)
}

/// `INHERIT_ONLY_ACE` (`winnt.h`): the ACE applies only to what the directory's children
/// inherit.
const INHERIT_ONLY_ACE: u8 = 0x08;

/// A private trustee of an allow ACE: the user, `SYSTEM`, `Administrators`; on a directory also
/// `OWNER RIGHTS`, and `CREATOR OWNER` on an inherit-only ACE (it names the owner of what is
/// created inside).
fn private_trustee(sid: &str, flags: u8, user: &str, kind: EntryKind) -> bool {
    sid == user
        || sid == SYSTEM
        || sid == ADMINISTRATORS
        || (kind == EntryKind::Directory
            && (sid == OWNER_RIGHTS || (sid == CREATOR_OWNER && flags & INHERIT_ONLY_ACE != 0)))
}

/// The privacy judgement of an owner and DACL (`None`: a NULL DACL): owned by `user`, and
/// every allow ACE names a private trustee; deny ACEs pass; allow ACEs of another layout
/// (object, callback) refuse.
#[must_use]
pub fn judge_descriptor(
    owner: &str,
    dacl: Option<&[AceInfo]>,
    user: &str,
    kind: EntryKind,
) -> Privacy {
    if owner != user {
        return Privacy::NotOwned;
    }
    let Some(aces) = dacl else {
        return Privacy::AclGrantsOthers;
    };
    for ace in aces {
        match ace.ace_type {
            ACE_DENIED => {}
            ACE_ALLOWED => {
                let sid = ace.sid.as_deref().unwrap_or("");
                if !private_trustee(sid, ace.flags, user, kind) {
                    return Privacy::AclGrantsOthers;
                }
            }
            // ACCESS_ALLOWED_COMPOUND, _OBJECT, _CALLBACK, _CALLBACK_OBJECT.
            4 | 5 | 9 | 11 => return Privacy::AclGrantsOthers,
            _ => {}
        }
    }
    Privacy::Private
}

fn judge(sec: &SecurityInfo, kind: EntryKind) -> io::Result<Privacy> {
    let user = sys::user_sid()?;
    Ok(judge_descriptor(
        &sec.owner,
        sec.dacl.as_deref(),
        &user,
        kind,
    ))
}

/// Opens `path` itself (never a reparse point's target) for its attributes and descriptor.
fn open_entry(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

pub(super) fn privacy(path: &Path, kind: EntryKind) -> io::Result<Privacy> {
    let f = open_entry(path)?;
    let m = f.metadata()?;
    if let Some(p) = classify_entry(m.file_attributes(), 0, kind) {
        return Ok(p);
    }
    judge(&sys::security_of(f.as_handle())?, kind)
}

pub(super) fn handle_privacy(file: &File) -> io::Result<Privacy> {
    let m = file.metadata()?;
    if let Some(p) = classify_entry(m.file_attributes(), 0, EntryKind::File) {
        return Ok(p);
    }
    judge(&sys::security_of(file.as_handle())?, EntryKind::File)
}

pub(super) fn open_no_follow(path: &Path, access: Access) -> io::Result<File> {
    let mut o = OpenOptions::new();
    match access {
        Access::Read => {
            o.read(true);
        }
        Access::ReadWriteCreate => {
            o.read(true).write(true).create(true);
        }
        Access::AppendCreate => {
            o.append(true).create(true);
        }
    }
    o.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let f = o.open(path)?;
    let attrs = f.metadata()?.file_attributes();
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a link or reparse point",
        ));
    }
    if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok(f)
}

pub(super) fn hard_links(path: &Path) -> io::Result<u64> {
    let f = open_entry(path)?;
    Ok(u64::from(sys::file_links(f.as_handle())?))
}

pub(super) fn is_reparse(m: &Metadata) -> bool {
    m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Ends the `daemon-stop` thread on drop, without calling the callback.
pub(super) struct StopInner {
    cancel: Arc<std::os::windows::io::OwnedHandle>,
}

impl Drop for StopInner {
    fn drop(&mut self) {
        let _ = sys::set_event(&self.cancel);
    }
}

pub(super) fn listen_for_stop(
    instance: &str,
    on_stop: Box<dyn FnOnce() + Send>,
) -> io::Result<(String, StopInner)> {
    let name = super::stop_event_name(instance);
    let user = sys::user_sid()?;
    // All event rights (0x1F0003) for the account only.
    let event = sys::create_named_event(&name, &format!("D:P(A;;0x1F0003;;;{user})"))?;
    let cancel = Arc::new(sys::create_event()?);
    let thread_cancel = cancel.clone();
    std::thread::Builder::new()
        .name("daemon-stop".into())
        .spawn(move || {
            // No timeout: the thread ends with the process or when the listener drops.
            if let Ok(0) = sys::wait_either(&event, &thread_cancel) {
                on_stop();
            }
        })?;
    Ok((name, StopInner { cancel }))
}

// ---- Detaching (design §2.10, §2.12) ----

pub(super) fn detach_session() -> io::Result<()> {
    Ok(())
}

pub(super) fn leave_launchd_job() -> io::Result<()> {
    Ok(())
}

pub(super) fn euid() -> u32 {
    0
}

pub(super) fn current_umask() -> u32 {
    0
}

pub(super) fn launchctl(_args: &[&OsStr], _deadline: Instant) -> io::Result<LaunchctlOutput> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "launchctl is macOS only",
    ))
}

/// `DETACHED_PROCESS`, `CREATE_NEW_PROCESS_GROUP`, `CREATE_BREAKAWAY_FROM_JOB` (`winbase.h`).
const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

/// The creation flags of a detached child.
#[must_use]
pub fn detached_flags(breakaway: Breakaway) -> u32 {
    let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    match breakaway {
        Breakaway::NotRequested => base,
        Breakaway::Explicit => base | CREATE_BREAKAWAY_FROM_JOB,
    }
}

pub(super) struct DetachedInner {
    process: OwnedHandle,
    pid: u32,
}

/// `CreateProcessW` through `windows::sys::create_detached`: the null device for stdin and
/// stderr, a pipe for stdout whose read end `ready` takes, and only those handles inherited.
pub(super) fn spawn_detached(
    exe: &Path,
    args: &[OsString],
    cwd: &Path,
    breakaway: Breakaway,
    ready: &mut ReadyInner,
    deadline: Instant,
) -> io::Result<DetachedInner> {
    use crate::windows::cmdline;
    let dir = detached_cwd(cwd)?;
    let (reader, writer) = std::io::pipe()?;
    let null = OpenOptions::new().read(true).write(true).open("NUL")?;
    let mut argv = vec![exe.as_os_str().to_owned()];
    argv.extend(args.iter().cloned());
    let app = cmdline::wide_nul(exe.as_os_str());
    let mut line = cmdline::command_line(&argv);
    let created = super::spawn_before(deadline, |held| {
        sys::create_detached(
            held,
            &app,
            &mut line,
            &dir,
            detached_flags(breakaway),
            [null.as_handle(), writer.as_handle(), null.as_handle()],
        )
    })
    .ok_or_else(|| super::timed_out("the daemon's launch"))??;
    // This process's copies of the child's ends close here; the child holds its own.
    drop(writer);
    drop(null);
    ready_attach(ready, reader);
    Ok(DetachedInner {
        process: created.process,
        pid: created.pid,
    })
}

/// The child's working directory as `CreateProcessW` takes it: without a verbatim prefix
/// when that names the same directory, as the PTY backend passes it
/// (`docs/design/pty-windows.md`).
fn detached_cwd(cwd: &Path) -> io::Result<Vec<u16>> {
    crate::windows::process_cwd(cwd).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the verbatim working directory names another directory without its prefix",
        )
    })
}

pub(super) fn detached_id(inner: &DetachedInner) -> u32 {
    inner.pid
}

pub(super) fn detached_exit_before(
    inner: &mut DetachedInner,
    deadline: Instant,
) -> io::Result<Option<String>> {
    super::receive_before(deadline, |left| {
        // Rounded up, so a wait shorter than a millisecond still blocks.
        let ms = u32::try_from(left.as_nanos().div_ceil(1_000_000)).unwrap_or(u32::MAX);
        if sys::wait(inner.process.as_handle(), ms)? {
            Ok(Some(format!(
                "exit code: {}",
                sys::exit_code(&inner.process)?
            )))
        } else {
            Ok(None)
        }
    })
}

// ---- Readiness channel: the child's stdout pipe ----

pub(super) struct ReadyInner {
    rx: Option<mpsc::Receiver<io::Result<ReadyRead>>>,
}

pub(super) fn ready_dir(_inner: &ReadyInner) -> Option<&Path> {
    None
}

/// The longest verdict line the starter accepts.
const MAX_READY_LINE: u64 = 4096;

/// Reads the child's stdout on a thread until its first line or its end.
fn ready_attach(inner: &mut ReadyInner, stdout: std::io::PipeReader) {
    let (tx, rx) = mpsc::channel();
    // Ends at the line, at the pipe's end, or with the process (a starter that timed out).
    std::thread::spawn(move || {
        let mut line = Vec::new();
        let r = BufReader::new(stdout)
            .take(MAX_READY_LINE)
            .read_until(b'\n', &mut line);
        let out = match r {
            Ok(_) if line.last() == Some(&b'\n') => {
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                Ok(ReadyRead::Line(String::from_utf8_lossy(&line).into_owned()))
            }
            Ok(_) => Ok(ReadyRead::Eof),
            // The child closed its end (ERROR_BROKEN_PIPE) is the end of the channel.
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(ReadyRead::Eof),
            Err(e) => Err(e),
        };
        let _ = tx.send(out);
    });
    inner.rx = Some(rx);
}

pub(super) fn ready_channel_in(_temp: &Path) -> io::Result<(ReadyInner, ReadyArg)> {
    Ok((ReadyInner { rx: None }, ReadyArg::Stdout))
}

pub(super) fn ready_read(
    inner: &mut ReadyInner,
    deadline: Instant,
    _child: Option<&mut DetachedInner>,
) -> io::Result<ReadyRead> {
    let Some(rx) = &inner.rx else {
        return Err(io::Error::other("the child's stdout was not attached"));
    };
    match super::recv_before(rx, deadline) {
        super::Received::Got(r) => {
            inner.rx = None;
            r
        }
        super::Received::Closed => {
            inner.rx = None;
            Ok(ReadyRead::Eof)
        }
        super::Received::TimedOut => Ok(ReadyRead::TimedOut),
    }
}

pub(super) fn ready_write_plist(_inner: &mut ReadyInner, _contents: &[u8]) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the launchd plist is macOS only",
    ))
}

pub(super) struct ReadyWriterInner(File);

pub(super) fn ready_writer(arg: &ReadyArg) -> io::Result<ReadyWriterInner> {
    match arg {
        ReadyArg::Stdout => Ok(ReadyWriterInner(File::from(sys::take_std_output()?))),
        ReadyArg::Socket(_) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the readiness channel on Windows is stdout",
        )),
    }
}

pub(super) fn ready_write(inner: &mut ReadyWriterInner, line: &str) -> io::Result<()> {
    inner.0.write_all(line.as_bytes())
}

pub(super) fn ready_close(inner: ReadyWriterInner) {
    // Dropping the file closes the pipe handle.
    drop(inner);
}

// ---- Parent job (design §2.12) ----

/// The job limit flags (`winnt.h`).
const KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
const BREAKAWAY_OK: u32 = 0x0000_0800;
const SILENT_BREAKAWAY_OK: u32 = 0x0000_1000;

/// The [`JobState`] of `IsProcessInJob` and the immediate job's `LimitFlags`.
#[must_use]
pub fn job_state_of(flags: Option<u32>) -> JobState {
    match flags {
        None => JobState::NONE,
        Some(f) => JobState {
            in_job: true,
            immediate: Some(JobLimits {
                kill_on_close: f & KILL_ON_JOB_CLOSE != 0,
                breakaway_ok: f & BREAKAWAY_OK != 0,
                silent_breakaway_ok: f & SILENT_BREAKAWAY_OK != 0,
            }),
        },
    }
}

pub(super) fn job_state() -> io::Result<JobState> {
    Ok(job_state_of(sys::own_job_flags()?))
}

pub(super) fn has_console() -> Option<bool> {
    Some(sys::has_console_window())
}

// ---- Stop ----

/// Windows sets the event first; a missing event is checked against the identity after.
pub(super) const CHECK_BEFORE_DELIVERY: bool = false;
/// `ERROR_FILE_NOT_FOUND`: no event of that name.
pub(super) const DELIVERY_GONE: i32 = 2;
/// `ERROR_ACCESS_DENIED`.
pub(super) const DELIVERY_DENIED: i32 = 5;

/// The opened stop event.
pub(super) struct StopDelivery(OwnedHandle);

pub(super) fn open_stop(target: &StopTarget) -> io::Result<StopDelivery> {
    Ok(StopDelivery(sys::open_event_modify(
        &super::stop_event_name(&target.instance),
    )?))
}

pub(super) fn fire_stop(d: &StopDelivery) -> io::Result<()> {
    sys::set_event(&d.0)
}

// ---- Browser ----

pub(super) fn open_url(url: &str) -> io::Result<()> {
    sys::shell_open(url)
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::System::Threading::{PROCESS_SET_QUOTA, PROCESS_TERMINATE};

    use super::*;

    const USER: &str = "S-1-5-21-1-2-3-1001";

    fn allow(sid: &str) -> AceInfo {
        AceInfo {
            ace_type: ACE_ALLOWED,
            flags: 0,
            sid: Some(sid.to_owned()),
        }
    }

    /// PR8-CL-2: a reparse point is a link whatever it points to, and a file that is a
    /// directory symlink is not `NotOfKind`.
    #[test]
    fn classify_entry() {
        const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
        let file_link = FILE_ATTRIBUTE_REPARSE_POINT;
        assert_eq!(
            super::classify_entry(file_link, IO_REPARSE_TAG_SYMLINK, EntryKind::File),
            Some(Privacy::Link)
        );
        let dir_link = FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY;
        assert_eq!(
            super::classify_entry(dir_link, IO_REPARSE_TAG_SYMLINK, EntryKind::File),
            Some(Privacy::Link)
        );
        assert_eq!(
            super::classify_entry(FILE_ATTRIBUTE_DIRECTORY, 0, EntryKind::File),
            Some(Privacy::NotOfKind)
        );
        assert_eq!(super::classify_entry(0x80, 0, EntryKind::File), None);
        assert_eq!(
            super::classify_entry(FILE_ATTRIBUTE_DIRECTORY, 0, EntryKind::Directory),
            None
        );
    }

    #[test]
    fn descriptor_judgement() {
        let private = [allow(USER), allow(SYSTEM), allow(ADMINISTRATORS)];
        assert_eq!(
            judge_descriptor(USER, Some(&private), USER, EntryKind::File),
            Privacy::Private
        );
        assert_eq!(
            judge_descriptor(ADMINISTRATORS, Some(&private), USER, EntryKind::File),
            Privacy::NotOwned
        );
        assert_eq!(
            judge_descriptor(USER, None, USER, EntryKind::File),
            Privacy::AclGrantsOthers
        );
        let users = [allow(USER), allow("S-1-5-32-545")];
        assert_eq!(
            judge_descriptor(USER, Some(&users), USER, EntryKind::Directory),
            Privacy::AclGrantsOthers
        );
        let inherit_only = |sid: &str| AceInfo {
            ace_type: ACE_ALLOWED,
            flags: INHERIT_ONLY_ACE,
            sid: Some(sid.to_owned()),
        };
        let creator = [
            allow(USER),
            inherit_only(CREATOR_OWNER),
            allow(OWNER_RIGHTS),
        ];
        assert_eq!(
            judge_descriptor(USER, Some(&creator), USER, EntryKind::Directory),
            Privacy::Private
        );
        // `CREATOR OWNER` on an ACE that applies to the directory itself is not private.
        let effective = [allow(USER), allow(CREATOR_OWNER)];
        assert_eq!(
            judge_descriptor(USER, Some(&effective), USER, EntryKind::Directory),
            Privacy::AclGrantsOthers
        );
        assert_eq!(
            judge_descriptor(USER, Some(&creator), USER, EntryKind::File),
            Privacy::AclGrantsOthers
        );
        let deny_everyone = [
            allow(USER),
            AceInfo {
                ace_type: ACE_DENIED,
                flags: 0,
                sid: Some("S-1-1-0".to_owned()),
            },
        ];
        assert_eq!(
            judge_descriptor(USER, Some(&deny_everyone), USER, EntryKind::File),
            Privacy::Private
        );
    }

    /// The created directory and file read back private, with the user as owner.
    #[test]
    fn created_entries_read_back_private() {
        let base = sys::local_app_data().unwrap();
        let tmp = base.join(format!("chaldea-host-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        create_private_dir(&tmp).unwrap();
        assert_eq!(
            privacy(&tmp, EntryKind::Directory).unwrap(),
            Privacy::Private
        );
        let f = super::super::create_private_file(&tmp.join("secret")).unwrap();
        assert_eq!(handle_privacy(&f).unwrap(), Privacy::Private);
        drop(f);
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// The stop event's name is global, a second listener of the same instance is refused,
    /// and setting the event runs the callback once.
    #[test]
    fn stop_event_round_trip() {
        let instance = uuid_like();
        let (tx, rx) = std::sync::mpsc::channel();
        let l = super::super::listen_for_stop(&instance, Box::new(move || tx.send(()).unwrap()))
            .unwrap();
        assert!(
            l.name().starts_with("Global\\chaldea-stop-"),
            "{}",
            l.name()
        );
        let dup = super::super::listen_for_stop(&instance, Box::new(|| {}));
        assert_eq!(dup.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        let h = sys::open_event_modify(l.name()).unwrap();
        sys::set_event(&h).unwrap();
        rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    }

    /// The helper role of this module's job tests (set only in the helper's environment).
    const HELPER: &str = "CHALDEA_HOST_JOB_HELPER";

    /// Runs this test binary's `test` as a helper with `role`, `flags` as its creation flags,
    /// and `before_go` applied to its pid before it is told to go (stdin); returns its `@@ `
    /// report. Every wait is bounded (60 s); the helper is killed past it.
    fn helper(test: &str, role: &str, flags: u32, before_go: impl FnOnce(u32)) -> String {
        let (status, out) = helper_run(test, role, flags, before_go);
        assert!(status.success(), "helper {role}: {status:?}\n{out}");
        out.lines()
            .find_map(|l| l.find("@@ ").map(|i| l[i + 3..].to_owned()))
            .unwrap_or_else(|| panic!("no report from helper {role}:\n{out}"))
    }

    /// [`helper`]'s run: its exit status and its stdout and stderr, whatever they are.
    fn helper_run(
        test: &str,
        role: &str,
        flags: u32,
        before_go: impl FnOnce(u32),
    ) -> (std::process::ExitStatus, String) {
        use std::io::{Read as _, Write as _};
        use std::os::windows::process::CommandExt as _;
        use std::process::{Command, Stdio};
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([test, "--exact", "--nocapture", "--test-threads=1"])
            .env(HELPER, role)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(flags);
        let mut child = crate::with_spawn_lock(|| cmd.spawn()).unwrap();
        let mut out = child.stdout.take().unwrap();
        let mut err = child.stderr.take().unwrap();
        let reader = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = out.read_to_string(&mut s);
            s
        });
        let err_reader = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = err.read_to_string(&mut s);
            s
        });
        before_go(child.id());
        let _ = child.stdin.take().unwrap().write_all(b"go\n");
        let end = Instant::now() + std::time::Duration::from_secs(60);
        let status = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break s;
            }
            if Instant::now() >= end {
                let _ = child.kill();
                let _ = child.wait();
                panic!("helper {role} did not exit within 60 s");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let out = reader.join().unwrap();
        let err = err_reader.join().unwrap();
        (status, format!("stdout:\n{out}\nstderr:\n{err}"))
    }

    /// The helper side: waits for `go`, then reports this process's job state.
    fn report_job_state() {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        let state = super::job_state().unwrap();
        println!("@@ {state:?}");
    }

    /// The CI sentinel of design §2.12: a process assigned to a job reads that job as its
    /// immediate job, with the limits it was given.
    #[test]
    fn job_state_reads_the_immediate_job() {
        if std::env::var_os(HELPER).is_some() {
            report_job_state();
            return;
        }
        for (flags, want) in [
            (KILL_ON_JOB_CLOSE | BREAKAWAY_OK, (true, true, false)),
            (KILL_ON_JOB_CLOSE | SILENT_BREAKAWAY_OK, (true, false, true)),
            (0, (false, false, false)),
        ] {
            let job = sys::create_job().unwrap();
            sys::set_job_limit(&job, flags).unwrap();
            let got = helper(
                "host::windows::tests::job_state_reads_the_immediate_job",
                "report",
                0,
                |pid| {
                    let p = sys::open_process(pid, PROCESS_SET_QUOTA | PROCESS_TERMINATE).unwrap();
                    sys::assign(&job, &p).unwrap();
                },
            );
            let expected = JobState {
                in_job: true,
                immediate: Some(JobLimits {
                    kill_on_close: want.0,
                    breakaway_ok: want.1,
                    silent_breakaway_ok: want.2,
                }),
            };
            assert_eq!(got, format!("{expected:?}"), "flags {flags:#x}");
        }
    }

    /// A-W1 (design §2.12), the CI preflight: this process's job state allows the starter's
    /// launch, and a helper launched with the flags the starter would choose is in no job.
    #[test]
    fn runner_can_break_away() {
        if std::env::var_os(HELPER).is_some() {
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            println!("@@ in_job={}", sys::in_any_job_now().unwrap());
            return;
        }
        let state = super::job_state();
        eprintln!("A-W1: this test process's job state: {state:?}");
        let state = state.unwrap_or_else(|e| panic!("A-W1: the job state cannot be read: {e}"));
        let breakaway = super::super::breakaway_for(&state).unwrap_or_else(|| {
            panic!("A-W1 does not hold: the immediate job allows no breakaway: {state:?}")
        });
        let flags = detached_flags(breakaway);
        eprintln!("A-W1: launching the helper with flags {flags:#x} ({breakaway:?})");
        let (status, out) = helper_run(
            "host::windows::tests::runner_can_break_away",
            "probe",
            flags,
            |_| {},
        );
        eprintln!("A-W1: helper (flags {flags:#x}) exited {status:?}; its output:\n{out}");
        let report = out
            .lines()
            .find_map(|l| l.find("@@ ").map(|i| l[i + 3..].to_owned()));
        assert!(
            status.success() && report.as_deref() == Some("in_job=false"),
            "A-W1 does not hold: job state {state:?}, breakaway {breakaway:?}, flags {flags:#x}, helper exited {status:?}, report {report:?}\n{out}"
        );
    }

    /// The detached child's working directory loses a verbatim prefix that names the same
    /// directory, and keeps an ordinary path as given.
    #[test]
    fn detached_cwd_strips_a_verbatim_prefix() {
        use crate::windows::cmdline::wide_nul;
        let root = PathBuf::from(std::env::var_os("SystemRoot").unwrap());
        let verbatim = PathBuf::from(format!(r"\\?\{}", root.display()));
        assert_eq!(detached_cwd(&verbatim).unwrap(), wide_nul(root.as_os_str()));
        assert_eq!(detached_cwd(&root).unwrap(), wide_nul(root.as_os_str()));
    }

    fn uuid_like() -> String {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{:032x}", n ^ u128::from(std::process::id()))
    }
}
