//! One tailer's synchronous core (`docs/design/c1-claude-adapter.md` D10, D13): the open and
//! its continuity check, the line reads, the checks before a commit, and the batches. It runs
//! on a blocking task; the store is reached only through a [`Backend`].
//!
//! Continuity is `sha256:<hex>` of the file's bytes `[0, offset)`. While the handle stays
//! open a running digest is extended with the bytes read. At every open the handle's
//! `file_id` and the path are compared with the stored cursor's, and the whole prefix is
//! read again and checked against the stored digest. Every poll, before any cursor advance,
//! the path must still name the handle's file and the handle's size must not be below the
//! offset the batch advances to (nor the read position), and the [`TAIL_WINDOW`] bytes before
//! the read position must still be those read (Claude rewrites its own tail when it removes a
//! discarded message). A failed check discards the batch
//! and starts a new generation at offset 0; reading starts again from 0. The same check runs
//! again after each commit: a truncate or replace between the check and the commit is
//! detected there (or at the next poll's check), the committed batch stays in history, and
//! the next poll starts a new generation; a check that cannot open the path drops the handle,
//! so the next poll opens and verifies again. A deeper rewrite in place that keeps the size
//! is invisible to these checks: the whole prefix is read again once at the final read, and
//! at a poll that sees the modification time move back. A detected break is kept until a new
//! generation is stored, so a failed generation commit does not let a later open continue
//! the old one. A stop is honoured at every chunk read or replayed and before every commit.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chaldea_core::event::TypedEvent;
use chaldea_core::ids::{Agent, LaunchId, ThreadId, Timestamp};
use chaldea_core::payload::{AssistantMessage, RawUnknown, RawUnknownReason, SourceKind};
use chaldea_pty_runtime::file::{OpenError, RegularFile, open_regular};
use sha2::{Digest, Sha256};

use super::acc::{ByteAcc, Meter, SpoolDir};
use super::claude::{Emit, LineParser, Outcome};
use crate::store::{
    BlobInput, Continuity, CursorPos, NewGeneration, ReadPos, TranscriptBatch, TranscriptCursor,
    TranscriptLine,
};

/// The read buffer.
const CHUNK: usize = 64 * 1024;
/// A batch closes after this many lines, or once the content it holds in memory reaches
/// `MAX_BLOB_BYTES`; the next batch follows at once.
pub(crate) const BATCH_LINES: usize = 1024;
const BATCH_MEMORY: usize = crate::store::MAX_BLOB_BYTES;
/// After `launch_exited`, a final read that fails this many polls in a row ends the tailer.
pub(crate) const FINAL_TRIES: u32 = 3;

const TEXT: &str = "text/plain; charset=utf-8";
const JSON: &str = "application/json";
const OCTET: &str = "application/octet-stream";

/// The binding a tailer reads for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Binding {
    pub(crate) thread_id: ThreadId,
    pub(crate) launch_id: LaunchId,
    pub(crate) session_id: String,
    pub(crate) path: String,
    pub(crate) agent: Agent,
    pub(crate) cli_version: Option<String>,
}

/// What a poll reads from the store first, in one snapshot.
#[derive(Debug, Clone)]
pub(crate) struct View {
    /// The launch is still the thread's current one, bound to this session and path.
    pub(crate) current: bool,
    /// Its `launch_exited` is stored: this poll is the final read.
    pub(crate) exited: bool,
    pub(crate) cursor: Option<TranscriptCursor>,
}

/// A batch's result.
#[derive(Debug)]
pub(crate) enum Commit {
    Done,
    StaleCursor,
    StaleBinding,
    /// Not stored (busy, queue full, shutdown): read again at the next poll.
    Failed(String),
}

/// The store side of a tailer.
pub(crate) trait Backend {
    /// # Errors
    ///
    /// The read failed.
    fn view(&self, b: &Binding) -> Result<View, String>;
    fn commit(&self, batch: TranscriptBatch) -> Commit;
    /// After a poll's read, before its checks (a test seam).
    fn after_read(&self) {}
}

/// What the driver does after a poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    /// Poll again now (a full batch, a new generation, a refreshed cursor).
    Again,
    /// Poll at the next period.
    Wait,
    /// The tailer ends.
    End(String),
}

