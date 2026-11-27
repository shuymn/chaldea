//! The build's dist rules (`dist.rs`, which `build.rs` includes), run by the workspace tests on
//! every host without `CHALDEA_WEB_DIST`.

use std::fs;
use std::path::{Path, PathBuf};

use super::dist::{DistFile, classify, collect, render, resolve, table};

const JS: &str = "text/javascript; charset=utf-8";

#[test]
fn resolve_keeps_absolute_and_joins_relative() {
    let ws = std::env::temp_dir().join("ws");
    let abs = std::env::temp_dir().join("dist");
    assert_eq!(resolve(abs.as_os_str(), &ws), Ok(abs.clone()));
    assert_eq!(
        resolve("packages/web/dist".as_ref(), &ws),
        Ok(ws.join("packages/web/dist"))
    );
    assert!(resolve("".as_ref(), &ws).unwrap_err().contains("empty"));
}

#[test]
fn classify_allows_index_and_typed_assets() {
    let ok = |p: &str| classify(Path::new(p)).unwrap().map(|(_, t)| t);
    assert_eq!(
        classify(Path::new("index.html")).unwrap(),
        Some(("/".to_owned(), "text/html; charset=utf-8"))
    );
    assert_eq!(ok("assets/index-B1_x.js"), Some(JS));
    assert_eq!(ok("assets/app.css"), Some("text/css; charset=utf-8"));
    assert_eq!(ok("assets/f.woff2"), Some("font/woff2"));
    assert_eq!(ok("assets/i.png"), Some("image/png"));
    assert_eq!(ok("assets/ghostty-vt.wasm"), Some("application/wasm"));
    assert_eq!(ok("assets/sub/deep.js"), Some(JS));
    // Skipped: source maps and Vite's metadata.
    assert_eq!(ok("assets/index.js.map"), None);
    assert_eq!(ok(".vite/manifest.json"), None);
}

#[test]
fn classify_refuses_everything_else() {
    for (p, why) in [
        ("favicon.ico", "only index.html"),
        ("sub/index.html", "only index.html"),
        ("assets", "only index.html"),
        ("assets/x.svg", "not allowed"),
        ("assets/x.html", "not allowed"),
        ("assets/x.JS", "not allowed"),
        ("assets/noext", "not allowed"),
        ("assets/a b.js", "character"),
        ("assets/a%2f.js", "character"),
        ("assets/.hidden.js", "character"),
        ("../index.html", "plain relative"),
        ("assets/../index.html", "plain relative"),
        ("../outside.map", "plain relative"),
        ("/abs/x.js.map", "plain relative"),
        ("", "an empty path"),
    ] {
        let e = classify(Path::new(p)).expect_err(p);
        assert!(e.contains(why), "{p}: {e}");
    }
}

/// The request path joins the native components with `/`: on Windows a `\` separator
/// becomes `/`.
#[test]
fn request_paths_join_native_components() {
    let url = |p: &str| classify(Path::new(p)).unwrap().unwrap().0;
    assert_eq!(url("assets/a.js"), "/assets/a.js");
    assert_eq!(url("assets/sub/b.js"), "/assets/sub/b.js");
    #[cfg(windows)]
    {
        assert_eq!(url(r"assets\a.js"), "/assets/a.js");
        assert_eq!(url(r"assets\sub\b.js"), "/assets/sub/b.js");
    }
}

/// Outside Windows a `\` is part of a file name, never a separator: refused.
#[cfg(not(windows))]
#[test]
fn backslash_in_a_name_is_refused() {
    for p in [r"assets\a.js", r"assets/sub\b.js", r"index\x.html"] {
        let e = classify(Path::new(p)).expect_err(p);
        assert!(e.contains("backslash"), "{p}: {e}");
    }
}

/// Skipping reads the native names first: a source map or `.vite/` entry is skipped whatever
/// its name holds, a `\` included.
#[cfg(not(windows))]
#[test]
fn skips_come_before_name_checks() {
    for p in [
        r"assets/debug\app.js.map",
        r"weird\name.map",
        r".vite/a\b.json",
    ] {
        assert_eq!(classify(Path::new(p)), Ok(None), "{p}");
    }
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    write(root, "index.html", b"x");
    write(root, r"assets/debug\app.js.map", b"{}");
    let (files, _) = collect(root).unwrap();
    assert_eq!(files.len(), 1);
}

/// A root-level file named `assets\app.js` next to `assets/app.js` fails the build instead of
/// becoming a second `/assets/app.js`.
#[cfg(not(windows))]
#[test]
fn collect_refuses_a_backslash_name_beside_its_twin() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    write(root, "index.html", b"x");
    write(root, "assets/app.js", b"1");
    write(root, r"assets\app.js", b"2");
    let e = collect(root).unwrap_err();
    assert!(e.contains("backslash"), "{e}");
}

/// Two files with one request path fail the table.
#[test]
fn duplicate_request_paths_are_refused() {
    let file = |url: &str, source: &str| DistFile {
        url: url.to_owned(),
        content_type: JS,
        source: PathBuf::from(source),
    };
    let ok = table(vec![file("/assets/b.js", "b"), file("/assets/a.js", "a")]).unwrap();
    let urls: Vec<&str> = ok.iter().map(|f| f.url.as_str()).collect();
    assert_eq!(urls, ["/assets/a.js", "/assets/b.js"]);
    let e = table(vec![
        file("/assets/a.js", "one"),
        file("/assets/b.js", "b"),
        file("/assets/a.js", "two"),
    ])
    .unwrap_err();
    assert!(e.contains("/assets/a.js") && e.contains("twice"), "{e}");
}

