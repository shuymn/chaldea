//! The command line, environment block, and current directory of `CreateProcessW`
//! (`docs/design/pty-windows.md` §2.2 "Buffers"). Pure: the comparison of environment keys
//! and the path normalization are passed in, so the rules run under test on every OS.

use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::io;

/// The longest command line, its NUL included (Microsoft, "CreateProcessW function").
pub(crate) const MAX_COMMAND_LINE: usize = 32_767;

/// An environment key order and equality (`CompareStringOrdinal(…, TRUE)` on Windows).
pub(crate) type Compare = fn(&[u16], &[u16]) -> Ordering;

/// UTF-16 units of `s`, without a terminator.
pub(crate) fn wide(s: &OsStr) -> Vec<u16> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        s.encode_wide().collect()
    }
    #[cfg(not(windows))]
    {
        s.to_string_lossy().encode_utf16().collect()
    }
}

/// `s` NUL-terminated.
pub(crate) fn wide_nul(s: &OsStr) -> Vec<u16> {
    let mut w = wide(s);
    w.push(0);
    w
}

const QUOTE: u16 = b'"' as u16;
const BACKSLASH: u16 = b'\\' as u16;
const SPACE: u16 = b' ' as u16;
const TAB: u16 = b'\t' as u16;

/// Appends one argument by std's rule: quoted when empty or containing a space or tab;
/// `n` backslashes before a `"` become `2n + 1` before it, and `n` trailing backslashes
/// before the closing quote become `2n`.
fn append_arg(out: &mut Vec<u16>, arg: &[u16]) {
    let quote = arg.is_empty() || arg.iter().any(|&c| c == SPACE || c == TAB);
    if quote {
        out.push(QUOTE);
    }
    let mut backslashes = 0usize;
    for &c in arg {
        if c == BACKSLASH {
            backslashes += 1;
        } else {
            if c == QUOTE {
                out.extend(std::iter::repeat_n(BACKSLASH, backslashes + 1));
            }
            backslashes = 0;
        }
        out.push(c);
    }
    if quote {
        out.extend(std::iter::repeat_n(BACKSLASH, backslashes));
        out.push(QUOTE);
    }
}

/// The NUL-terminated command line: `argv[0]` always quoted (validation rejects a `"` in
/// it), then each argument after one space, quoted by std's rule.
pub(crate) fn command_line(argv: &[OsString]) -> Vec<u16> {
    let mut out = Vec::new();
    if let Some((program, args)) = argv.split_first() {
        out.push(QUOTE);
        out.extend(wide(program));
        out.push(QUOTE);
        for a in args {
            out.push(SPACE);
            append_arg(&mut out, &wide(a));
        }
    }
    out.push(0);
    out
}

/// Why an environment cannot form a block.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EnvError {
    /// An empty key, a key containing `=`, or a NUL in a key or value.
    Invalid,
    /// Two keys equal under the comparison.
    Duplicate,
}

/// The environment block, each entry encoded once: every entry checked, then `env` sorted
/// once by `cmp` and adjacent keys compared for duplicates, then each `KEY=VALUE\0` in that
/// order and a final `\0`; an empty `env` gives `\0\0`.
pub(crate) fn env_block(env: &[(OsString, OsString)], cmp: Compare) -> Result<Vec<u16>, EnvError> {
    let eq = u16::from(b'=');
    let mut entries = Vec::with_capacity(env.len());
    for (k, v) in env {
        let (k, v) = (wide(k), wide(v));
        if k.is_empty() || k.contains(&eq) || k.contains(&0) || v.contains(&0) {
            return Err(EnvError::Invalid);
        }
        entries.push((k, v));
    }
    entries.sort_by(|a, b| cmp(&a.0, &b.0));
    if entries
        .windows(2)
        .any(|w| cmp(&w[0].0, &w[1].0) == Ordering::Equal)
    {
        return Err(EnvError::Duplicate);
    }
    let mut out = Vec::new();
    if entries.is_empty() {
        out.push(0);
    }
    for (k, v) in entries {
        out.extend(k);
        out.push(eq);
        out.extend(v);
        out.push(0);
    }
    out.push(0);
    Ok(out)
}

