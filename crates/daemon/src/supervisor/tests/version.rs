//! The CLI version probe (C1 D11): `claude --version` before the reservation, its version in
//! `launch_started` and on the launch row; a failure, an exit other than 0, output over the
//! cap, or the deadline records `null`. The child is FakeCli answering `--version` (both
//! OSes), and the deadline is passed on the rig's stopped clock, not in real time.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chaldea_core::ids::{LaunchId, ThreadId};
use serde_json::json;

use super::{Rig, json as body};
use crate::store::tests::support::waits;
use crate::supervisor::version::{MAX_OUTPUT, VERSION_PROBE, probe};

/// FakeCli, built by `cargo test --workspace` (testkit's binary) next to the test binaries'
/// `deps/` directory.
pub(crate) fn fake_cli() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap();
    let p = dir.join(format!("chaldea-fake-cli{}", std::env::consts::EXE_SUFFIX));
    assert!(
        p.is_file(),
        "{} is missing: build with cargo test --workspace",
        p.display()
    );
    p
}

const CLAUDE_VERSION: &str = "2.1.331 (Claude Code)";

/// FakeCli `sleeper` with `flags`, ending in `--` so the probe's `--version` follows it.
pub(crate) fn fake_program(flags: &[&str]) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec![fake_cli().into(), "--mode".into(), "sleeper".into()];
    argv.extend(flags.iter().map(OsString::from));
    argv.push("--".into());
    argv
}

async fn rig_running(flags: &'static [&'static str]) -> Rig {
    Rig::with(chaldea_core::adapter::HostOs::Macos, move |c| {
        let argv = fake_program(flags);
        c.program = Some(Arc::new(move |_| Some(argv.clone())));
    })
    .await
}

async fn spawn(rig: &Rig, agent: &str) -> (ThreadId, LaunchId) {
    let cwd = rig.cwd.path().to_string_lossy().into_owned();
    let r = rig
        .cmd("spawn", &json!({ "agent": agent, "cwd": cwd }))
        .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let v = body(&r);
    assert_eq!(v["outcome"], "started", "{v}");
    (
        serde_json::from_value(v["thread_id"].clone()).unwrap(),
        serde_json::from_value(v["launch_id"].clone()).unwrap(),
    )
}

fn started_versions(rig: &Rig, t: ThreadId) -> Vec<serde_json::Value> {
    rig.events(t)
        .into_iter()
        .filter(|(k, _)| k == "launch_started")
        .map(|(_, p)| p["cli_version"].clone())
        .collect()
}

#[test]
fn probe_bound_is_five_seconds() {
    assert_eq!(VERSION_PROBE, Duration::from_secs(5));
    assert_eq!(MAX_OUTPUT, 4096);
}

/// A Claude launch records the probe's version in `launch_started` and on its launch row
/// (where hook `raw_unknown` events take it from), at spawn and again at resume; a Codex
/// launch records `null`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn version_recorded() {
    let rig = rig_running(&["--version-text", CLAUDE_VERSION]).await;
    let (t, l) = spawn(&rig, "claude").await;
    assert_eq!(started_versions(&rig, t), [json!("2.1.331")]);
    assert_eq!(rig.launch(l).cli_version.as_deref(), Some("2.1.331"));
    rig.bind(t, l, &l.to_string()).await;
    let r = rig.cmd("stop", &json!({ "thread_id": t })).await;
    assert_eq!(r.status, 200);
    let r = rig
        .cmd(
            "resume",
            &json!({ "thread_id": t, "expected_launch_id": l }),
        )
        .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let l2: LaunchId = serde_json::from_value(body(&r)["launch_id"].clone()).unwrap();
    assert_eq!(
        started_versions(&rig, t),
        [json!("2.1.331"), json!("2.1.331")]
    );
    assert_eq!(rig.launch(l2).cli_version.as_deref(), Some("2.1.331"));
    let (t, l) = spawn(&rig, "codex").await;
    assert_eq!(started_versions(&rig, t), [json!(null)]);
    assert_eq!(rig.launch(l).cli_version, None);
    rig.srv.stop().await;
}

