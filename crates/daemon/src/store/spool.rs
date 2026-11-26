//! Spooling a value above `MAX_BLOB_BYTES` (`docs/design/event-store.md` §2.5): the
//! producer writes the bytes in fixed-size buffers to a temporary file in the state
//! directory, computing the whole SHA-256, the size, and the preview head as it goes, and
//! hands the writer the file, which inserts it as ordered parts.

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use chaldea_core::MAX_SAFE_INTEGER;
use chaldea_core::payload::{BlobRef, Content};
use sha2::{Digest, Sha256};

use super::content::{
    ContentError, MAX_BLOB_BYTES, PREVIEW_READ_BYTES, Prepared, hex, prepare_content, preview,
};
use super::error::StoreError;
use super::schema::PART_BYTES;
use super::write_tx::BlobInput;

/// The buffer a spool fills before each write to its file.
pub(crate) const SPOOL_BUFFER_BYTES: usize = 64 * 1024;

/// The prefix of every spool file's name in the state directory.
pub const SPOOL_FILE_PREFIX: &str = "chaldea-spool-";

/// A temporary file this process created and owns: closed, then removed, when dropped.
struct SpoolFile {
    path: PathBuf,
    /// Taken only by `Drop`.
    file: Option<Mutex<File>>,
}

impl SpoolFile {
    fn create(dir: &Path) -> io::Result<SpoolFile> {
        let path = dir.join(format!(
            "{SPOOL_FILE_PREFIX}{}.tmp",
            uuid::Uuid::new_v4().simple()
        ));
        // Owner-only like the state directory's other files (mode 0600; on Windows a
        // protected DACL), created new and never through a link.
        let file = chaldea_pty_runtime::host::create_private_file(&path)?;
        Ok(SpoolFile {
            path,
            file: Some(Mutex::new(file)),
        })
    }

    fn lock(&self) -> io::Result<MutexGuard<'_, File>> {
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| io::Error::other("the spool file is closed"))?;
        Ok(super::lock(file))
    }

    fn with<T>(&self, f: impl FnOnce(&mut File) -> io::Result<T>) -> io::Result<T> {
        f(&mut *self.lock()?)
    }
}

impl Drop for SpoolFile {
    fn drop(&mut self) {
        // Closed first: Windows does not remove a file with an open handle.
        drop(self.file.take());
        if let Err(e) = std::fs::remove_file(&self.path) {
            tracing::warn!(path = %self.path.display(), error = %e, "removing a blob spool file failed");
        }
    }
}

/// A producer's spool for one value of unknown length. [`BlobSpool::finish`] turns it into
/// a [`Prepared`] like [`prepare_content`]: inline or an in-memory blob at or below
/// `MAX_BLOB_BYTES` (read back from the file), a spooled blob above it. Dropped without
/// `finish`, its file is removed.
pub struct BlobSpool {
    file: SpoolFile,
    buf: Vec<u8>,
    hasher: Sha256,
    size: u64,
    head: Vec<u8>,
}

impl fmt::Debug for BlobSpool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlobSpool")
            .field("path", &self.file.path)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl BlobSpool {
    /// A new spool whose file is created in `dir` (the state directory).
    ///
    /// # Errors
    ///
    /// The file could not be created.
    pub fn create(dir: &Path) -> io::Result<BlobSpool> {
        Ok(BlobSpool {
            file: SpoolFile::create(dir)?,
            buf: Vec::with_capacity(SPOOL_BUFFER_BYTES),
            hasher: Sha256::new(),
            size: 0,
            head: Vec::with_capacity(PREVIEW_READ_BYTES),
        })
    }

    /// The bytes written so far.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Appends `bytes`.
    ///
    /// # Errors
    ///
    /// A write to the file failed, or the value would exceed `MAX_SAFE_INTEGER` bytes.
    pub fn append(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        let total = u64::try_from(bytes.len())
            .ok()
            .and_then(|n| self.size.checked_add(n))
            .filter(|n| *n <= MAX_SAFE_INTEGER)
            .ok_or_else(|| {
                io::Error::other(ContentError::TooLarge(
                    self.size.saturating_add(bytes.len() as u64),
                ))
            })?;
        self.hasher.update(bytes);
        if self.head.len() < PREVIEW_READ_BYTES {
            let take = (PREVIEW_READ_BYTES - self.head.len()).min(bytes.len());
            self.head.extend_from_slice(&bytes[..take]);
        }
        self.size = total;
        while !bytes.is_empty() {
            let take = (SPOOL_BUFFER_BYTES - self.buf.len()).min(bytes.len());
            self.buf.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.buf.len() == SPOOL_BUFFER_BYTES {
                self.flush_buf()?;
            }
        }
        Ok(())
    }

    fn flush_buf(&mut self) -> io::Result<()> {
        let buf = &self.buf;
        self.file.with(|f| f.write_all(buf))?;
        self.buf.clear();
        Ok(())
    }

    /// The prepared content of everything appended, with `media_type` for a blob.
    ///
    /// # Errors
    ///
    /// A file error.
    pub fn finish(mut self, media_type: &str) -> io::Result<Prepared> {
        self.flush_buf()?;
        if self.size <= MAX_BLOB_BYTES as u64 {
            let mut bytes = Vec::with_capacity(usize::try_from(self.size).unwrap_or(0));
            self.file.with(|f| {
                f.seek(SeekFrom::Start(0))?;
                f.read_to_end(&mut bytes)
            })?;
            return prepare_content(bytes, media_type).map_err(io::Error::other);
        }
        let sha256 = hex(&self.hasher.finalize_reset());
        let (preview, preview_lossy) = preview(&self.head);
        let content = Content::Blob {
            blob: BlobRef {
                sha256: sha256.clone(),
                size: self.size,
                media_type: media_type.to_owned(),
            },
            preview,
            preview_lossy,
        };
        Ok(Prepared {
            content,
            blob: Some(BlobInput::Spooled(SpooledBlob {
                file: Arc::new(self.file),
                sha256,
                size: self.size,
            })),
        })
    }
}