/// What tests observe of a tailer.
#[derive(Debug, Default, Clone)]
pub(crate) struct Stats {
    /// The most bytes one line held in memory at once (its raw bytes and its text).
    pub(crate) peak_line_memory: usize,
    /// Batches discarded by the checks before a commit.
    pub(crate) discarded: u32,
    /// Breaks found by the check after a commit.
    pub(crate) late: u32,
    /// Final reads whose prefix check did not complete, logged as unverified.
    pub(crate) unverified: u32,
}

/// The stored cursor, as far as the tailer compares it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cursor {
    generation: u64,
    path: String,
    file_id: String,
    read_offset: u64,
    continuity: Continuity,
}

impl From<TranscriptCursor> for Cursor {
    fn from(c: TranscriptCursor) -> Self {
        Cursor {
            generation: c.generation,
            path: c.path,
            file_id: c.file_id,
            read_offset: c.read_offset,
            continuity: c.read_continuity,
        }
    }
}

impl Cursor {
    fn pos(&self) -> CursorPos {
        CursorPos {
            generation: self.generation,
            path: self.path.clone(),
            file_id: self.file_id.clone(),
            read_offset: self.read_offset,
        }
    }
}

/// The bytes before the read position compared at every poll: Claude 2.1.331 removes a
/// discarded message (a tombstone) by truncating its transcript at that line and writing the
/// rest back in place, and only within the file's last 64 KiB (`performRemoveByUuid`,
/// `kw=65536`); since the read position is never past the end, such a rewrite of read bytes
/// lies within this many bytes before it.
pub(crate) const TAIL_WINDOW: usize = 64 * 1024;

/// The last [`TAIL_WINDOW`] bytes before a position, as read. Up to twice that is held, so
/// a push trims at most once per window of bytes pushed.
#[derive(Debug, Clone, Default)]
struct Recent(Vec<u8>);

impl Recent {
    fn push(&mut self, bytes: &[u8]) {
        let bytes = &bytes[bytes.len().saturating_sub(TAIL_WINDOW)..];
        if self.0.len() + bytes.len() > 2 * TAIL_WINDOW {
            let keep = TAIL_WINDOW - bytes.len();
            self.0.drain(..self.0.len() - keep);
        }
        self.0.extend_from_slice(bytes);
    }

    fn bytes(&self) -> &[u8] {
        &self.0[self.0.len().saturating_sub(TAIL_WINDOW)..]
    }
}

/// The open handle, the running digest of `[0, pos)`, the bytes just before `pos`, and the
/// latest modification time seen.
struct OpenFile {
    file: RegularFile,
    digest: Sha256,
    pos: u64,
    recent: Recent,
    mtime: Option<std::time::SystemTime>,
}

/// Why a poll could not finish; it is retried at the next one.
#[derive(Debug)]
enum Fail {
    Io(io::Error),
    Refused(&'static str),
    Store(String),
    Stopped,
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fail::Io(e) => write!(f, "{e}"),
            Fail::Refused(why) => f.write_str(why),
            Fail::Store(e) => write!(f, "store: {e}"),
            Fail::Stopped => f.write_str("stopped"),
        }
    }
}

impl From<io::Error> for Fail {
    fn from(e: io::Error) -> Self {
        Fail::Io(e)
    }
}

/// `sha256:<hex>` of a digest's state.
pub(crate) fn continuity(d: &Sha256) -> Continuity {
    use std::fmt::Write;
    let mut s = String::from("sha256:");
    for b in d.clone().finalize() {
        let _ = write!(s, "{b:02x}");
    }
    Continuity(s)
}

/// One tailer's state between polls.
pub(crate) struct Tail {
    b: Binding,
    dir: SpoolDir,
    stop: Arc<AtomicBool>,
    cursor: Option<Cursor>,
    open: Option<OpenFile>,
    /// The generation (0: none) whose continuity a check disproved: until a new generation
    /// is stored, the next open starts one whatever its continuity.
    disproved: Option<u64>,
    final_failures: u32,
    /// The last poll's view: the launch's exit stored or not; `None` when the view failed.
    exited: Option<bool>,
    /// The final read checked the whole prefix against the cursor's digest.
    prefix_checked: bool,
    /// The next prefix check sets the stop as it starts (tests).
    #[cfg(test)]
    stop_at_prefix_check: bool,
    /// The last failure logged, so a repeated one is logged once.
    last_failure: Option<String>,
    pub(crate) stats: Stats,
}

