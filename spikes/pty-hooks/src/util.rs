//! Small OS-agnostic helpers: ids, tokens, byte comparison, paths.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const EMITTER_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const FAKE_CLI_VERSION: &str = concat!("fake-", env!("CARGO_PKG_VERSION"));

/// Random bytes from UUID v4s, skipping the fixed version/variant bytes (6 and 8).
fn random_bytes(n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let u = uuid::Uuid::new_v4();
        for (i, b) in u.as_bytes().iter().enumerate() {
            if i != 6 && i != 8 && out.len() < n {
                out.push(*b);
            }
        }
    }
    out
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 8-hex run id.
pub fn run_id() -> String {
    hex(&random_bytes(4))
}

/// 256-bit random token as 64 hex chars.
pub fn token() -> String {
    hex(&random_bytes(32))
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Offset of the first differing byte, or `None` when equal.
pub fn first_diff(a: &[u8], b: &[u8]) -> Option<usize> {
    if a == b {
        return None;
    }
    Some(
        a.iter()
            .zip(b)
            .position(|(x, y)| x != y)
            .unwrap_or(a.len().min(b.len())),
    )
}

pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn bin_name(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

/// A binary built next to the running executable.
pub fn sibling_bin(name: &str) -> PathBuf {
    exe_dir().join(bin_name(name))
}

/// Lowercased process name without a trailing `.exe`.
pub fn normalize_proc_name(name: &str) -> String {
    let n = name.to_ascii_lowercase();
    n.strip_suffix(".exe").map(str::to_string).unwrap_or(n)
}

pub fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"._-".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `pid:name/pid:name` with percent-encoded names.
pub fn encode_ancestry(chain: &[(u32, String)]) -> String {
    chain
        .iter()
        .map(|(p, n)| format!("{p}:{}", percent_encode(n)))
        .collect::<Vec<_>>()
        .join("/")
}

pub fn decode_ancestry(s: &str) -> Vec<(u32, String)> {
    s.split('/')
        .filter_map(|part| {
            let (p, n) = part.split_once(':')?;
            Some((p.parse().ok()?, percent_decode(n)))
        })
        .collect()
}

/// Path component safe for fixture directories.
pub fn sanitize_component(s: &str) -> String {
    let c: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if c.is_empty() || c.chars().all(|c| c == '.') {
        "_".into()
    } else {
        c
    }
}

/// Writes `bytes` to `path` via a sibling temp file unique to this write
/// (pid and a counter) and rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let tmp = path.with_extension(format!("tmp-{}-{n}", std::process::id()));
    let r = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_64_hex() {
        let t = token();
        assert_eq!(t.len(), 64);
        assert!(t.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(t, token());
    }

    #[test]
    fn ancestry_roundtrip() {
        let chain = vec![(1, "a b/c:d".to_string()), (2, "日本".to_string())];
        assert_eq!(decode_ancestry(&encode_ancestry(&chain)), chain);
    }

    #[test]
    fn first_diff_offsets() {
        assert_eq!(first_diff(b"abc", b"abc"), None);
        assert_eq!(first_diff(b"abc", b"abd"), Some(2));
        assert_eq!(first_diff(b"ab", b"abc"), Some(2));
    }
}
