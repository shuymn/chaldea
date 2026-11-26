//! The input encoders (`docs/design/input-encoders.md` §2.3–§2.8): the bytes Chaldea writes
//! to a CLI's PTY for a prompt, or a [`Rejection`] with no bytes, and the prompt the CLI is
//! expected to record.
//!
//! Every function is pure and total: the row comes from [`crate::adapter`] by
//! `(Agent, HostOs)`, and the submit delay is data the caller's writer runs as a pause.

use std::time::Duration;

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::{canonical_combining_class, is_combining_mark};
use unicode_segmentation::UnicodeSegmentation;

use crate::adapter::{Capabilities, HostOs, capabilities};
use crate::ids::Agent;
use crate::input::RejectReason;
use crate::payload::InputEncoding;

/// What the encoded text is for: `Send` submits it into an empty composer and adds the submit;
/// `AttachPaste` is a recognised human paste inserted where the human is editing
/// (`docs/mvp-design.md` §2.3, §2.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Send,
    AttachPaste,
}

/// One encoded input: the parts of one writer unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoded {
    /// All text bytes: one `Part::Bytes`. Never empty.
    pub text: Vec<u8>,
    /// `Some` exactly for `Purpose::Send`.
    pub submit: Option<Submit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Submit {
    /// `Part::Pause` between the text and the submit; the row's `submit_delay`.
    pub delay: Duration,
    /// `Part::Bytes`; always [`SUBMIT`].
    pub bytes: &'static [u8],
}

/// One CR: Return for both encodings.
pub const SUBMIT: &[u8] = b"\r";

/// Why nothing was encoded. `reason` is what the input records (`InputInput::Rejected`);
/// `at` is diagnostic only (never persisted, never logged with the text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rejection {
    pub reason: RejectReason,
    /// Byte offset of the first offending scalar; `None` for `EmptyPrompt` and for an
    /// unsupported combination.
    pub at: Option<usize>,
}

/// Encoded size bound: `text.len() + submit.bytes.len() ≤ MAX_EXPANSION × input.len() +
/// MAX_OVERHEAD`. Every output scalar costs at most 36 bytes (a 5-digit record pair, or the
/// Shift+Enter pair), and the `win_keys` composition never yields more scalars than the input
/// has bytes.
pub const MAX_EXPANSION: usize = 36;
/// Three pastes' markers and two pads (38 bytes), and the 1-byte submit.
pub const MAX_OVERHEAD: usize = 39;

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
/// Shift+Enter key-down and key-up: Vk 13, Sc 28, Uc 13, Cs `SHIFT_PRESSED`, Rc 1.
const SHIFT_ENTER: &[u8] = b"\x1b[13;28;13;1;16;1_\x1b[13;28;13;0;16;1_";

const UNSUPPORTED: Rejection = Rejection {
    reason: RejectReason::LossyText,
    at: None,
};

/// The supported `(agent, encoding, purpose)` combinations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Path {
    ClaudePaste,
    CodexPaste,
    WinKeys,
    /// `Send` only.
    Positional,
}

fn path(agent: Agent, encoding: InputEncoding, purpose: Purpose) -> Option<Path> {
    match (agent, encoding, purpose) {
        (Agent::Claude, InputEncoding::BracketedPaste, _) => Some(Path::ClaudePaste),
        (Agent::Codex, InputEncoding::BracketedPaste, _) => Some(Path::CodexPaste),
        (Agent::Codex, InputEncoding::WinKeys, _) => Some(Path::WinKeys),
        (Agent::Codex, InputEncoding::Positional, Purpose::Send) => Some(Path::Positional),
        (Agent::Claude, InputEncoding::WinKeys | InputEncoding::Positional, _)
        | (Agent::Codex, InputEncoding::Positional, Purpose::AttachPaste) => None,
    }
}

/// The rejections of `docs/design/input-encoders.md` §2.5 for `(agent, encoding, purpose)`,
/// first class first: control characters, lossy text, an empty prompt, leading or trailing
/// whitespace, TAB. An unsupported combination is `LossyText` with no offset.
pub fn check(
    agent: Agent,
    encoding: InputEncoding,
    purpose: Purpose,
    text: &str,
) -> Result<(), Rejection> {
    let p = path(agent, encoding, purpose).ok_or(UNSUPPORTED)?;
    check_path(p, purpose, text)
}

