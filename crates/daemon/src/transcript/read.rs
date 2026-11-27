//! One tailer's synchronous core (`docs/design/c1-claude-adapter.md` D10, D13): the open and
//! its continuity check, the line reads, the checks before a commit, and the batches. It runs
//! on a blocking task; the store is reached only through a [`Backend`].
//!
//! Continuity is `sha256:<hex>` of the file's bytes `[0, offset)`. While the handle stays
//! open a running digest is extended with the bytes read. At every open the handle's
//! `file_id` and the path are compared with the stored cursor's, and the whole prefix is
//! read again and checked against the stored digest. Every poll, before any cursor advance,
//! the path must still name the handle's file and the handle's size must not be below the
//! offset the batch advances to (nor the read position). A failed check discards the batch
//! and starts a new generation at offset 0; reading starts again from 0.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chaldea_core::event::TypedEvent;
use chaldea_core::ids::{Agent, LaunchId, ThreadId};
use chaldea_core::payload::{AssistantMessage, RawUnknown, RawUnknownReason, SourceKind};
use chaldea_pty_runtime::file::{OpenError, RegularFile, open_regular};
use sha2::{Digest, Sha256};

use super::acc::{ByteAcc, SpoolDir};
use super::claude::{self, Line};
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
    /// The most bytes one line held in memory (its raw bytes and its text).
    pub(crate) peak_line_memory: usize,
    /// Batches discarded by the checks before a commit.
    pub(crate) discarded: u32,
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

