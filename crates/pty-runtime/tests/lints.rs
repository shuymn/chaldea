//! The `unsafe` carve-out (`docs/design/pty-unix.md` §2.7, `docs/design/pty-windows.md`
//! §2.7): exactly two `#[allow(unsafe_code)]`, on `unix::sys` and `windows::sys`, and a
//! crate lints table that restates the workspace's clippy set with only the listed
//! additions. Every other crate keeps the workspace table (`forbid`). Every injected fault
//! panic in the workspace skips the panic hook (`fake::inject_panic`), but the listed ones.

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
    scrub(text, false)
}

/// `text` with comments and char literals blanked; string literals are kept, but a quote
/// inside a raw one is blanked, so every kept literal is one `"…"` with `\` escapes.
fn without_comments(text: &str) -> String {
    scrub(text, true)
}

fn scrub(text: &str, keep_strings: bool) -> String {
    let b: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let blank = |c: char| if c == '\n' { '\n' } else { ' ' };
    let lit = |c: char| if keep_strings { c } else { blank(c) };
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
        } else if let Some(r) = raw_start(&b, i) {
            let mut j = r + 1;
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
                out.push(if b[j] == '"' { ' ' } else { lit(b[j]) });
                j += 1;
            }
            out.push('"');
            i = j;
        } else if c == '"' {
            out.push('"');
            i += 1;
            while i < b.len() && b[i] != '"' {
                if b[i] == '\\' {
                    out.push(lit('\\'));
                    i += 1;
                }
                if i < b.len() {
                    out.push(lit(b[i]));
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

/// The index of the `r` when a raw string literal (`r`, `br`, or `cr` prefix) may start
/// at `i`.
fn raw_start(b: &[char], i: usize) -> Option<usize> {
    if ident(i.checked_sub(1).and_then(|k| b.get(k))) {
        return None;
    }
    let r = match b[i] {
        'r' => i,
        'b' | 'c' if b.get(i + 1) == Some(&'r') => i + 1,
        _ => return None,
    };
    matches!(b.get(r + 1), Some('"' | '#')).then_some(r)
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
                    .skip_while(|c| c.is_whitespace())
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
            !uses_unsafe(&code) || rel == "src/unix/sys.rs" || rel == "src/windows/sys.rs",
            "unsafe outside unix::sys and windows::sys: {rel}"
        );
    }
    allows.sort();
    let allow = |m: &str| {
        (
            m.to_owned(),
            "[allow(unsafe_code)]".to_owned(),
            "pub(crate) mod sys;".to_owned(),
        )
    };
    assert_eq!(
        allows,
        vec![allow("src/unix/mod.rs"), allow("src/windows/mod.rs")]
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
    // Raw byte and C strings: a quote inside one does not end it, and a backslash in a
    // raw one does not escape.
    let src = r##"const _: &[u8] = br#"""#;
#[allow(/* scope */ unsafe_code)] const _: () = ();
const _: &core::ffi::CStr = cr"\";
#[allow(unsafe_code)] fn f() {}
const _: &core::ffi::CStr = c"x";
#[allow(unsafe_code)] fn g() {}
"##;
    let found: Vec<_> = attributes(&code_only(src))
        .into_iter()
        .filter(|a| a.1 == "[allow(unsafe_code)]")
        .collect();
    assert_eq!(found.len(), 3, "{found:?}");
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

/// The calls that raise a panic through the hook.
const RAISERS: [&str; 3] = ["panic!(", "assert!(", "panic_any("];

/// Every fault panic in `text` raised through the hook, as `(call, message)`: a
/// `panic_any` call (its only use is a fault's payload), or a `panic!`/`assert!` with a
/// message that names an injected fault (`injected …` or `an injected …`, the workspace's
/// convention for fault messages; an assertion's own failure message never says so).
fn hooked_faults(text: &str) -> Vec<(&'static str, String)> {
    let b: Vec<char> = without_comments(text).chars().collect();
    let at = |i: usize, s: &str| s.chars().enumerate().all(|(k, c)| b.get(i + k) == Some(&c));
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == '"' {
            i = literal(&b, i).1;
            continue;
        }
        let raiser = RAISERS
            .into_iter()
            .find(|r| at(i, r) && !ident(i.checked_sub(1).and_then(|k| b.get(k))));
        let Some(raiser) = raiser else {
            i += 1;
            continue;
        };
        // The string literals among the call's own arguments (not inside a nested call,
        // as in `assert!(why.contains("injected"))`).
        let mut j = i + raiser.len();
        let mut depth = 1;
        let mut msgs = Vec::new();
        while j < b.len() && depth > 0 {
            match b[j] {
                '"' => {
                    let (s, end) = literal(&b, j);
                    if depth == 1 {
                        msgs.push(s);
                    }
                    j = end;
                    continue;
                }
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            j += 1;
        }
        let fault = msgs
            .iter()
            .find(|m| m.starts_with("injected") || m.starts_with("an injected"));
        if raiser == "panic_any(" {
            out.push((raiser, msgs.first().cloned().unwrap_or_default()));
        } else if let Some(m) = fault {
            out.push((raiser, m.clone()));
        }
        i += raiser.len();
    }
    out
}

/// The content of the string literal whose opening quote is at `i`, and the index after
/// its closing quote.
fn literal(b: &[char], i: usize) -> (String, usize) {
    let mut s = String::new();
    let mut j = i + 1;
    while j < b.len() && b[j] != '"' {
        if b[j] == '\\' {
            j += 1;
        }
        if let Some(c) = b.get(j) {
            s.push(*c);
        }
        j += 1;
    }
    (s, j + 1)
}

/// Fault panics that keep the hook, because the hook's work is the behaviour under test.
const HOOKED: [(&str, &str); 4] = [
    // The daemon's own hook requests the stop and logs `panic in …`; the panic-path
    // tests of the lifecycle read both.
    (
        "crates/daemon/src/lifecycle/shutdown.rs",
        "injected panic before the runtime",
    ),
    // A daemon child's decoder panics; the e2e test reads the hook's `panic in` line.
    (
        "crates/cli/tests/support/faults.rs",
        "injected parser panic",
    ),
    (
        "crates/cli/tests/support/faults.rs",
        "injected panic after the parser",
    ),
    // The emitter's watchdog hook turns the panic into its exit 0.
    (
        "crates/cli/src/hook_emit/mod.rs",
        "injected hook-emit panic",
    ),
];

/// Every injected fault panic in the workspace's code and tests unwinds through
/// `fake::inject_panic`, which skips the hook (so nothing waits on a backtrace), but the
/// ones in [`HOOKED`].
#[test]
fn fault_panics_skip_the_hook() {
    let root = root().canonicalize().unwrap();
    let me = crate_dir().join("tests/lints.rs").canonicalize().unwrap();
    let mut found = Vec::new();
    let mut kept = [false; HOOKED.len()];
    for e in std::fs::read_dir(root.join("crates")).unwrap() {
        let dir = e.unwrap().path();
        let mut files = Vec::new();
        for sub in ["src", "tests"] {
            if dir.join(sub).is_dir() {
                sources(&dir.join(sub), &mut files);
            }
        }
        for f in files {
            let f = f.canonicalize().unwrap();
            if f == me {
                continue;
            }
            let rel = f
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            for (call, msg) in hooked_faults(&std::fs::read_to_string(&f).unwrap()) {
                match HOOKED
                    .iter()
                    .position(|h| *h == (rel.as_str(), msg.as_str()))
                {
                    Some(k) => kept[k] = true,
                    None => found.push(format!("{rel}: {call}\"{msg}\"")),
                }
            }
        }
    }
    assert!(
        found.is_empty(),
        "fault panics through the hook (use fake::inject_panic): {found:#?}"
    );
    // A listed one that is gone leaves the list.
    assert_eq!(kept, [true; HOOKED.len()], "{HOOKED:?}");
}

#[test]
fn the_fault_scan_finds_only_faults() {
    let src = r##"
// panic!("injected in a comment");
/* assert!(false, "injected in a block") */
let s = "panic!(\"injected in a string\")";
let r = r#"assert!(x, "injected in a raw string")"#;
assert!(ok, "the reply: {r:?}");
assert!(cut || why.contains("injected"), "{why}");
assert_eq!(a, b, "injected");
debug_assert!(x, "injected");
panic!("unexpected {m:?}");
assert!(!flag, "injected spawn panic");
panic!("an injected (nested) panic");
std::panic::panic_any(Payload(tx));
fake::inject_panic("injected resize panic");
"##;
    let found: Vec<String> = hooked_faults(src)
        .into_iter()
        .map(|(c, m)| format!("{c}{m}"))
        .collect();
    assert_eq!(
        found,
        [
            "assert!(injected spawn panic",
            "panic!(an injected (nested) panic",
            "panic_any(",
        ]
    );
}
