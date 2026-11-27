//! The PWA build's asset table (docs/design/w1-pwa.md §2.1 "Embedding"), std only: the
//! daemon's `build.rs` includes this file with `#[path]`, and `pwa::tests` includes it too, so
//! the workspace tests check it on every host without `CHALDEA_WEB_DIST`.
//!
//! A dist directory holds `index.html` and files under `assets/` of an allowed type; source
//! maps (`*.map`) and Vite's `.vite/` are skipped. Anything else fails the build.

#[allow(
    clippy::duplicate_mod,
    reason = "build.rs reaches no crate module, so this file carries its own copy of the trim"
)]
#[path = "../../lifecycle/trim.rs"]
mod trim;

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// One embedded file: its request path, its `Content-Type`, and where it is on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistFile {
    pub url: String,
    pub content_type: &'static str,
    pub source: PathBuf,
}

/// The types allowed under `assets/`, by extension. No `.html` or `.svg` (a document type a
/// browser would run script in at the daemon's origin).
const ASSET_TYPES: &[(&str, &str)] = &[
    ("js", "text/javascript; charset=utf-8"),
    ("css", "text/css; charset=utf-8"),
    ("woff2", "font/woff2"),
    ("png", "image/png"),
    ("wasm", "application/wasm"),
];

const INDEX_TYPE: &str = "text/html; charset=utf-8";

/// The dist directory `value` names: absolute, or relative to `workspace` (the repository
/// root).
///
/// # Errors
///
/// When `value` is empty.
pub fn resolve(value: &OsStr, workspace: &Path) -> Result<PathBuf, String> {
    if value.is_empty() {
        return Err("set but empty".to_owned());
    }
    let p = Path::new(value);
    Ok(if p.is_absolute() {
        p.to_path_buf()
    } else {
        workspace.join(p)
    })
}

/// What a file at `rel` (relative to the dist root) becomes: its request path (`/` for
/// `index.html`, else `/` and its native components joined with `/`, so a Windows `\`
/// separator becomes `/`) and type, or `None` when it is skipped.
///
/// # Errors
///
/// When the path leaves the root, is not UTF-8, has a `\` in a name, is outside `index.html`
/// and `assets/`, has a type outside the allowlist, or a name with a character outside
/// `A-Z a-z 0-9 . _ -`.
pub fn classify(rel: &Path) -> Result<Option<(String, &'static str)>, String> {
    // The skips read the native names, before any name is checked.
    let first = rel.components().next().map(Component::as_os_str);
    let map = rel
        .file_name()
        .is_some_and(|n| n.as_encoded_bytes().ends_with(b".map"));
    if first == Some(OsStr::new(".vite")) || map {
        return Ok(None);
    }
    let parts = parts(rel)?;
    let shown = parts.join("/");
    let last = parts.last().map_or("", String::as_str);
    if parts.len() == 1 && last == "index.html" {
        return Ok(Some(("/".to_owned(), INDEX_TYPE)));
    }
    if parts.len() < 2 || parts[0] != "assets" {
        return Err(format!(
            "{shown}: only index.html and files under assets/ are allowed"
        ));
    }
    if let Some(bad) = parts.iter().find(|p| {
        p.starts_with('.')
            || !p
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    }) {
        return Err(format!(
            "{shown}: the name {bad:?} has a character outside A-Z a-z 0-9 . _ - or starts with ."
        ));
    }
    let ext = last.rsplit_once('.').map_or("", |(_, e)| e);
    ASSET_TYPES
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, t)| Some((format!("/{shown}"), *t)))
        .ok_or_else(|| {
            let allowed: Vec<String> = ASSET_TYPES.iter().map(|(e, _)| format!(".{e}")).collect();
            format!(
                "{shown}: the type is not allowed under assets/ (allowed: {})",
                allowed.join(" ")
            )
        })
}

/// `rel`'s native components as UTF-8 names. Outside Windows a `\` is part of a name, not a
/// separator, and is refused, so no name can stand for a path elsewhere in the dist.
fn parts(rel: &Path) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for c in rel.components() {
        let Component::Normal(name) = c else {
            return Err(format!(
                "{}: not a plain relative path",
                rel.to_string_lossy()
            ));
        };
        let name = name
            .to_str()
            .ok_or_else(|| format!("{}: not UTF-8", rel.to_string_lossy()))?;
        if name.contains('\\') {
            return Err(format!(
                "{}: a backslash in the name {name:?}",
                rel.to_string_lossy()
            ));
        }
        out.push(name.to_owned());
    }
    if out.is_empty() {
        return Err("an empty path".to_owned());
    }
    Ok(out)
}

