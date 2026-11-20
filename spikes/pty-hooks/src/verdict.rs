//! The single place that turns evidence into a scenario `status` or a probe
//! `verdict`. Every scenario and probe declares the cases it requires up
//! front; missing, setup-failed, readiness-failed, or timed-out evidence is
//! `inconclusive`, never `pass` / `go` and never `blocker`.

use serde::Serialize;
use serde_json::{Value, json};

use crate::report::Check;

pub fn key(id: &str, case: &str) -> String {
    format!("{id} {case}")
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Eval {
    pub missing: Vec<String>,
    pub failed: Vec<String>,
    pub inconclusive: Vec<String>,
    pub passed: Vec<String>,
    /// Checks outside the required list that failed or were inconclusive.
    pub extra_failed: Vec<String>,
    pub extra_inconclusive: Vec<String>,
}

impl Eval {
    /// Every required case is present with a definite (pass/fail) result.
    pub fn complete(&self) -> bool {
        self.missing.is_empty() && self.inconclusive.is_empty()
    }

    pub fn all_pass(&self) -> bool {
        self.complete()
            && self.failed.is_empty()
            && self.extra_failed.is_empty()
            && self.extra_inconclusive.is_empty()
            && !self.passed.is_empty()
    }

    pub fn result_of(&self, k: &str) -> &'static str {
        if self.passed.iter().any(|p| p == k) {
            "pass"
        } else if self.failed.iter().any(|p| p == k) {
            "fail"
        } else if self.inconclusive.iter().any(|p| p == k) {
            "inconclusive"
        } else {
            "missing"
        }
    }

    /// The blocker shape: every listed variant is present and definitely failed.
    pub fn all_present_and_failed(&self, keys: &[String]) -> bool {
        !keys.is_empty() && keys.iter().all(|k| self.result_of(k) == "fail")
    }

    pub fn json(&self) -> Value {
        json!(self)
    }
}

/// Evaluates `required` against `checks` (the last check per key wins).
pub fn evaluate(required: &[String], checks: &[Check]) -> Eval {
    let mut e = Eval::default();
    let last = |k: &str| checks.iter().rev().find(|c| key(&c.id, &c.case) == k);
    for k in required {
        match last(k).map(|c| c.result.as_str()) {
            None => e.missing.push(k.clone()),
            Some("pass") => e.passed.push(k.clone()),
            Some("fail") => e.failed.push(k.clone()),
            Some(_) => e.inconclusive.push(k.clone()),
        }
    }
    for c in checks {
        let k = key(&c.id, &c.case);
        if required.contains(&k) {
            continue;
        }
        match c.result.as_str() {
            "pass" => {}
            "fail" => e.extra_failed.push(k),
            _ => e.extra_inconclusive.push(k),
        }
    }
    e
}

/// Scenario status: errors → error; any failure → fail; anything missing or
/// inconclusive (or nothing checked at all) → inconclusive; else pass.
pub fn status(required: &[String], checks: &[Check], errors: &[String]) -> &'static str {
    if !errors.is_empty() {
        return "error";
    }
    let e = evaluate(required, checks);
    if !e.failed.is_empty() || !e.extra_failed.is_empty() {
        "fail"
    } else if !e.complete() || !e.extra_inconclusive.is_empty() || checks.is_empty() {
        "inconclusive"
    } else {
        "pass"
    }
}

/// Probe verdict. `blocker` / `tuning` are the probe's documented conditions,
/// computed by the caller from `Eval` (a blocker must be demonstrated across
/// all of its required variants; use `Eval::all_present_and_failed`).
pub fn probe(
    required: &[String],
    checks: &[Check],
    blocker: impl Fn(&Eval) -> Option<String>,
    tuning: impl Fn(&Eval) -> Option<String>,
) -> (&'static str, String, Value) {
    let e = evaluate(required, checks);
    let (v, why) = if e.all_pass() {
        ("go", "every required case present and passed".to_string())
    } else if let Some(b) = blocker(&e) {
        ("blocker", b)
    } else if !e.complete() {
        (
            "inconclusive",
            format!(
                "missing: {:?}; inconclusive: {:?}",
                e.missing, e.inconclusive
            ),
        )
    } else if let Some(t) = tuning(&e) {
        ("tuning", t)
    } else {
        (
            "inconclusive",
            format!(
                "required cases failed without demonstrating the blocker or tuning condition: {:?}",
                e.failed
            ),
        )
    };
    (v, why, e.json())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: &str, case: &str, r: &str) -> Check {
        Check {
            id: id.into(),
            case: case.into(),
            result: r.into(),
            evidence: Value::Null,
        }
    }

    fn req(keys: &[(&str, &str)]) -> Vec<String> {
        keys.iter().map(|(i, c)| key(i, c)).collect()
    }

    #[test]
    fn missing_case_is_inconclusive() {
        let r = req(&[("A-6", "cr/a"), ("A-6", "cr/b")]);
        let checks = vec![c("A-6", "cr/a", "pass")];
        assert_eq!(status(&r, &checks, &[]), "inconclusive");
        let (v, _, _) = probe(&r, &checks, |_| None, |_| None);
        assert_eq!(v, "inconclusive");
    }

    #[test]
    fn all_readiness_failed_is_inconclusive_not_blocker() {
        let r = req(&[("A-6", "cr/a"), ("A-6", "lf/a")]);
        let checks = vec![
            c("A-6", "cr/a", "inconclusive"),
            c("A-6", "lf/a", "inconclusive"),
        ];
        let all = r.clone();
        let (v, _, _) = probe(
            &r,
            &checks,
            |e| e.all_present_and_failed(&all).then(|| "corrupted".into()),
            |_| Some("t".into()),
        );
        assert_eq!(v, "inconclusive");
        assert_eq!(status(&r, &checks, &[]), "inconclusive");
    }

    #[test]
    fn one_failing_required_case_is_not_go() {
        let r = req(&[("B-7", "x"), ("B-7", "y")]);
        let checks = vec![c("B-7", "x", "pass"), c("B-7", "y", "fail")];
        let (v, _, _) = probe(&r, &checks, |_| None, |_| None);
        assert_ne!(v, "go");
        assert_eq!(status(&r, &checks, &[]), "fail");
    }

    #[test]
    fn complete_pass_is_go_and_blocker_needs_all_variants() {
        let r = req(&[("C", "a"), ("C", "b")]);
        let pass = vec![c("C", "a", "pass"), c("C", "b", "pass")];
        assert_eq!(probe(&r, &pass, |_| None, |_| None).0, "go");
        let half = vec![c("C", "a", "fail"), c("C", "b", "pass")];
        let all = r.clone();
        let b = |e: &Eval| e.all_present_and_failed(&all).then(|| "b".to_string());
        assert_eq!(probe(&r, &half, b, |_| None).0, "inconclusive");
        let both = vec![c("C", "a", "fail"), c("C", "b", "fail")];
        assert_eq!(probe(&r, &both, b, |_| None).0, "blocker");
    }
}
