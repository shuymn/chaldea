//! Bytes of unknown length kept for one `Content`: in memory up to `MAX_BLOB_BYTES`, then in
//! a blob spool in the state directory (`docs/design/c1-claude-adapter.md` D14), so a value of
//! any size is kept whole and memory stays bounded.

use std::cell::Cell;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::store::{BlobSpool, MAX_BLOB_BYTES, Prepared, prepare_content};

/// Where an accumulator's spool goes: the state directory.
#[derive(Debug, Clone)]
pub(crate) struct SpoolDir {
    path: PathBuf,
    /// Spool files the directory may hold at once; one more fails to be created as a full
    /// descriptor table would (tests).
    #[cfg(test)]
    pub(crate) limit: Option<usize>,
    /// Counts replayed chunks and sets a stop at each (tests).
    #[cfg(test)]
    pub(crate) replay: Option<ReplayProbe>,
}

/// What a test observes of a spool's replay.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub(crate) struct ReplayProbe {
    pub(crate) chunks: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) stop: std::sync::Arc<AtomicBool>,
}

impl SpoolDir {
    pub(crate) fn new(path: PathBuf) -> SpoolDir {
        SpoolDir {
            path,
            #[cfg(test)]
            limit: None,
            #[cfg(test)]
            replay: None,
        }
    }

    fn create(&self) -> io::Result<BlobSpool> {
        #[cfg(test)]
        if let Some(limit) = self.limit {
            let held = std::fs::read_dir(&self.path)?
                .filter_map(Result::ok)
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with(crate::store::SPOOL_FILE_PREFIX)
                })
                .count();
            if held >= limit {
                return Err(io::Error::other("too many open files (test limit)"));
            }
        }
        BlobSpool::create(&self.path)
    }
}

/// The bytes the accumulators of one line hold in memory together: now and at most.
#[derive(Debug, Clone, Default)]
pub(crate) struct Meter(Rc<(Cell<usize>, Cell<usize>)>);

impl Meter {
    fn change(&self, from: usize, to: usize) {
        let now = self.0.0.get() - from + to;
        self.0.0.set(now);
        self.0.1.set(self.0.1.get().max(now));
    }

    /// The most bytes held at once.
    pub(crate) fn peak(&self) -> usize {
        self.0.1.get()
    }
}

/// Bytes in memory, then, past `MAX_BLOB_BYTES`, in a spool.
pub(crate) struct ByteAcc {
    dir: SpoolDir,
    mem: Vec<u8>,
    spool: Option<BlobSpool>,
    meter: Meter,
    /// The memory reported to the meter.
    counted: usize,
}

impl ByteAcc {
    pub(crate) fn new(dir: SpoolDir, meter: Meter) -> ByteAcc {
        ByteAcc {
            dir,
            mem: Vec::new(),
            spool: None,
            meter,
            counted: 0,
        }
    }

    fn recount(&mut self) {
        let now = self.mem.capacity();
        self.meter.change(self.counted, now);
        self.counted = now;
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
            self.recount();
            return Ok(());
        }
        let mut s = self.dir.create()?;
        s.append(&self.mem)?;
        s.append(bytes)?;
        self.mem = Vec::new();
        self.recount();
        self.spool = Some(s);
        Ok(())
    }

    /// Moves everything `other` holds to the end of this one; a spool is replayed chunk by
    /// chunk until `stop` is set.
    ///
    /// # Errors
    ///
    /// A spool error, or `stop` set during the replay.
    pub(crate) fn append(&mut self, mut other: ByteAcc, stop: &AtomicBool) -> io::Result<()> {
        match other.spool.take() {
            None => self.push(&other.mem),
            Some(s) => s.replay(&mut |b| {
                #[cfg(test)]
                if let Some(p) = &self.dir.replay {
                    p.chunks.fetch_add(1, Ordering::Relaxed);
                    p.stop.store(true, Ordering::Release);
                }
                if stop.load(Ordering::Acquire) {
                    return Err(io::Error::other("stopped"));
                }
                self.push(b)
            }),
        }
    }

    /// The bytes as `Content` with `media_type` for a blob.
    ///
    /// # Errors
    ///
    /// A spool error.
    pub(crate) fn finish(mut self, media_type: &str) -> io::Result<Prepared> {
        match self.spool.take() {
            None => {
                let mem = std::mem::take(&mut self.mem);
                prepare_content(mem, media_type).map_err(io::Error::other)
            }
            Some(s) => s.finish(media_type),
        }
    }
}

impl Drop for ByteAcc {
    fn drop(&mut self) {
        self.meter.change(self.counted, 0);
    }
}
