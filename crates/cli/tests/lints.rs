//! PR8-CL-16: both crates that create children forbid a direct `Command::spawn`, `output`, and
//! `status` through clippy's `disallowed-methods`, so every child goes through the spawn lock.
//! The daemon also forbids tokio's timer constructors (docs/design/deadlines.md §3 D2).

use std::path::Path;

fn disallowed(toml: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(toml).unwrap();
    let start = text
        .find("disallowed-methods")
        .expect("a disallowed-methods list");
    let body = &text[start..];
    let end = body.find("\n]").expect("the list's end");
    body[..end]
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            let rest = &l[l.find("path = \"")? + 8..];
            Some(rest[..rest.find('"')?].to_owned())
        })
        .collect()
}

#[test]
fn clippy_tomls_disallow_spawn() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for krate in ["cli", "daemon"] {
        let paths = disallowed(&crates.join(krate).join("clippy.toml"));
        for m in [
            "std::process::Command::spawn",
            "std::process::Command::output",
            "std::process::Command::status",
        ] {
            assert!(
                paths.iter().any(|p| p == m),
                "{krate}/clippy.toml lacks {m}: {paths:?}"
            );
        }
    }
}

/// docs/design/deadlines.md §3 D2: the daemon disallows tokio's timer constructors outside
/// its named exception functions (`scripts/lint-probe.sh` checks that the entries fire).
#[test]
fn clippy_tomls_list_the_bans() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let paths = disallowed(&crates.join("daemon").join("clippy.toml"));
    for m in [
        "tokio::time::timeout",
        "tokio::time::timeout_at",
        "tokio::time::sleep",
        "tokio::time::sleep_until",
    ] {
        assert!(
            paths.iter().any(|p| p == m),
            "daemon/clippy.toml lacks {m}: {paths:?}"
        );
    }
}
