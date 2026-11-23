//! The state directory (design §2.4): where it is, and the creation or adoption that makes it
//! ours: created private, adopted empty, or carrying our marker, and private in every case;
//! an existing directory is never chmodded or re-ACLed.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use chaldea_pty_runtime::host::{self, Access, EntryKind, Privacy};

use super::names;

/// The marker's content.
pub(crate) const MARKER_CONTENT: &[u8] = b"{\"chaldea_state_dir\":1}\n";
/// The prefix of an unfinished marker temporary.
const MARKER_TMP_PREFIX: &str = ".CHALDEA_STATE.tmp-";
/// The environment variable `--state-dir` falls back to (design D14).
pub const STATE_DIR_ENV: &str = "CHALDEA_STATE_DIR";

/// The state directory every subcommand uses: `flag`, else `CHALDEA_STATE_DIR`, else the
/// default (`$HOME/.chaldea` on macOS, `<FOLDERID_LocalAppData>\Chaldea` on Windows), made
/// absolute against the working directory.
///
/// # Errors
///
/// No default base could be found, or the working directory is unreadable.
pub fn resolve(flag: Option<PathBuf>) -> io::Result<PathBuf> {
    let chosen = match flag {
        Some(p) => p,
        None => match std::env::var_os(STATE_DIR_ENV) {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => default_dir()?,
        },
    };
    std::path::absolute(chosen)
}

fn default_dir() -> io::Result<PathBuf> {
    let base = host::default_state_base()?;
    Ok(if cfg!(windows) {
        base.join("Chaldea")
    } else {
        base.join(".chaldea")
    })
}

/// A refusal of the state directory, with the message to print.
#[derive(Debug)]
pub(crate) struct Refused(pub(crate) String);

fn fix_hint(dir: &Path) -> String {
    if cfg!(windows) {
        format!(
            "remove the access entries of other accounts from {}",
            dir.display()
        )
    } else {
        format!(
            "run `chmod 700 {}` and remove its ACL entries",
            dir.display()
        )
    }
}

fn is_link(m: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // FILE_ATTRIBUTE_REPARSE_POINT
        if m.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    m.file_type().is_symlink()
}

/// Creates or adopts `dir` (absolute) and returns its canonical path.
pub(crate) fn prepare(dir: &Path, instance: &str) -> Result<PathBuf, Refused> {
    // Without trailing separators, so the final entry itself is inspected: `dir/` would
    // resolve a link named `dir` to its target.
    let trimmed = without_trailing_separators(dir);
    let dir = trimmed.as_path();
    let refuse = |why: String| {
        Refused(format!(
            "the state directory {} is refused: {why}",
            dir.display()
        ))
    };
    let created = match std::fs::symlink_metadata(dir) {
        Ok(m) if is_link(&m) => return Err(refuse("it is a link".to_owned())),
        Ok(_) => false,
        Err(e) if e.kind() == io::ErrorKind::NotFound => match host::create_private_dir(dir) {
            Ok(()) => true,
            // A concurrent first start created it: check what is there.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => false,
            Err(e) => return Err(refuse(format!("it cannot be created: {e}"))),
        },
        Err(e) => return Err(refuse(format!("it cannot be inspected: {e}"))),
    };
    match host::privacy(dir, EntryKind::Directory) {
        Ok(Privacy::Private) => {}
        Ok(Privacy::Link) => return Err(refuse("it is a link".to_owned())),
        Ok(Privacy::NotOfKind) => return Err(refuse("it is not a directory".to_owned())),
        Ok(p) => {
            return Err(refuse(format!(
                "it is not private to this account ({}); {}",
                p.describe(),
                fix_hint(dir)
            )));
        }
        Err(e) => return Err(refuse(format!("its access cannot be checked: {e}"))),
    }
    if created || is_empty(dir).map_err(|e| refuse(format!("it cannot be listed: {e}")))? {
        publish_marker(dir, instance).map_err(refuse)?;
    } else {
        match validate_marker(dir) {
            Ok(true) => {}
            Ok(false) => {
                return Err(refuse(
                    "it is not empty and is not a Chaldea state directory".to_owned(),
                ));
            }
            Err(e) => return Err(refuse(format!("its marker cannot be read: {e}"))),
        }
    }
    std::fs::canonicalize(dir).map_err(|e| refuse(format!("it cannot be resolved: {e}")))
}

