//! `chaldea stop <thread> --assume-dead` without `--launch` (`docs/design/supervisor-recovery.md`
//! §2.5 "Preview"): the current launch's exit, death state, recorded identity, and evidence,
//! read from the thread stream backward to the launch's `launch_exited` (its `launch_reserved`
//! for `unknown_after_crash`), bound to one serving daemon run. It sends no command.

use std::cell::Cell;
use std::fmt::Write as _;
use std::io::Write;
use std::time::Duration;

use chaldea_core::MAX_SAFE_INTEGER;
use chaldea_core::event::{EventEnvelope, TypedEvent};
use chaldea_core::ids::{Epoch, LaunchId, StreamId, ThreadId};
use chaldea_core::payload::{
    EvidenceSource, ExitReason, LaunchEvidence, OWNER_ASSERTED, Observed, ObservedClass, Unchecked,
};
use chaldea_core::sync::{EventsResponse, LaunchInfo};
use chaldea_daemon::lifecycle::ExitCode;

use super::{Client, End, Found, Req, code_name};
use crate::poll::poll_until_with;

/// One cutoff for every request of the preview.
const PREVIEW_CUTOFF: Duration = Duration::from_secs(30);
/// The first page's size; halved, at the same `before_seq`, after a body-limit error.
const FIRST_LIMIT: usize = 16;

const NO_PROBE: &str = "The running daemon has not recorded a probe of this launch: it may still be running, or it was skipped (see the daemon log).";
const NO_REPORT: &str = "No check report was recorded for this launch (it ended under a daemon version that did not record one).";
const PROVEN: &str = "The launch's end was proven when it exited, so no check report was needed.";
const RESTARTED: &str = "the daemon restarted during the read";
const STOPPED: &str = "the daemon stopped during the read";

/// A wire enum's text.
fn wire<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn class(o: Observed) -> &'static str {
    match o.class() {
        ObservedClass::Dead => "dead",
        ObservedClass::Alive => "alive",
        ObservedClass::NotObserved => "not observed",
    }
}

fn meaning(u: Unchecked) -> &'static str {
    match u {
        Unchecked::UnrecordedSessionMembers => {
            "members of the old session without a recorded identity were not checked"
        }
        Unchecked::UnrecordedJobAndOutsideJob => {
            "processes without a recorded identity were not checked: members of the old job (ended when the old daemon exited, not observed) and processes outside the job"
        }
        Unchecked::TreeUnproven => {
            "the kill did not prove the process tree dead: processes it did not list may exist"
        }
        Unchecked::NoIdentity => "no process identity was recorded for the launch's leader",
        Unchecked::CleanupUnreleased => {
            "the tree was proven dead, but the cleanup of the failed start did not complete"
        }
        Unchecked::ResultNotReceived => "the check's result did not reach the daemon in time",
    }
}

/// What the backward scan kept of launch `L`, in seq order. The scan ends at its stop event
/// or interrupted: every other way out of it names why.
#[derive(Default)]
struct Scan {
    events: Vec<EventEnvelope>,
    /// Why it was interrupted, if it was.
    interrupted: Option<String>,
}

