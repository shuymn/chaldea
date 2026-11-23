//! The committed Quint traces and their ITF decoding, shared by the replay tests
//! (`quint_traces.rs`, `quint_sync_traces.rs`).
//!
//! ITF encoding (https://apalache-mc.org/docs/adr/015adr-trace.html): integers as
//! `{"#bigint": "<n>"}`, sets as `{"#set": [..]}`, maps as `{"#map": [[k, v], ..]}`, sum
//! type values as `{"tag": .., "value": ..}`, records as objects, lists as arrays.

#![allow(dead_code, reason = "each test binary uses a subset")]

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Every `*.itf.json` directly under `specs/traces/<sub>` (`""` for `specs/traces`), sorted
/// by file name and parsed; fails when there is none.
pub fn read_traces(sub: &str) -> Vec<(String, Value)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../specs/traces")
        .join(sub);
    let shown = Path::new("specs/traces").join(sub);
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", shown.display()))
        .map(|e| e.expect("directory entry").path())
        .filter(|p| p.to_str().is_some_and(|s| s.ends_with(".itf.json")))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no trace under {}", shown.display());
    files
        .iter()
        .map(|path| {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .expect("UTF-8 file name")
                .to_owned();
            let text = fs::read_to_string(path).expect("read trace");
            let itf = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
            (name, itf)
        })
        .collect()
}

/// A signed integer.
pub fn int(v: &Value) -> i64 {
    let s = v["#bigint"]
        .as_str()
        .unwrap_or_else(|| panic!("not an ITF integer: {v}"));
    s.parse().unwrap_or_else(|e| panic!("bad integer {s}: {e}"))
}

/// A non-negative integer.
pub fn uint(v: &Value) -> u64 {
    let s = v["#bigint"]
        .as_str()
        .unwrap_or_else(|| panic!("not an ITF integer: {v}"));
    s.parse().unwrap_or_else(|e| panic!("bad integer {s}: {e}"))
}

pub fn list(v: &Value) -> &[Value] {
    v.as_array().unwrap_or_else(|| panic!("not a list: {v}"))
}

pub fn tag(v: &Value) -> (&str, &Value) {
    let t = v["tag"]
        .as_str()
        .unwrap_or_else(|| panic!("not a variant: {v}"));
    (t, &v["value"])
}
