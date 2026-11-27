//! Transcript tailer tests (C1-4 line half, C1-8, C1-9, C1-10 tailer half): real files in the
//! OS temp directory, a real store, and the tailer's polls driven one at a time.

mod continuity;
mod fixtures;
mod large;
mod lines;
mod panic;
mod refuse;
mod resume;

use std::cell::Cell;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use chaldea_core::ids::{Agent, LaunchId, StreamId, ThreadId};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use super::acc::SpoolDir;
use super::read::{Backend, Binding, Commit, Step, Tail, View};
use super::tailer::{binding_view, commit};
use crate::store::tests::support::{self, TestStore};
use crate::store::{NewEvent, TranscriptBatch, TranscriptCursor};

/// The store side over a test store, with hooks between a poll's read and its checks and
/// before a batch of lines is committed, and generation commits that fail on demand.
pub(crate) struct TestBackend<'a> {
    pub(crate) store: &'a TestStore,
    pub(crate) after_read: Box<dyn Fn() + 'a>,
    pub(crate) before_commit: Box<dyn Fn() + 'a>,
    /// Generation commits still to fail as a full queue would.
    pub(crate) fail_generations: Cell<u32>,
}

impl Backend for TestBackend<'_> {
    fn view(&self, b: &Binding) -> Result<View, String> {
        let b = b.clone();
        self.store
            .store()
            .read(Instant::now() + Duration::from_secs(10), move |r| {
                binding_view(r, &b)
            })
            .map_err(|e| format!("{e:?}"))
    }

    fn commit(&self, batch: TranscriptBatch) -> Commit {
        if batch.new_generation.is_none() {
            (self.before_commit)();
        } else if self.fail_generations.get() > 0 {
            self.fail_generations.set(self.fail_generations.get() - 1);
            return Commit::Failed("queue full (test)".into());
        }
        commit(self.store.store(), batch)
    }

    fn after_read(&self) {
        (self.after_read)();
    }
}

/// A store with one Claude thread bound to session `s1` and a transcript path in a temp
/// directory.
pub(crate) struct Fx {
    pub(crate) store: TestStore,
    pub(crate) files: TempDir,
    pub(crate) path: PathBuf,
    pub(crate) t: ThreadId,
    pub(crate) l: LaunchId,
}

impl Fx {
    pub(crate) fn new() -> Fx {
        Fx::at(|dir| dir.join("session.jsonl"))
    }

    /// A fixture whose bound path is `path(files)`.
    pub(crate) fn at(path: impl FnOnce(&Path) -> PathBuf) -> Fx {
        let store = TestStore::new();
        let files = tempfile::tempdir().unwrap();
        let path = path(files.path());
        let (t, l) = (support::thread(1), support::launch(1));
        support::bound_thread(&store, t, l, Some(&path.to_string_lossy()));
        Fx {
            store,
            files,
            path,
            t,
            l,
        }
    }

    pub(crate) fn binding(&self, l: LaunchId) -> Binding {
        Binding {
            thread_id: self.t,
            launch_id: l,
            session_id: "s1".into(),
            path: self.path.to_string_lossy().into_owned(),
            agent: Agent::Claude,
            cli_version: Some("1.0.0".into()),
        }
    }

    pub(crate) fn tail(&self) -> Tail {
        self.tail_of(self.l)
    }

    pub(crate) fn tail_of(&self, l: LaunchId) -> Tail {
        Tail::new(
            self.binding(l),
            SpoolDir::new(self.store.dir().to_owned()),
            Arc::new(AtomicBool::new(false)),
        )
    }

    /// A tailer whose state directory holds at most `limit` spool files at once.
    pub(crate) fn tail_with_spool_limit(&self, limit: usize) -> Tail {
        let mut dir = SpoolDir::new(self.store.dir().to_owned());
        dir.limit = Some(limit);
        Tail::new(self.binding(self.l), dir, Arc::new(AtomicBool::new(false)))
    }

