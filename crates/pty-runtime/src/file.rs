//! Opening a file another program writes, for reading only when it is a regular file
//! (`docs/design/c1-claude-adapter.md` D13): the transcript tailer's open.
//!
//! The path must be absolute. On Unix the open is non-blocking, so a FIFO or device returns
//! at once and fails the type check; on Windows the device and pipe namespaces are refused
//! before the open. The type and the [`FileId`] are read from the opened handle, and every
//! read goes through that handle.

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// The identity of an open file: device and inode on Unix, volume serial number and 128-bit
/// file id on Windows. Two handles with equal ids name the same file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileId(String);

impl FileId {
    /// The id's text, as the store keeps it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why [`open_regular`] read nothing.
#[derive(Debug)]
pub enum OpenError {
    /// The path is not absolute.
    NotAbsolute,
    /// A Windows device or pipe namespace path (`\\.\`, `\\?\` other than a drive or UNC
    /// share, `\??\`, or the UNC share `pipe`).
    DeviceNamespace,
    /// The opened file is not a regular file (a directory, FIFO, socket, or device).
    NotRegular,
    /// The open or the handle's query failed (`NotFound` before the file exists).
    Io(io::Error),
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenError::NotAbsolute => f.write_str("the path is not absolute"),
            OpenError::DeviceNamespace => f.write_str("a device or pipe namespace path"),
            OpenError::NotRegular => f.write_str("not a regular file"),
            OpenError::Io(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for OpenError {}

impl From<io::Error> for OpenError {
    fn from(e: io::Error) -> Self {
        OpenError::Io(e)
    }
}

/// A regular file open for reading, with the id read from its handle.
#[derive(Debug)]
pub struct RegularFile {
    file: File,
    id: FileId,
}

impl RegularFile {
    /// The id of the open file.
    #[must_use]
    pub fn id(&self) -> &FileId {
        &self.id
    }

    /// The open file's current size, from its handle.
    ///
    /// # Errors
    ///
    /// The handle's query failed.
    pub fn size(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }
}

impl Read for RegularFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Seek for RegularFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}

/// Opens `path` for reading when it is an absolute path to a regular file.
///
/// # Errors
///
/// [`OpenError`]; nothing is read on any error.
pub fn open_regular(path: &Path) -> Result<RegularFile, OpenError> {
    if !path.is_absolute() {
        return Err(OpenError::NotAbsolute);
    }
    imp::open_regular(path)
}

/// Whether the Windows path `text` names the device or pipe namespace: `\\.\…`, the NT
/// namespace `\??\…`, a verbatim `\\?\…` other than a drive (`\\?\C:\`) or a UNC share
/// (`\\?\UNC\`), or the named-pipe share `pipe` of any server (`\\server\pipe\…`,
/// `\\?\UNC\server\pipe\…`), with either separator and in any case.
#[cfg(any(windows, test))]
fn device_namespace(text: &str) -> bool {
    let text = text.replace('/', "\\").to_ascii_lowercase();
    // The share of a UNC path's `server\share\…` part.
    let pipe_share = |unc: &str| {
        unc.split('\\')
            .nth(1)
            .is_some_and(|share| share.trim_end_matches(['.', ' ']) == "pipe")
    };
    if text.starts_with("\\\\.\\") || text.starts_with("\\??\\") {
        return true;
    }
    if let Some(rest) = text.strip_prefix("\\\\?\\") {
        if let Some(unc) = rest.strip_prefix("unc\\") {
            return pipe_share(unc);
        }
        let b = rest.as_bytes();
        return !(b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':');
    }
    text.strip_prefix("\\\\").is_some_and(pipe_share)
}

#[cfg(target_os = "macos")]
mod imp {
    use std::fs::OpenOptions;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::Path;

    use super::{FileId, OpenError, RegularFile};

