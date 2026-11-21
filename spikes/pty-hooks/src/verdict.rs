//! The single place that turns case outcomes into a scenario `status` or a
//! probe `verdict`. Completeness is checked first: any required case that is
//! missing, harness-failed, or not exercised makes the result `inconclusive`,
//! even when another case failed. Only then does every case passing give
//! `pass` / `go`, and anything else `fail` / `fail_observed`.
//!
//! The harness never classifies a platform failure as a blocker or tuning
//! issue; `fail_observed` evidence is classified manually against design §1 C.

use serde::Serialize;
use serde_json::{Value, json};

use crate::report::Check;

pub fn key(id: &str, case: &str) -> String {
    format!("{id} {case}")
}

/// One case's outcome. `PlatformFail` needs positive evidence of what the
/// platform did (bytes compared and differing, a successful enumeration that
/// shows an escapee, positive death evidence); its evidence goes in the
/// check's `evidence`. A missing hook, an enumeration or lookup failure, or a
/// relay / gate / writer / observer failure is `HarnessFail` or `Missing`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    PlatformFail { cause: String },
    HarnessFail { cause: String },
    Missing,
    NotExercised { reason: String },
}

impl Outcome {
    pub fn platform(cause: impl Into<String>) -> Outcome {
        Outcome::PlatformFail {
            cause: cause.into(),
        }
    }
    pub fn harness(cause: impl Into<String>) -> Outcome {
        Outcome::HarnessFail {
            cause: cause.into(),
        }
    }
    pub fn not_exercised(reason: impl Into<String>) -> Outcome {
        Outcome::NotExercised {
            reason: reason.into(),
        }
    }
    /// `Pass` when `ok`, else a platform failure with `cause` (use only where
    /// the comparison itself is the positive evidence).
    pub fn observed(ok: bool, cause: impl Into<String>) -> Outcome {
        if ok {
            Outcome::Pass
        } else {
            Outcome::platform(cause)
        }
    }
    pub fn tag(&self) -> &'static str {
        match self {
            Outcome::Pass => "pass",
            Outcome::PlatformFail { .. } => "platform_fail",
            Outcome::HarnessFail { .. } => "harness_fail",
            Outcome::Missing => "missing",
            Outcome::NotExercised { .. } => "not_exercised",
        }
    }
    pub fn cause(&self) -> Option<String> {
        match self {
            Outcome::PlatformFail { cause } | Outcome::HarnessFail { cause } => Some(cause.clone()),
            Outcome::NotExercised { reason } => Some(reason.clone()),
            _ => None,
        }
    }
    /// Reads an outcome back from a report check (unknown tags are missing).
    pub fn from_check(c: &Check) -> Outcome {
        let cause = c.cause.clone().unwrap_or_default();
        match c.result.as_str() {
            "pass" => Outcome::Pass,
            "platform_fail" => Outcome::PlatformFail { cause },
            "harness_fail" => Outcome::HarnessFail { cause },
            "not_exercised" => Outcome::NotExercised { reason: cause },
            _ => Outcome::Missing,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Eval {
    pub missing: Vec<String>,
    pub harness_fail: Vec<(String, String)>,
    pub not_exercised: Vec<(String, String)>,
    pub platform_fail: Vec<(String, String)>,
    pub passed: Vec<String>,
}

impl Eval {
    /// Every required case (and every extra case) is present with a
    /// definite outcome: pass or a platform failure.
    pub fn complete(&self) -> bool {
        self.missing.is_empty() && self.harness_fail.is_empty() && self.not_exercised.is_empty()
    }
    pub fn json(&self) -> Value {
        json!(self)
    }
    fn add(&mut self, k: String, o: Outcome) {
        match o {
            Outcome::Pass => self.passed.push(k),
            Outcome::PlatformFail { cause } => self.platform_fail.push((k, cause)),
            Outcome::HarnessFail { cause } => self.harness_fail.push((k, cause)),
            Outcome::Missing => self.missing.push(k),
            Outcome::NotExercised { reason } => self.not_exercised.push((k, reason)),
        }
    }
}

/// Evaluates `required` against `checks` (the last check per key wins);
/// checks outside the list count too.
pub fn evaluate(required: &[String], checks: &[Check]) -> Eval {
    let mut e = Eval::default();
    let last = |k: &str| checks.iter().rev().find(|c| key(&c.id, &c.case) == k);
    for k in required {
        let o = last(k).map(Outcome::from_check).unwrap_or(Outcome::Missing);
        e.add(k.clone(), o);
    }
    let mut seen: Vec<String> = Vec::new();
    for c in checks.iter().rev() {
        let k = key(&c.id, &c.case);
        if required.contains(&k) || seen.contains(&k) {
            continue;
        }
        seen.push(k.clone());
        e.add(k, Outcome::from_check(c));
    }
    e
}

fn decide(e: &Eval, nothing: bool) -> Option<bool> {
    if !e.complete() || nothing {
        None
    } else {
        Some(e.platform_fail.is_empty())
    }
}

/// Scenario status: errors → `error`; incomplete (or nothing checked) →
/// `inconclusive`; all pass → `pass`; else `fail`.
pub fn status(required: &[String], checks: &[Check], errors: &[String]) -> &'static str {
    if !errors.is_empty() {
        return "error";
    }
    match decide(&evaluate(required, checks), checks.is_empty()) {
        None => "inconclusive",
        Some(true) => "pass",
        Some(false) => "fail",
    }
}

/// Probe verdict: `inconclusive` | `go` | `fail_observed`, never `blocker`.
pub fn probe(required: &[String], checks: &[Check]) -> (&'static str, String, Value) {
    let e = evaluate(required, checks);
    let (v, why) = match decide(&e, required.is_empty() && checks.is_empty()) {
        None => (
            "inconclusive",
            format!(
                "incomplete: missing {:?}; harness_fail {:?}; not_exercised {:?}",
                e.missing, e.harness_fail, e.not_exercised
            ),
        ),
        Some(true) => ("go", "every required case present and passed".to_string()),
        Some(false) => (
            "fail_observed",
            format!(
                "platform failures observed (classified manually from the raw evidence): {:?}",
                e.platform_fail
            ),
        ),
    };
    (v, why, e.json())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: &str, case: &str, o: Outcome) -> Check {
        Check {
            id: id.into(),
            case: case.into(),
            result: o.tag().into(),
            cause: o.cause(),
            evidence: Value::Null,
        }
    }

    fn req(keys: &[(&str, &str)]) -> Vec<String> {
        keys.iter().map(|(i, c)| key(i, c)).collect()
    }

    #[test]
    fn missing_case_is_inconclusive() {
        let r = req(&[("A-6", "cr/a"), ("A-6", "cr/b")]);
        let checks = vec![c("A-6", "cr/a", Outcome::Pass)];
        assert_eq!(status(&r, &checks, &[]), "inconclusive");
        assert_eq!(probe(&r, &checks).0, "inconclusive");
    }

    /// Completeness wins: a failed case plus a missing suffix is inconclusive.
    #[test]
    fn completeness_wins_over_a_failing_case() {
        let r = req(&[("C", "a"), ("C", "b")]);
        let checks = vec![c("C", "a", Outcome::platform("bytes differ"))];
        assert_eq!(status(&r, &checks, &[]), "inconclusive");
        assert_eq!(probe(&r, &checks).0, "inconclusive");
        let checks = vec![
            c("C", "a", Outcome::platform("bytes differ")),
            c("C", "b", Outcome::not_exercised("gate never opened")),
        ];
        assert_eq!(probe(&r, &checks).0, "inconclusive");
    }

    #[test]
    fn harness_fail_never_yields_fail_observed() {
        let r = req(&[("C", "a"), ("C", "b")]);
        for other in [Outcome::Pass, Outcome::platform("x")] {
            let checks = vec![
                c("C", "a", Outcome::harness("writer failed")),
                c("C", "b", other),
            ];
            assert_eq!(probe(&r, &checks).0, "inconclusive");
            assert_eq!(status(&r, &checks, &[]), "inconclusive");
        }
        // Outside the required list too.
        let checks = vec![
            c("C", "a", Outcome::platform("x")),
            c("C", "b", Outcome::Pass),
            c("C", "extra", Outcome::harness("lookup failed")),
        ];
        assert_eq!(probe(&r, &checks).0, "inconclusive");
    }

    #[test]
    fn complete_outcomes_give_go_or_fail_observed() {
        let r = req(&[("C", "a"), ("C", "b")]);
        let pass = vec![c("C", "a", Outcome::Pass), c("C", "b", Outcome::Pass)];
        assert_eq!(probe(&r, &pass).0, "go");
        assert_eq!(status(&r, &pass, &[]), "pass");
        let one = vec![
            c("C", "a", Outcome::platform("escapee")),
            c("C", "b", Outcome::Pass),
        ];
        assert_eq!(probe(&r, &one).0, "fail_observed");
        assert_eq!(status(&r, &one, &[]), "fail");
    }

    /// No combination of outcomes yields `blocker` (or anything outside the three verdicts).
    #[test]
    fn no_path_yields_blocker() {
        let all = [
            Outcome::Pass,
            Outcome::platform("p"),
            Outcome::harness("h"),
            Outcome::Missing,
            Outcome::not_exercised("n"),
        ];
        let r = req(&[("C", "a"), ("C", "b")]);
        for a in &all {
            for b in &all {
                let checks = vec![c("C", "a", a.clone()), c("C", "b", b.clone())];
                let v = probe(&r, &checks).0;
                assert!(["go", "inconclusive", "fail_observed"].contains(&v), "{v}");
            }
        }
        assert_eq!(probe(&[], &[]).0, "inconclusive");
    }
}
