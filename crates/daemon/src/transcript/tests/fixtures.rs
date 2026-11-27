//! The committed transcript excerpts (`fixtures/claude/<cli-version>/<case>/transcript.jsonl`)
//! each decode to the events their `transcript.meta.json` records, and nothing else.

use std::path::Path;

use super::{Fx, inline};
use crate::transcript::read::Step;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/claude");

#[test]
fn every_transcript_fixture_decodes_to_its_events() {
    let mut seen = 0;
    for version in std::fs::read_dir(FIXTURES).unwrap() {
        let version = version.unwrap().path();
        if !version.is_dir() {
            continue;
        }
        for case in std::fs::read_dir(&version).unwrap() {
            let case = case.unwrap().path();
            if case.join("transcript.jsonl").exists() {
                check(&case);
                seen += 1;
            }
        }
    }
    assert!(seen >= 4, "{seen} transcript fixtures");
}

fn check(case: &Path) {
    let bytes = std::fs::read(case.join("transcript.jsonl")).unwrap();
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(case.join("transcript.meta.json")).unwrap()).unwrap();
    assert_eq!(
        meta["lines"].as_u64().unwrap(),
        bytes.iter().filter(|b| **b == b'\n').count() as u64,
        "{}",
        case.display()
    );
    let fx = Fx::new();
    fx.write(&bytes);
    let mut tail = fx.tail();
    assert_eq!(fx.settle(&mut tail), Step::Wait, "{}", case.display());
    let got: Vec<serde_json::Value> = fx
        .events()
        .into_iter()
        .filter(|e| e.kind != "transcript_generation")
        .map(|e| {
            assert_eq!(e.kind, "assistant_message", "{}: {e:?}", case.display());
            serde_json::json!({
                "kind": e.kind,
                "text": inline(&e.payload["text"]),
                "occurred_at": e.occurred_at,
            })
        })
        .collect();
    assert_eq!(
        serde_json::Value::Array(got),
        meta["events"],
        "{}",
        case.display()
    );
    assert_eq!(fx.cursor().unwrap().read_offset, bytes.len() as u64);
}
