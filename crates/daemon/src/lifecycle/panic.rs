//! The panic hook (design §2.13): requests `Panic` on the active gate first, then pushes one
//! line with the location and a fixed task or thread kind; never the payload, its message, a
//! backtrace, or a thread's name. It uses no `tracing`, and the only locks it takes are the
//! active-gate mutex (to upgrade) and `Notify`'s inside `notify_one`.

use std::cell::Cell;
use std::io::Write;

use super::gate::{self, StopCause};
use super::log::{diag, process_logger};
use crate::http::TaskKind;

thread_local! {
    /// The kind of the registered task being polled on this thread.
    static TASK_KIND: Cell<Option<TaskKind>> = const { Cell::new(None) };
}

/// The runtime's worker and blocking threads are named this (`runtime` in a panic line).
pub(crate) const RUNTIME_THREAD: &str = "chaldea-runtime";

/// Sets the task kind for one poll and restores the previous value on drop.
pub(crate) struct TaskKindGuard(Option<TaskKind>);

impl TaskKindGuard {
    pub(crate) fn enter(kind: TaskKind) -> TaskKindGuard {
        TaskKindGuard(TASK_KIND.with(|k| k.replace(Some(kind))))
    }
}

impl Drop for TaskKindGuard {
    fn drop(&mut self) {
        let prev = self.0;
        TASK_KIND.with(|k| k.set(prev));
    }
}

/// The fixed label of the panicking context.
pub(crate) fn kind_label() -> &'static str {
    if let Some(kind) = TASK_KIND.with(Cell::get) {
        return match kind {
            TaskKind::Accept => "accept",
            TaskKind::Connection => "connection",
            TaskKind::Socket => "socket",
            TaskKind::Write => "write",
            TaskKind::Runner => "runner",
        };
    }
    match std::thread::current().name() {
        Some("store-writer") => "store-writer",
        Some("daemon-log") => "daemon-log",
        Some("daemon-stop") => "daemon-stop",
        Some("main") => "main",
        Some(n) if n == RUNTIME_THREAD => "runtime",
        _ => "thread",
    }
}

fn hook(info: &std::panic::PanicHookInfo<'_>) {
    if let Some(g) = gate::active() {
        g.request(StopCause::Panic);
    }
    let at = info.location().map_or_else(
        || "an unknown location".to_owned(),
        |l| format!("{}:{}:{}", l.file(), l.line(), l.column()),
    );
    let line = diag(&format!(
        "chaldea: panic in {} at {at} (payload omitted)\n",
        kind_label()
    ));
    match process_logger() {
        Some(l) => l.push(line),
        None => {
            let _ = std::io::stderr().write_all(line.as_bytes());
        }
    }
}

/// Replaces the default hook (which would print the payload).
pub(crate) fn install() {
    std::panic::set_hook(Box::new(hook));
}
