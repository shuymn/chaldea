//! The command line, environment block, and current directory of `CreateProcessW`
//! (`docs/design/pty-windows.md` §2.2 "Buffers"). Pure: the comparison of environment keys
//! and the path normalization are passed in, so the rules run under test on every OS.

use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

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

/// Whether two keys of `env` are equal under `cmp`.
pub(crate) fn has_duplicate_key(env: &[(OsString, OsString)], cmp: Compare) -> bool {
    let mut keys: Vec<Vec<u16>> = env.iter().map(|(k, _)| wide(k)).collect();
    keys.sort_by(|a, b| cmp(a, b));
    keys.windows(2)
        .any(|w| cmp(&w[0], &w[1]) == Ordering::Equal)
}

/// The environment block: `env` sorted by `cmp`, each `KEY=VALUE\0`, then a final `\0`; an
/// empty `env` gives `\0\0`.
pub(crate) fn env_block(env: &[(OsString, OsString)], cmp: Compare) -> Vec<u16> {
    let mut entries: Vec<(Vec<u16>, Vec<u16>)> =
        env.iter().map(|(k, v)| (wide(k), wide(v))).collect();
    entries.sort_by(|a, b| cmp(&a.0, &b.0));
    let mut out = Vec::new();
    if entries.is_empty() {
        out.push(0);
    }
    for (k, v) in entries {
        out.extend(k);
        out.push(u16::from(b'='));
        out.extend(v);
        out.push(0);
    }
    out.push(0);
    out
}

/// What the current directory passed to `CreateProcessW` is.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Cwd {
    /// No verbatim prefix: passed as given.
    AsGiven,
    /// The prefix removed; the result names the same directory.
    Stripped(OsString),
    /// A verbatim path whose stripped form Win32 normalization would change.
    Refused,
}

/// The verbatim-cwd rule: `\\?\X` becomes `X` and `\\?\UNC\X` becomes `\\X`, accepted only
/// when `absolute(stripped)` returns exactly the stripped spelling (`OsStr` equality), so
/// Win32 normalization (trailing dots and spaces, `.` and `..`, `/`) leaves it unchanged.
pub(crate) fn verbatim_cwd(cwd: &OsStr, absolute: impl Fn(&Path) -> io::Result<PathBuf>) -> Cwd {
    let s = cwd.to_string_lossy();
    if s.len() != cwd.len() {
        // Not valid Unicode: no prefix to recognise, passed as given.
        return Cwd::AsGiven;
    }
    let stripped = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        return Cwd::AsGiven;
    };
    match absolute(Path::new(&stripped)) {
        Ok(a) if a.as_os_str() == OsStr::new(&stripped) => Cwd::Stripped(stripped.into()),
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
        let got = env_block(&env, cmp());
        assert_eq!(text(&got), "A=1\0b=2\0C=3\0\0");
    }

    #[test]
    fn empty_env_block_is_two_nuls() {
        assert_eq!(env_block(&[], cmp()), vec![0, 0]);
    }

    #[test]
    fn keys_differing_in_case_are_duplicates() {
        let env = vec![("Path".into(), "a".into()), ("PATH".into(), "b".into())];
        assert!(has_duplicate_key(&env, cmp()));
        let env = vec![("A".into(), "a".into()), ("B".into(), "b".into())];
        assert!(!has_duplicate_key(&env, cmp()));
    }

    /// A model of Win32 path normalization for the corpus on non-Windows hosts: `/` becomes
    /// `\`, trailing dots and spaces of a component are dropped, `.` and `..` resolve.
    #[cfg_attr(windows, allow(dead_code, reason = "Windows uses std::path::absolute"))]
    fn model_absolute(p: &Path) -> io::Result<PathBuf> {
        let s = p.to_string_lossy().replace('/', "\\");
        let (root, rest) = if let Some(r) = s.strip_prefix(r"\\") {
            let mut it = r.splitn(3, '\\');
            let server = it.next().unwrap_or_default();
            let share = it.next().unwrap_or_default();
            (
                format!(r"\\{server}\{share}"),
                it.next().unwrap_or_default().to_owned(),
            )
        } else if s.get(1..3) == Some(r":\") {
            (format!(r"{}\", &s[..2]), s[3..].to_owned())
        } else {
            return Err(io::ErrorKind::InvalidInput.into());
        };
        let mut parts: Vec<&str> = Vec::new();
        for c in rest.split('\\') {
            match c.trim_end_matches(['.', ' ']) {
                "" => {}
                ".." => {
                    parts.pop();
                }
                c => parts.push(c),
            }
        }
        let sep = if root.ends_with('\\') || parts.is_empty() {
            ""
        } else {
            "\\"
        };
        Ok(PathBuf::from(format!("{root}{sep}{}", parts.join("\\"))))
    }

    fn absolute(p: &Path) -> io::Result<PathBuf> {
        #[cfg(windows)]
        return std::path::absolute(p);
        #[cfg(not(windows))]
        model_absolute(p)
    }

    /// PR4-CL-10: the verbatim prefix is removed only when the result names the same
    /// directory.
    #[test]
    fn cwd_normalization_rule() {
        let v = |s: &str| verbatim_cwd(OsStr::new(s), absolute);
        assert_eq!(v(r"\\?\C:\a"), Cwd::Stripped(r"C:\a".into()));
        assert_eq!(v(r"\\?\UNC\s\x"), Cwd::Stripped(r"\\s\x".into()));
        assert_eq!(v(r"\\?\C:\a."), Cwd::Refused);
        assert_eq!(v(r"\\?\C:\a\..\b"), Cwd::Refused);
        assert_eq!(v(r"\\?\C:\a/b"), Cwd::Refused);
        assert_eq!(v(r"C:\a"), Cwd::AsGiven);
        assert_eq!(
            verbatim_cwd(
                OsStr::new(r"\\?\C:\a"),
                |_| Err(io::ErrorKind::Other.into())
            ),
            Cwd::Refused
        );
    }
}