impl Tail {
    pub(crate) fn new(b: Binding, dir: SpoolDir, stop: Arc<AtomicBool>) -> Tail {
        Tail {
            b,
            dir,
            stop,
            cursor: None,
            open: None,
            disproved: None,
            final_failures: 0,
            exited: None,
            prefix_checked: false,
            #[cfg(test)]
            stop_at_prefix_check: false,
            last_failure: None,
            stats: Stats::default(),
        }
    }

    /// One poll.
    pub(crate) fn poll(&mut self, be: &dyn Backend) -> Step {
        let view = match be.view(&self.b) {
            Ok(v) => v,
            Err(e) => {
                self.exited = None;
                return self.failed(&Fail::Store(e), false);
            }
        };
        self.exited = Some(view.exited);
        if !view.current {
            return Step::End("the launch is no longer the bound one".into());
        }
        let stored = view.cursor.map(Cursor::from);
        if self
            .disproved
            .is_some_and(|g| stored.as_ref().map_or(0, |c| c.generation) != g)
        {
            // Another job stored a generation.
            self.disproved = None;
        }
        if stored != self.cursor {
            // A cursor this tailer did not write (the first poll, a rebuild, another
            // tailer): open again and verify against it.
            self.cursor = stored;
            self.open = None;
        }
        match self.read(be, view.exited) {
            Ok(step) => {
                self.final_failures = 0;
                self.last_failure = None;
                step
            }
            Err(Fail::Stopped) => Step::End("stopped".into()),
            Err(f) => self.failed(&f, view.exited),
        }
    }

    /// Whether the last poll's view had the launch's exit stored (its read was a final
    /// read); `None` when the view could not be read.
    pub(crate) fn exited(&self) -> Option<bool> {
        self.exited
    }

    /// Whether the final read checked the whole prefix.
    pub(crate) fn prefix_checked(&self) -> bool {
        self.prefix_checked
    }

    /// Records that the final read's prefix check did not complete.
    pub(crate) fn unverified(&mut self, why: &str) {
        self.stats.unverified += 1;
        tracing::warn!(thread = %self.b.thread_id, launch = %self.b.launch_id, why, "the transcript's continuity is unverified: the final read's prefix check did not complete");
    }

    /// Makes the next prefix check set the stop as it starts (tests).
    #[cfg(test)]
    pub(crate) fn stop_at_prefix_check(&mut self) {
        self.stop_at_prefix_check = true;
    }

    fn failed(&mut self, f: &Fail, last: bool) -> Step {
        let text = f.to_string();
        if self.last_failure.as_deref() != Some(text.as_str()) {
            tracing::warn!(thread = %self.b.thread_id, launch = %self.b.launch_id, error = %text, "the transcript read failed; it is retried at the next poll");
            self.last_failure = Some(text.clone());
        }
        if last {
            self.final_failures += 1;
            if self.final_failures >= FINAL_TRIES {
                if !self.prefix_checked {
                    self.unverified("the final read failed before its prefix check");
                }
                tracing::warn!(thread = %self.b.thread_id, launch = %self.b.launch_id, error = %text, "the final transcript read failed {FINAL_TRIES} times; the tailer ends and the bytes stay in the file");
                return Step::End(format!("the final read failed: {text}"));
            }
        }
        Step::Wait
    }

    fn stopped(&self) -> Result<(), Fail> {
        if self.stop.load(Ordering::Acquire) {
            Err(Fail::Stopped)
        } else {
            Ok(())
        }
    }

    fn open_path(&self) -> Result<Result<RegularFile, Step>, Fail> {
        match open_regular(std::path::Path::new(&self.b.path)) {
            Ok(f) => Ok(Ok(f)),
            Err(OpenError::NotAbsolute) => {
                tracing::warn!(thread = %self.b.thread_id, launch = %self.b.launch_id, "the transcript path is not absolute; nothing is read");
                Ok(Err(Step::End("the path is not absolute".into())))
            }
            Err(OpenError::DeviceNamespace) => {
                tracing::warn!(thread = %self.b.thread_id, launch = %self.b.launch_id, "the transcript path names a device or pipe; nothing is read");
                Ok(Err(Step::End("a device or pipe path".into())))
            }
            Err(OpenError::NotRegular) => Err(Fail::Refused("not a regular file")),
            Err(OpenError::Io(e)) => Err(Fail::Io(e)),
        }
    }