    pub(crate) fn backend(&self) -> TestBackend<'_> {
        TestBackend {
            store: &self.store,
            after_read: Box::new(|| {}),
            before_commit: Box::new(|| {}),
            fail_generations: Cell::new(0),
        }
    }

    /// Polls until the tailer waits or ends (at most 100 polls); the last step.
    pub(crate) fn settle(&self, tail: &mut Tail) -> Step {
        self.settle_with(tail, &self.backend())
    }

    pub(crate) fn settle_with(&self, tail: &mut Tail, be: &dyn Backend) -> Step {
        for _ in 0..100 {
            let s = tail.poll(be);
            if s != Step::Again {
                return s;
            }
        }
        panic!("the tailer did not settle");
    }

    pub(crate) fn write(&self, bytes: &[u8]) {
        std::fs::write(&self.path, bytes).unwrap();
    }

    pub(crate) fn append(&self, bytes: &[u8]) {
        let mut f = OpenOptions::new().append(true).open(&self.path).unwrap();
        f.write_all(bytes).unwrap();
    }

    pub(crate) fn events(&self) -> Vec<Ev> {
        events(&self.store, self.t)
    }

    pub(crate) fn cursor(&self) -> Option<TranscriptCursor> {
        let t = self.t;
        self.store.read(move |r| r.transcript_cursor(t))
    }

    pub(crate) fn add(&self, events: Vec<NewEvent>) {
        self.store.append(events).unwrap();
    }
}

/// One stored transcript event, as the tests compare it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Ev {
    pub(crate) kind: String,
    pub(crate) payload: serde_json::Value,
    pub(crate) generation: Option<u64>,
    pub(crate) offset_end: Option<u64>,
    pub(crate) occurred_at: String,
}

impl Ev {
    /// `assistant_message` text or `raw_unknown` reason, or the generation number.
    pub(crate) fn short(&self) -> String {
        match self.kind.as_str() {
            "assistant_message" => format!("text:{}", inline(&self.payload["text"])),
            "raw_unknown" => format!(
                "raw:{}:{}",
                self.payload["reason"].as_str().unwrap(),
                inline(&self.payload["raw"])
            ),
            "transcript_generation" => {
                format!("generation:{}", self.payload["generation"])
            }
            k => k.to_owned(),
        }
    }
}

/// An inline `Content`'s text (`<blob N>` for a blob).
pub(crate) fn inline(c: &serde_json::Value) -> String {
    match c.get("inline") {
        Some(s) => s.as_str().unwrap().to_owned(),
        None => format!("<blob {}>", c["blob"]["size"]),
    }
}

/// The transcript-sourced events of thread `t` and its generations, in order.
pub(crate) fn events(store: &TestStore, t: ThreadId) -> Vec<Ev> {
    store.read(move |r| {
        Ok(r.events_after(StreamId::Thread(t), 0, 100_000)?
            .events
            .into_iter()
            .filter(|e| e.kind.0 == "transcript_generation" || e.source.transcript.is_some())
            .map(|e| Ev {
                kind: e.kind.0.clone(),
                payload: serde_json::from_str(e.payload.get()).unwrap(),
                generation: e.source.transcript.as_ref().map(|p| p.generation),
                offset_end: e.source.transcript.as_ref().map(|p| p.offset_end),
                occurred_at: e.occurred_at.0.clone(),
            })
            .collect())
    })
}

/// The short forms of `events`.
pub(crate) fn shorts(evs: &[Ev]) -> Vec<String> {
    evs.iter().map(Ev::short).collect()
}

/// `sha256:<hex>` of `bytes`.
pub(crate) fn digest(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::from("sha256:");
    for b in Sha256::digest(bytes) {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// An assistant line with `text` (JSON-escaped by the caller when needed).
pub(crate) fn assistant(text: &str) -> String {
    format!(
        "{{\"type\":\"assistant\",\"timestamp\":\"2026-11-26T12:30:31.281Z\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"{text}\"}}]}}}}\n"
    )
}

/// A known line that produces no event.
pub(crate) const MODE: &str = "{\"type\":\"mode\",\"mode\":\"normal\"}\n";

/// The store keeps serving: a write job and a read both answer.
pub(crate) fn serving(store: &TestStore) {
    store.run(|_| Ok(())).unwrap();
    store.read(|r| r.thread(support::thread(1)).map(|_| ()));
}