fn write(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, bytes).unwrap();
}

#[test]
fn collect_lists_the_embedded_files_sorted() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    write(root, "index.html", b"<!doctype html>");
    write(root, "assets/b.css", b"b");
    write(root, "assets/a.js", b"a");
    write(root, "assets/a.js.map", b"{}");
    write(root, ".vite/manifest.json", b"{}");
    let (files, dirs) = collect(root).unwrap();
    let urls: Vec<&str> = files.iter().map(|f| f.url.as_str()).collect();
    assert_eq!(urls, ["/", "/assets/a.js", "/assets/b.css"]);
    assert_eq!(files[1].source, root.join("assets").join("a.js"));
    assert_eq!(dirs, vec![root.to_path_buf(), root.join("assets")]);
}

#[test]
fn collect_refuses_an_invalid_dist() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let missing = root.join("missing");
    assert!(collect(&missing).is_err());
    write(root, "assets/a.js", b"a");
    assert!(collect(root).unwrap_err().contains("no index.html"));
    write(root, "index.html", b"x");
    collect(root).unwrap();
    write(root, "robots.txt", b"x");
    assert!(collect(root).unwrap_err().contains("robots.txt"));
    fs::remove_file(root.join("robots.txt")).unwrap();
    write(root, "assets/logo.svg", b"<svg/>");
    assert!(collect(root).unwrap_err().contains("logo.svg"));
}

#[cfg(unix)]
#[test]
fn collect_refuses_a_symlink() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    write(root, "index.html", b"x");
    write(root, "elsewhere.js", b"x");
    fs::create_dir_all(root.join("assets")).unwrap();
    std::os::unix::fs::symlink(root.join("elsewhere.js"), root.join("assets/a.js")).unwrap();
    fs::remove_file(root.join("elsewhere.js")).unwrap();
    assert!(collect(root).unwrap_err().contains("symlink"));
}

/// The root's spelling is kept apart from its terminal separators and `.`: a verbatim path
/// with a `..` component is not resolved into another directory (Windows does not interpret
/// `..` in that syntax), as `lifecycle::tests::state_dir` checks for the state directory.
#[cfg(windows)]
#[test]
fn collect_refuses_a_verbatim_root_with_a_parent_component() {
    let tmp = tempfile::tempdir().unwrap();
    let parent = fs::canonicalize(tmp.path()).unwrap();
    write(&parent.join("dist"), "index.html", b"x");
    let mut raw = parent.as_os_str().to_os_string();
    raw.push(r"\missing\..\dist");
    let path = Path::new(&raw);
    assert!(fs::symlink_metadata(path).is_err());
    assert!(collect(path).is_err());
    let mut slashed = raw.clone();
    slashed.push(r"\");
    assert!(collect(Path::new(&slashed)).is_err());
}

/// Only terminal separators and `.` are dropped; the rest of the root is read as given.
#[test]
fn collect_keeps_the_root_spelling() {
    let d = tempfile::tempdir().unwrap();
    let dist = d.path().join("dist");
    write(&dist, "index.html", b"x");
    let sep = std::path::MAIN_SEPARATOR_STR;
    for suffix in [
        "",
        sep,
        &format!("{sep}{sep}"),
        &format!("{sep}."),
        &format!("{sep}.{sep}"),
    ] {
        let mut root = dist.clone().into_os_string();
        root.push(suffix);
        let (files, dirs) = collect(Path::new(&root)).unwrap_or_else(|e| panic!("{suffix:?}: {e}"));
        assert_eq!(files.len(), 1, "{suffix:?}");
        assert_eq!(dirs[0], dist, "{suffix:?}");
    }
}

/// A dist root that is a symlink is refused, also when the value ends in a separator (which
/// `lstat` would otherwise follow).
#[cfg(unix)]
#[test]
fn collect_refuses_a_symlinked_root() {
    let d = tempfile::tempdir().unwrap();
    let valid = d.path().join("valid");
    write(&valid, "index.html", b"x");
    let link = d.path().join("link");
    std::os::unix::fs::symlink(&valid, &link).unwrap();
    collect(&valid).unwrap();
    let mut slashed = link.clone().into_os_string();
    slashed.push("/");
    let mut dotted = link.clone().into_os_string();
    dotted.push("//.");
    for root in [link.into_os_string(), slashed, dotted] {
        let e = collect(Path::new(&root)).unwrap_err();
        assert!(e.contains("symlink"), "{}: {e}", root.to_string_lossy());
    }
}

#[test]
fn render_writes_an_include_per_file() {
    assert!(
        render(&[])
            .unwrap()
            .contains("pub static EMBEDDED: &[Asset] = &[\n];")
    );
    let source = PathBuf::from(r"C:\work\dist\assets\a.js");
    let text = render(&[DistFile {
        url: "/assets/a.js".to_owned(),
        content_type: JS,
        source,
    }])
    .unwrap();
    assert!(
        text.contains(r#"Asset { path: "/assets/a.js", content_type: "text/javascript; charset=utf-8", bytes: include_bytes!("C:\\work\\dist\\assets\\a.js") },"#),
        "{text}"
    );
}