fn check_path(p: Path, purpose: Purpose, t: &str) -> Result<(), Rejection> {
    let reject = |reason, at| Err(Rejection { reason, at });
    // 1. ESC or a C0 control other than TAB, CR, LF.
    if let Some((at, _)) = t
        .char_indices()
        .find(|&(_, c)| c < ' ' && !matches!(c, '\t' | '\r' | '\n'))
    {
        return reject(RejectReason::ControlCharacter, Some(at));
    }
    // 2. DEL on Codex's paste and on keys; C1 on Codex's paste.
    let lossy = |c: char| match p {
        Path::CodexPaste => ('\u{7f}'..='\u{9f}').contains(&c),
        Path::WinKeys => c == '\u{7f}',
        Path::ClaudePaste | Path::Positional => false,
    };
    if let Some((at, _)) = t.char_indices().find(|&(_, c)| lossy(c)) {
        return reject(RejectReason::LossyText, Some(at));
    }
    // 3. A send with nothing to submit; an attach paste with no text.
    let empty = match purpose {
        Purpose::Send => t.trim().is_empty(),
        Purpose::AttachPaste => t.is_empty(),
    };
    if empty {
        return reject(RejectReason::EmptyPrompt, None);
    }
    if p == Path::WinKeys {
        // 4. Codex on Windows trims both ends.
        if t.starts_with(char::is_whitespace) {
            return reject(RejectReason::LeadingTrailingWhitespace, Some(0));
        }
        if t.ends_with(char::is_whitespace) {
            return reject(
                RejectReason::LeadingTrailingWhitespace,
                Some(t.trim_end().len()),
            );
        }
        // 5. Codex on Windows takes TAB as its queue key.
        if let Some(at) = t.find('\t') {
            return reject(RejectReason::Tab, Some(at));
        }
    }
    Ok(())
}

/// With `row = capabilities(agent, os)`: [`check`] on the row's PTY encoding, then the bytes
/// of that encoding and, for `Send`, the submit after the row's `submit_delay`.
pub fn encode(
    agent: Agent,
    os: HostOs,
    text: &str,
    purpose: Purpose,
) -> Result<Encoded, Rejection> {
    encode_row(capabilities(agent, os), text, purpose)
}

fn encode_row(row: &Capabilities, text: &str, purpose: Purpose) -> Result<Encoded, Rejection> {
    let p = path(row.agent(), row.pty_encoding(), purpose).ok_or(UNSUPPORTED)?;
    check_path(p, purpose, text)?;
    let bytes = match (p, purpose) {
        (Path::ClaudePaste, _) | (Path::CodexPaste, Purpose::AttachPaste) => paste(text),
        (Path::CodexPaste, Purpose::Send) => {
            // A space, the text, a space: each its own paste (§2.6).
            let mut out = paste(" ");
            out.extend_from_slice(&paste(text));
            out.extend_from_slice(&paste(" "));
            out
        }
        (Path::WinKeys, _) => win_keys(text, purpose),
        // No bytes travel through the PTY for a positional prompt; no table row has it.
        (Path::Positional, _) => return Err(UNSUPPORTED),
    };
    let submit = (purpose == Purpose::Send).then_some(Submit {
        delay: row.submit_delay(),
        bytes: SUBMIT,
    });
    Ok(Encoded {
        text: bytes,
        submit,
    })
}

/// The prompt the CLI is expected to record for a sent `text`; `None` when
/// `check(agent, encoding, Purpose::Send, text)` fails or the combination is unsupported.
#[must_use]
pub fn expected_prompt(agent: Agent, encoding: InputEncoding, text: &str) -> Option<String> {
    let p = path(agent, encoding, Purpose::Send)?;
    check_path(p, Purpose::Send, text).ok()?;
    Some(match p {
        Path::ClaudePaste => tab4(&nfc(claude_kept(&lf(text)).trim_end())),
        Path::CodexPaste => lf(text).trim().to_owned(),
        Path::WinKeys => lf(&compose_marks(text)),
        Path::Positional => lf(text),
    })
}

