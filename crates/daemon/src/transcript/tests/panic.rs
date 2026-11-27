//! C1-4, line half: under the production panic hook and an active gate, a panic in the
//! transcript line parser keeps the line as `raw_unknown` `schema_mismatch`, requests no
//! stop, and the next line and the next request are served. The hook is process-wide, so
//! the check runs in a helper process (this test binary re-executed with `--exact`).

use std::process::{Command, Stdio};

use super::{Fx, assistant, serving, shorts};
use crate::transcript::claude::tests::PANIC_TYPE;

const HELPER: &str = "CHALDEA_TRANSCRIPT_PANIC_HELPER";
const NAME: &str =
    "transcript::tests::panic::a_line_parser_panic_is_schema_mismatch_under_the_production_hook";

#[test]
fn a_line_parser_panic_is_schema_mismatch_under_the_production_hook() {
    if std::env::var_os(HELPER).is_some() {
        in_helper();
        return;
    }
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([NAME, "--exact", "--nocapture", "--test-threads=1"])
        .env(HELPER, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = crate::lifecycle::spawn_locked(&mut cmd)
        .unwrap()
        .wait_with_output()
        .unwrap();
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("@@ served"), "{stdout}\n{stderr}");
    // The hook's line, for the caught panic and for the control.
    assert_eq!(stderr.matches("chaldea: panic in").count(), 2, "{stderr}");
}

fn in_helper() {
    let gate = crate::lifecycle::production_panic_hook();
    let fx = Fx::new();
    let panicking = format!("{{\"type\":\"{PANIC_TYPE}\"}}\n");
    fx.write(
        [panicking.as_str(), &assistant("after")]
            .concat()
            .as_bytes(),
    );
    let mut tail = fx.tail();
    fx.settle(&mut tail);
    assert_eq!(
        shorts(&fx.events()),
        [
            "generation:1".to_owned(),
            format!("raw:schema_mismatch:{}", panicking.trim_end()),
            "text:after".to_owned(),
        ]
    );
    assert!(!gate.fired(), "{:?}", gate.stopping());
    serving(&fx.store);
    // The control: the same hook requests the stop for a panic outside the parser.
    let _ = std::thread::spawn(|| panic!("outside the parser")).join();
    assert!(gate.fired());
    println!("@@ served");
}