    fn read(&mut self, be: &dyn Backend, last: bool) -> Result<Step, Fail> {
        self.stopped()?;
        if self.open.is_none() {
            let f = match self.open_path()? {
                Ok(f) => f,
                Err(end) => return Ok(end),
            };
            if self.disproved.is_some() {
                return self.new_generation(be, f);
            }
            let verified = self.verify(f);
            if last && matches!(verified, Err(Fail::Stopped)) {
                self.unverified("the final read stopped during the prefix check at its open");
            }
            match verified? {
                Ok(open) => {
                    self.prefix_checked |= last;
                    self.open = Some(open);
                }
                Err(f) => {
                    self.disprove();
                    return self.new_generation(be, f);
                }
            }
        }
        let Some(mut open) = self.open.take() else {
            return Ok(Step::Wait);
        };
        // The whole prefix is checked again once at the final read, and whenever the file's
        // modification time moved back; normal appends keep the light checks.
        let mtime = open.file.modified().ok();
        let moved_back = matches!((open.mtime, mtime), (Some(was), Some(now)) if now < was);
        open.mtime = mtime.or(open.mtime);
        if (last && !self.prefix_checked) || moved_back {
            match self.prefix_holds(&mut open) {
                Ok(true) => self.prefix_checked |= last,
                Ok(false) => {
                    drop(open);
                    self.disprove();
                    tracing::info!(thread = %self.b.thread_id, launch = %self.b.launch_id, moved_back, "the transcript's read prefix changed in place; a new generation starts");
                    return self.restart(be);
                }
                Err(e) => {
                    if matches!(e, Fail::Stopped) && last {
                        self.unverified("the final read stopped during its prefix check");
                    }
                    self.open = Some(open);
                    return Err(e);
                }
            }
        }
        let start = open.pos;
        let batch = match read_lines(
            &mut open,
            &self.b,
            &self.dir,
            &self.stop,
            last,
            &mut self.stats,
        ) {
            Ok(b) => b,
            Err(e) => {
                self.open = Some(open);
                return Err(e);
            }
        };
        be.after_read();
        match self.check(&open, batch.advance.max(start)) {
            Check::Same => {
                // The tail before the read position, where Claude's own rewrites land.
                match self.window_holds(&mut open) {
                    Ok(true) => {}
                    Ok(false) => {
                        self.stats.discarded += 1;
                        drop(open);
                        self.disprove();
                        tracing::info!(thread = %self.b.thread_id, launch = %self.b.launch_id, "the transcript's tail before the read position changed; a new generation starts");
                        return self.restart(be);
                    }
                    Err(e) => {
                        self.open = Some(open);
                        return Err(e);
                    }
                }
            }
            Check::Unknown(e) => {
                self.open = Some(open);
                return Err(e.into());
            }
            Check::Disproved { replaced, size } => {
                self.stats.discarded += 1;
                drop(open);
                self.disprove();
                tracing::info!(thread = %self.b.thread_id, launch = %self.b.launch_id, replaced, size, "transcript continuity is disproved; a new generation starts");
                return self.restart(be);
            }
        }
        if batch.lines.is_empty() && batch.advance == start {
            self.open = Some(open);
            return Ok(if last {
                Step::End("the launch exited and its transcript was read".into())
            } else {
                Step::Wait
            });
        }
        let Some(cursor) = self.cursor.clone() else {
            return Ok(Step::Wait);
        };
        let advance_to = ReadPos {
            read_offset: batch.advance,
            continuity: continuity(&batch.digest),
        };
        let out = self.batch(Some(cursor.pos()), None, batch.lines, advance_to.clone());
        if let Err(e) = self.stopped() {
            self.open = Some(open);
            return Err(e);
        }
        match be.commit(out) {
            Commit::Done => {
                open.digest = batch.digest;
                open.pos = batch.advance;
                open.recent = batch.recent;
                if let Some(c) = &mut self.cursor {
                    c.read_offset = advance_to.read_offset;
                    c.continuity = advance_to.continuity;
                }
                // The check again: a truncate or replace after the one before the commit.
                match self.check(&open, batch.advance) {
                    Check::Same => {}
                    Check::Disproved { replaced, size } => {
                        self.stats.late += 1;
                        self.disprove();
                        tracing::info!(thread = %self.b.thread_id, launch = %self.b.launch_id, replaced, size, "transcript continuity broke while a batch was committed; the batch stays and a new generation starts");
                        return Ok(Step::Again);
                    }
                    Check::Unknown(e) => {
                        // Unproved either way: the next poll opens the path again and
                        // verifies the prefix.
                        tracing::info!(thread = %self.b.thread_id, launch = %self.b.launch_id, error = %e, "the check after a transcript commit failed; the file is opened and verified again");
                        return Ok(Step::Again);
                    }
                }
                self.open = Some(open);
                Ok(if batch.more {
                    Step::Again
                } else if last && batch.eof {
                    Step::End("the launch exited and its transcript was read".into())
                } else {
                    Step::Wait
                })
            }
            Commit::StaleCursor => {
                self.cursor = None;
                Ok(Step::Again)
            }
            Commit::StaleBinding => Ok(Step::End("the binding changed (StaleBinding)".into())),
            Commit::Failed(e) => {
                // Read again from the same position at the next poll.
                self.open = Some(open);
                Err(Fail::Store(e))
            }
        }
    }