impl Client<'_> {
    /// The serving daemon's port and run `instance`, polled until the cutoff.
    fn serving(&self, cutoff: std::time::Instant) -> Result<(u16, String), End> {
        poll_until_with(&Cell::new(cutoff), self.period, self.now, || {
            match (self.discover)() {
                Ok(Found::Port(p, instance)) => Some(Ok((p, instance))),
                Ok(Found::NotRunning) => Some(Err(End::Fail(
                    "daemon not running; run `chaldea daemon start`".to_owned(),
                ))),
                Ok(Found::NotYet) => None,
                Err(m) => Some(Err(End::Fail(m))),
            }
        })
        .unwrap_or(Err(End::Timeout))
    }

    /// One backward page of `stream`, read from the daemon run `run`.
    fn page(
        &self,
        cutoff: std::time::Instant,
        run: &str,
        stream: StreamId,
        before: u64,
        limit: usize,
    ) -> Result<EventsResponse, End> {
        let path = format!("/api/events?stream_id={stream}&before_seq={before}&limit={limit}");
        self.exchange_in(
            cutoff,
            &Req::Get(&path),
            true,
            Some(run),
            &mut |r| match r.status {
                408 | 503 => None,
                200 => Some(
                    serde_json::from_slice::<EventsResponse>(&r.body)
                        .map_err(|e| End::Fail(format!("the events page is not readable: {e}"))),
                ),
                s => Some(Err(End::Fail(format!(
                    "the daemon answered the events read with {s} {}",
                    code_name(&r)
                )))),
            },
        )
    }

    /// The backward scan of §2.5 step 2 for launch `l` of thread `t`, from the run `run`.
    fn scan(&self, cutoff: std::time::Instant, run: &str, t: ThreadId, l: &LaunchInfo) -> Scan {
        let stream = StreamId::Thread(t);
        let to_reserved = l.exit_reason == Some(ExitReason::UnknownAfterCrash);
        let mut scan = Scan::default();
        let mut before = MAX_SAFE_INTEGER;
        let mut limit = FIRST_LIMIT;
        let mut epoch: Option<Epoch> = None;
        'pages: loop {
            let resp = match self.page(cutoff, run, stream, before, limit) {
                Ok(r) => r,
                Err(End::TooLarge) if limit > 1 => {
                    limit /= 2;
                    continue;
                }
                Err(End::TooLarge) => {
                    scan.interrupted = Some("an event is larger than the client reads".into());
                    break;
                }
                Err(End::Timeout) => {
                    scan.interrupted = Some("the daemon did not answer in time".into());
                    break;
                }
                Err(End::Fail(m)) => {
                    scan.interrupted = Some(m);
                    break;
                }
                Err(End::Restarted) => {
                    scan.interrupted = Some(RESTARTED.into());
                    break;
                }
                Err(End::Stopped) => {
                    scan.interrupted = Some(STOPPED.into());
                    break;
                }
                Err(End::Unverified) => {
                    scan.interrupted = Some(super::UNVERIFIED.into());
                    break;
                }
            };
            let EventsResponse::Replay {
                stream_id,
                epoch: page_epoch,
                mut events,
                has_more_before,
                ..
            } = resp
            else {
                scan.interrupted = Some("the daemon answered with a snapshot".into());
                break;
            };
            if stream_id != stream {
                scan.interrupted = Some(format!("the daemon answered for {stream_id}"));
                break;
            }
            if epoch.get_or_insert_with(|| page_epoch.clone()) != &page_epoch {
                scan.interrupted = Some("the history was reset (its epoch changed)".into());
                break;
            }
            if events.is_empty() {
                scan.interrupted = Some("the history ended before the launch's exit".into());
                break;
            }
            events.sort_by_key(|e| std::cmp::Reverse(e.seq));
            let lowest = events.last().map_or(before, |e| e.seq);
            for e in events {
                if e.launch_id != Some(l.launch_id) {
                    continue;
                }
                let kind = e.kind.as_str();
                let stop = match kind {
                    "launch_exited" => !to_reserved,
                    "launch_reserved" => to_reserved,
                    _ => false,
                };
                if matches!(
                    kind,
                    "launch_evidence"
                        | "launch_death_confirmed"
                        | "launch_exited"
                        | "launch_reserved"
                ) {
                    scan.events.push(e);
                }
                if stop {
                    break 'pages;
                }
            }
            if !has_more_before {
                scan.interrupted = Some("the history ended before the launch's exit".into());
                break;
            }
            before = lowest;
        }
        scan.events.sort_by_key(|e| e.seq);
        scan
    }

    /// §2.5 "Preview": exit 0 for a complete scan, 1 for an interrupted one.
    pub(crate) fn preview(
        &self,
        t: ThreadId,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> ExitCode {
        let cutoff = self.cutoff(PREVIEW_CUTOFF);
        // Step 1.
        let instance = match self.serving(cutoff) {
            Ok((_, i)) => i,
            Err(e) => return super::ended(e, err),
        };
        let summary = match self.summary_until(cutoff, t, true, Some(&instance), &|_| Some(Ok(())))
        {
            Ok(s) => s,
            Err(e) => return super::ended(e, err),
        };
        let Some(l) = summary.launch else {
            let _ = writeln!(out, "The thread has no launch.");
            return ExitCode::Ok;
        };
        let Some(reason) = l.exit_reason else {
            let _ = writeln!(
                out,
                "Launch {} has not exited; stop it with `chaldea stop {t}`.",
                l.launch_id
            );
            return ExitCode::Ok;
        };
        // Step 2.
        let mut scan = self.scan(cutoff, &instance, t, &l);
        // The re-check, from the same serving run: the same launch, exit, and death state.
        let again = self.summary_until(cutoff, t, true, Some(&instance), &|_| Some(Ok(())));
        // Whether `L` is still current and unconfirmed, as far as the re-check read.
        let mut unconfirmed = !l.death_confirmed;
        match again {
            Ok(s) => {
                let now = s.launch.as_ref();
                if now.is_some_and(|m| m.launch_id == l.launch_id && m.death_confirmed) {
                    unconfirmed = false;
                }
                if !now.is_some_and(|m| {
                    m.launch_id == l.launch_id
                        && m.exit_reason == l.exit_reason
                        && m.death_confirmed == l.death_confirmed
                }) {
                    scan.interrupted
                        .get_or_insert_with(|| "the launch changed during the read".into());
                }
            }
            Err(End::Restarted) => {
                scan.interrupted.get_or_insert_with(|| RESTARTED.into());
            }
            Err(End::Stopped) => {
                scan.interrupted.get_or_insert_with(|| STOPPED.into());
            }
            Err(End::Unverified) => {
                scan.interrupted
                    .get_or_insert_with(|| super::UNVERIFIED.into());
            }
            Err(_) => {
                scan.interrupted
                    .get_or_insert_with(|| "the daemon could not be read again".into());
            }
        }
        let complete = scan.interrupted.is_none();
        let text = render(t, &l, reason, &instance, &scan, complete, unconfirmed);
        let _ = out.write_all(text.as_bytes());
        if complete {
            ExitCode::Ok
        } else {
            ExitCode::Failed
        }
    }
}

