//! End-to-end regression tests that drive the built `chs` binary against a
//! real PTY. Each uses a fresh root under the OS temp dir.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use chs::report::{Report, read_report};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("chs-it-{name}-{}", chs::util::run_id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn chs(dir: &Path, args: &[&str]) -> (Option<i32>, Duration, Report) {
    let out = dir.join("out");
    let t = Instant::now();
    let st = Command::new(env!("CARGO_BIN_EXE_chs"))
        .args(args)
        .arg("--root")
        .arg(dir.join("root"))
        .arg("--out")
        .arg(&out)
        .env("CODEX_HOME", dir.join("codex-home"))
        .status()
        .unwrap();
    let rep = read_report(&out.join("report.json")).expect("report.json written");
    (st.code(), t.elapsed(), rep)
}

fn check<'a>(r: &'a Report, id: &str, case: &str) -> &'a chs::report::Check {
    r.checks
        .iter()
        .find(|c| c.id == id && c.case == case)
        .unwrap_or_else(|| panic!("{id} {case} missing: {:#?}", r.checks))
}

/// B-7 / C8: the primary mechanism (killpg / job) kills the group members, the
/// recorded-pid fallback kills the session escapee, and the harness-started
/// `unrelated` sleeper survives.
#[test]
fn tree_kill_primary_fallback_and_negative_control() {
    let d = tmp("tree");
    let (code, _, r) = chs(&d, &["fake", "tree-kill"]);
    assert_eq!(r.status, "pass", "{:#?}", r.checks);
    assert_eq!(code, Some(0));
    for role in chs::os::spawn_tree_roles() {
        let c = check(&r, "B-7", &format!("tree/{role}"));
        assert_eq!(c.result, "pass", "{c:#?}");
        let by = c.evidence["killed_by"].as_str().unwrap();
        let want = if *role == "setsid" || *role == "breakaway" {
            "fallback"
        } else {
            "primary"
        };
        if chs::os::os_name() != "windows" {
            assert_eq!(by, want, "{role}: {c:#?}");
        }
    }
    assert_eq!(
        check(&r, "B-7", "tree/unrelated_negative_control").result,
        "pass"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// B-10 / C6: a child that stops reading mid-64 KiB paste neither deadlocks the
/// scenario nor the teardown.
#[test]
fn non_reading_child_does_not_hang_teardown() {
    let d = tmp("nonread");
    let (code, elapsed, r) = chs(
        &d,
        &[
            "fake",
            "teardown-case",
            "--case",
            "non-reading-child",
            "--deadline-secs",
            "30",
        ],
    );
    assert_eq!(r.status, "pass", "{:#?} {:?}", r.checks, r.errors);
    assert_eq!(code, Some(0));
    assert!(elapsed < Duration::from_secs(15), "{elapsed:?}");
    let ev = &check(&r, "B-10", "non-reading-child").evidence;
    assert_eq!(ev["paste"]["state"], "write_timeout");
    assert_eq!(ev["stop"]["survivors"].as_array().unwrap().len(), 0);
    let _ = std::fs::remove_dir_all(&d);
}

/// B-10: a scenario that ignores its cutoff is ended by the watchdog inside the
/// hard deadline, the tree is killed, and report.json is written.
#[test]
fn watchdog_kills_tree_at_hard_deadline() {
    let d = tmp("hard");
    let (code, elapsed, r) = chs(
        &d,
        &[
            "fake",
            "teardown-case",
            "--case",
            "hard-deadline",
            "--deadline-secs",
            "12",
        ],
    );
    assert_eq!(r.status, "timeout", "{:?}", r.errors);
    assert_eq!(code, Some(124));
    assert!(elapsed <= Duration::from_millis(12_500), "{elapsed:?}");
    let killed = r.details["watchdog"]["killed"].as_array().unwrap();
    assert!(!killed.is_empty());
    for k in killed {
        assert!(!k["recorded"].as_array().unwrap().is_empty());
        for p in k["recorded"].as_array().unwrap() {
            let (pid, st) = (
                p["pid"].as_u64().unwrap() as u32,
                p["start_time"].as_u64().unwrap(),
            );
            assert!(!chs::os::is_alive(pid, st), "survivor {p}");
        }
    }
    let _ = std::fs::remove_dir_all(&d);
}
