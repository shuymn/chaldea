//! The CLI version probe (C1 D11): `claude --version` before the reservation, its version in
//! `launch_started` and in hook `raw_unknown` events; a failure or timeout records `null`.

use std::ffi::OsString;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chaldea_core::ids::{LaunchId, ThreadId};
use serde_json::json;

use super::{Rig, json as body};
use crate::supervisor::version::{VERSION_PROBE, probe};

/// A shell running `script`: the probe appends `--version`, which becomes `$0` (POSIX) or an
/// ignored trailing word (`cmd /c echo`).
fn shell(script: &str) -> Vec<OsString> {
    if cfg!(windows) {
        let comspec = std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());
        vec![comspec, "/d".into(), "/c".into(), script.into()]
    } else {
        vec!["/bin/sh".into(), "-c".into(), script.into()]
    }
}

const PRINT_VERSION: &str = if cfg!(windows) {
    "echo 2.1.331 (Claude Code)"
} else {
    "echo '2.1.331 (Claude Code)'"
};

async fn rig_running(script: &'static str) -> Rig {
    Rig::with(chaldea_core::adapter::HostOs::Macos, move |c| {
        c.program = Some(Arc::new(move |_| Some(shell(script))));
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

fn started_version(rig: &Rig, t: ThreadId) -> serde_json::Value {
    rig.events(t)
        .into_iter()
        .find(|(k, _)| k == "launch_started")
        .map(|(_, p)| p["cli_version"].clone())
        .unwrap()
}

/// A Claude launch records the probe's version in `launch_started` and on its launch row
/// (where hook `raw_unknown` events take it from); a Codex launch records `null`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn version_recorded() {
    let rig = rig_running(PRINT_VERSION).await;
    let (t, l) = spawn(&rig, "claude").await;
    assert_eq!(started_version(&rig, t), json!("2.1.331"));
    assert_eq!(rig.launch(l).cli_version.as_deref(), Some("2.1.331"));
    let (t, l) = spawn(&rig, "codex").await;
    assert_eq!(started_version(&rig, t), json!(null));
    assert_eq!(rig.launch(l).cli_version, None);
    rig.srv.stop().await;
}

/// A probe that cannot run, or exits nonzero, records `null` and the launch proceeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn version_failure_is_null() {
    // The rig's program does not exist on the host.
    let rig = Rig::new().await;
    let (t, _) = spawn(&rig, "claude").await;
    assert_eq!(started_version(&rig, t), json!(null));
    rig.srv.stop().await;
    let rig = rig_running("exit 3").await;
    let (t, _) = spawn(&rig, "claude").await;
    assert_eq!(started_version(&rig, t), json!(null));
    rig.srv.stop().await;
}

/// A rig whose supervisor reads the system clock, so the probe's bound passes in real time.
async fn rig_real_time() -> Rig {
    Rig::with(chaldea_core::adapter::HostOs::Macos, |c| {
        c.clock = Arc::new(crate::http::SystemMonotonic);
    })
    .await
}

/// Runs the probe directly with `script`, returning its version and how long it took.
async fn timed(rig: &Rig, script: &str) -> (Option<String>, Duration) {
    let argv = shell(script);
    let start = Instant::now();
    let sup = rig.sup.clone();
    let v = tokio::task::spawn_blocking(move || probe(&sup, &argv, sup.now() + VERSION_PROBE))
        .await
        .unwrap();
    (v, start.elapsed())
}

/// A child that does not exit by the bound gives `None` at the bound, killed and reaped.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn version_timeout() {
    let rig = rig_real_time().await;
    let (v, took) = timed(&rig, "sleep 30").await;
    assert_eq!(v, None);
    assert!(
        took >= VERSION_PROBE && took < VERSION_PROBE + Duration::from_secs(3),
        "{took:?}"
    );
    rig.until("the killed probe is reaped", || {
        rig.sup.version_left.reap();
        rig.sup.version_left.is_empty() && rig.sup.tracker.count() == 0
    })
    .await;
    rig.srv.stop().await;
}

/// A descendant holding stdout after the child exited does not extend the bound: `None` at
/// the bound, not when the descendant ends.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn version_descendant_holds_stdout() {
    let rig = rig_real_time().await;
    let (v, took) = timed(&rig, "(sleep 30 &); echo '2.1.331 (Claude Code)'").await;
    assert_eq!(v, None);
    assert!(
        took >= VERSION_PROBE && took < VERSION_PROBE + Duration::from_secs(3),
        "{took:?}"
    );
    rig.srv.stop().await;
}

/// Stdout is read while the child runs, at most 4 KiB: a child printing far more than a pipe
/// holds neither blocks nor delays the probe.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn version_output_capped() {
    let rig = rig_real_time().await;
    let (v, took) = timed(
        &rig,
        "echo '2.1.331 (Claude Code)'; head -c 1048576 /dev/zero | tr '\\0' x; exit 0",
    )
    .await;
    assert_eq!(v.as_deref(), Some("2.1.331"));
    assert!(took < VERSION_PROBE, "{took:?}");
    rig.srv.stop().await;
}