/// What the current directory passed to `CreateProcessW` is.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Cwd {
    /// No verbatim prefix: passed as given.
    AsGiven,
    /// The prefix removed (UTF-16 units, no terminator); the result names the same
    /// directory.
    Stripped(Vec<u16>),
    /// A verbatim path whose stripped form Win32 normalization would change.
    Refused,
}

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// The verbatim-cwd rule over UTF-16 units, so no lossy conversion can change which
/// directory is named: `\\?\X` becomes `X` and `\\?\UNC\X` becomes `\\X`, accepted only
/// when `absolute(stripped)` returns exactly the stripped units, so Win32 normalization
/// (trailing dots and spaces, `.` and `..`, `/`) leaves it unchanged.
pub(crate) fn verbatim_cwd(cwd: &[u16], absolute: impl Fn(&[u16]) -> io::Result<Vec<u16>>) -> Cwd {
    let unc = units(r"\\?\UNC\");
    let verbatim = units(r"\\?\");
    let stripped = if let Some(rest) = cwd.strip_prefix(unc.as_slice()) {
        let mut s = units(r"\\");
        s.extend_from_slice(rest);
        s
    } else if let Some(rest) = cwd.strip_prefix(verbatim.as_slice()) {
        rest.to_vec()
    } else {
        return Cwd::AsGiven;
    };
    match absolute(&stripped) {
        Ok(a) if a == stripped => Cwd::Stripped(stripped),
        _ => Cwd::Refused,
    }
}

/// The four NUL-terminated buffers of one spawn, built during validation.
#[derive(Debug)]
pub(crate) struct Buffers {
    pub(crate) app: Vec<u16>,
    /// Kept immutable; each attempt gets a fresh copy.
    pub(crate) cmdline: Vec<u16>,
    pub(crate) env: Vec<u16>,
    pub(crate) cwd: Vec<u16>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ordinal comparison ignoring case, for keys of the test corpus (the Windows tests
    /// below use the real `CompareStringOrdinal`).
    #[cfg(not(windows))]
    fn upper_ordinal(a: &[u16], b: &[u16]) -> Ordering {
        let up = |s: &[u16]| -> Vec<u16> {
            String::from_utf16_lossy(s)
                .to_uppercase()
                .encode_utf16()
                .collect()
        };
        up(a).cmp(&up(b))
    }

    fn cmp() -> Compare {
        #[cfg(windows)]
        return super::super::sys::compare_ordinal_ignore_case;
        #[cfg(not(windows))]
        upper_ordinal
    }

    fn text(w: &[u16]) -> String {
        String::from_utf16_lossy(w)
    }

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    /// PR4-CL-9: std's quoting rule on a fixed corpus.
    #[test]
    fn command_line_matches_std_quoting() {
        let cases: &[(&[&str], &str)] = &[
            (&[r"C:\p\a.exe"], r#""C:\p\a.exe""#),
            (&[r"C:\p a\a.exe", "x"], r#""C:\p a\a.exe" x"#),
            (&[r"C:\a.exe", "a b"], r#""C:\a.exe" "a b""#),
            (&[r"C:\a.exe", ""], r#""C:\a.exe" """#),
            (&[r"C:\a.exe", "a\tb"], "\"C:\\a.exe\" \"a\tb\""),
            (&[r"C:\a.exe", r#"x"y"#], r#""C:\a.exe" x\"y"#),
            (&[r"C:\a.exe", r#"x\"y"#], r#""C:\a.exe" x\\\"y"#),
            (&[r"C:\a.exe", r"C:\dir\"], r#""C:\a.exe" C:\dir\"#),
            (&[r"C:\a.exe", r"C:\my dir\"], r#""C:\a.exe" "C:\my dir\\""#),
            (&[r"C:\a.exe", r#"\\""#], r#""C:\a.exe" \\\\\""#),
            (&[r"C:\a.exe", "日本語"], r#""C:\a.exe" 日本語"#),
            (&[r"C:\a.exe", "a\nb"], "\"C:\\a.exe\" a\nb"),
        ];
        for (argv, want) in cases {
            let got = command_line(&os(argv));
            assert_eq!(got.last(), Some(&0), "{argv:?}");
            assert_eq!(text(&got[..got.len() - 1]), *want, "{argv:?}");
        }
    }

    /// PR4-CL-8: sorted case-insensitively, each `KEY=VALUE\0`, then `\0`.
    #[test]
    fn env_block_is_sorted_case_insensitively() {
        let env = vec![
            ("b".into(), "2".into()),
            ("A".into(), "1".into()),
            ("C".into(), "3".into()),
        ];
        let got = env_block(&env, cmp()).unwrap();
        assert_eq!(text(&got), "A=1\0b=2\0C=3\0\0");
    }

    #[test]
    fn empty_env_block_is_two_nuls() {
        assert_eq!(env_block(&[], cmp()), Ok(vec![0, 0]));
    }

    #[test]
    fn keys_differing_in_case_are_duplicates() {
        let env = vec![("Path".into(), "a".into()), ("PATH".into(), "b".into())];
        assert_eq!(env_block(&env, cmp()), Err(EnvError::Duplicate));
        let env = vec![("A".into(), "a".into()), ("B".into(), "b".into())];
        assert!(env_block(&env, cmp()).is_ok());
    }

    /// Entry checks come before the duplicate check.
    #[test]
    fn invalid_entries_are_refused() {
        let entry = |k: &str, v: &str| vec![(OsString::from(k), OsString::from(v))];
        for env in [
            entry("", "a"),
            entry("A=B", "a"),
            entry("A\0", "a"),
            entry("A", "a\0"),
        ] {
            assert_eq!(env_block(&env, cmp()), Err(EnvError::Invalid), "{env:?}");
        }
        let mut env = entry("A", "a");
        env.extend(entry("a", "b"));
        env.extend(entry("", "c"));
        assert_eq!(env_block(&env, cmp()), Err(EnvError::Invalid));
    }

    /// A model of Win32 path normalization for the corpus on non-Windows hosts, over UTF-16
    /// units (other units pass through unchanged): `/` becomes `\`, trailing dots and
    /// spaces of a component are dropped, `.` and `..` resolve.
    #[cfg_attr(windows, allow(dead_code, reason = "Windows uses std::path::absolute"))]
    fn model_absolute(p: &[u16]) -> io::Result<Vec<u16>> {
        let bs = u16::from(b'\\');
        let s: Vec<u16> = p
            .iter()
            .map(|&c| if c == u16::from(b'/') { bs } else { c })
            .collect();
        let (root, rest): (Vec<u16>, &[u16]) = if s.starts_with(&[bs, bs]) {
            let r = &s[2..];
            let mut it = r.splitn(3, |&c| c == bs);
            let server = it.next().unwrap_or_default();
            let share = it.next().unwrap_or_default();
            let mut root = vec![bs, bs];
            root.extend_from_slice(server);
            root.push(bs);
            root.extend_from_slice(share);
            (root, it.next().unwrap_or_default())
        } else if s.len() >= 3 && s[1] == u16::from(b':') && s[2] == bs {
            (s[..3].to_vec(), &s[3..])
        } else {
            return Err(io::ErrorKind::InvalidInput.into());
        };
        let mut parts: Vec<&[u16]> = Vec::new();
        for c in rest.split(|&c| c == bs) {
            let mut c = c;
            while let [head @ .., last] = c
                && (*last == u16::from(b'.') || *last == u16::from(b' '))
            {
                c = head;
            }
            match c {
                [] => {}
                [a, b] if *a == u16::from(b'.') && *b == u16::from(b'.') => {
                    parts.pop();
                }
                c => parts.push(c),
            }
        }
        let mut out = root;
        for (i, c) in parts.iter().enumerate() {
            if i > 0 || out.last() != Some(&bs) {
                out.push(bs);
            }
            out.extend_from_slice(c);
        }
        Ok(out)
    }

    #[cfg(windows)]
    fn absolute(p: &[u16]) -> io::Result<Vec<u16>> {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let a = std::path::absolute(std::path::PathBuf::from(OsString::from_wide(p)))?;
        Ok(a.as_os_str().encode_wide().collect())
    }

    #[cfg(not(windows))]
    fn absolute(p: &[u16]) -> io::Result<Vec<u16>> {
        model_absolute(p)
    }

    /// PR4-CL-10: the verbatim prefix is removed only when the result names the same
    /// directory, compared unit for unit (an unpaired surrogate is kept, never replaced).
    #[test]
    fn cwd_normalization_rule() {
        let v = |s: &str| verbatim_cwd(&units(s), absolute);
        assert_eq!(v(r"\\?\C:\a"), Cwd::Stripped(units(r"C:\a")));
        assert_eq!(v(r"\\?\UNC\s\x"), Cwd::Stripped(units(r"\\s\x")));
        assert_eq!(v(r"\\?\C:\a."), Cwd::Refused);
        assert_eq!(v(r"\\?\C:\a\..\b"), Cwd::Refused);
        assert_eq!(v(r"\\?\C:\a/b"), Cwd::Refused);
        assert_eq!(v(r"C:\a"), Cwd::AsGiven);
        assert_eq!(
            verbatim_cwd(&units(r"\\?\C:\a"), |_| Err(io::ErrorKind::Other.into())),
            Cwd::Refused
        );
        let mut lone = units(r"\\?\C:\a");
        lone.push(0xD800);
        let mut want = units(r"C:\a");
        want.push(0xD800);
        assert_eq!(verbatim_cwd(&lone, absolute), Cwd::Stripped(want));
    }
}