/// `path` without its trailing separators; a root keeps its own, and nothing else changes (a
/// rebuild from components would resolve `..` in a Windows verbatim path). In a verbatim path
/// only `\` is a separator.
pub(crate) fn without_trailing_separators(path: &Path) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut b = path.as_os_str().as_bytes();
        while b.len() > 1
            && b.ends_with(b"/")
            && Path::new(std::ffi::OsStr::from_bytes(b)).parent().is_some()
        {
            b = &b[..b.len() - 1];
        }
        PathBuf::from(std::ffi::OsStr::from_bytes(b))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        // In a verbatim path (`\\?\`) only `\` separates; `/` is part of a name.
        let verbatim = matches!(
            path.components().next(),
            Some(std::path::Component::Prefix(p)) if p.kind().is_verbatim()
        );
        let mut w: Vec<u16> = path.as_os_str().encode_wide().collect();
        let sep = |c: &u16| *c == u16::from(b'\\') || (!verbatim && *c == u16::from(b'/'));
        while w.last().is_some_and(sep)
            && Path::new(&std::ffi::OsString::from_wide(&w))
                .parent()
                .is_some()
        {
            w.pop();
        }
        PathBuf::from(std::ffi::OsString::from_wide(&w))
    }
}

/// Empty, ignoring unfinished marker temporaries (regular files only).
fn is_empty(dir: &Path) -> io::Result<bool> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(MARKER_TMP_PREFIX)
        {
            return Ok(false);
        }
        let m = std::fs::symlink_metadata(entry.path())?;
        if is_link(&m) || !m.file_type().is_file() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether `CHALDEA_STATE` is a regular file with the marker's content (`false` when absent).
fn validate_marker(dir: &Path) -> io::Result<bool> {
    let f = match host::open_no_follow(&dir.join(names::MARKER), Access::Read) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let mut buf = Vec::new();
    f.take(64).read_to_end(&mut buf)?;
    Ok(buf == MARKER_CONTENT)
}

/// A test pause between the marker's temporary and its link (`daemon_start` holds a child
/// there while a concurrent start publishes its own marker); never set in production.
static MARKER_PAUSE: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>> = std::sync::OnceLock::new();

/// Sets the pause between the marker's temporary and its link, once per process (tests of the
/// concurrent first start).
#[doc(hidden)]
pub fn set_marker_pause(pause: Box<dyn Fn() + Send + Sync>) {
    let _ = MARKER_PAUSE.set(pause);
}

/// Publishes the marker without replacing anything: a private temporary, linked to
/// `CHALDEA_STATE` (which fails if the name exists), then the temporary removed. A concurrent
/// start that won leaves a complete marker, which is validated instead.
pub(crate) fn publish_marker(dir: &Path, instance: &str) -> Result<(), String> {
    let tmp = dir.join(format!("{MARKER_TMP_PREFIX}{instance}"));
    // Only a temporary file this call created is ours to remove.
    let mut created = false;
    let written = (|| {
        let mut f = host::create_private_file(&tmp)?;
        created = true;
        f.write_all(MARKER_CONTENT)?;
        f.sync_all()
    })();
    if let Err(e) = written {
        if created {
            let _ = std::fs::remove_file(&tmp);
        }
        return Err(format!("its marker cannot be written: {e}"));
    }
    if let Some(pause) = MARKER_PAUSE.get() {
        pause();
    }
    let linked = std::fs::hard_link(&tmp, dir.join(names::MARKER));
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => match validate_marker(dir) {
            Ok(true) => Ok(()),
            Ok(false) => Err("it carries a marker that is not Chaldea's".to_owned()),
            Err(e) => Err(format!("its marker cannot be read: {e}")),
        },
        Err(e) => Err(format!("its marker cannot be published: {e}")),
    }
}
