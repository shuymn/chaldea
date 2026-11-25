//! Environment key comparison as the backend's environment block compares keys: on Windows
//! `CompareStringOrdinal(…, TRUE)` over the keys' UTF-16 units (the block refuses two keys
//! equal under it, `docs/design/pty-windows.md` §2.2 "Buffers"); exact bytes on macOS.

use std::ffi::OsStr;

#[cfg(any(windows, test))]
use crate::windows::cmdline::{Compare, wide};

/// Whether `a` and `b` name the same variable in a spawned child's environment.
#[must_use]
pub fn env_key_eq(a: &OsStr, b: &OsStr) -> bool {
    #[cfg(windows)]
    {
        eq_with(crate::windows::sys::compare_ordinal_ignore_case, a, b)
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

/// Whether `key` starts with `prefix` under the same comparison: on Windows the key's first
/// units, as many as `prefix` has, compare equal to it.
#[must_use]
pub fn env_key_has_prefix(key: &OsStr, prefix: &OsStr) -> bool {
    #[cfg(windows)]
    {
        prefix_with(
            crate::windows::sys::compare_ordinal_ignore_case,
            key,
            prefix,
        )
    }
    #[cfg(not(windows))]
    {
        key.as_encoded_bytes()
            .starts_with(prefix.as_encoded_bytes())
    }
}

#[cfg(any(windows, test))]
fn eq_with(cmp: Compare, a: &OsStr, b: &OsStr) -> bool {
    cmp(&wide(a), &wide(b)).is_eq()
}

#[cfg(any(windows, test))]
fn prefix_with(cmp: Compare, key: &OsStr, prefix: &OsStr) -> bool {
    let (k, p) = (wide(key), wide(prefix));
    k.len() >= p.len() && cmp(&k[..p.len()], &p).is_eq()
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::*;

    /// The Windows comparison's case folding, for the rule's tests on every OS.
    fn upper(a: &[u16], b: &[u16]) -> Ordering {
        let up = |s: &[u16]| -> Vec<u16> {
            String::from_utf16_lossy(s)
                .to_uppercase()
                .encode_utf16()
                .collect()
        };
        up(a).cmp(&up(b))
    }

    fn os(s: &str) -> &OsStr {
        OsStr::new(s)
    }

    #[test]
    fn windows_rule_folds_case() {
        assert!(eq_with(upper, os("claudecode"), os("CLAUDECODE")));
        assert!(!eq_with(upper, os("CLAUDECODE_X"), os("CLAUDECODE")));
        assert!(prefix_with(upper, os("Chaldea_Hook_Token"), os("CHALDEA_")));
        assert!(prefix_with(upper, os("CHALDEA_"), os("CHALDEA_")));
        assert!(!prefix_with(upper, os("CHALDEA"), os("CHALDEA_")));
        assert!(!prefix_with(upper, os("XCHALDEA_A"), os("CHALDEA_")));
    }

    #[test]
    fn host_rule() {
        assert!(env_key_eq(os("CLAUDECODE"), os("CLAUDECODE")));
        assert!(env_key_has_prefix(os("CHALDEA_URL"), os("CHALDEA_")));
        assert!(!env_key_has_prefix(os("CODEX_HOME"), os("CHALDEA_")));
        // Mixed case: the same variable on Windows, another one on macOS.
        assert_eq!(
            env_key_eq(os("claudecode"), os("CLAUDECODE")),
            cfg!(windows)
        );
        assert_eq!(
            env_key_has_prefix(os("Chaldea_Hook_Token"), os("CHALDEA_")),
            cfg!(windows)
        );
    }
}