/// The open handle and the running digest of `[0, pos)`.
struct OpenFile {
    file: RegularFile,
    digest: Sha256,
    pos: u64,
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
    /// A check failed and the path could not be opened again: the next open starts a
    /// generation whatever its continuity.
    force_generation: bool,
    final_failures: u32,
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
            force_generation: false,
            final_failures: 0,
            last_failure: None,
            stats: Stats::default(),
        }
    }

    /// One poll.
    pub(crate) fn poll(&mut self, be: &dyn Backend) -> Step {
        let view = match be.view(&self.b) {
            Ok(v) => v,
            Err(e) => return self.failed(&Fail::Store(e), false),
        };
        if !view.current {
            return Step::End("the launch is no longer the bound one".into());
        }
        let stored = view.cursor.map(Cursor::from);
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

    fn failed(&mut self, f: &Fail, last: bool) -> Step {
        let text = f.to_string();
        if self.last_failure.as_deref() != Some(text.as_str()) {
            tracing::warn!(thread = %self.b.thread_id, launch = %self.b.launch_id, error = %text, "the transcript read failed; it is retried at the next poll");
            self.last_failure = Some(text.clone());
        }
        if last {
            self.final_failures += 1;
            if self.final_failures >= FINAL_TRIES {
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
            if self.force_generation {
                return self.new_generation(be, f);
            }
            match self.verify(f)? {
                Ok(open) => self.open = Some(open),
                Err(f) => return self.new_generation(be, f),
            }
        }
        let Some(mut open) = self.open.take() else {
            return Ok(Step::Wait);
        };
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
        // The checks before any cursor advance: the path still names the handle's file, and
        // the handle's size is not below the read position or the batch's advance.
        // A missing path or another file there disproves continuity; an open that fails
        // otherwise proves nothing and is retried.
        let same = match open_regular(std::path::Path::new(&self.b.path)) {
            Ok(f) => f.id() == open.file.id(),
            Err(OpenError::Io(e)) if e.kind() == io::ErrorKind::NotFound => false,
            Err(OpenError::NotRegular) => false,
            Err(OpenError::Io(e)) => {
                self.open = Some(open);
                return Err(e.into());
            }
            Err(OpenError::NotAbsolute | OpenError::DeviceNamespace) => {
                return Ok(Step::End("the path cannot be read".into()));
            }
        };
        let size = match open.file.size() {
            Ok(n) => n,
            Err(e) => {
                self.open = Some(open);
                return Err(e.into());
            }
        };
        if !same || size < batch.advance.max(start) {
            self.stats.discarded += 1;
            drop(open);
            tracing::info!(thread = %self.b.thread_id, launch = %self.b.launch_id, replaced = !same, size, "transcript continuity is disproved; a new generation starts");
            return match self.open_path() {
                Ok(Ok(f)) => self.new_generation(be, f),
                Ok(Err(end)) => Ok(end),
                Err(e) => {
                    self.force_generation = true;
                    Err(e)
                }
            };
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
        match be.commit(out) {
            Commit::Done => {
                open.digest = batch.digest;
                open.pos = batch.advance;
                self.open = Some(open);
                if let Some(c) = &mut self.cursor {
                    c.read_offset = advance_to.read_offset;
                    c.continuity = advance_to.continuity;
                }
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
        f.seek(SeekFrom::Start(0))?;
        let mut digest = Sha256::new();
        let mut buf = vec![0u8; CHUNK];
        let mut left = c.read_offset;
        while left > 0 {
            self.stopped()?;
            let want = usize::try_from(left.min(CHUNK as u64)).unwrap_or(CHUNK);
            let n = match f.read(&mut buf[..want]) {
                Ok(0) => return Ok(Err(f)),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            digest.update(&buf[..n]);
            left -= n as u64;
        }
        if continuity(&digest) != c.continuity {
            return Ok(Err(f));
        }
        Ok(Ok(OpenFile {
            file: f,
            digest,
            pos: c.read_offset,
        }))
    }

    /// Commits a new generation of `f` alone at offset 0.
    fn new_generation(&mut self, be: &dyn Backend, f: RegularFile) -> Result<Step, Fail> {
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
                    file: f,
                    digest: Sha256::new(),
                    pos: 0,
                });
                self.force_generation = false;
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

/// One poll's lines.
struct Batch {
    lines: Vec<TranscriptLine>,
    /// Just after the last complete line read (or the partial line, on a final read).
    advance: u64,
    /// The digest of `[0, advance)`.
    digest: Sha256,
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

/// One line's bytes up to its newline (excluded), copied to `raw` and the digest as they
/// are read.
struct LineReader<'a, 'b> {
    src: &'a mut Src<'b>,
    raw: &'a mut ByteAcc,
    digest: &'a mut Sha256,
    stop: &'a AtomicBool,
    /// The bytes before the newline.
    len: u64,
    /// The newline was read.
    newline: bool,
    done: bool,
}

impl Read for LineReader<'_, '_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.done || out.is_empty() {
            return Ok(0);
        }
        if self.stop.load(Ordering::Acquire) {
            return Err(io::Error::other("stopped"));
        }
        if !self.src.fill()? {
            self.done = true;
            return Ok(0);
        }
        let avail = &self.src.buf[self.src.pos..self.src.end];
        let upto = avail.iter().position(|&b| b == b'\n');
        let n = upto.unwrap_or(avail.len()).min(out.len());
        out[..n].copy_from_slice(&avail[..n]);
        self.raw.push(&avail[..n])?;
        self.digest.update(&avail[..n]);
        self.src.pos += n;
        self.len += n as u64;
        if upto == Some(n) {
            self.digest.update(b"\n");
            self.src.pos += 1;
            self.newline = true;
            self.done = true;
        }
        Ok(n)
    }
}

/// Reads the lines after `open.pos` up to the batch bounds or the end of the file.
fn read_lines(
    open: &mut OpenFile,
    b: &Binding,
    dir: &SpoolDir,
    stop: &AtomicBool,
    last: bool,
    stats: &mut Stats,
) -> Result<Batch, Fail> {
    open.file.seek(SeekFrom::Start(open.pos))?;
    let mut digest = open.digest.clone();
    let mut at = open.pos;
    let mut lines = Vec::new();
    let mut memory = 0usize;
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
        if lines.len() >= BATCH_LINES || memory >= BATCH_MEMORY {
            return Ok(Batch {
                lines,
                advance: at,
                digest,
                more: true,
                eof: false,
            });
        }
        if !src.fill()? {
            break;
        }
        let mut line_digest = digest.clone();
        let mut raw = ByteAcc::new(dir.clone());
        let mut lr = LineReader {
            src: &mut src,
            raw: &mut raw,
            digest: &mut line_digest,
            stop,
            len: 0,
            newline: false,
            done: false,
        };
        let parsed = crate::lifecycle::recoverable(|| claude::parse(&mut lr, dir));
        // Whatever the parser left of the line (after an error or a panic).
        let drained = io::copy(&mut lr, &mut io::sink());
        if stop.load(Ordering::Acquire) {
            return Err(Fail::Stopped);
        }
        drained?;
        let (len, newline) = (lr.len, lr.newline);
        if !newline && !last {
            // A line still being written: read again from its start at the next poll.
            break;
        }
        let end = at + len + u64::from(newline);
        let (event, occurred_at, held) = if newline {
            match parsed {
                Ok(Ok(Line::Assistant { text, occurred_at })) => {
                    let held = raw.peak_mem.max(text.peak_mem);
                    let p = text.finish(TEXT)?;
                    (
                        Some((
                            TypedEvent::AssistantMessage(AssistantMessage {
                                text: p.content,
                                turn_ref: None,
                            }),
                            p.blob,
                        )),
                        occurred_at,
                        held,
                    )
                }
                Ok(Ok(Line::Skip)) => (None, None, raw.peak_mem),
                Ok(Ok(Line::Raw(reason))) => {
                    let held = raw.peak_mem;
                    (Some(raw_event(b, reason, raw)?), None, held)
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(_panic) => {
                    tracing::warn!(thread = %b.thread_id, launch = %b.launch_id, offset = at, "the transcript line parser panicked; the line is kept as raw_unknown");
                    let held = raw.peak_mem;
                    (
                        Some(raw_event(b, RawUnknownReason::SchemaMismatch, raw)?),
                        None,
                        held,
                    )
                }
            }
        } else {
            // A final read: the trailing fragment is kept verbatim.
            if len == 0 {
                break;
            }
            let held = raw.peak_mem;
            (
                Some(raw_event(b, RawUnknownReason::PartialLine, raw)?),
                None,
                held,
            )
        };
        stats.peak_line_memory = stats.peak_line_memory.max(held);
        digest = line_digest;
        if let Some((event, blob)) = event {
            memory += match &blob {
                Some(BlobInput::Bytes(v)) => v.len(),
                _ => 0,
            };
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
        more: false,
        eof: true,
    })
}

/// A line kept verbatim.
fn raw_event(
    b: &Binding,
    reason: RawUnknownReason,
    raw: ByteAcc,
) -> io::Result<(TypedEvent, Option<BlobInput>)> {
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
    ))
}
