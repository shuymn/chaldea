//! The `unsafe` carve-out (`docs/design/pty-unix.md` §2.7): one `#[allow(unsafe_code)]`, on
//! `unix::sys`, and a crate lints table that restates the workspace's clippy set with only
//! the listed additions. Every other crate keeps the workspace table (`forbid`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn root() -> PathBuf {
    crate_dir().join("../..")
}

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            sources(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// `key = "value"` lines of one `[section]` of a manifest.
fn table(manifest: &Path, section: &str) -> BTreeMap<String, String> {
    let text = std::fs::read_to_string(manifest).unwrap();
    let mut out = BTreeMap::new();
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            inside = line == format!("[{section}]");
            continue;
        }
        if !inside || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = line.split_once('=').unwrap();
        out.insert(k.trim().to_owned(), v.trim().trim_matches('"').to_owned());
    }
    out
}

fn strip_comment(line: &str) -> &str {
    line.find("//").map_or(line, |i| &line[..i])
}

#[test]
fn unsafe_is_allowed_only_in_sys() {
    let mut files = Vec::new();
    sources(&crate_dir().join("src"), &mut files);
    let mut allows = Vec::new();
    for f in &files {
        let text = std::fs::read_to_string(f).unwrap();
        let rel = f
            .strip_prefix(crate_dir())
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let code = strip_comment(line);
            assert!(!code.contains("#![allow(unsafe_code)]"), "{rel}:{}", i + 1);
            assert!(!code.contains("expect(unsafe_code"), "{rel}:{}", i + 1);
            if code.contains("allow(unsafe_code") {
                allows.push((rel.clone(), lines.get(i + 1).map(|l| l.trim().to_owned())));
            }
            let uses_unsafe = ["unsafe {", "unsafe fn", "unsafe impl", "unsafe extern"]
                .iter()
                .any(|k| code.contains(k));
            assert!(
                !uses_unsafe || rel == "src/unix/sys.rs",
                "unsafe outside unix::sys: {rel}:{}",
                i + 1
            );
        }
    }
    assert_eq!(
        allows,
        vec![("src/unix/mod.rs".to_owned(), Some("mod sys;".to_owned()))]
    );
}

/// The workspace's clippy keys, each with its value.
fn workspace_clippy() -> BTreeMap<String, String> {
    let t = table(&root().join("Cargo.toml"), "workspace.lints.clippy");
    assert!(!t.is_empty());
    t
}

#[test]
fn inherited_lints_match_the_workspace() {
    let local = table(&crate_dir().join("Cargo.toml"), "lints.clippy");
    for (k, v) in workspace_clippy() {
        assert_eq!(local.get(&k), Some(&v), "clippy::{k}");
    }
}

#[test]
fn local_lints_are_the_listed_ones() {
    let ws = workspace_clippy();
    let added: BTreeMap<String, String> = table(&crate_dir().join("Cargo.toml"), "lints.clippy")
        .into_iter()
        .filter(|(k, _)| !ws.contains_key(k))
        .collect();
    let expected = [
        ("multiple_unsafe_ops_per_block", "deny"),
        ("undocumented_unsafe_blocks", "deny"),
    ];
    let expected: BTreeMap<String, String> = expected
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    assert_eq!(added, expected);
    let rust = table(&crate_dir().join("Cargo.toml"), "lints.rust");
    assert_eq!(rust.get("unsafe_code").map(String::as_str), Some("deny"));
    assert_eq!(
        rust.get("unsafe_op_in_unsafe_fn").map(String::as_str),
        Some("deny")
    );
    assert_eq!(rust.len(), 2);
    assert_eq!(
        table(&root().join("Cargo.toml"), "workspace.lints.rust")
            .get("unsafe_code")
            .map(String::as_str),
        Some("forbid")
    );
}

#[test]
fn other_crates_keep_the_workspace_table() {
    for e in std::fs::read_dir(root().join("crates")).unwrap() {
        let dir = e.unwrap().path();
        if dir.canonicalize().unwrap() == crate_dir().canonicalize().unwrap() {
            continue;
        }
        let manifest = dir.join("Cargo.toml");
        let lints = table(&manifest, "lints");
        assert_eq!(
            lints.get("workspace").map(String::as_str),
            Some("true"),
            "{}",
            manifest.display()
        );
    }
}