    /// Opens the path again and starts a new generation of the file there.
    fn restart(&mut self, be: &dyn Backend) -> Result<Step, Fail> {
        match self.open_path()? {
            Ok(f) => self.new_generation(be, f),
            Err(end) => Ok(end),
        }
    }

    /// Records that the stored generation's continuity is disproved.
    fn disprove(&mut self) {
        self.disproved = Some(self.cursor.as_ref().map_or(0, |c| c.generation));
    }

    /// Whether the path still names `open`'s file and the handle's size is at least
    /// `upto`. A missing path or another file there disproves continuity; an open that
    /// fails otherwise proves nothing.
    fn check(&self, open: &OpenFile, upto: u64) -> Check {
        let same = match open_regular(std::path::Path::new(&self.b.path)) {
            Ok(f) => f.id() == open.file.id(),
            Err(OpenError::Io(e)) if e.kind() == io::ErrorKind::NotFound => false,
            Err(OpenError::NotRegular) => false,
            Err(OpenError::Io(e)) => return Check::Unknown(e),
            // Refused before the first open, so not seen here.
            Err(OpenError::NotAbsolute | OpenError::DeviceNamespace) => false,
        };
        let size = open.file.size();
        match size {
            // Another file at the path is a break whatever the old handle says.
            _ if !same => Check::Disproved {
                replaced: true,
                size: size.unwrap_or(0),
            },
            Ok(size) if size >= upto => Check::Same,
            Ok(size) => Check::Disproved {
                replaced: false,
                size,
            },
            Err(e) => Check::Unknown(e),
        }
    }

    /// A batch of this tailer's binding.
    fn batch(
        &self,
        expect: Option<CursorPos>,
        new_generation: Option<NewGeneration>,
        lines: Vec<TranscriptLine>,
        advance_to: ReadPos,
    ) -> TranscriptBatch {
        TranscriptBatch {
            thread_id: self.b.thread_id,
            launch_id: self.b.launch_id,
            session_id: self.b.session_id.clone(),
            path: self.b.path.clone(),
            agent: self.b.agent,
            cli_version: self.b.cli_version.clone(),
            expect,
            new_generation,
            lines,
            advance_to,
        }
    }

    /// The continuity check at an open: `Ok` with the handle positioned at the cursor and
    /// the digest of its prefix, `Err` with the handle when continuity is disproved.
    fn verify(&self, mut f: RegularFile) -> Result<Result<OpenFile, RegularFile>, Fail> {
        let Some(c) = &self.cursor else {
            return Ok(Err(f));
        };
        if c.path != self.b.path || c.file_id != f.id().as_str() || f.size()? < c.read_offset {
            return Ok(Err(f));
        }
        let Some((digest, recent)) = self.hash_prefix(&mut f, c.read_offset)? else {
            return Ok(Err(f));
        };
        if continuity(&digest) != c.continuity {
            return Ok(Err(f));
        }
        Ok(Ok(OpenFile {
            mtime: f.modified().ok(),
            file: f,
            digest,
            pos: c.read_offset,
            recent,
        }))
    }