/// A probe that cannot run, or exits other than 0, records `null` and the launch proceeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn version_failure_is_null() {
    // The rig's program does not exist on the host.
    let rig = Rig::new().await;
    let (t, _) = spawn(&rig, "claude").await;
    assert_eq!(started_versions(&rig, t), [json!(null)]);
    rig.srv.stop().await;
    let rig = rig_running(&["--version-text", CLAUDE_VERSION, "--version-exit", "3"]).await;
    let (t, _) = spawn(&rig, "claude").await;
    assert_eq!(started_versions(&rig, t), [json!(null)]);
    rig.srv.stop().await;
}

/// Runs the probe of FakeCli with `flags` on the rig's stopped clock, its deadline
/// [`VERSION_PROBE`] ahead; `at_spawn` runs once the child is tracked.
async fn probed(rig: &Rig, flags: &[&str], at_spawn: impl FnOnce()) -> Option<String> {
    rig.clock.stop();
    let argv = fake_program(flags);
    let sup = rig.sup.clone();
    let deadline = sup.now() + VERSION_PROBE;
    let mut task = tokio::task::spawn_blocking(move || probe(&sup, &argv, deadline));
    rig.until("the probe's child is tracked", || {
        rig.sup.tracker.count() == 1
    })
    .await;
    at_spawn();
    // Real time bounds only a hang: on the stopped clock the deadline passes only when moved.
    // At the bound, the clock is moved past the deadline so the probe ends and the test fails
    // rather than waiting for the child.
    let Ok(v) = waits::bounded(Duration::from_secs(30), &mut task).await else {
        rig.clock.advance(VERSION_PROBE);
        panic!("the probe did not return before its deadline");
    };
    v.unwrap()
}

/// The child gone and its token released: nothing of the probe is left.
async fn nothing_left(rig: &Rig) {
    rig.until("the probe's child is reaped", || {
        rig.sup.version_left.reap();
        rig.sup.version_left.is_empty() && rig.sup.tracker.count() == 0
    })
    .await;
}

/// A child that does not exit gives `None` once the deadline passes (the clock moved to it),
/// killed and reaped; before that, nothing returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn version_timeout() {
    let rig = Rig::new().await;
    let v = probed(&rig, &["--version-sleep-ms", "600000"], || {
        rig.clock.advance(VERSION_PROBE);
    })
    .await;
    assert_eq!(v, None);
    nothing_left(&rig).await;
    rig.srv.stop().await;
}

/// A descendant holding stdout after the child exited 0 does not delay the probe: the
/// version, on a clock that never reaches the deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn version_descendant_holds_stdout() {
    let rig = Rig::new().await;
    let v = probed(
        &rig,
        &["--version-text", CLAUDE_VERSION, "--version-descendant"],
        || {},
    )
    .await;
    assert_eq!(v.as_deref(), Some("2.1.331"));
    nothing_left(&rig).await;
    rig.srv.stop().await;
}

/// An exit other than 0 gives `None` at once, a descendant holding stdout included.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn version_nonzero_exit_is_immediate() {
    let rig = Rig::new().await;
    let v = probed(
        &rig,
        &[
            "--version-text",
            CLAUDE_VERSION,
            "--version-exit",
            "3",
            "--version-descendant",
        ],
        || {},
    )
    .await;
    assert_eq!(v, None);
    nothing_left(&rig).await;
    rig.srv.stop().await;
}

/// Output of exactly [`MAX_OUTPUT`] bytes gives the version; one byte more gives `None`, as
/// does far more, and so does a child still running past the cap, none waiting for the
/// deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn version_output_capped() {
    let rig = Rig::new().await;
    // The text and its newline, then `x`s.
    let fill = usize::try_from(MAX_OUTPUT).unwrap() - CLAUDE_VERSION.len() - 1;
    let at_cap = fill.to_string();
    let over = (fill + 1).to_string();
    // A child still running past the cap (it lingers 600 s) gives `None` too.
    for (flood, linger, want) in [
        (at_cap.as_str(), "0", Some("2.1.331")),
        (over.as_str(), "0", None),
        ("1048576", "0", None),
        (over.as_str(), "600000", None),
    ] {
        let v = probed(
            &rig,
            &[
                "--version-text",
                CLAUDE_VERSION,
                "--version-flood",
                flood,
                "--version-linger-ms",
                linger,
            ],
            || {},
        )
        .await;
        assert_eq!(v.as_deref(), want, "{flood} {linger}");
        nothing_left(&rig).await;
        rig.clock.resume();
    }
    rig.srv.stop().await;
}