/// §2.5 preview step 3.
#[allow(clippy::too_many_lines, reason = "one report, in its printed order")]
fn render(
    t: ThreadId,
    l: &LaunchInfo,
    reason: ExitReason,
    instance: &str,
    scan: &Scan,
    complete: bool,
    unconfirmed: bool,
) -> String {
    let mut o = String::new();
    let launch: LaunchId = l.launch_id;
    let typed: Vec<(&EventEnvelope, TypedEvent)> = scan
        .events
        .iter()
        .filter_map(|e| TypedEvent::decode(e).ok().map(|d| (e, d)))
        .collect();
    let evidence: Vec<(&EventEnvelope, &LaunchEvidence)> = typed
        .iter()
        .filter_map(|(e, d)| match d {
            TypedEvent::LaunchEvidence(p) => Some((*e, p)),
            _ => None,
        })
        .collect();
    let confirmation = typed.iter().find_map(|(e, d)| match d {
        TypedEvent::LaunchDeathConfirmed(p) => {
            Some((e.seq, e.recorded_at.0.clone(), p.method.clone()))
        }
        _ => None,
    });
    let _ = writeln!(o, "launch: {launch}");
    let _ = writeln!(o, "exit reason: {}", wire(&reason));
    match (&confirmation, l.death_confirmed) {
        (Some((_, _, m)), true) => {
            let _ = writeln!(o, "death: confirmed (method: {m})");
        }
        (_, true) => {
            let _ = writeln!(o, "death: confirmed");
        }
        (_, false) => {
            let _ = writeln!(o, "death: unconfirmed");
        }
    }
    // The recorded identity.
    match l.pid {
        Some(pid) => {
            let start = evidence.iter().rev().find_map(|(_, p)| {
                p.identities
                    .iter()
                    .find(|i| i.pid == pid)
                    .and_then(|i| i.start_time.clone())
            });
            match start {
                Some(s) => {
                    let _ = writeln!(o, "recorded identity: pid {pid}, start time {s}");
                }
                None => {
                    let _ = writeln!(
                        o,
                        "recorded identity: pid {pid}, start time: not read by this preview (recorded in `launch_started`)"
                    );
                }
            }
        }
        None => {
            let _ = writeln!(o, "recorded identity: none");
        }
    }
    let exited = typed.iter().find_map(|(_, d)| match d {
        TypedEvent::LaunchExited(p) => Some(p.clone()),
        _ => None,
    });
    if let Some(f) = exited.as_ref().and_then(|x| x.failed_start.as_ref()) {
        let _ = writeln!(
            o,
            "identity recorded at the failed start: pid {}, start time {}",
            f.pid,
            f.start_time.as_deref().unwrap_or("not recorded")
        );
    }
    // The evidence, in seq order; what follows a confirmation is listed under it.
    if evidence.is_empty() {
        let _ = writeln!(o, "evidence: none");
    } else {
        let _ = writeln!(o, "evidence:");
    }
    let mut under_confirmation = false;
    for (e, p) in &evidence {
        if let Some((seq, at, m)) = &confirmation
            && e.seq > *seq
            && !under_confirmation
        {
            under_confirmation = true;
            let _ = writeln!(o, "  after the confirmation by {m} at {at}:");
        }
        let indent = if under_confirmation { "    " } else { "  " };
        let run = match (&p.source, &p.probe_run) {
            (EvidenceSource::Probe, Some(r)) if r == instance => " (this daemon run)",
            (EvidenceSource::Probe, _) => " (an earlier daemon run)",
            _ => "",
        };
        let _ = writeln!(o, "{indent}{} {}{run}", e.recorded_at.0, wire(&p.source));
        for i in &p.identities {
            let _ = writeln!(
                o,
                "{indent}  pid {}, start time {}: {} ({})",
                i.pid,
                i.start_time.as_deref().unwrap_or("not recorded"),
                wire(&i.observed),
                class(i.observed)
            );
        }
        for u in &p.unchecked {
            let _ = writeln!(o, "{indent}  not covered: {}", meaning(*u));
        }
    }
    // What is missing.
    let this_run_probe = evidence.iter().any(|(_, p)| {
        p.source == EvidenceSource::Probe && p.probe_run.as_deref() == Some(instance)
    });
    // The prober's candidates (RD1): a `daemon_restart` leader, and a failed start's child
    // recorded with a start time.
    let probed = match reason {
        ExitReason::DaemonRestart => true,
        ExitReason::SpawnFailed => exited
            .as_ref()
            .and_then(|x| x.failed_start.as_ref())
            .and_then(|f| f.start_time.as_deref())
            .is_some_and(|s| !s.is_empty()),
        _ => false,
    };
    // A confirmed launch (by the summary or by an event the scan read) is no candidate
    // (RD1), so its missing probe says nothing.
    if probed && unconfirmed && confirmation.is_none() && !this_run_probe {
        let _ = writeln!(o, "{NO_PROBE}");
    }
    match reason {
        ExitReason::Stopped | ExitReason::Exit | ExitReason::SpawnFailed
            if complete && evidence.is_empty() =>
        {
            // Evidence is recorded only for an exit whose end was not proven.
            let proven = exited.as_ref().is_some_and(|x| x.death_confirmed)
                || confirmation
                    .as_ref()
                    .is_some_and(|(_, _, m)| m != OWNER_ASSERTED);
            let _ = writeln!(o, "{}", if proven { PROVEN } else { NO_REPORT });
        }
        ExitReason::UnknownAfterCrash => {
            let reserved = typed.iter().find_map(|(e, d)| match d {
                TypedEvent::LaunchReserved(p) => Some((wire(&p.agent), e.recorded_at.0.clone())),
                _ => None,
            });
            match reserved {
                Some((agent, at)) => {
                    let _ = writeln!(
                        o,
                        "No process identity was recorded (agent {agent}, reserved at {at})."
                    );
                }
                None => {
                    let _ = writeln!(o, "No process identity was recorded.");
                }
            }
        }
        _ => {}
    }
    if let Some(why) = &scan.interrupted {
        let _ = writeln!(
            o,
            "The history could not be read completely ({why}); the list above may be missing evidence."
        );
    }
    if unconfirmed && confirmation.is_none() {
        if complete {
            let _ = writeln!(
                o,
                "To confirm that this launch has ended: chaldea stop {t} --assume-dead --launch {launch}"
            );
        } else {
            let _ = writeln!(
                o,
                "Run `chaldea stop {t} --assume-dead` again to read the evidence before confirming that this launch has ended."
            );
        }
    }
    o
}
