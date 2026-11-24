//! The negative fixture of `scripts/lint-probe.sh` (docs/design/deadlines.md §3 D4): one call
//! of each timer constructor `crates/daemon/clippy.toml` disallows, which the script expects
//! clippy to reject, each exactly once. Built only with the `lint-probe` feature, which no
//! workspace command enables; nothing here runs.

use std::time::Duration;

use tokio::time::sleep_until;

#[allow(
    dead_code,
    reason = "a lint fixture; clippy reads it, nothing calls it"
)]
async fn banned() {
    let _ = tokio::time::timeout(Duration::ZERO, async {}).await;
    let _ = tokio::time::timeout_at(tokio::time::Instant::now(), async {}).await;
    tokio::time::sleep(Duration::ZERO).await;
    sleep_until(tokio::time::Instant::now()).await;
}