impl Write for BlobSpool {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.append(bytes)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A value above `MAX_BLOB_BYTES` in a spool file, with the SHA-256 and size its producer
/// computed; the writer recomputes both from the file as it inserts the parts. Clones share
/// the file, which is removed when the last one drops.
#[derive(Clone)]
pub struct SpooledBlob {
    file: Arc<SpoolFile>,
    sha256: String,
    size: u64,
}

impl fmt::Debug for SpooledBlob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpooledBlob")
            .field("sha256", &self.sha256)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl PartialEq for SpooledBlob {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.file, &other.file)
    }
}

impl Eq for SpooledBlob {}

impl SpooledBlob {
    /// The SHA-256 its producer computed.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// The size its producer counted.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The spool file's path (it exists until the last clone drops).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.file.path
    }

    /// The first `PREVIEW_READ_BYTES` bytes of the file.
    pub(crate) fn head(&self) -> io::Result<Vec<u8>> {
        self.file.with(|f| {
            f.seek(SeekFrom::Start(0))?;
            let mut head = Vec::with_capacity(PREVIEW_READ_BYTES);
            Read::by_ref(f)
                .take(PREVIEW_READ_BYTES as u64)
                .read_to_end(&mut head)?;
            Ok(head)
        })
    }

    /// Reads the file from its start in parts of `PART_BYTES` (the last one shorter), in
    /// one buffer, calling `each(index, bytes)` in order; returns the SHA-256 and the length
    /// of everything read.
    pub(crate) fn for_each_part(
        &self,
        mut each: impl FnMut(u64, &[u8]) -> Result<(), StoreError>,
    ) -> Result<(String, u64), StoreError> {
        let io = |e: io::Error| StoreError::from(ContentError::Spool(e.to_string()));
        let mut file = self.file.lock().map_err(io)?;
        file.seek(SeekFrom::Start(0)).map_err(io)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; PART_BYTES];
        let mut total = 0u64;
        let mut index = 0u64;
        loop {
            let n = fill(&mut file, &mut buf).map_err(io)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            total += n as u64;
            each(index, &buf[..n])?;
            index += 1;
            if n < PART_BYTES {
                break;
            }
        }
        Ok((hex(&hasher.finalize()), total))
    }
}

/// Reads until `buf` is full or the file ends; returns how many bytes were read.
fn fill(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut at = 0;
    while at < buf.len() {
        match file.read(&mut buf[at..]) {
            Ok(0) => break,
            Ok(n) => at += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Memory does not grow with the value: the spool appends through one buffer and one
    /// preview head whose capacities never change, and the writer reads every part into one
    /// reused buffer of at most `PART_BYTES`, for a value of 64 MiB + 1 and one of 192 MiB.
    #[test]
    fn buffers_stay_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let chunk: Vec<u8> = (0..(3usize << 20) + 17)
            .map(|i| u8::try_from(i % 249).unwrap())
            .collect();
        for len in [MAX_BLOB_BYTES + 1, 3 * MAX_BLOB_BYTES] {
            let mut spool = BlobSpool::create(dir.path()).unwrap();
            let caps = (spool.buf.capacity(), spool.head.capacity());
            let mut at = 0;
            while at < len {
                let n = chunk.len().min(len - at);
                spool.append(&chunk[..n]).unwrap();
                at += n;
            }
            assert_eq!((spool.buf.capacity(), spool.head.capacity()), caps);
            assert!(caps.0 <= SPOOL_BUFFER_BYTES && caps.1 <= PREVIEW_READ_BYTES);
            let p = spool.finish("application/octet-stream").unwrap();
            let Some(BlobInput::Spooled(blob)) = p.blob else {
                panic!("spooled")
            };
            let mut seen = Vec::new();
            let (sha, total) = blob
                .for_each_part(|index, bytes| {
                    seen.push((index, bytes.as_ptr() as usize, bytes.len()));
                    Ok(())
                })
                .unwrap();
            assert_eq!((sha.as_str(), total), (blob.sha256(), len as u64));
            assert_eq!(seen.len(), len.div_ceil(PART_BYTES));
            for (i, (index, ptr, n)) in seen.iter().enumerate() {
                assert_eq!(*index, i as u64);
                assert_eq!(*ptr, seen[0].1, "part {i} is not read into the same buffer");
                assert!(*n <= PART_BYTES);
            }
            let path = blob.path().to_owned();
            drop(blob);
            assert!(!path.exists());
        }
    }
}
