//! The spelling-preserving trim of a path's trailing separators, std only: the state directory
//! (`state_dir`) uses it, and the PWA dist rules (`http/pwa/dist.rs`, which the daemon's
//! `build.rs` also includes) include this file with `#[path]`.

use std::path::{Path, PathBuf};

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