/// Whether a recorded `UserPromptSubmit.prompt` acknowledges a sent `text`: for Claude, the
/// form Claude 2.1.331 records (`fixtures/claude/2.1.331/README.md`), either
/// [`expected_prompt`] or, for a paste large enough to be wrapped, the wrapper around the
/// text's line ends as LF, trailing whitespace removed, and each TAB as four spaces, with no
/// NFC (DEL and C1 removed or kept: the captures cover neither in a wrapped paste); for Codex,
/// exactly [`expected_prompt`]. `false` whenever `expected_prompt` is `None`.
#[must_use]
pub fn prompt_matches(agent: Agent, encoding: InputEncoding, text: &str, recorded: &str) -> bool {
    let Some(expected) = expected_prompt(agent, encoding, text) else {
        return false;
    };
    if recorded == expected {
        return true;
    }
    if agent != Agent::Claude {
        return false;
    }
    let base = lf(text);
    let Some(body) = strip_paste_wrapper(recorded) else {
        return false;
    };
    body == tab4(base.trim_end()) || body == tab4(claude_kept(&base).trim_end())
}

/// `t` without the scalars Claude drops from a pasted prompt: DEL and the C1 controls.
fn claude_kept(t: &str) -> String {
    t.chars()
        .filter(|c| !matches!(u32::from(*c), 0x7F..=0x9F))
        .collect()
}

/// Claude's large-paste wrapper, exactly
/// `\n\n<pasted_content id="ID">\n` + body + `\n</pasted_content id="ID">\n` with `ID`
/// matching `[A-Za-z0-9_-]+`: the body.
fn strip_paste_wrapper(s: &str) -> Option<&str> {
    let rest = s.strip_prefix("\n\n<pasted_content id=\"")?;
    let q = rest.find('"')?;
    let id = &rest[..q];
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    let body = rest[q..].strip_prefix("\">\n")?;
    body.strip_suffix(&format!("\n</pasted_content id=\"{id}\">\n"))
}

/// Each CRLF, then each remaining CR, becomes LF.
fn lf(t: &str) -> String {
    t.replace("\r\n", "\n").replace('\r', "\n")
}

fn nfc(t: &str) -> String {
    t.nfc().collect()
}

/// Each TAB becomes four spaces.
fn tab4(t: &str) -> String {
    t.replace('\t', "    ")
}

fn is_cjk_compat_ideograph(c: char) -> bool {
    matches!(u32::from(c), 0xF900..=0xFAFF | 0x2F800..=0x2FA1F)
}

fn is_combining_or_extending(c: char) -> bool {
    canonical_combining_class(c) > 0 || is_combining_mark(c)
}

/// NFC per extended grapheme cluster, only for clusters holding a scalar with canonical
/// combining class > 0 or `General_Category=Mark`. CJK compatibility ideographs inside such a
/// cluster are kept as they are; only the scalars between them are composed. Every other
/// cluster is unchanged, so whole-prompt NFC's U+FA19 → U+795E never happens.
fn compose_marks(t: &str) -> String {
    let mut out = String::with_capacity(t.len());
    for g in t.graphemes(true) {
        if !g.chars().any(is_combining_or_extending) {
            out.push_str(g);
            continue;
        }
        let mut run = String::new();
        for c in g.chars() {
            if is_cjk_compat_ideograph(c) {
                out.extend(run.nfc());
                run.clear();
                out.push(c);
            } else {
                run.push(c);
            }
        }
        out.extend(run.nfc());
    }
    out
}

/// One bracketed paste of `t`, each CRLF, LF, and lone CR as one CR.
fn paste(t: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(t.len() + PASTE_START.len() + PASTE_END.len());
    out.extend_from_slice(PASTE_START);
    let b = t.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\r' if b.get(i + 1) == Some(&b'\n') => {
                out.push(b'\r');
                i += 2;
            }
            b'\n' => {
                out.push(b'\r');
                i += 1;
            }
            x => {
                out.push(x);
                i += 1;
            }
        }
    }
    out.extend_from_slice(PASTE_END);
    out
}

