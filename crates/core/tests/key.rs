//! `core::input::key` (`docs/design/supervisor-send.md` §2.6), PR10b-CL-16's encoding part.

use chaldea_core::input::key::{KeyError, KeyToken, MAX_KEYS, encode_keys};

/// Each token's bytes with DECCKM off and on; 0 and 33 tokens and a non-printable `char`
/// are refused.
#[test]
fn encode() {
    use KeyToken as K;
    let cases: [(KeyToken, &[u8], &[u8]); 13] = [
        (K::Enter, b"\r", b"\r"),
        (K::Escape, b"\x1b", b"\x1b"),
        (K::Tab, b"\t", b"\t"),
        (K::Backspace, b"\x7f", b"\x7f"),
        (K::CtrlC, b"\x03", b"\x03"),
        (K::Up, b"\x1b[A", b"\x1bOA"),
        (K::Down, b"\x1b[B", b"\x1bOB"),
        (K::Right, b"\x1b[C", b"\x1bOC"),
        (K::Left, b"\x1b[D", b"\x1bOD"),
        (K::Char('y'), b"y", b"y"),
        (K::Char(' '), b" ", b" "),
        (K::Char('~'), b"~", b"~"),
        (K::Char('1'), b"1", b"1"),
    ];
    for (t, normal, app) in cases {
        assert_eq!(encode_keys(&[t], false).unwrap(), normal, "{t:?}");
        assert_eq!(encode_keys(&[t], true).unwrap(), app, "{t:?} under DECCKM");
    }
    assert_eq!(
        encode_keys(&[K::Down, K::Down, K::Enter], false).unwrap(),
        b"\x1b[B\x1b[B\r"
    );
    assert_eq!(encode_keys(&[], false), Err(KeyError::Count));
    assert!(encode_keys(&[K::Tab; MAX_KEYS], false).is_ok());
    assert_eq!(
        encode_keys(&[K::Tab; MAX_KEYS + 1], false),
        Err(KeyError::Count)
    );
    for c in ['\u{7f}', '\n', 'é', '\u{1b}'] {
        assert_eq!(
            encode_keys(&[K::Char(c)], false),
            Err(KeyError::NotPrintable),
            "{c:?}"
        );
    }
    // The wire form: names in snake_case, a character as `{ "char": c }`.
    let keys: Vec<KeyToken> =
        serde_json::from_str(r#"["enter","ctrl_c","up",{"char":"y"}]"#).unwrap();
    assert_eq!(keys, [K::Enter, K::CtrlC, K::Up, K::Char('y')]);
    assert_eq!(
        serde_json::to_string(&keys).unwrap(),
        r#"["enter","ctrl_c","up",{"char":"y"}]"#
    );
}