    /// The digest of `f`'s first `len` bytes, read again, and their last [`TAIL_WINDOW`]
    /// bytes; `None` when the file is shorter.
    fn hash_prefix(&self, f: &mut RegularFile, len: u64) -> Result<Option<(Sha256, Recent)>, Fail> {
        #[cfg(test)]
        if self.stop_at_prefix_check {
            self.stop.store(true, Ordering::Release);
        }
        f.seek(SeekFrom::Start(0))?;
        let mut digest = Sha256::new();
        let mut recent = Recent::default();
        let mut buf = vec![0u8; CHUNK];
        let mut left = len;
        while left > 0 {
            self.stopped()?;
            let want = usize::try_from(left.min(CHUNK as u64)).unwrap_or(CHUNK);
            let n = match f.read(&mut buf[..want]) {
                Ok(0) => return Ok(None),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            digest.update(&buf[..n]);
            recent.push(&buf[..n]);
            left -= n as u64;
        }
        Ok(Some((digest, recent)))
    }

    /// Whether the file's bytes just before the read position are still those read.
    fn window_holds(&self, open: &mut OpenFile) -> Result<bool, Fail> {
        let want = open.recent.bytes();
        open.file
            .seek(SeekFrom::Start(open.pos - want.len() as u64))?;
        let mut got = vec![0u8; want.len()];
        match open.file.read_exact(&mut got) {
            Ok(()) => Ok(got == want),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Whether the open file's bytes before the read position still have the digest read.
    fn prefix_holds(&self, open: &mut OpenFile) -> Result<bool, Fail> {
        let pos = open.pos;
        Ok(self
            .hash_prefix(&mut open.file, pos)?
            .is_some_and(|(d, _)| continuity(&d) == continuity(&open.digest)))
    }

    /// Commits a new generation of `f` alone at offset 0.
    fn new_generation(&mut self, be: &dyn Backend, f: RegularFile) -> Result<Step, Fail> {
        self.stopped()?;
        let empty = continuity(&Sha256::new());
        let batch = self.batch(
            self.cursor.as_ref().map(Cursor::pos),
            Some(NewGeneration {
                file_id: f.id().as_str().to_owned(),
                continuity: empty.clone(),
            }),
            Vec::new(),
            ReadPos {
                read_offset: 0,
                continuity: empty.clone(),
            },
        );
        match be.commit(batch) {
            Commit::Done => {
                self.cursor = Some(Cursor {
                    generation: self.cursor.as_ref().map_or(1, |c| c.generation + 1),
                    path: self.b.path.clone(),
                    file_id: f.id().as_str().to_owned(),
                    read_offset: 0,
                    continuity: empty,
                });
                self.open = Some(OpenFile {
                    mtime: f.modified().ok(),
                    file: f,
                    digest: Sha256::new(),
                    pos: 0,
                    recent: Recent::default(),
                });
                self.disproved = None;
                Ok(Step::Again)
            }
            Commit::StaleCursor => {
                self.open = None;
                self.cursor = None;
                Ok(Step::Again)
            }
            Commit::StaleBinding => Ok(Step::End("the binding changed (StaleBinding)".into())),
            Commit::Failed(e) => Err(Fail::Store(e)),
        }
    }
}

/// Marks the thread as one of a tailer's blocking tasks while it lives (tests).
#[cfg(test)]
pub(crate) struct OnBlocking(bool);

#[cfg(test)]
thread_local! {
    static ON_BLOCKING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The launches whose `Tail` was dropped off a tailer's blocking task (tests).
#[cfg(test)]
pub(crate) static DROPPED_OFF_BLOCKING: std::sync::Mutex<Vec<LaunchId>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
impl OnBlocking {
    pub(crate) fn enter() -> OnBlocking {
        OnBlocking(ON_BLOCKING.replace(true))
    }
}

#[cfg(test)]
impl Drop for OnBlocking {
    fn drop(&mut self) {
        ON_BLOCKING.set(self.0);
    }
}

#[cfg(test)]
impl Drop for Tail {
    fn drop(&mut self) {
        if !ON_BLOCKING.get() {
            DROPPED_OFF_BLOCKING
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(self.b.launch_id);
        }
    }
}

/// What [`Tail::check`] found.
enum Check {
    Same,
    Disproved { replaced: bool, size: u64 },
    Unknown(io::Error),
}

/// One poll's lines.
struct Batch {
    lines: Vec<TranscriptLine>,
    /// Just after the last complete line read (or the partial line, on a final read).
    advance: u64,
    /// The digest of `[0, advance)`.
    digest: Sha256,
    /// The last bytes before `advance`.
    recent: Recent,
    /// The batch closed at its bounds with more to read.
    more: bool,
    /// The read reached the end of the file.
    eof: bool,
}

/// The handle's bytes from a position, through one buffer.
struct Src<'a> {
    file: &'a mut RegularFile,
    buf: Box<[u8]>,
    pos: usize,
    end: usize,
    eof: bool,
}

impl Src<'_> {
    fn fill(&mut self) -> io::Result<bool> {
        if self.pos < self.end {
            return Ok(true);
        }
        if self.eof {
            return Ok(false);
        }
        loop {
            match self.file.read(&mut self.buf) {
                Ok(0) => {
                    self.eof = true;
                    return Ok(false);
                }
                Ok(n) => {
                    self.pos = 0;
                    self.end = n;
                    return Ok(true);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

/// An event a line produces, with its blob and time.
type LineEvent = (TypedEvent, Option<BlobInput>, Option<Timestamp>);

/// A line [`next_line`] read.
struct LineRead {
    /// The bytes before the newline.
    len: u64,
    /// Their last [`TAIL_WINDOW`] bytes.
    tail: Recent,
    newline: bool,
    event: Option<LineEvent>,
    /// The most bytes the line held in memory at once.
    peak: usize,
}

/// Reads the next line from `src` and classifies it, extending `digest` with its bytes;
/// `None` at the end of the file at a line start, or for a line still being written (not a
/// final read).
///
/// The reading, the copy kept for `raw_unknown`, and the text's spools stay outside
/// `lifecycle::recoverable`; only the parser's `feed` and `finish`, which do no I/O, run
/// inside it.
fn next_line(
    src: &mut Src<'_>,
    digest: &mut Sha256,
    b: &Binding,
    dir: &SpoolDir,
    stop: &AtomicBool,
    last: bool,
    at: u64,
) -> Result<Option<LineRead>, Fail> {
    let meter = Meter::default();
    let mut raw = ByteAcc::new(dir.clone(), meter.clone());
    let mut text = ByteAcc::new(dir.clone(), meter.clone());
    let mut aside: Option<ByteAcc> = None;
    // `None` once the parser panicked.
    let mut parser = Some(LineParser::default());
    let mut emits = Vec::new();
    let (mut len, mut newline) = (0u64, false);
    let mut tail = Recent::default();
    loop {
        if stop.load(Ordering::Acquire) {
            return Err(Fail::Stopped);
        }
        if !src.fill()? {
            break;
        }
        let avail = &src.buf[src.pos..src.end];
        let upto = avail.iter().position(|&c| c == b'\n');
        let piece = &avail[..upto.unwrap_or(avail.len())];
        raw.push(piece)?;
        tail.push(piece);
        digest.update(piece);
        len += piece.len() as u64;
        if let Some(p) = &mut parser {
            if crate::lifecycle::recoverable(|| p.feed(piece, &mut emits)).is_err() {
                parser = None;
                emits.clear();
            }
            for e in emits.drain(..) {
                match e {
                    Emit::Text(v) => text.push(&v)?,
                    Emit::Aside(v) => aside
                        .get_or_insert_with(|| ByteAcc::new(dir.clone(), meter.clone()))
                        .push(&v)?,
                    Emit::KeepAside => {
                        if let Some(a) = aside.take() {
                            text.append(a, stop).map_err(|e| {
                                if stop.load(Ordering::Acquire) {
                                    Fail::Stopped
                                } else {
                                    e.into()
                                }
                            })?;
                        }
                    }
                    Emit::DropAside => aside = None,
                }
            }
        }
        src.pos += piece.len();
        if upto.is_some() {
            src.pos += 1;
            digest.update(b"\n");
            newline = true;
            break;
        }
    }
    if !newline && (!last || len == 0) {
        return Ok(None);
    }
    drop(aside);
    let event = if newline {
        let outcome = parser.map(|p| crate::lifecycle::recoverable(|| p.finish()));
        match outcome {
            Some(Ok(Outcome::Assistant {
                occurred_at,
                turn_ref,
            })) => {
                drop(raw);
                let p = text.finish(TEXT)?;
                Some((
                    TypedEvent::AssistantMessage(AssistantMessage {
                        text: p.content,
                        turn_ref,
                    }),
                    p.blob,
                    occurred_at,
                ))
            }
            Some(Ok(Outcome::Skip)) => None,
            Some(Ok(Outcome::Raw(reason))) => Some(raw_event(b, reason, raw)?),
            Some(Err(_)) | None => {
                tracing::warn!(thread = %b.thread_id, launch = %b.launch_id, offset = at, "the transcript line parser panicked; the line is kept as raw_unknown");
                Some(raw_event(b, RawUnknownReason::SchemaMismatch, raw)?)
            }
        }
    } else {
        // A final read: the trailing fragment is kept verbatim.
        Some(raw_event(b, RawUnknownReason::PartialLine, raw)?)
    };
    Ok(Some(LineRead {
        len,
        tail,
        newline,
        event,
        peak: meter.peak(),
    }))
}

/// Reads the lines after `open.pos` up to the batch bounds or the end of the file.
///
/// A batch closes after a line whose blob is spooled, so one write job holds at most one
/// spool. A read or spool error after some progress closes the batch before the failed line
/// (read again at the next poll).
fn read_lines(
    open: &mut OpenFile,
    b: &Binding,
    dir: &SpoolDir,
    stop: &AtomicBool,
    last: bool,
    stats: &mut Stats,
) -> Result<Batch, Fail> {
    open.file.seek(SeekFrom::Start(open.pos))?;
    let start = open.pos;
    let mut digest = open.digest.clone();
    let mut recent = open.recent.clone();
    let mut at = open.pos;
    let mut lines = Vec::new();
    let mut memory = 0usize;
    let mut spooled = false;
    let mut src = Src {
        file: &mut open.file,
        buf: vec![0u8; CHUNK].into_boxed_slice(),
        pos: 0,
        end: 0,
        eof: false,
    };
    loop {
        if stop.load(Ordering::Acquire) {
            return Err(Fail::Stopped);
        }
        if lines.len() >= BATCH_LINES || memory >= BATCH_MEMORY || spooled {
            return Ok(Batch {
                lines,
                advance: at,
                digest,
                recent,
                more: true,
                eof: false,
            });
        }
        let mut line_digest = digest.clone();
        let (len, tail, newline, event, peak) = match next_line(
            &mut src,
            &mut line_digest,
            b,
            dir,
            stop,
            last,
            at,
        ) {
            Ok(None) => break,
            Ok(Some(LineRead {
                len,
                tail,
                newline,
                event,
                peak,
            })) => (len, tail, newline, event, peak),
            Err(Fail::Io(e)) if at > start => {
                tracing::debug!(thread = %b.thread_id, launch = %b.launch_id, offset = at, error = %e, "a transcript line failed to read; the lines before it are committed");
                return Ok(Batch {
                    lines,
                    advance: at,
                    digest,
                    recent,
                    more: true,
                    eof: false,
                });
            }
            Err(e) => return Err(e),
        };
        stats.peak_line_memory = stats.peak_line_memory.max(peak);
        digest = line_digest;
        recent.push(tail.bytes());
        if newline {
            recent.push(b"\n");
        }
        let end = at + len + u64::from(newline);
        if let Some((event, blob, occurred_at)) = event {
            match &blob {
                Some(BlobInput::Bytes(v)) => memory += v.len(),
                Some(BlobInput::Spooled(_)) => spooled = true,
                None => {}
            }
            lines.push(TranscriptLine {
                offset_start: at,
                offset_end: end,
                event,
                occurred_at,
                continuity: continuity(&digest),
                blobs: blob.into_iter().collect(),
            });
        }
        at = end;
        if !newline {
            break;
        }
    }
    Ok(Batch {
        lines,
        advance: at,
        digest,
        recent,
        more: false,
        eof: true,
    })
}

/// A line kept verbatim.
fn raw_event(b: &Binding, reason: RawUnknownReason, raw: ByteAcc) -> io::Result<LineEvent> {
    let content_type = match reason {
        RawUnknownReason::Unparseable | RawUnknownReason::PartialLine => OCTET,
        _ => JSON,
    };
    let p = raw.finish(content_type)?;
    Ok((
        TypedEvent::RawUnknown(RawUnknown {
            source: SourceKind::Transcript,
            reason,
            agent: Some(b.agent),
            cli_version: b.cli_version.clone(),
            content_type: content_type.to_owned(),
            raw: p.content,
        }),
        p.blob,
        None,
    ))
}