/// The `win_keys` text: `pad + lf(compose_marks(t)) + pad` as keys, the pad one space for
/// `Send` and empty for `AttachPaste`.
fn win_keys(t: &str, purpose: Purpose) -> Vec<u8> {
    let body = lf(&compose_marks(t));
    let pad = purpose == Purpose::Send;
    let mut out = Vec::with_capacity(body.len() * 12 + 2);
    if pad {
        out.push(b' ');
    }
    for c in body.chars() {
        push_key(&mut out, c);
    }
    if pad {
        out.push(b' ');
    }
    out
}

/// One scalar as keys: printable ASCII and astral scalars as UTF-8, LF as Shift+Enter, every
/// other BMP scalar as a key-down and key-up record carrying its value.
fn push_key(out: &mut Vec<u8>, c: char) {
    if c == '\n' {
        out.extend_from_slice(SHIFT_ENTER);
        return;
    }
    match u16::try_from(u32::from(c)) {
        Ok(unit) if !(' '..='~').contains(&c) => {
            for key_down in [b'1', b'0'] {
                out.extend_from_slice(b"\x1b[0;0;");
                push_decimal(out, unit);
                out.extend_from_slice(&[b';', key_down]);
                out.extend_from_slice(b";0;1_");
            }
        }
        _ => {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
}

/// `n` in decimal, no leading zeros.
fn push_decimal(out: &mut Vec<u8>, n: u16) {
    let mut digits = [0u8; 5];
    let mut i = digits.len();
    let mut n = n;
    loop {
        i -= 1;
        digits[i] = b"0123456789"[usize::from(n % 10)];
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(&digits[i..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    use InputEncoding::{BracketedPaste, Positional, WinKeys};
    use Purpose::{AttachPaste, Send};

    /// Every supported `(agent, encoding, purpose)`.
    const SUPPORTED: [(Agent, InputEncoding, Purpose); 7] = [
        (Agent::Claude, BracketedPaste, Send),
        (Agent::Claude, BracketedPaste, AttachPaste),
        (Agent::Codex, BracketedPaste, Send),
        (Agent::Codex, BracketedPaste, AttachPaste),
        (Agent::Codex, WinKeys, Send),
        (Agent::Codex, WinKeys, AttachPaste),
        (Agent::Codex, Positional, Send),
    ];
    const BOTH: [Purpose; 2] = [Send, AttachPaste];

    fn err(reason: RejectReason, at: Option<usize>) -> Result<(), Rejection> {
        Err(Rejection { reason, at })
    }

    /// A table row whose PTY encoding is `encoding`, for `encode` through the table.
    fn os_of(agent: Agent, encoding: InputEncoding) -> Option<HostOs> {
        match (agent, encoding) {
            (Agent::Claude, BracketedPaste) => Some(HostOs::Windows),
            (Agent::Codex, BracketedPaste) => Some(HostOs::Macos),
            (Agent::Codex, WinKeys) => Some(HostOs::Windows),
            _ => None,
        }
    }

    /// `check` gives `want`, and `encode` through the table agrees where a row exists.
    fn assert_check(
        (agent, encoding, purpose): (Agent, InputEncoding, Purpose),
        text: &str,
        want: Result<(), Rejection>,
    ) {
        assert_eq!(
            check(agent, encoding, purpose, text),
            want,
            "{agent:?} {encoding:?} {purpose:?} {text:?}"
        );
        if let Some(os) = os_of(agent, encoding) {
            assert_eq!(
                encode(agent, os, text, purpose).map(|_| ()),
                want,
                "encode {agent:?} {os:?} {purpose:?} {text:?}"
            );
        }
    }

    fn record(unit: u32) -> Vec<u8> {
        format!("\x1b[0;0;{unit};1;0;1_\x1b[0;0;{unit};0;0;1_").into_bytes()
    }

    #[test]
    fn controls_rejected() {
        for c in (0u8..0x20).filter(|b| !matches!(b, b'\t' | b'\r' | b'\n')) {
            let c = char::from(c);
            let cases = [
                (format!("{c}ab"), 0),
                (format!("a{c}b"), 1),
                (format!("ab{c}"), 2),
                // Past newlines and a multi-byte scalar.
                (format!("a\nb\r\n日{c}"), 8),
            ];
            for (text, at) in &cases {
                for combo in SUPPORTED {
                    assert_check(combo, text, err(RejectReason::ControlCharacter, Some(*at)));
                }
            }
        }
        for (text, at) in [
            ("x\x1b[201~y", 1),
            ("\x1b[0;0;65;1;0;1_", 0),
            // Rule 1 comes before lossy text, emptiness, whitespace, and TAB.
            (" \u{7f}\t\u{85}\x01", 5),
        ] {
            for combo in SUPPORTED {
                assert_check(combo, text, err(RejectReason::ControlCharacter, Some(at)));
            }
        }
    }

    #[test]
    fn del_and_c1_on_codex_paste() {
        for u in 0x7f..=0x9f {
            let c = char::from_u32(u).unwrap();
            for purpose in BOTH {
                assert_check(
                    (Agent::Codex, BracketedPaste, purpose),
                    &format!("a{c}b"),
                    err(RejectReason::LossyText, Some(1)),
                );
                // ESC anywhere wins.
                assert_check(
                    (Agent::Codex, BracketedPaste, purpose),
                    &format!("a{c}\x1b"),
                    err(RejectReason::ControlCharacter, Some(1 + c.len_utf8())),
                );
            }
        }
    }

    #[test]
    fn del_on_win_keys() {
        for purpose in BOTH {
            assert_check(
                (Agent::Codex, WinKeys, purpose),
                "a\u{7f}b",
                err(RejectReason::LossyText, Some(1)),
            );
        }
    }

    #[test]
    fn del_and_c1_elsewhere_accepted() {
        for u in 0x7f..=0x9f {
            let c = char::from_u32(u).unwrap();
            let text = format!("a{c}b");
            for purpose in BOTH {
                assert_check((Agent::Claude, BracketedPaste, purpose), &text, Ok(()));
            }
            assert_check((Agent::Codex, Positional, Send), &text, Ok(()));
            if u == 0x7f {
                continue;
            }
            let sent = encode(Agent::Codex, HostOs::Windows, &text, Send).unwrap();
            assert_eq!(sent.text, [b" a", &record(u)[..], b"b "].concat(), "{u:#x}");
            let pasted = encode(Agent::Codex, HostOs::Windows, &text, AttachPaste).unwrap();
            assert_eq!(pasted.text, [b"a", &record(u)[..], b"b"].concat(), "{u:#x}");
        }
        assert_eq!(
            encode(Agent::Codex, HostOs::Windows, "a\u{85}b", Send)
                .unwrap()
                .text,
            b" a\x1b[0;0;133;1;0;1_\x1b[0;0;133;0;0;1_b "
        );
    }

    #[test]
    fn empty_and_blank() {
        for combo in SUPPORTED {
            assert_check(combo, "", err(RejectReason::EmptyPrompt, None));
        }
        for text in [" ", "\u{3000}", "\n", "\r\n", "\t", " \t "] {
            for combo in SUPPORTED.into_iter().filter(|c| c.2 == Send) {
                assert_check(combo, text, err(RejectReason::EmptyPrompt, None));
            }
        }
        for text in ["    ", "\n\n"] {
            assert_check((Agent::Claude, BracketedPaste, AttachPaste), text, Ok(()));
            assert_check((Agent::Codex, BracketedPaste, AttachPaste), text, Ok(()));
            assert_check(
                (Agent::Codex, WinKeys, AttachPaste),
                text,
                err(RejectReason::LeadingTrailingWhitespace, Some(0)),
            );
        }
        // U+0085 is both C1 and White_Space: rule 2 first on Codex's paste.
        for purpose in BOTH {
            assert_check(
                (Agent::Codex, BracketedPaste, purpose),
                "\u{85}",
                err(RejectReason::LossyText, Some(0)),
            );
        }
        for combo in [
            (Agent::Claude, BracketedPaste, Send),
            (Agent::Codex, WinKeys, Send),
            (Agent::Codex, Positional, Send),
        ] {
            assert_check(combo, "\u{85}", err(RejectReason::EmptyPrompt, None));
        }
    }

    #[test]
    fn d6_rejections() {
        use RejectReason::{LeadingTrailingWhitespace as Lt, LossyText, Tab};
        let cases = [
            ("\u{3000}一行目\n二行目", Lt, 0),
            ("一行目\n二行目\n", Lt, 19),
            ("a  ", Lt, 1),
            ("\u{a0}a", Lt, 0),
            ("\tx", Lt, 0),
            (" a\tb", Lt, 0),
            ("\t\u{7f}", LossyText, 1),
            ("列1\t列2", Tab, 4),
        ];
        for (text, reason, at) in cases {
            for purpose in BOTH {
                assert_check(
                    (Agent::Codex, WinKeys, purpose),
                    text,
                    err(reason, Some(at)),
                );
                assert_check((Agent::Claude, BracketedPaste, purpose), text, Ok(()));
                let codex_paste = if text.contains('\u{7f}') {
                    err(LossyText, Some(1))
                } else {
                    Ok(())
                };
                assert_check((Agent::Codex, BracketedPaste, purpose), text, codex_paste);
            }
            assert_check((Agent::Codex, Positional, Send), text, Ok(()));
        }
    }

    /// A record pair is 32, 34, or 36 bytes for a 3-, 4-, or 5-digit unit.
    #[test]
    fn record_width() {
        for (c, width) in [
            ('\u{80}', 32),
            ('\u{3e8}', 34),
            ('\u{270f}', 34),
            ('\u{2710}', 36),
            ('\u{fffd}', 36),
        ] {
            let e = encode(Agent::Codex, HostOs::Windows, &c.to_string(), AttachPaste).unwrap();
            assert_eq!(e.text, record(u32::from(c)), "{c:?}");
            assert_eq!(e.text.len(), width, "{c:?}");
        }
    }

    #[test]
    fn compose_marks_cases() {
        let cases = [
            ("か\u{3099}", "が"),
            ("e\u{301}", "é"),
            // A class-0 spacing mark: needs General_Category=Mark, not the class alone.
            ("\u{9c7}\u{9be}", "\u{9cb}"),
            // CJK compatibility ideographs stay (whole-prompt NFC gives U+795E).
            ("\u{fa19}", "\u{fa19}"),
            ("\u{fa19}\u{301}", "\u{fa19}\u{301}"),
            // The scalars after a kept ideograph are still composed: U+0344 is NFC-unstable.
            ("\u{fa19}\u{344}", "\u{fa19}\u{308}\u{301}"),
            // No precomposed form.
            ("\u{31f7}\u{309a}", "\u{31f7}\u{309a}"),
            ("\u{fa19}か\u{3099}", "\u{fa19}が"),
            // Hangul L V holds no mark: unchanged (NFC would give U+AC00).
            ("\u{1100}\u{1161}", "\u{1100}\u{1161}"),
            ("日本語 abc", "日本語 abc"),
        ];
        for (input, want) in cases {
            assert_eq!(compose_marks(input), want, "{input:?}");
            assert_eq!(
                expected_prompt(Agent::Codex, WinKeys, input).as_deref(),
                Some(want),
                "{input:?}"
            );
        }
    }

    #[test]
    fn submit_follows_the_row() {
        let delay = Duration::from_millis(123);
        for (agent, encoding) in [
            (Agent::Claude, BracketedPaste),
            (Agent::Codex, BracketedPaste),
            (Agent::Codex, WinKeys),
        ] {
            let row = Capabilities::new(agent, HostOs::Macos, encoding, encoding, delay);
            assert_eq!(
                encode_row(&row, "abc", Send).unwrap().submit,
                Some(Submit {
                    delay,
                    bytes: b"\r"
                }),
                "{agent:?} {encoding:?}"
            );
            assert_eq!(encode_row(&row, "abc", AttachPaste).unwrap().submit, None);
        }
    }

    fn wrap(id: &str, body: &str) -> String {
        format!("\n\n<pasted_content id=\"{id}\">\n{body}\n</pasted_content id=\"{id}\">\n")
    }

    fn claude_matches(text: &str, recorded: &str) -> bool {
        prompt_matches(Agent::Claude, BracketedPaste, text, recorded)
    }

    /// C1-5 (D8): Claude's forms as captured from 2.1.331: a small paste recorded with LF
    /// line ends, DEL and C1 removed, trailing whitespace removed, NFC, and each TAB as four
    /// spaces; a wrapped paste with the same except NFC.
    #[test]
    fn claude_captured_forms() {
        let text = "\n a\tb\u{fa19}e\u{301}\u{7f}\u{80}  \r\n";
        let small = "\n a    b\u{795e}\u{e9}";
        assert_eq!(
            expected_prompt(Agent::Claude, BracketedPaste, text).as_deref(),
            Some(small)
        );
        assert!(claude_matches(text, small));
        let body = "\n a    b\u{fa19}e\u{301}";
        assert!(claude_matches(text, &wrap("4500", body)));
        // The captures cover no DEL or C1 in a wrapped paste: kept is accepted too.
        assert!(claude_matches(
            text,
            &wrap("4500", "\n a    b\u{fa19}e\u{301}\u{7f}\u{80}")
        ));
    }

    /// C1-5 (D8): each transform the captures did not show is refused.
    #[test]
    fn claude_dropped_transforms() {
        let text = "\n a\tb\u{fa19}e\u{301}\u{7f}\u{80}  \r\n";
        for (why, recorded) in [
            ("TAB left unexpanded", "\n a\tb\u{795e}\u{e9}".to_owned()),
            (
                "no NFC in a small paste",
                "\n a    b\u{fa19}e\u{301}".to_owned(),
            ),
            (
                "DEL and C1 kept in a small paste",
                "\n a    b\u{795e}\u{e9}\u{7f}\u{80}".to_owned(),
            ),
            (
                "trailing whitespace kept",
                "\n a    b\u{795e}\u{e9}  \n".to_owned(),
            ),
            (
                "leading whitespace trimmed",
                "a    b\u{795e}\u{e9}".to_owned(),
            ),
            ("CR kept", "\n a    b\u{795e}\u{e9}\r".to_owned()),
            (
                "NFC inside the wrapper",
                wrap("4500", "\n a    b\u{795e}\u{e9}"),
            ),
            (
                "TAB unexpanded inside the wrapper",
                wrap("4500", "\n a\tb\u{fa19}e\u{301}"),
            ),
            (
                "trailing whitespace kept inside the wrapper",
                wrap("4500", "\n a    b\u{fa19}e\u{301}  \n"),
            ),
        ] {
            assert!(!claude_matches(text, &recorded), "{why}: {recorded:?}");
        }
    }

    #[test]
    fn claude_mismatches() {
        assert!(claude_matches("abc", &wrap("a-1_Z", "abc")));
        let cases = [
            ("a\tb", "a  b".to_owned()),
            ("\u{e9}", "e\u{301}".to_owned()),
            (
                "abc",
                "\n\n<pasted_content id=\"a1\">\nabc\n</pasted_content id=\"b2\">\n".to_owned(),
            ),
            ("abc", wrap("", "abc")),
            ("abc", wrap("a b", "abc")),
            ("abc", wrap("a1", "ab")),
            ("a\nb", "a\rb".to_owned()),
            (" a ", "a".to_owned()),
        ];
        for (text, recorded) in cases {
            assert!(!claude_matches(text, &recorded), "{text:?} {recorded:?}");
        }
    }

    #[test]
    fn codex_expected_forms() {
        let long: String = (1..=60)
            .map(|i| format!("{i:03}行目：日本語の長い入力テストです。"))
            .collect::<Vec<_>>()
            .join("\n");
        let same = |t: &str| (t.to_owned(), Some(t.to_owned()), Some(t.to_owned()));
        let some = |t: &str| Some(t.to_owned());
        // Corpus F: (text, bracketed paste form, `win_keys` form).
        let cases = [
            same("Hello, world! 0123456789 ~#$%^&*()_+-=[]{}|;':\",./<>"),
            same("日本語の入力テスト、句読点。「括弧」"),
            same("𠮷野家"),
            same("👨\u{200d}👩\u{200d}👧\u{200d}👦"),
            same("🇯🇵"),
            same("\u{2764}\u{fe0f}"),
            ("か\u{3099}".to_owned(), some("か\u{3099}"), some("が")),
            ("e\u{301}".to_owned(), some("e\u{301}"), some("é")),
            same("\u{fa19}"),
            same("一行目の日本語\n二行目、句読点。\n三行目「括弧」"),
            (
                "一行目の日本語\r\n二行目、句読点。\r\n三行目「括弧」".to_owned(),
                some("一行目の日本語\n二行目、句読点。\n三行目「括弧」"),
                some("一行目の日本語\n二行目、句読点。\n三行目「括弧」"),
            ),
            same("一行目の日本語\n\n三行目「括弧」"),
            same("/review の件"),
            same("!ls は実行しない"),
            same("?の意味"),
            same("参照してください @README.md"),
            same("一行目\n/二行目"),
            same("一行目\n!二行目"),
            same(&long),
            same("\u{31f7}\u{309a}"),
            (
                "\u{3000}一行目\n二行目\n".to_owned(),
                some("一行目\n二行目"),
                None,
            ),
            ("列1\t列2".to_owned(), some("列1\t列2"), None),
        ];
        for (text, paste_form, keys_form) in cases {
            assert_eq!(
                expected_prompt(Agent::Codex, BracketedPaste, &text),
                paste_form,
                "{text:?}"
            );
            assert_eq!(
                expected_prompt(Agent::Codex, WinKeys, &text),
                keys_form,
                "{text:?}"
            );
            for (encoding, form) in [(BracketedPaste, &paste_form), (WinKeys, &keys_form)] {
                if let Some(form) = form {
                    assert!(prompt_matches(Agent::Codex, encoding, &text, form));
                    assert!(!prompt_matches(
                        Agent::Codex,
                        encoding,
                        &text,
                        &wrap("p1", form)
                    ));
                }
            }
        }
        assert_eq!(
            expected_prompt(Agent::Codex, Positional, "a\r\nb\rc").as_deref(),
            Some("a\nb\nc")
        );
        assert!(!prompt_matches(Agent::Codex, Positional, "a\rb", "a\rb"));
        assert_eq!(
            expected_prompt(Agent::Codex, Positional, " a ").as_deref(),
            Some(" a ")
        );
        assert_eq!(
            expected_prompt(Agent::Codex, BracketedPaste, " a ").as_deref(),
            Some("a")
        );
        assert!(!prompt_matches(Agent::Codex, BracketedPaste, " a ", " a "));
        // Neither whole-prompt NFC nor NFC alone on keys.
        assert!(!prompt_matches(
            Agent::Codex,
            WinKeys,
            "\u{1100}\u{1161}",
            "\u{ac00}"
        ));
        assert!(!prompt_matches(
            Agent::Codex,
            WinKeys,
            "\u{fa19}",
            "\u{795e}"
        ));
    }

    #[test]
    fn unsupported_combinations() {
        let unsupported = [
            (Agent::Claude, WinKeys, Send),
            (Agent::Claude, WinKeys, AttachPaste),
            (Agent::Claude, Positional, Send),
            (Agent::Claude, Positional, AttachPaste),
            (Agent::Codex, Positional, AttachPaste),
        ];
        for (agent, encoding, purpose) in unsupported {
            for text in ["abc", "", "\x1b", " a\t"] {
                assert_eq!(
                    check(agent, encoding, purpose, text),
                    err(RejectReason::LossyText, None),
                    "{agent:?} {encoding:?} {purpose:?} {text:?}"
                );
            }
        }
        for encoding in [WinKeys, Positional] {
            assert_eq!(expected_prompt(Agent::Claude, encoding, "abc"), None);
            assert!(!prompt_matches(Agent::Claude, encoding, "abc", "abc"));
        }
        // No PTY bytes for a positional row, should one ever reach the encoder.
        let row = Capabilities::new(
            Agent::Codex,
            HostOs::Macos,
            Positional,
            Positional,
            Duration::ZERO,
        );
        for purpose in BOTH {
            assert_eq!(
                encode_row(&row, "abc", purpose),
                Err(Rejection {
                    reason: RejectReason::LossyText,
                    at: None
                })
            );
        }
    }
}
