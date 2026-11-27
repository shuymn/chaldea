//! Bytes of unknown length kept for one `Content`: in memory up to `MAX_BLOB_BYTES`, then in
//! a blob spool in the state directory (`docs/design/c1-claude-adapter.md` D14), so a value of
//! any size is kept whole and memory stays bounded.

use std::io;
use std::path::{Path, PathBuf};

use crate::store::{BlobSpool, MAX_BLOB_BYTES, Prepared, prepare_content};

use super::json::StrSink;

/// Where an accumulator's spool goes: the state directory.
#[derive(Debug, Clone)]
pub(crate) struct SpoolDir(pub(crate) PathBuf);

impl SpoolDir {
    fn create(&self) -> io::Result<BlobSpool> {
        BlobSpool::create(Path::new(&self.0))
    }
}

/// Bytes in memory, then, past `MAX_BLOB_BYTES`, in a spool.
pub(crate) struct ByteAcc {
    dir: SpoolDir,
    mem: Vec<u8>,
    spool: Option<BlobSpool>,
    /// The most bytes this accumulator held in memory at once.
    pub(crate) peak_mem: usize,
}

impl ByteAcc {
    pub(crate) fn new(dir: SpoolDir) -> ByteAcc {
        ByteAcc {
            dir,
            mem: Vec::new(),
            spool: None,
            peak_mem: 0,
        }
    }

    /// Appends `bytes`; the first append past `MAX_BLOB_BYTES` moves the memory to a spool.
    ///
    /// # Errors
    ///
    /// The spool could not be created or written.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> io::Result<()> {
        if let Some(s) = &mut self.spool {
            return s.append(bytes);
        }
        let need = self.mem.len() + bytes.len();
        if need <= MAX_BLOB_BYTES {
            // Grown by doubling, capped at the limit, so memory never exceeds it.
            if need > self.mem.capacity() {
                let target = (self.mem.capacity() * 2).max(need).min(MAX_BLOB_BYTES);
                self.mem.reserve_exact(target - self.mem.len());
            }
            self.mem.extend_from_slice(bytes);
            self.peak_mem = self.peak_mem.max(self.mem.capacity());
            return Ok(());
        }
        let mut s = self.dir.create()?;
        s.append(&self.mem)?;
        s.append(bytes)?;
        self.mem = Vec::new();
        self.spool = Some(s);
        Ok(())
    }

    /// Moves everything `other` holds to the end of this one.
    ///
    /// # Errors
    ///
    /// A spool error.
    pub(crate) fn append(&mut self, other: ByteAcc) -> io::Result<()> {
        match other.spool {
            None => self.push(&other.mem),
            Some(s) => s.replay(&mut |b| self.push(b)),
        }
    }

    /// The bytes as `Content` with `media_type` for a blob.
    ///
    /// # Errors
    ///
    /// A spool error.
    pub(crate) fn finish(self, media_type: &str) -> io::Result<Prepared> {
        match self.spool {
            None => prepare_content(self.mem, media_type).map_err(io::Error::other),
            Some(s) => s.finish(media_type),
        }
    }
}

impl StrSink for ByteAcc {
    fn push(&mut self, bytes: &[u8]) -> io::Result<()> {
        ByteAcc::push(self, bytes)
    }
}
