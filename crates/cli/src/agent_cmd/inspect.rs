//! `chaldea inspect <thread> [--json]` (C1 D2): the thread's snapshot from one
//! `GET /api/events` read through the `cursor_ahead` fallback, or its summary when the
//! snapshot is too large or not built in time, then the live screen of the launch read.

use std::fmt::Write as _;
use std::io::Write;
use std::time::Duration;

use chaldea_core::MAX_SAFE_INTEGER;
use chaldea_core::http::{ErrorBody, ErrorCode};
use chaldea_core::ids::{LaunchId, StreamId, ThreadId};
use chaldea_core::state::AttentionCause;
use chaldea_core::sync::{
    EventsResponse, InspectOutput, ScreenResponse, StreamSnapshot, ThreadSnapshot,
};
use chaldea_daemon::lifecycle::ExitCode;

use super::{Client, End, Req, print, say};

/// The cutoff of each of the three reads, above the daemon's 5 s read deadline so that its
/// own 408 arrives first.
const READ_CUTOFF: Duration = Duration::from_secs(15);
/// The snapshot read's body limit (the client's default stays 1 MiB).
const SNAPSHOT_MAX_BODY: usize = 16 * 1024 * 1024;

fn error(code: ErrorCode) -> ErrorBody {
    ErrorBody {
        code,
        retry_after: None,
    }
}

/// The daemon's error body, or `internal` when it is not one.
fn daemon_error(body: &[u8]) -> ErrorBody {
    serde_json::from_slice(body).unwrap_or_else(|_| error(ErrorCode::Internal))
}

/// A client-side end as an error: over the limit, out of time, or anything else.
fn client_error(e: &End) -> ErrorBody {
    error(match e {
        End::TooLarge => ErrorCode::PayloadTooLarge,
        End::Timeout => ErrorCode::RequestTimeout,
        _ => ErrorCode::Internal,
    })
}

fn end_message(e: &End) -> String {
    match e {
        End::Fail(m) => m.clone(),
        End::Timeout => "the daemon did not answer in time".to_owned(),
        End::TooLarge => "the daemon's response is larger than the client reads".to_owned(),
        End::Restarted | End::Stopped | End::Unverified => "the read was interrupted".to_owned(),
    }
}

/// How the snapshot read ended.
enum SnapshotRead {
    Read(Box<ThreadSnapshot>),
    /// Too large or not in time: the summary is read instead.
    Fallback(ErrorBody),
    /// Any other failure, with its message.
    Failed(ErrorBody, String),
}

/// A wire enum's text.
fn wire<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
        .unwrap_or_default()
}

/// An attention cause: its kind, then its other fields as `name value`.
fn cause(c: &AttentionCause) -> String {
    let Ok(serde_json::Value::Object(m)) = serde_json::to_value(c) else {
        return String::new();
    };
    let mut s = m.get("kind").map(wire).unwrap_or_default();
    for (k, v) in m.iter().filter(|(k, _)| *k != "kind") {
        let _ = write!(s, " {k} {}", wire(v));
    }
    s
}

