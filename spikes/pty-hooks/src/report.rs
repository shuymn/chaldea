//! `report.json` (§4.9). A shared sink lets the watchdog write whatever is
//! known at the moment it fires.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::verdict::Outcome;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Check {
    pub id: String,
    pub case: String,
    /// `pass` | `platform_fail` | `harness_fail` | `missing` | `not_exercised`.
    pub result: String,
    /// Failure cause or not-exercised reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    pub evidence: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Probe {
    pub id: String,
    pub verdict: String,
    pub why: String,
    pub evidence: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Report {
    pub schema: String,
    pub scenario: String,
    pub run_id: String,
    pub status: String,
    pub host: Value,
    pub config: Value,
    pub checks: Vec<Check>,
    pub probes: Vec<Probe>,
    pub errors: Vec<String>,
    /// Cases this run must produce (`"<id> <case>"`); see `verdict`.
    #[serde(default)]
    pub required: Vec<String>,
    /// Scenario-specific raw facts (owned pids, teardown timing, ...).
    #[serde(default)]
    pub details: Value,
}

impl Report {
    pub fn new(scenario: &str, run_id: &str) -> Report {
        Report {
            schema: "chs-report/1".into(),
            scenario: scenario.into(),
            run_id: run_id.into(),
            status: "running".into(),
            host: json!({"os": crate::os::os_name(), "os_version": crate::os::os_version(),
                         "os_build": crate::os::os_build()}),
            config: json!({}),
            details: json!({}),
            ..Default::default()
        }
    }

    pub fn check(&mut self, id: &str, case: &str, outcome: Outcome, evidence: Value) {
        self.checks.push(Check {
            id: id.into(),
            case: case.into(),
            result: outcome.tag().into(),
            cause: outcome.cause(),
            evidence,
        });
    }

    /// Status from the required cases and checks (`verdict::status`); probes
    /// count as checks with go → pass, fail_observed → platform failure,
    /// anything else → not exercised.
    pub fn derive_status(&self) -> String {
        let mut checks = self.checks.clone();
        for p in &self.probes {
            let o = match p.verdict.as_str() {
                "go" => Outcome::Pass,
                "fail_observed" => Outcome::platform(p.why.clone()),
                _ => Outcome::not_exercised(p.why.clone()),
            };
            checks.push(Check {
                id: p.id.clone(),
                case: "verdict".into(),
                result: o.tag().into(),
                cause: o.cause(),
                evidence: Value::Null,
            });
        }
        crate::verdict::status(&self.required, &checks, &self.errors).into()
    }

    pub fn require(&mut self, id: &str, case: &str) {
        let k = crate::verdict::key(id, case);
        if !self.required.contains(&k) {
            self.required.push(k);
        }
    }

    pub fn detail(&mut self, key: &str, v: Value) {
        if let Value::Object(m) = &mut self.details {
            m.insert(key.into(), v);
        }
    }
}

/// Called between a snapshot's serialization and its publication (tests
/// force an interleaving with it).
pub type PublishHook = Arc<dyn Fn() + Send + Sync>;

/// Report shared between the scenario and the watchdog.
#[derive(Clone)]
pub struct Sink {
    pub path: PathBuf,
    pub report: Arc<Mutex<Report>>,
    pub publish_hook: Option<PublishHook>,
}

impl Sink {
    pub fn new(out_dir: &Path, report: Report) -> Sink {
        Sink {
            path: out_dir.join("report.json"),
            report: Arc::new(Mutex::new(report)),
            publish_hook: None,
        }
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut Report) -> R) -> R {
        let mut g = self.report.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut g)
    }

    /// Publishes the current snapshot. The report's lock is held from
    /// serialization through the rename, so every writer (the sweep's
    /// checkpoints, the watchdog) publishes whole snapshots in order and the
    /// file ends with the latest one. A failed write is recorded in the
    /// report's `errors` and published again; if that fails too, it goes to
    /// stderr.
    pub fn write(&self) -> std::io::Result<()> {
        let mut g = self.report.lock().unwrap_or_else(|e| e.into_inner());
        let r = self.publish(&g);
        if let Err(e) = &r {
            g.errors.push(format!(
                "report write failed ({}): {e}",
                self.path.display()
            ));
            if let Err(e2) = self.publish(&g) {
                eprintln!(
                    "chs: report {} could not be written: {e}; then: {e2}",
                    self.path.display()
                );
            }
        }
        r
    }

    fn publish(&self, r: &Report) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(r).map_err(std::io::Error::other)?;
        if let Some(h) = &self.publish_hook {
            h();
        }
        crate::util::write_atomic(&self.path, &bytes)
    }
}

pub fn read_report(path: &Path) -> Option<Report> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// M2: a checkpoint and the watchdog's final snapshot, forced to
    /// interleave (the checkpoint pauses after serializing while the
    /// watchdog writes), leave valid JSON holding the latest snapshot.
    #[test]
    fn concurrent_writers_publish_the_latest_snapshot() {
        let dir = std::env::temp_dir().join(format!("chs-sink-{}", crate::util::token()));
        std::fs::create_dir_all(&dir).unwrap();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let done_rx = Mutex::new(done_rx);
        let first = std::sync::atomic::AtomicBool::new(true);
        let mut sink = Sink::new(&dir, Report::new("x", "00000000"));
        sink.publish_hook = Some(Arc::new(move || {
            if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                // The checkpoint has serialized: let the watchdog run, and
                // give it time to finish if nothing holds it back.
                let _ = go_tx.send(());
                let _ = done_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_millis(500));
            }
        }));
        let checkpoint = {
            let sink = sink.clone();
            std::thread::spawn(move || sink.write())
        };
        go_rx.recv().unwrap();
        let watchdog = {
            let sink = sink.clone();
            std::thread::spawn(move || {
                sink.with(|r| {
                    r.status = "timeout".into();
                    r.errors.push("hard deadline".into());
                });
                let w = sink.write();
                let _ = done_tx.send(());
                w
            })
        };
        checkpoint.join().unwrap().unwrap();
        watchdog.join().unwrap().unwrap();
        let r = read_report(&dir.join("report.json")).expect("valid JSON");
        assert_eq!(r.status, "timeout");
        assert_eq!(r.errors, ["hard deadline"]);
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() != "report.json")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// M2: a write that fails is recorded in the report's `errors`.
    #[test]
    fn failed_write_is_recorded() {
        let dir = std::env::temp_dir().join(format!("chs-sink-missing-{}", crate::util::token()));
        let sink = Sink::new(&dir, Report::new("x", "00000000"));
        assert!(sink.write().is_err());
        let errors = sink.with(|r| r.errors.clone());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("report write failed"), "{errors:?}");
    }
}
