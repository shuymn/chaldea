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
            host: json!({"os": crate::os::os_name(), "os_version": crate::os::os_version()}),
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

/// Report shared between the scenario and the watchdog.
#[derive(Clone)]
pub struct Sink {
    pub path: PathBuf,
    pub report: Arc<Mutex<Report>>,
}

impl Sink {
    pub fn new(out_dir: &Path, report: Report) -> Sink {
        Sink {
            path: out_dir.join("report.json"),
            report: Arc::new(Mutex::new(report)),
        }
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut Report) -> R) -> R {
        let mut g = self.report.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut g)
    }

    pub fn write(&self) -> std::io::Result<()> {
        let bytes = self.with(|r| serde_json::to_vec_pretty(r).unwrap_or_default());
        crate::util::write_atomic(&self.path, &bytes)
    }
}

pub fn read_report(path: &Path) -> Option<Report> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}