/// `s` with control characters escaped, so daemon text cannot drive the terminal.
fn visible(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

impl Client<'_> {
    fn snapshot_read(&self, t: ThreadId) -> SnapshotRead {
        let path = format!(
            "/api/events?stream_id={}&after_seq={MAX_SAFE_INTEGER}",
            StreamId::Thread(t)
        );
        let r = self.exchange(
            self.cutoff(READ_CUTOFF),
            &Req::GetUpTo(&path, SNAPSHOT_MAX_BODY),
            true,
            &mut |r| match r.status {
                503 => None,
                200 => Some(Ok(
                    match serde_json::from_slice::<EventsResponse>(&r.body) {
                        Ok(EventsResponse::Snapshot {
                            snapshot: StreamSnapshot::Thread(s),
                            ..
                        }) => SnapshotRead::Read(s),
                        Ok(_) => SnapshotRead::Failed(
                            error(ErrorCode::Internal),
                            "the daemon answered the snapshot read with a page".to_owned(),
                        ),
                        Err(e) => SnapshotRead::Failed(
                            error(ErrorCode::Internal),
                            format!("the snapshot is not readable: {e}"),
                        ),
                    },
                )),
                408 => Some(Ok(SnapshotRead::Fallback(daemon_error(&r.body)))),
                404 => Some(Ok(SnapshotRead::Failed(
                    daemon_error(&r.body),
                    "no such thread or not readable".to_owned(),
                ))),
                s => Some(Ok(SnapshotRead::Failed(
                    daemon_error(&r.body),
                    format!("the daemon answered the snapshot read with {s}"),
                ))),
            },
        );
        match r {
            Ok(read) => read,
            Err(e @ (End::TooLarge | End::Timeout)) => SnapshotRead::Fallback(client_error(&e)),
            Err(e) => SnapshotRead::Failed(client_error(&e), end_message(&e)),
        }
    }

    /// The screen, checked to be of launch `expected`.
    fn screen_read(
        &self,
        t: ThreadId,
        expected: Option<LaunchId>,
    ) -> Result<ScreenResponse, ErrorBody> {
        let path = format!("/api/threads/{t}/screen");
        let r = self.exchange(
            self.cutoff(READ_CUTOFF),
            &Req::Get(&path),
            true,
            &mut |r| match r.status {
                503 => None,
                200 => Some(Ok(serde_json::from_slice::<ScreenResponse>(&r.body)
                    .map_err(|_| error(ErrorCode::Internal)))),
                _ => Some(Ok(Err(daemon_error(&r.body)))),
            },
        );
        let screen = r.unwrap_or_else(|e| Err(client_error(&e)))?;
        if Some(screen.launch_id) == expected {
            Ok(screen)
        } else {
            Err(error(ErrorCode::LaunchChanged))
        }
    }

    /// `chaldea inspect`: 0 when the snapshot or the summary was read, 1 when neither was,
    /// 4 when the summary read ran out of time.
    pub(crate) fn inspect(
        &self,
        t: ThreadId,
        json: bool,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> ExitCode {
        let mut o = InspectOutput {
            thread_id: t,
            snapshot: None,
            summary: None,
            screen: None,
            snapshot_error: None,
            screen_error: None,
        };
        let launch: Option<LaunchId> = match self.snapshot_read(t) {
            SnapshotRead::Read(s) => {
                let l = s.launch.as_ref().map(|l| l.launch_id);
                o.snapshot = Some(*s);
                l
            }
            SnapshotRead::Failed(e, m) => {
                o.snapshot_error = Some(e);
                say(err, &m);
                return finish(&o, json, out, ExitCode::Failed);
            }
            SnapshotRead::Fallback(e) => {
                o.snapshot_error = Some(e);
                match self.summary(self.cutoff(READ_CUTOFF), t) {
                    Ok(s) => {
                        let l = s.launch.as_ref().map(|l| l.launch_id);
                        o.summary = Some(s);
                        l
                    }
                    Err(e) => {
                        say(err, &end_message(&e));
                        let code = if matches!(e, End::Timeout) {
                            ExitCode::Timeout
                        } else {
                            ExitCode::Failed
                        };
                        return finish(&o, json, out, code);
                    }
                }
            }
        };
        match self.screen_read(t, launch) {
            Ok(s) => o.screen = Some(s),
            Err(e) => o.screen_error = Some(e),
        }
        finish(&o, json, out, ExitCode::Ok)
    }
}

fn finish(o: &InspectOutput, json: bool, out: &mut dyn Write, code: ExitCode) -> ExitCode {
    if json {
        print(out, o);
    } else {
        // Empty when neither the snapshot nor the summary was read.
        let _ = out.write_all(plain(o).as_bytes());
    }
    code
}

/// The plain output: the state line, attention, launch, session, inputs, cards, and recent
/// events one per line, then the screen rows.
fn plain(o: &InspectOutput) -> String {
    let mut s = String::new();
    let (closed, process, turn, task) = match (&o.snapshot, &o.summary) {
        (Some(sn), _) => (
            sn.thread.closed,
            wire(&sn.thread.process_state),
            wire(&sn.thread.turn_state),
            wire(&sn.thread.task_state),
        ),
        (None, Some(su)) => (
            su.closed,
            wire(&su.process_state),
            wire(&su.turn_state),
            wire(&su.task_state),
        ),
        (None, None) => return s,
    };
    let closed = if closed { ", closed" } else { "" };
    let _ = writeln!(
        s,
        "thread {}: process {process}, turn {turn}, task {task}{closed}",
        o.thread_id
    );
    let launch = o.snapshot.as_ref().map_or_else(
        || o.summary.as_ref().and_then(|x| x.launch.as_ref()),
        |x| x.launch.as_ref(),
    );
    match &o.snapshot {
        Some(sn) => {
            if sn.thread.attention.is_empty() {
                s.push_str("attention: none\n");
            } else {
                let causes: Vec<String> = sn.thread.attention.iter().map(cause).collect();
                let _ = writeln!(s, "attention: {}", visible(&causes.join(", ")));
            }
        }
        None => {
            let n = o.summary.as_ref().map_or(0, |x| x.attention_count);
            let _ = writeln!(s, "attention: {n} cause(s)");
        }
    }
    match launch {
        Some(l) => {
            let pid = l.pid.map_or_else(|| "-".to_owned(), |p| p.to_string());
            let state = l.exit_reason.map_or_else(
                || "not exited".to_owned(),
                |r| format!("exited ({})", wire(&r)),
            );
            let death = if l.death_confirmed {
                ", death confirmed"
            } else {
                ""
            };
            let _ = writeln!(s, "launch: {} pid {pid}, {state}{death}", l.launch_id);
        }
        None => s.push_str("launch: none\n"),
    }
    match &o.snapshot {
        Some(sn) => {
            match &sn.session {
                Some(x) => {
                    let _ = writeln!(s, "session: {}", visible(&x.session_id));
                }
                None => s.push_str("session: none\n"),
            }
            let _ = writeln!(s, "inputs: {}", sn.inputs.len());
            for i in &sn.inputs {
                let _ = writeln!(
                    s,
                    "  {} {} by {}",
                    i.input_id,
                    wire(&i.state),
                    visible(&wire(&i.created_by))
                );
            }
            let _ = writeln!(s, "cards: {}", sn.open_approvals.len());
            for c in &sn.open_approvals {
                let _ = writeln!(s, "  {} {}", c.approval_id, visible(&c.tool_name));
            }
            let _ = writeln!(s, "recent events: {}", sn.recent_events.len());
            for e in &sn.recent_events {
                let _ = writeln!(s, "  {} {}", e.seq, visible(&wire(&e.kind)));
            }
        }
        None => {
            let why = o
                .snapshot_error
                .as_ref()
                .map_or_else(String::new, |e| wire(&e.code));
            let _ = writeln!(
                s,
                "The snapshot could not be read ({why}): attention causes, the session, inputs, cards, and recent events are not shown."
            );
        }
    }
    match (&o.screen, &o.screen_error) {
        (Some(sc), _) => {
            let truncated = if sc.truncated { ", truncated" } else { "" };
            let _ = writeln!(
                s,
                "screen: {}x{}, cursor {},{}{truncated}",
                sc.cols, sc.rows, sc.cursor.row, sc.cursor.col
            );
            for l in &sc.lines {
                let _ = writeln!(s, "| {}", visible(l));
            }
        }
        (None, e) => {
            let why = e.as_ref().map_or_else(String::new, |e| wire(&e.code));
            let _ = writeln!(s, "screen: not read ({why})");
        }
    }
    s
}
