//! The build's dist rules (`dist.rs`, which `build.rs` includes), run by the workspace tests on
//! every host without `CHALDEA_WEB_DIST`.

use std::fs;
use std::path::{Path, PathBuf};

use super::dist::{DistFile, classify, collect, render, resolve};

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
    ] {
        let e = classify(Path::new(p)).expect_err(p);
        assert!(e.contains(why), "{p}: {e}");
    }
}

/// A `\` separator (a Windows path) becomes `/` in the request path.
#[test]
fn backslashes_become_slashes() {
    let url = |p: &str| classify(Path::new(p)).unwrap().unwrap().0;
    assert_eq!(url(r"assets\a.js"), "/assets/a.js");
    assert_eq!(url(r"assets\sub\b.js"), "/assets/sub/b.js");
    assert_eq!(url("assets/a.js"), "/assets/a.js");
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
