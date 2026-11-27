//! Embeds the PWA build named by `CHALDEA_WEB_DIST` (docs/design/w1-pwa.md §2.1
//! "Embedding", §3 D1): `OUT_DIR/pwa_assets.rs` holds its asset table, empty when the variable
//! is unset, so a Rust-only build needs no Node. A set variable naming an invalid dist fails
//! the build. std only.

#[path = "src/http/pwa/dist.rs"]
mod dist;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const VAR: &str = "CHALDEA_WEB_DIST";

fn main() {
    println!("cargo::rerun-if-env-changed={VAR}");
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=src/http/pwa/dist.rs");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let table = match env::var_os(VAR) {
        None => Ok(dist::render(&[]).expect("an empty table renders")),
        Some(value) => embed(&value),
    };
    match table {
        Ok(text) => fs::write(out.join("pwa_assets.rs"), text).expect("write pwa_assets.rs"),
        // Cargo fails the build on this line, after the script exits.
        Err(e) => println!("cargo::error={VAR}: {e}"),
    }
}

/// The table of the dist `value` names, after declaring every path it read.
fn embed(value: &std::ffi::OsStr) -> Result<String, String> {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let workspace = manifest
        .ancestors()
        .nth(2)
        .map_or_else(|| manifest.clone(), Path::to_path_buf);
    let root = dist::resolve(value, &workspace)?;
    rerun(&root);
    let (files, dirs) = dist::collect(&root)?;
    for d in &dirs {
        rerun(d);
    }
    for f in &files {
        rerun(&f.source);
    }
    dist::render(&files)
}

fn rerun(p: &Path) {
    println!("cargo::rerun-if-changed={}", p.display());
}
