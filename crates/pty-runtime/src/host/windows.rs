//! `host` on Windows: std for opens (with `FILE_FLAG_OPEN_REPARSE_POINT`) and attributes,
//! `windows::sys` for security descriptors, known folders, and events.

use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsHandle;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, READ_CONTROL,
};

use super::{Access, EntryKind, Privacy, ProcessIdentity, SigchldDisposition, SignalState};
use crate::windows::sys::{self, ACE_ALLOWED, ACE_DENIED, SecurityInfo};

/// `ERROR_INVALID_PARAMETER`: `OpenProcess` of a pid no process has.
const ERROR_INVALID_PARAMETER: i32 = 87;

/// The well-known SIDs of the private trustees (`docs/design/daemon-lifecycle.md` §2.4).
const SYSTEM: &str = "S-1-5-18";
const ADMINISTRATORS: &str = "S-1-5-32-544";
const CREATOR_OWNER: &str = "S-1-3-0";
const OWNER_RIGHTS: &str = "S-1-3-4";

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

pub(super) fn reset_process_signals() -> io::Result<()> {
    Ok(())
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

/// One ACE as the privacy judgement sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AceView {
    pub ace_type: u8,
    pub flags: u8,
    pub sid: Option<String>,
}

fn private_trustee(sid: &str, user: &str, kind: EntryKind) -> bool {
    sid == user
        || sid == SYSTEM
        || sid == ADMINISTRATORS
        || (kind == EntryKind::Directory && (sid == CREATOR_OWNER || sid == OWNER_RIGHTS))
}

/// The privacy judgement of an owner and DACL (`None`: a NULL DACL): owned by `user`, and
/// every allow ACE names a private trustee; deny ACEs pass; allow ACEs of another layout
/// (object, callback) refuse.
#[must_use]
pub fn judge_descriptor(
    owner: &str,
    dacl: Option<&[AceView]>,
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
                if !private_trustee(sid, user, kind) {
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
    let aces: Option<Vec<AceView>> = sec.dacl.as_ref().map(|d| {
        d.iter()
            .map(|a| AceView {
                ace_type: a.ace_type,
                flags: a.flags,
                sid: a.sid.clone(),
            })
            .collect()
    });
    Ok(judge_descriptor(&sec.owner, aces.as_deref(), &user, kind))
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

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str = "S-1-5-21-1-2-3-1001";

    fn allow(sid: &str) -> AceView {
        AceView {
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
        let creator = [allow(USER), allow(CREATOR_OWNER), allow(OWNER_RIGHTS)];
        assert_eq!(
            judge_descriptor(USER, Some(&creator), USER, EntryKind::Directory),
            Privacy::Private
        );
        assert_eq!(
            judge_descriptor(USER, Some(&creator), USER, EntryKind::File),
            Privacy::AclGrantsOthers
        );
        let deny_everyone = [
            allow(USER),
            AceView {
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

    fn uuid_like() -> String {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{:032x}", n ^ u128::from(std::process::id()))
    }
}