    pub(super) fn open_regular(path: &Path) -> Result<RegularFile, OpenError> {
        // Non-blocking, so a FIFO or device returns at once and fails the type check below;
        // for a regular file the flag changes nothing. No controlling terminal is taken.
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(path)?;
        let m = file.metadata()?;
        if !m.file_type().is_file() {
            return Err(OpenError::NotRegular);
        }
        let id = FileId(format!("{}:{}", m.dev(), m.ino()));
        Ok(RegularFile { file, id })
    }
}

#[cfg(windows)]
mod imp {
    use std::fs::OpenOptions;
    use std::os::windows::io::AsHandle;
    use std::path::Path;

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    };

    use super::{FileId, OpenError, RegularFile};
    use crate::windows::sys;

    pub(super) fn open_regular(path: &Path) -> Result<RegularFile, OpenError> {
        if super::device_namespace(&path.as_os_str().to_string_lossy()) {
            return Err(OpenError::DeviceNamespace);
        }
        // std shares read, write, and delete, so the writer keeps appending, renaming, and
        // removing the file while it is open here.
        let file = OpenOptions::new().read(true).open(path)?;
        let info = sys::handle_file(file.as_handle())?;
        if !info.disk
            || info.attributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
        {
            return Err(OpenError::NotRegular);
        }
        let id = FileId(format!("{:016x}:{:032x}", info.volume_serial, info.id));
        Ok(RegularFile { file, id })
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    use std::io;
    use std::path::Path;

    use super::{OpenError, RegularFile};

    pub(super) fn open_regular(_path: &Path) -> Result<RegularFile, OpenError> {
        Err(OpenError::Io(io::Error::new(
            io::ErrorKind::Unsupported,
            "not supported on this OS",
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_path_is_refused_before_any_open() {
        assert!(matches!(
            open_regular(Path::new("transcript.jsonl")),
            Err(OpenError::NotAbsolute)
        ));
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn a_regular_file_reads_through_its_handle_with_a_stable_id() {
        let dir = std::env::temp_dir().join(format!("chaldea-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        std::fs::write(&path, b"abc\n").unwrap();
        let mut f = open_regular(&path).unwrap();
        let again = open_regular(&path).unwrap();
        assert_eq!(f.id(), again.id());
        assert_eq!(f.size().unwrap(), 4);
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        assert_eq!(s, "abc\n");
        // Another file at the same path has another id.
        drop(again);
        let moved = dir.join("old.jsonl");
        std::fs::rename(&path, &moved).unwrap();
        std::fs::write(&path, b"abc\n").unwrap();
        assert_ne!(open_regular(&path).unwrap().id(), f.id());
        drop(f);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn a_directory_is_not_regular() {
        let dir = std::env::temp_dir().join(format!("chaldea-file-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let r = open_regular(&dir);
        assert!(
            matches!(r, Err(OpenError::NotRegular | OpenError::Io(_))),
            "{r:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn device_and_pipe_namespaces_are_refused() {
        let refused = [
            r"\\.\pipe\chaldea-none",
            r"\\.\NUL",
            "//./pipe/chaldea-none",
            r"\\?\pipe\chaldea-none",
            r"\\?\GLOBALROOT\Device\Null",
            r"\??\C:\work\t.jsonl",
            r"\\localhost\pipe\chaldea-probe",
            r"\\server\PIPE\x",
            "//server/Pipe/x",
            r"\\server\pipe.\x",
            r"\\?\UNC\server\pipe\x",
            r"\\?\unc\server\PiPe\x",
            "//?/UNC/server/pipe/x",
        ];
        for p in refused {
            assert!(device_namespace(p), "{p}");
        }
        for p in [
            r"\\?\C:\work\t.jsonl",
            r"\\?\UNC\server\share\t.jsonl",
            r"\\server\share\pipe\t.jsonl",
            r"\\server\pipes\t.jsonl",
            r"C:\work\pipe\t.jsonl",
        ] {
            assert!(!device_namespace(p), "{p}");
        }
        #[cfg(windows)]
        for p in refused {
            assert!(
                matches!(
                    open_regular(Path::new(p)),
                    Err(OpenError::DeviceNamespace | OpenError::NotAbsolute)
                ),
                "{p}"
            );
        }
    }
}
