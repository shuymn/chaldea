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

/// `text` with comments and the contents of string and char literals blanked, so only
/// code remains; lengths and line breaks are kept.
fn code_only(text: &str) -> String {
    let b: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let blank = |c: char| if c == '\n' { '\n' } else { ' ' };
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        if c == '/' && next == Some('/') {
            while i < b.len() && b[i] != '\n' {
                out.push(' ');
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            let mut depth = 0;
            while i < b.len() {
                if b[i] == '/' && b.get(i + 1) == Some(&'*') {
                    depth += 1;
                    out.push_str("  ");
                    i += 2;
                } else if b[i] == '*' && b.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    out.push_str("  ");
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    out.push(blank(b[i]));
                    i += 1;
                }
            }
        } else if c == 'r'
            && (next == Some('"') || next == Some('#'))
            && !ident(b.get(i.wrapping_sub(1)))
        {
            let mut j = i + 1;
            let mut hashes = 0;
            while b.get(j) == Some(&'#') {
                hashes += 1;
                j += 1;
            }
            if b.get(j) != Some(&'"') {
                out.push(c);
                i += 1;
                continue;
            }
            out.push('"');
            j += 1;
            while j < b.len() {
                if b[j] == '"' && (0..hashes).all(|k| b.get(j + 1 + k) == Some(&'#')) {
                    j += 1 + hashes;
                    break;
                }
                out.push(blank(b[j]));
                j += 1;
            }
            out.push('"');
            i = j;
        } else if c == '"' {
            out.push('"');
            i += 1;
            while i < b.len() && b[i] != '"' {
                if b[i] == '\\' {
                    out.push(' ');
                    i += 1;
                }
                if i < b.len() {
                    out.push(blank(b[i]));
                    i += 1;
                }
            }
            out.push('"');
            i += 1;
        } else if c == '\'' && (next == Some('\\') || b.get(i + 2) == Some(&'\'')) {
            // A char literal (a lifetime has no closing quote two places on).
            out.push('\'');
            i += 1;
            while i < b.len() && b[i] != '\'' {
                if b[i] == '\\' {
                    out.push(' ');
                    i += 1;
                }
                out.push(' ');
                i += 1;
            }
            out.push('\'');
            i += 1;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

fn ident(c: Option<&char>) -> bool {
    c.is_some_and(|c| c.is_alphanumeric() || *c == '_')
}

/// Every attribute in `code` as `(inner, body without whitespace, the code after it)`.
fn attributes(code: &str) -> Vec<(bool, String, String)> {
    let b: Vec<char> = code.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == '#' {
            let mut j = i + 1;
            let inner = b.get(j) == Some(&'!');
            if inner {
                j += 1;
            }
            while b.get(j).is_some_and(|c| c.is_whitespace()) {
                j += 1;
            }
            if b.get(j) == Some(&'[') {
                let mut depth = 0;
                let mut body = String::new();
                while j < b.len() {
                    match b[j] {
                        '[' => depth += 1,
                        ']' => depth -= 1,
                        _ => {}
                    }
                    if !b[j].is_whitespace() {
                        body.push(b[j]);
                    }
                    j += 1;
                    if depth == 0 {
                        break;
                    }
                }
                let after: String = b[j..]
                    .iter()
                    .collect::<String>()
                    .trim_start()
                    .chars()
                    .take(40)
                    .collect();
                out.push((inner, body, after));
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Whether `code` uses `unsafe` as a block, fn, impl, trait, or extern block.
fn uses_unsafe(code: &str) -> bool {
    code.match_indices("unsafe").any(|(i, _)| {
        let before = code[..i].chars().next_back();
        let rest = code[i + "unsafe".len()..].trim_start();
        !ident(before.as_ref())
            && (rest.starts_with('{')
                || ["fn", "impl", "trait", "extern"].iter().any(|k| {
                    rest.starts_with(k) && !ident(rest[k.len()..].chars().next().as_ref())
                }))
    })
}

#[test]
fn unsafe_is_allowed_only_in_sys() {
    let mut files = Vec::new();
    sources(&crate_dir().join("src"), &mut files);
    let mut allows = Vec::new();
    for f in &files {
        let rel = f
            .strip_prefix(crate_dir())
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let code = code_only(&std::fs::read_to_string(f).unwrap());
        for (inner, body, after) in attributes(&code) {
            if !body.contains("unsafe_code") {
                continue;
            }
            assert!(!inner, "inner unsafe_code attribute in {rel}: {body}");
            assert!(
                !body.contains("expect("),
                "expect(unsafe_code) in {rel}: {body}"
            );
            if body.contains("allow(") {
                allows.push((
                    rel.clone(),
                    body,
                    after.lines().next().unwrap_or("").to_owned(),
                ));
            }
        }
        assert!(
            !uses_unsafe(&code) || rel == "src/unix/sys.rs",
            "unsafe outside unix::sys: {rel}"
        );
    }
    assert_eq!(
        allows,
        vec![(
            "src/unix/mod.rs".to_owned(),
            "[allow(unsafe_code)]".to_owned(),
            "mod sys;".to_owned()
        )]
    );
}

#[test]
fn the_scan_sees_through_comments_and_strings() {
    let src = "#[allow(/* scope */ unsafe_code)]\nmod a;\n#![ allow ( unsafe_code ) ]\nlet s = \"unsafe { }\"; // unsafe {\nunsafe  {}\n";
    let code = code_only(src);
    let found: Vec<_> = attributes(&code)
        .into_iter()
        .filter(|a| a.1.contains("unsafe_code"))
        .collect();
    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(found[0].1, "[allow(unsafe_code)]");
    assert!(found[1].0);
    assert!(uses_unsafe(&code));
    assert!(!uses_unsafe(&code_only(
        "let s = \"unsafe {\"; // unsafe fn\nfn unsafe_x() {}"
    )));
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