/// Every embedded file under `root`, sorted by request path, and every directory read (for
/// `rerun-if-changed`).
///
/// # Errors
///
/// When `root` is a symlink or not a directory, has no `index.html`, holds a symlink, holds a
/// file [`classify`] refuses, or two files with one request path.
pub fn collect(root: &Path) -> Result<(Vec<DistFile>, Vec<PathBuf>), String> {
    let root = without_terminal_dots(root);
    let root = root.as_path();
    let meta = fs::symlink_metadata(root).map_err(|e| format!("{}: {e}", root.display()))?;
    if meta.is_symlink() {
        return Err(format!(
            "{}: a symlink (name the directory itself)",
            root.display()
        ));
    }
    if !meta.is_dir() {
        return Err(format!("{}: not a directory", root.display()));
    }
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    walk(root, Path::new(""), &mut files, &mut dirs)?;
    if !files.iter().any(|f| f.url == "/") {
        return Err(format!("{}: no index.html", root.display()));
    }
    Ok((table(files)?, dirs))
}

/// `root` without its terminal separators and `.` components, so `lstat` reads its last name
/// itself instead of following it; the rest keeps its spelling (`trim`: no rebuild from
/// components, which would resolve `..` in a Windows verbatim path, where `.` is a name too
/// and is kept).
fn without_terminal_dots(root: &Path) -> PathBuf {
    let mut p = trim::without_trailing_separators(root);
    #[cfg(windows)]
    let verbatim = matches!(
        p.components().next(),
        Some(Component::Prefix(x)) if x.kind().is_verbatim()
    );
    #[cfg(not(windows))]
    let verbatim = false;
    while !verbatim
        && let Some(s) = p.to_str()
        && let Some(rest) = s.strip_suffix('.')
        && rest.ends_with(std::path::is_separator)
    {
        p = trim::without_trailing_separators(Path::new(rest));
    }
    p
}

/// `files` sorted by request path.
///
/// # Errors
///
/// When two files have one request path.
pub fn table(mut files: Vec<DistFile>) -> Result<Vec<DistFile>, String> {
    files.sort_by(|a, b| a.url.cmp(&b.url));
    if let Some(w) = files.windows(2).find(|w| w[0].url == w[1].url) {
        return Err(format!(
            "{} and {} both serve {}: a request path appears twice",
            w[0].source.display(),
            w[1].source.display(),
            w[0].url
        ));
    }
    Ok(files)
}

fn walk(
    root: &Path,
    rel: &Path,
    files: &mut Vec<DistFile>,
    dirs: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let dir = root.join(rel);
    dirs.push(dir.clone());
    let mut entries: Vec<_> = fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    entries.sort();
    for name in entries {
        let rel = rel.join(&name);
        let path = root.join(&rel);
        let meta = fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.is_symlink() {
            return Err(format!(
                "{}: a symlink (the dist must hold plain files)",
                path.display()
            ));
        }
        if meta.is_dir() {
            if rel.as_os_str() == ".vite" {
                continue;
            }
            walk(root, &rel, files, dirs)?;
        } else if let Some((url, content_type)) = classify(&rel)? {
            files.push(DistFile {
                url,
                content_type,
                source: path,
            });
        }
    }
    Ok(())
}

/// The generated table: `EMBEDDED` over `files`, each file's bytes through `include_bytes!`.
///
/// # Errors
///
/// When a source path is not UTF-8.
pub fn render(files: &[DistFile]) -> Result<String, String> {
    let mut out = String::from(
        "// Generated by the daemon's build.rs from CHALDEA_WEB_DIST. Do not edit.\n\
         /// The PWA build embedded at compile time; empty in a build without one.\n\
         pub static EMBEDDED: &[Asset] = &[\n",
    );
    for f in files {
        let source = f
            .source
            .to_str()
            .ok_or_else(|| format!("{}: not UTF-8", f.source.display()))?;
        let _ = writeln!(
            out,
            "    Asset {{ path: {:?}, content_type: {:?}, bytes: include_bytes!({source:?}) }},",
            f.url, f.content_type
        );
    }
    out.push_str("];\n");
    Ok(out)
}
