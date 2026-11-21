//! Paste corpus, validator, newline mapping, bracketed-paste encoder, and
//! the Windows Codex `win_keys` input encoding (§4.4).

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::pty::{GateMode, Guard, WriterTx};

pub const PASTE_START: &[u8] = b"\x1b[200~";
pub const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Newline {
    Cr,
    Lf,
    Crlf,
}

impl Newline {
    pub const ALL: [Newline; 3] = [Newline::Cr, Newline::Lf, Newline::Crlf];

    pub fn parse(s: &str) -> Option<Newline> {
        match s {
            "cr" => Some(Newline::Cr),
            "lf" => Some(Newline::Lf),
            "crlf" => Some(Newline::Crlf),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Newline::Cr => "cr",
            Newline::Lf => "lf",
            Newline::Crlf => "crlf",
        }
    }
    fn bytes(self) -> &'static [u8] {
        match self {
            Newline::Cr => b"\r",
            Newline::Lf => b"\n",
            Newline::Crlf => b"\r\n",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub id: String,
    pub text: String,
    /// Part of the C-1 required subset.
    pub required: bool,
    /// Too large for real CLIs (> 8 KiB).
    pub fake_only: bool,
}

fn sized(target: usize) -> String {
    let unit = "日本語テキスト𠮷😀abc\n";
    let mut s = String::new();
    while s.len() + unit.len() <= target {
        s.push_str(unit);
    }
    while s.len() < target {
        s.push('x');
    }
    s
}

pub fn corpus() -> Vec<Entry> {
    let e = |id: &str, text: &str, required: bool| Entry {
        id: id.into(),
        text: text.into(),
        required,
        fake_only: false,
    };
    let mut v = vec![
        e(
            "ascii",
            "Hello, world! 0123456789 ~!@#$%^&*()_+-=[]{}|;':\",./<>?` tab\there",
            false,
        ),
        e("japanese", "日本語の入力テスト、句読点。「括弧」", true),
        e("halfwidth_kana", "ｶﾀｶﾅ", false),
        e("astral", "𠮷野家", true),
        e("emoji", "😀", false),
        e("emoji_zwj", "👨\u{200d}👩\u{200d}👧\u{200d}👦", false),
        e("emoji_flag", "🇯🇵", false),
        e("emoji_vs16", "❤\u{fe0f}", false),
        e("decomposed_kana", "か\u{3099}", true),
        e("decomposed_latin", "e\u{301}", true),
        e(
            "multiline_lf",
            "一行目の日本語\n二行目、句読点。\n三行目「括弧」",
            true,
        ),
        e(
            "multiline_crlf",
            "一行目の日本語\r\n二行目、句読点。\r\n三行目「括弧」",
            false,
        ),
        e("size_4k_plus_1", &sized(4097), false),
    ];
    v.push(Entry {
        id: "size_64k".into(),
        text: sized(64 * 1024),
        required: false,
        fake_only: true,
    });
    v
}

/// Rejected payload: offset and value of the first forbidden byte.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rejected {
    pub offset: usize,
    pub byte: u8,
}

/// Rejects ESC and every C0 control other than TAB, CR, LF.
pub fn validate(payload: &[u8]) -> Result<(), Rejected> {
    match payload
        .iter()
        .position(|&b| b < 0x20 && !matches!(b, b'\t' | b'\r' | b'\n'))
    {
        Some(offset) => Err(Rejected {
            offset,
            byte: payload[offset],
        }),
        None => Ok(()),
    }
}

/// LF and CRLF both map to `mode`; other bytes (including a lone CR) are kept.
pub fn map_newlines(payload: &[u8], mode: Newline) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + payload.len() / 8);
    let mut i = 0;
    while i < payload.len() {
        match payload[i] {
            b'\r' if payload.get(i + 1) == Some(&b'\n') => {
                out.extend_from_slice(mode.bytes());
                i += 2;
            }
            b'\n' => {
                out.extend_from_slice(mode.bytes());
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// Logical form for real-CLI comparison: CRLF and CR become LF.
pub fn normalize_lf(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'\r' {
            out.push(b'\n');
            i += if s.get(i + 1) == Some(&b'\n') { 2 } else { 1 };
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    out
}

/// Input transformations a CLI is known to apply to a pasted prompt before
/// `UserPromptSubmit` (observed on Claude Code 2.1.322). Each is checked
/// explicitly; only a difference none of them explains is a platform failure.
pub const CLI_TRANSFORMS: &[&str] = &["tab_expand", "nfc", "paste_wrapper"];

/// How an observed prompt compares with the pasted text (both LF-normalized).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptMatch {
    Exact,
    /// Equal after these known CLI transforms (smallest set that explains it).
    Transformed(Vec<&'static str>),
    /// No known transform explains the difference (first differing byte offset).
    Differs(usize),
}

/// Strips Claude's large-paste wrapper, exactly
/// `\n\n<pasted_content id="ID">\n` + body + `\n</pasted_content id="ID">\n`.
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

/// Compares `got` (the prompt) with `want` (the pasted text), trying every
/// combination of `CLI_TRANSFORMS`: TAB expanded to 4 spaces and NFC
/// composition applied to `want`, the paste wrapper stripped from `got`.
pub fn match_prompt(want: &[u8], got: &[u8]) -> PromptMatch {
    use unicode_normalization::UnicodeNormalization;
    if want == got {
        return PromptMatch::Exact;
    }
    let diff = || crate::util::first_diff(want, got).unwrap_or(0);
    let (Ok(w), Ok(g)) = (std::str::from_utf8(want), std::str::from_utf8(got)) else {
        return PromptMatch::Differs(diff());
    };
    let mut best: Option<Vec<&'static str>> = None;
    for mask in 1u8..8 {
        let (tab, nfc, wrap) = (mask & 1 != 0, mask & 2 != 0, mask & 4 != 0);
        let mut w2 = w.to_string();
        if tab {
            w2 = w2.replace('\t', "    ");
        }
        if nfc {
            w2 = w2.nfc().collect();
        }
        let g2 = if wrap {
            match strip_paste_wrapper(g) {
                Some(b) => b,
                None => continue,
            }
        } else {
            g
        };
        if w2 == g2 {
            let used: Vec<&'static str> =
                [(tab, "tab_expand"), (nfc, "nfc"), (wrap, "paste_wrapper")]
                    .into_iter()
                    .filter_map(|(on, n)| on.then_some(n))
                    .collect();
            if best.as_ref().is_none_or(|b| used.len() < b.len()) {
                best = Some(used);
            }
        }
    }
    match best {
        Some(t) => PromptMatch::Transformed(t),
        None => PromptMatch::Differs(diff()),
    }
}

/// Bracketed paste wire bytes split into writes of about `chunk` bytes (0 =
/// one write). A cut that would fall inside an escape sequence moves to the
/// sequence's end (conhost drops a split `ESC[201~` prefix, §3); cuts inside
/// multi-byte UTF-8 characters stay.
pub fn encode(payload: &[u8], mode: Newline, chunk: usize) -> Vec<Vec<u8>> {
    let mut wire = PASTE_START.to_vec();
    wire.extend_from_slice(&map_newlines(payload, mode));
    wire.extend_from_slice(PASTE_END);
    if chunk == 0 {
        vec![wire]
    } else {
        chunk_escape_safe(&wire, chunk)
    }
}

/// `(start, end)` of every escape sequence in `b`: ESC `[` … final byte
/// (0x40..=0x7e), or ESC plus one byte; an unterminated one runs to the end.
fn escape_spans(b: &[u8]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != 0x1b {
            i += 1;
            continue;
        }
        let end = if b.get(i + 1) == Some(&b'[') {
            b[i + 2..]
                .iter()
                .position(|c| (0x40..=0x7e).contains(c))
                .map_or(b.len(), |p| i + 2 + p + 1)
        } else {
            (i + 2).min(b.len())
        };
        spans.push((i, end));
        i = end;
    }
    spans
}

fn chunk_escape_safe(wire: &[u8], n: usize) -> Vec<Vec<u8>> {
    let spans = escape_spans(wire);
    let mut out = Vec::new();
    let mut start = 0;
    while start < wire.len() {
        let mut cut = (start + n).min(wire.len());
        if let Some(&(_, e)) = spans.iter().find(|(s, e)| *s < cut && cut < *e) {
            cut = e;
        }
        out.push(wire[start..cut].to_vec());
        start = cut;
    }
    out
}

/// Bytes for ungated `write_key` (keys from a person at the keyboard / probe client). `w-` names use
/// win32-input-mode encoding (`CSI Vk;Sc;Uc;Kd;Cs;Rc _`, key-down then key-up).
pub fn key_bytes(name: &str) -> Option<Vec<u8>> {
    let w = |vk: u32, sc: u32, uc: u32, cs: u32| {
        format!("\x1b[{vk};{sc};{uc};1;{cs};1_\x1b[{vk};{sc};{uc};0;{cs};1_").into_bytes()
    };
    Some(match name {
        "down" => b"\x1b[B".to_vec(),
        "enter" => b"\r".to_vec(),
        "esc" => b"\x1b".to_vec(),
        "w-down" => w(0x28, 0x50, 0, 0x100),
        "w-enter" => w(0x0d, 0x1c, 0x0d, 0),
        "w-esc" => w(0x1b, 0x01, 0x1b, 0),
        _ => return None,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    /// `sent`, `rejected`, `readiness_gate_failed` (unconfirmed), `write_timeout`, `write_error`.
    pub state: String,
    pub rejected: Option<Rejected>,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub detail: Option<String>,
    /// `win_keys` only: lossy transforms found in the payload (F-1d).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lossy: Vec<Lossy>,
    /// `win_keys` only: adapter-side changes (`compose_marks`, `escape_space`,
    /// `unicode_records:marks`, `astral_mark_left_raw`, and each accepted
    /// lossy reason).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<String>,
}

pub struct PasteOpts {
    pub mode: Newline,
    pub chunk: usize,
    pub enter_delay: Duration,
    pub write_deadline: Instant,
}

/// Validate → gate → bracketed write → delay → CR as a separate write.
/// `ready` is the launch's readiness (bound, unambiguous SessionStart); the
/// DECSET 2004 half of the gate is admitted here and re-checked by the writer
/// immediately before the first byte of the paste and of the CR.
pub fn paste(tx: &WriterTx, payload: &[u8], opts: &PasteOpts, ready: impl Fn() -> bool) -> Outcome {
    let before = tx.written.load(std::sync::atomic::Ordering::SeqCst);
    let out = |state: &str, rejected, detail: Option<String>| Outcome {
        state: state.into(),
        rejected,
        bytes_before: before,
        bytes_after: tx.written.load(std::sync::atomic::Ordering::SeqCst),
        detail,
        lossy: Vec::new(),
        changes: Vec::new(),
    };
    if let Err(r) = validate(payload) {
        return out("rejected", Some(r), None);
    }
    let generation = match (ready(), tx.gate.admit()) {
        (true, Some(g)) => g,
        _ => {
            return out(
                "readiness_gate_failed",
                None,
                Some("unconfirmed; not retried".into()),
            );
        }
    };
    let guard = || Guard {
        gate: Some((tx.gate.clone(), generation)),
        mode: GateMode::Paste,
        deadline: opts.write_deadline,
    };
    if let Err(e) = tx.write_guarded(encode(payload, opts.mode, opts.chunk), guard()) {
        let state = match e.as_str() {
            "write_timeout" | "expired_before_start" | "gate_closed_before_start" => e.clone(),
            _ => "write_error".into(),
        };
        return out(&state, None, Some(e));
    }
    std::thread::sleep(opts.enter_delay);
    if let Err(e) = tx.write_guarded(vec![b"\r".to_vec()], guard()) {
        return out("enter_not_sent", None, Some(e));
    }
    out("sent", None, None)
}

// ---------------- win_keys (Codex on Windows, F-1) ----------------

/// The newline key for `win_keys`: `csi-u` = `ESC[13;2u` (Codex reassembles
/// it into Shift+Enter), `w32` = a win32-input-mode Shift+Enter key-down and
/// key-up record. Each newline is one write of these bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum NewlineKey {
    #[serde(rename = "csi-u")]
    CsiU,
    #[serde(rename = "w32")]
    W32,
}

impl NewlineKey {
    pub const ALL: [NewlineKey; 2] = [NewlineKey::CsiU, NewlineKey::W32];

    pub fn parse(s: &str) -> Option<NewlineKey> {
        match s {
            "csi-u" => Some(NewlineKey::CsiU),
            "w32" => Some(NewlineKey::W32),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            NewlineKey::CsiU => "csi-u",
            NewlineKey::W32 => "w32",
        }
    }
    pub fn bytes(self) -> &'static [u8] {
        match self {
            NewlineKey::CsiU => b"\x1b[13;2u",
            NewlineKey::W32 => b"\x1b[13;28;13;1;16;1_\x1b[13;28;13;0;16;1_",
        }
    }
}

/// How a prompt reaches the CLI. Chosen by the adapter capability
/// `input_encoding`, never by DECSET 2004.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEncoding {
    BracketedPaste,
    WinKeys { newline: NewlineKey },
}

impl InputEncoding {
    pub fn name(self) -> &'static str {
        match self {
            InputEncoding::BracketedPaste => "bracketed_paste",
            InputEncoding::WinKeys { .. } => "win_keys",
        }
    }
}

/// Adapter capability (F-1f): `win_keys` for Codex on Windows (Codex 0.169.0
/// reads Win32 key records there, so a bracketed paste never arrives as a
/// paste, §3); bracketed paste for everything else. `os` is the runtime OS
/// name (`std::env::consts::OS`).
pub fn input_encoding(agent: &str, os: &str, newline: NewlineKey) -> InputEncoding {
    if agent == "codex" && os == "windows" {
        InputEncoding::WinKeys { newline }
    } else {
        InputEncoding::BracketedPaste
    }
}

/// A transform Codex would apply to the payload that the encoder does not
/// make silently (F-1d): leading/trailing whitespace (`trim_submission`) and
/// TAB (the queue key unless inside a burst).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Lossy {
    Trim,
    Tab,
}

impl Lossy {
    pub fn parse(s: &str) -> Option<Lossy> {
        match s {
            "trim" => Some(Lossy::Trim),
            "tab" => Some(Lossy::Tab),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Lossy::Trim => "trim",
            Lossy::Tab => "tab",
        }
    }
}

/// Lossy transforms the payload would undergo (Unicode whitespace, so
/// U+3000 and a trailing LF count as trimmed).
pub fn lossy_input(payload: &str) -> Vec<Lossy> {
    let mut v = Vec::new();
    if payload.starts_with(char::is_whitespace) || payload.ends_with(char::is_whitespace) {
        v.push(Lossy::Trim);
    }
    if payload.contains('\t') {
        v.push(Lossy::Tab);
    }
    v
}

fn is_cjk_compat_ideograph(c: char) -> bool {
    matches!(c as u32, 0xF900..=0xFAFF | 0x2F800..=0x2FA1F)
}

fn is_combining_or_extending(c: char) -> bool {
    unicode_normalization::char::canonical_combining_class(c) > 0
        || unicode_normalization::char::is_combining_mark(c)
}

/// NFC per extended grapheme cluster, only for clusters holding a combining
/// or extending character (canonical combining class > 0, or a Mn/Mc/Me
/// mark). CJK compatibility ideographs are kept as they are even inside such
/// a cluster; only the characters around them are normalized. Every other
/// cluster is unchanged, so whole-prompt NFC's U+FA19 → U+795E never happens.
pub fn compose_marks(text: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    use unicode_segmentation::UnicodeSegmentation;
    let mut out = String::with_capacity(text.len());
    for g in text.graphemes(true) {
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

/// Lines of `text` with CRLF, CR, and LF each one newline.
fn split_lines(text: &str) -> Vec<String> {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(str::to_string)
        .collect()
}

/// Why `encode_win_keys` wrote nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WinKeysReject {
    /// ESC or a C0 control other than TAB/CR/LF.
    Control(Rejected),
    NotUtf8,
    /// A lossy transform the caller did not accept (`--allow-lossy`).
    LossyInput(Vec<Lossy>),
    /// The expected prompt would be empty (nothing for Codex to submit).
    Empty,
}

/// Unicode records for `win_keys` (`--unicode-records`): `none` sends every
/// character as UTF-8; `marks` sends each BMP scalar of General Category
/// Mn, Me, or Cf (U+200D, U+309A, variation selectors, ...) left after
/// composition as a win32-input-mode key-down + key-up record pair
/// (`ESC[0;0;<unit>;1;0;1_ESC[0;0;<unit>;0;0;1_`, `Vk=Sc=Cs=0`) inside the
/// write of its line. Astral scalars in those categories stay UTF-8 (a
/// surrogate pair sent as records would reach Codex's parser as
/// high-down, high-up, low-down, low-up and be lost) and are reported as
/// `astral_mark_left_raw`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UnicodeRecords {
    #[default]
    None,
    Marks,
}

impl UnicodeRecords {
    pub fn parse(s: &str) -> Option<UnicodeRecords> {
        match s {
            "none" => Some(UnicodeRecords::None),
            "marks" => Some(UnicodeRecords::Marks),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            UnicodeRecords::None => "none",
            UnicodeRecords::Marks => "marks",
        }
    }
}

/// The win32-input-mode key-down and key-up records carrying one UTF-16
/// unit with no virtual key, scan code, or modifiers.
pub fn unit_records(unit: u16) -> Vec<u8> {
    format!("\x1b[0;0;{unit};1;0;1_\x1b[0;0;{unit};0;0;1_").into_bytes()
}

/// One line's write under `records`, whether records were used, and
/// whether an astral Mn/Me/Cf scalar was left as UTF-8.
fn line_bytes(line: &str, records: UnicodeRecords) -> (Vec<u8>, bool, bool) {
    let mut out = Vec::with_capacity(line.len());
    let (mut used, mut astral) = (false, false);
    for c in line.chars() {
        if records == UnicodeRecords::Marks && crate::marks::is_mn_me_cf(c) {
            if let Ok(unit) = u16::try_from(c as u32) {
                out.extend(unit_records(unit));
                used = true;
                continue;
            }
            astral = true;
        }
        let mut b = [0u8; 4];
        out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
    }
    (out, used, astral)
}

/// The `win_keys` writes for `payload` with every character as UTF-8
/// (`--unicode-records none`); see [`encode_win_keys_with`].
pub fn encode_win_keys(
    payload: &[u8],
    newline: NewlineKey,
    allow: &[Lossy],
) -> Result<(Vec<Vec<u8>>, Vec<String>), WinKeysReject> {
    encode_win_keys_with(payload, newline, allow, UnicodeRecords::None)
}

/// The `win_keys` writes for `payload`: validate, refuse unaccepted lossy
/// input, compose marks, escape with one leading and one trailing space
/// (Codex trims both on submit; this disarms `/`, `!`, `?` at the start and
/// an `@` popup at the end), then one write per line (marks as records
/// under `records`) and one write of the newline key per newline. The
/// trailing CR is not included.
pub fn encode_win_keys_with(
    payload: &[u8],
    newline: NewlineKey,
    allow: &[Lossy],
    records: UnicodeRecords,
) -> Result<(Vec<Vec<u8>>, Vec<String>), WinKeysReject> {
    validate(payload).map_err(WinKeysReject::Control)?;
    let text = std::str::from_utf8(payload).map_err(|_| WinKeysReject::NotUtf8)?;
    let lossy = lossy_input(text);
    let refused: Vec<Lossy> = lossy
        .iter()
        .copied()
        .filter(|l| !allow.contains(l))
        .collect();
    if !refused.is_empty() {
        return Err(WinKeysReject::LossyInput(refused));
    }
    if expected_win_keys(text, allow).is_empty() {
        return Err(WinKeysReject::Empty);
    }
    let mut changes = Vec::new();
    let composed = compose_marks(text);
    if composed != text {
        changes.push("compose_marks".to_string());
    }
    changes.push("escape_space".to_string());
    changes.extend(lossy.iter().map(|l| format!("allow_lossy:{}", l.name())));
    let lines = split_lines(&composed);
    let last = lines.len() - 1;
    let mut writes = Vec::new();
    let (mut used, mut astral) = (false, false);
    for (i, line) in lines.iter().enumerate() {
        let mut w = String::new();
        if i == 0 {
            w.push(' ');
        }
        w.push_str(line);
        if i == last {
            w.push(' ');
        }
        if !w.is_empty() {
            let (b, u, a) = line_bytes(&w, records);
            used |= u;
            astral |= a;
            writes.push(b);
        }
        if i != last {
            writes.push(newline.bytes().to_vec());
        }
    }
    if used {
        changes.push("unicode_records:marks".to_string());
    }
    if astral {
        changes.push("astral_mark_left_raw".to_string());
    }
    Ok((writes, changes))
}

/// The `win_keys` oracle: the prompt Codex should report for `payload`:
/// marks composed as above, LF newlines, nothing else changed; with an
/// accepted `trim`, the trimmed form (TAB, if accepted, stays literal).
pub fn expected_win_keys(payload: &str, allow: &[Lossy]) -> String {
    let s = split_lines(&compose_marks(payload)).join("\n");
    if allow.contains(&Lossy::Trim) {
        s.trim().to_string()
    } else {
        s
    }
}

pub struct KeysOpts {
    pub newline: NewlineKey,
    pub records: UnicodeRecords,
    pub allow: Vec<Lossy>,
    pub enter_delay: Duration,
    pub write_deadline: Instant,
}

/// `win_keys` counterpart of `paste()`: validate → lossy check → readiness
/// → every line and newline key as one `WriteMsg::Data` → delay → CR as a
/// separate write. Readiness is `ready()` (bound SessionStart plus the end of
/// the bootstrap turn); the writer re-checks only the launch's revocation
/// (`GateMode::Keys`), since Codex ignores bracketed paste here.
pub fn type_keys(
    tx: &WriterTx,
    payload: &[u8],
    opts: &KeysOpts,
    ready: impl Fn() -> bool,
) -> Outcome {
    let before = tx.written.load(std::sync::atomic::Ordering::SeqCst);
    let out = |state: &str, rejected, detail: Option<String>, lossy, changes| Outcome {
        state: state.into(),
        rejected,
        bytes_before: before,
        bytes_after: tx.written.load(std::sync::atomic::Ordering::SeqCst),
        detail,
        lossy,
        changes,
    };
    let lossy = std::str::from_utf8(payload)
        .map(lossy_input)
        .unwrap_or_default();
    let (writes, changes) =
        match encode_win_keys_with(payload, opts.newline, &opts.allow, opts.records) {
            Ok(v) => v,
            Err(WinKeysReject::Control(r)) => {
                return out("rejected", Some(r), None, lossy, Vec::new());
            }
            Err(WinKeysReject::NotUtf8) => {
                return out("rejected", None, Some("not_utf8".into()), lossy, Vec::new());
            }
            Err(WinKeysReject::Empty) => {
                return out(
                    "rejected",
                    None,
                    Some("empty_prompt".into()),
                    lossy,
                    Vec::new(),
                );
            }
            Err(WinKeysReject::LossyInput(l)) => {
                let names: Vec<&str> = l.iter().map(|x| x.name()).collect();
                return out(
                    "rejected",
                    None,
                    Some(format!("lossy_input: {}", names.join(","))),
                    lossy,
                    Vec::new(),
                );
            }
        };
    let generation = match (ready(), tx.gate.admit_keys()) {
        (true, Some(g)) => g,
        _ => {
            return out(
                "readiness_gate_failed",
                None,
                Some("unconfirmed; not retried".into()),
                lossy,
                changes,
            );
        }
    };
    let guard = || Guard {
        gate: Some((tx.gate.clone(), generation)),
        mode: GateMode::Keys,
        deadline: opts.write_deadline,
    };
    if let Err(e) = tx.write_guarded(writes, guard()) {
        let state = match e.as_str() {
            "write_timeout" | "expired_before_start" | "gate_closed_before_start" => e.clone(),
            _ => "write_error".into(),
        };
        return out(&state, None, Some(e), lossy, changes);
    }
    // The Enter delay never runs past the write deadline; a CR due after it
    // is never written (`enter_not_sent`).
    std::thread::sleep(
        opts.enter_delay.min(
            opts.write_deadline
                .saturating_duration_since(Instant::now()),
        ),
    );
    if let Err(e) = tx.write_guarded(vec![b"\r".to_vec()], guard()) {
        // Never written (cancelled before start or the gate closed): the
        // Enter was not sent. Anything else (a write that started but did
        // not report completion in time) may have reached the PTY: the
        // send is `unconfirmed` and the hooks decide; it is never resent.
        let state = match e.as_str() {
            "expired_before_start" | "gate_closed_before_start" => "enter_not_sent",
            _ => "unconfirmed",
        };
        return out(state, None, Some(e), lossy, changes);
    }
    out("sent", None, None, lossy, changes)
}

/// One corpus F entry (Codex on Windows, `--subset f1`). `recorded` entries
/// go to `evidence.extra` and never enter the verdict (F-1d, F-1e); `allow`
/// is the `--allow-lossy` set they run with.
#[derive(Debug, Clone, Serialize)]
pub struct FEntry {
    pub id: String,
    pub text: String,
    pub recorded: bool,
    pub allow: Vec<Lossy>,
}

/// A multiline Japanese entry of more than 1000 characters (about 3.5 KiB),
/// so Codex's `[Pasted Content N chars]` path is reached if the input bursts.
fn long_japanese() -> String {
    (1..=60)
        .map(|i| format!("{i:03}行目：日本語の長い入力テストです。"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn corpus_f() -> Vec<FEntry> {
    let e = |id: &str, text: &str| FEntry {
        id: id.into(),
        text: text.into(),
        recorded: false,
        allow: Vec::new(),
    };
    let r = |id: &str, text: &str, allow: Vec<Lossy>| FEntry {
        id: id.into(),
        text: text.into(),
        recorded: true,
        allow,
    };
    vec![
        e(
            "ascii",
            "Hello, world! 0123456789 ~#$%^&*()_+-=[]{}|;':\",./<>",
        ),
        e("japanese", "日本語の入力テスト、句読点。「括弧」"),
        e("astral", "𠮷野家"),
        e("emoji_zwj", "👨\u{200d}👩\u{200d}👧\u{200d}👦"),
        e("emoji_flag", "🇯🇵"),
        e("emoji_vs16", "\u{2764}\u{FE0F}"),
        e("decomposed_kana", "か\u{3099}"),
        e("decomposed_latin", "e\u{301}"),
        e("cjk_compat", "\u{FA19}"),
        e(
            "multiline_lf",
            "一行目の日本語\n二行目、句読点。\n三行目「括弧」",
        ),
        e(
            "multiline_crlf",
            "一行目の日本語\r\n二行目、句読点。\r\n三行目「括弧」",
        ),
        e("multiline_blank", "一行目の日本語\n\n三行目「括弧」"),
        e("slash_first", "/review の件"),
        e("bang_first", "!ls は実行しない"),
        e("question_first", "?の意味"),
        e("at_end", "参照してください @README.md"),
        e("slash_second_line", "一行目\n/二行目"),
        e("bang_second_line", "一行目\n!二行目"),
        e("long_multiline", &long_japanese()),
        r("small_kana_mark", "\u{31F7}\u{309A}", Vec::new()),
        r("lossy_trim", "\u{3000}一行目\n二行目\n", vec![Lossy::Trim]),
        r("lossy_tab", "列1\t列2", vec![Lossy::Tab]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// L2: an empty expected prompt is refused before anything is written,
    /// including all-whitespace input accepted with `--allow-lossy trim`.
    #[test]
    fn empty_expected_prompt_is_rejected() {
        assert!(encode_win_keys(b"", NewlineKey::CsiU, &[]).is_err());
        assert!(encode_win_keys("　 \n".as_bytes(), NewlineKey::CsiU, &[Lossy::Trim]).is_err());
        assert!(encode_win_keys(b"a", NewlineKey::CsiU, &[]).is_ok());
    }

    fn corpus_text(id: &str) -> Vec<u8> {
        let e = corpus().into_iter().find(|e| e.id == id).unwrap();
        normalize_lf(e.text.as_bytes())
    }

    /// A-6 on Claude Code 2.1.322: the three real observed prompts (saved
    /// verbatim from `UserPromptSubmit.prompt`) are each explained by one
    /// known CLI transform, and only by that one.
    #[test]
    fn real_claude_prompts_match_known_transforms() {
        let cases: [(&str, &str, &str); 4] = [
            (
                "ascii",
                include_str!("../testdata/claude-2.1.322/ups-ascii.prompt.txt"),
                "tab_expand",
            ),
            (
                "decomposed_kana",
                include_str!("../testdata/claude-2.1.322/ups-decomposed_kana.prompt.txt"),
                "nfc",
            ),
            (
                "decomposed_latin",
                include_str!("../testdata/claude-2.1.322/ups-decomposed_latin.prompt.txt"),
                "nfc",
            ),
            (
                "size_4k_plus_1",
                include_str!("../testdata/claude-2.1.322/ups-size_4k_plus_1.prompt.txt"),
                "paste_wrapper",
            ),
        ];
        for (id, got, t) in cases {
            assert_eq!(
                match_prompt(&corpus_text(id), got.as_bytes()),
                PromptMatch::Transformed(vec![t]),
                "{id}"
            );
        }
    }

    /// Differences no listed transform explains stay platform evidence.
    #[test]
    fn unexplained_differences_still_differ() {
        assert_eq!(match_prompt(b"abc", b"abc"), PromptMatch::Exact);
        assert_eq!(match_prompt(b"abc", b"abd"), PromptMatch::Differs(2));
        // A tab replaced by 2 spaces is not the known 4-space expansion.
        assert_eq!(match_prompt(b"a\tb", b"a  b"), PromptMatch::Differs(1));
        // NFD is not applied to the prompt side: composed input that
        // arrives decomposed is a difference.
        assert!(matches!(
            match_prompt("\u{e9}".as_bytes(), "e\u{301}".as_bytes()),
            PromptMatch::Differs(_)
        ));
        // A wrapper with mismatched ids, or a body that differs, is not stripped.
        let bad = "\n\n<pasted_content id=\"a1\">\nabc\n</pasted_content id=\"b2\">\n";
        assert!(matches!(
            match_prompt(b"abc", bad.as_bytes()),
            PromptMatch::Differs(_)
        ));
        let lost = "\n\n<pasted_content id=\"a1\">\nab\n</pasted_content id=\"a1\">\n";
        assert!(matches!(
            match_prompt(b"abc", lost.as_bytes()),
            PromptMatch::Differs(_)
        ));
    }

    #[test]
    fn validator_rejects_esc_and_c0() {
        assert!(validate("日本語\t\r\n".as_bytes()).is_ok());
        assert_eq!(
            validate(b"ab\x1bc"),
            Err(Rejected {
                offset: 2,
                byte: 0x1b
            })
        );
        assert_eq!(validate(b"\x03"), Err(Rejected { offset: 0, byte: 3 }));
        assert_eq!(validate(b"x\x00"), Err(Rejected { offset: 1, byte: 0 }));
        assert!(validate(b"\x7f").is_ok());
    }

    #[test]
    fn newline_mapping_defined_for_lf_and_crlf() {
        assert_eq!(map_newlines(b"a\nb\r\nc\rd", Newline::Cr), b"a\rb\rc\rd");
        assert_eq!(map_newlines(b"a\nb\r\nc", Newline::Lf), b"a\nb\nc");
        assert_eq!(map_newlines(b"a\nb\r\nc", Newline::Crlf), b"a\r\nb\r\nc");
        assert_eq!(normalize_lf(b"a\r\nb\rc\n"), b"a\nb\nc\n");
    }

    #[test]
    fn corpus_sizes() {
        let c = corpus();
        assert_eq!(
            c.iter()
                .find(|e| e.id == "size_4k_plus_1")
                .unwrap()
                .text
                .len(),
            4097
        );
        assert_eq!(
            c.iter().find(|e| e.id == "size_64k").unwrap().text.len(),
            65536
        );
        assert!(c.iter().all(|e| validate(e.text.as_bytes()).is_ok()));
    }

    #[test]
    fn chunked_encoding_concatenates_to_whole() {
        let whole = encode("日本\n".as_bytes(), Newline::Crlf, 0).concat();
        assert_eq!(
            encode("日本\n".as_bytes(), Newline::Crlf, 7).concat(),
            whole
        );
        assert!(whole.starts_with(PASTE_START) && whole.ends_with(PASTE_END));
    }

    /// F-3d: every chunk size over the whole corpus concatenates to the
    /// unchunked wire and never cuts inside `ESC[200~` / `ESC[201~`.
    #[test]
    fn chunking_never_splits_an_escape() {
        for e in corpus() {
            for m in Newline::ALL {
                let whole = encode(e.text.as_bytes(), m, 0).concat();
                for n in 1..=16 {
                    let parts = encode(e.text.as_bytes(), m, n);
                    assert_eq!(parts.concat(), whole, "{} {n}", e.id);
                    let spans = escape_spans(&whole);
                    let mut at = 0;
                    for p in &parts[..parts.len() - 1] {
                        at += p.len();
                        assert!(
                            !spans.iter().any(|(s, e)| *s < at && at < *e),
                            "{} chunk {n}: cut at {at} inside an escape",
                            e.id
                        );
                    }
                }
            }
        }
    }

    /// F-3d: at chunk 7, one cut falls inside the 4-byte `𠮷` and one inside
    /// the 3-byte `日` (split UTF-8 is coverage we keep), while the cut that
    /// would land inside `ESC[201~` moves to its end.
    #[test]
    fn chunking_keeps_utf8_splits() {
        // Wire: ESC[200~ (0..6), "aaaaaa" (6..12), 𠮷 (12..16), "bbbb" (16..20),
        // 日 (20..23), ESC[201~ (23..29). Cuts at 7, 14, 21, then 28 → 29.
        let parts = encode("aaaaaa𠮷bbbb日".as_bytes(), Newline::Cr, 7);
        let ends: Vec<usize> = parts
            .iter()
            .scan(0, |a, p| {
                *a += p.len();
                Some(*a)
            })
            .collect();
        assert_eq!(ends, vec![7, 14, 21, 29]);
        assert!(parts[1].ends_with(&"𠮷".as_bytes()[..2]));
        assert!(parts[2].ends_with(&"日".as_bytes()[..1]));
        assert_eq!(parts[3], [&"日".as_bytes()[1..], PASTE_END].concat());
    }

    #[test]
    fn compose_marks_per_cluster() {
        let cases: [(&str, &str); 7] = [
            ("か\u{3099}", "が"),
            ("e\u{301}", "é"),
            // A class-0 spacing mark: needs grapheme clusters, not ccc alone.
            ("\u{9C7}\u{9BE}", "\u{9CB}"),
            // CJK compatibility ideographs stay (whole-prompt NFC → U+795E).
            ("\u{FA19}", "\u{FA19}"),
            ("\u{FA19}\u{301}", "\u{FA19}\u{301}"),
            // No precomposed form: unchanged (F-1e records what arrives).
            ("\u{31F7}\u{309A}", "\u{31F7}\u{309A}"),
            ("日本語 abc", "日本語 abc"),
        ];
        for (input, want) in cases {
            assert_eq!(compose_marks(input), want, "{input:?}");
        }
        // Not whole-prompt NFC: a compatibility ideograph next to a composed
        // cluster keeps its form.
        assert_eq!(compose_marks("\u{FA19}か\u{3099}"), "\u{FA19}が");
    }

    #[test]
    fn win_keys_writes_one_newline_key_per_newline() {
        for key in NewlineKey::ALL {
            let (w, changes) =
                encode_win_keys("一行目\r\n二行目\n三か\u{3099}".as_bytes(), key, &[]).unwrap();
            assert_eq!(
                w,
                vec![
                    " 一行目".as_bytes().to_vec(),
                    key.bytes().to_vec(),
                    "二行目".as_bytes().to_vec(),
                    key.bytes().to_vec(),
                    "三が ".as_bytes().to_vec(),
                ]
            );
            assert_eq!(changes, vec!["compose_marks", "escape_space"]);
        }
        assert_eq!(NewlineKey::CsiU.bytes().len(), 7);
        assert_eq!(
            NewlineKey::W32.bytes(),
            b"\x1b[13;28;13;1;16;1_\x1b[13;28;13;0;16;1_"
        );
        // Triggers at the start and an @ token at the end are escaped by spaces.
        let (w, _) = encode_win_keys(b"/review", NewlineKey::CsiU, &[]).unwrap();
        assert_eq!(w, vec![b" /review ".to_vec()]);
        // A blank middle line is just two newline keys.
        let (w, _) = encode_win_keys(b"a\n\nb", NewlineKey::CsiU, &[]).unwrap();
        assert_eq!(w.len(), 4);
    }

    /// `--unicode-records marks`: each BMP Mn/Me/Cf scalar left after
    /// composition becomes a key-down + key-up record pair inside its line's
    /// write; everything else stays UTF-8; astral marks stay UTF-8 and are
    /// reported. Literal wire bytes.
    #[test]
    fn marks_become_unit_records() {
        let enc = |s: &str, r| encode_win_keys_with(s.as_bytes(), NewlineKey::W32, &[], r).unwrap();
        let zwj: &[u8] = b"\x1b[0;0;8205;1;0;1_\x1b[0;0;8205;0;0;1_";
        let (w, ch) = enc("👨\u{200d}👩\u{200d}👧\u{200d}👦", UnicodeRecords::Marks);
        let want = [
            " 👨".as_bytes(),
            zwj,
            "👩".as_bytes(),
            zwj,
            "👧".as_bytes(),
            zwj,
            "👦 ".as_bytes(),
        ]
        .concat();
        assert_eq!(w, vec![want]);
        assert_eq!(ch, ["escape_space", "unicode_records:marks"]);
        // U+31F7 (Lo) stays UTF-8; U+309A (Mn) is a record.
        let (w, _) = enc("\u{31F7}\u{309A}", UnicodeRecords::Marks);
        let kana = [
            " \u{31F7}".as_bytes(),
            b"\x1b[0;0;12442;1;0;1_\x1b[0;0;12442;0;0;1_",
            b" ",
        ];
        assert_eq!(w, vec![kana.concat()]);
        // VS16 (U+FE0F) is a record.
        let (w, _) = enc("\u{2764}\u{FE0F}", UnicodeRecords::Marks);
        let vs16 = [
            " \u{2764}".as_bytes(),
            b"\x1b[0;0;65039;1;0;1_\x1b[0;0;65039;0;0;1_",
            b" ",
        ];
        assert_eq!(w, vec![vs16.concat()]);
        // A standalone U+3099 has nothing to compose with: a record.
        let (w, _) = enc("\u{3099}", UnicodeRecords::Marks);
        assert_eq!(
            w,
            vec![b" \x1b[0;0;12441;1;0;1_\x1b[0;0;12441;0;0;1_ ".to_vec()]
        );
        // Composition runs first: no mark left, no record.
        let (w, ch) = enc("か\u{3099}", UnicodeRecords::Marks);
        assert_eq!(
            (w, ch),
            (
                vec![" が ".as_bytes().to_vec()],
                vec!["compose_marks".into(), "escape_space".into()]
            )
        );
        // CJK compatibility ideograph: not a mark.
        let (w, _) = enc("\u{FA19}", UnicodeRecords::Marks);
        assert_eq!(w, vec![" \u{FA19} ".as_bytes().to_vec()]);
        // An astral Mn (U+E0100) stays UTF-8 and is reported; no surrogate
        // unit is ever sent as a record.
        let (w, ch) = enc("\u{845B}\u{E0100}", UnicodeRecords::Marks);
        assert_eq!(w, vec![" \u{845B}\u{E0100} ".as_bytes().to_vec()]);
        assert_eq!(ch, ["escape_space", "astral_mark_left_raw"]);
        // Records stay inside the line's write; newline keys are separate.
        let (w, _) = enc("a\u{200d}\nb", UnicodeRecords::Marks);
        assert_eq!(w.len(), 3);
        assert_eq!(w[1], NewlineKey::W32.bytes());
        // `none`: today's UTF-8 bytes.
        let (w, ch) = enc("👨\u{200d}👩", UnicodeRecords::None);
        assert_eq!(w, vec![" 👨\u{200d}👩 ".as_bytes().to_vec()]);
        assert_eq!(ch, ["escape_space"]);
        // The oracle does not depend on the records mode.
        assert_eq!(
            expected_win_keys("\u{31F7}\u{309A}", &[]),
            "\u{31F7}\u{309A}"
        );
    }

    #[test]
    fn win_keys_rejects_controls_and_lossy_input() {
        assert!(matches!(
            encode_win_keys(b"ab\x1bc", NewlineKey::CsiU, &[]),
            Err(WinKeysReject::Control(_))
        ));
        assert!(matches!(
            encode_win_keys(b"x\x00", NewlineKey::W32, &[]),
            Err(WinKeysReject::Control(_))
        ));
        let trim = "\u{3000}一行目\n二行目\n";
        assert_eq!(lossy_input(trim), vec![Lossy::Trim]);
        assert_eq!(
            encode_win_keys(trim.as_bytes(), NewlineKey::CsiU, &[]),
            Err(WinKeysReject::LossyInput(vec![Lossy::Trim]))
        );
        let (w, ch) = encode_win_keys(trim.as_bytes(), NewlineKey::CsiU, &[Lossy::Trim]).unwrap();
        assert_eq!(w[0], " \u{3000}一行目".as_bytes());
        assert!(ch.contains(&"allow_lossy:trim".to_string()));
        assert_eq!(expected_win_keys(trim, &[Lossy::Trim]), "一行目\n二行目");
        let tab = "列1\t列2";
        assert_eq!(
            encode_win_keys(tab.as_bytes(), NewlineKey::CsiU, &[]),
            Err(WinKeysReject::LossyInput(vec![Lossy::Tab]))
        );
        let (w, _) = encode_win_keys(tab.as_bytes(), NewlineKey::CsiU, &[Lossy::Tab]).unwrap();
        // The accepted TAB stays literal inside the same write as its text.
        assert_eq!(w, vec![" 列1\t列2 ".as_bytes().to_vec()]);
        assert_eq!(expected_win_keys(tab, &[Lossy::Tab]), tab);
    }

    #[test]
    fn expected_form_is_composition_and_lf_only() {
        assert_eq!(expected_win_keys("a\r\nb\rc", &[]), "a\nb\nc");
        assert_eq!(expected_win_keys("\u{FA19}\u{301}", &[]), "\u{FA19}\u{301}");
        assert_eq!(expected_win_keys("e\u{301}", &[]), "é");
    }

    /// F-1f: the encoding is an adapter capability of (agent, OS), and the
    /// macOS A-6 path still sends a bracketed paste.
    #[test]
    fn input_encoding_capability() {
        let k = NewlineKey::CsiU;
        assert_eq!(
            input_encoding("codex", "windows", k),
            InputEncoding::WinKeys { newline: k }
        );
        for (a, os) in [
            ("codex", "macos"),
            ("claude", "windows"),
            ("claude", "macos"),
        ] {
            assert_eq!(
                input_encoding(a, os, k),
                InputEncoding::BracketedPaste,
                "{a} {os}"
            );
        }
        assert!(encode(b"x", Newline::Cr, 0)[0].starts_with(PASTE_START));
    }

    #[test]
    fn corpus_f_shape() {
        let c = corpus_f();
        for e in c.iter().filter(|e| !e.recorded) {
            assert!(lossy_input(&e.text).is_empty(), "{}", e.id);
            assert!(encode_win_keys(e.text.as_bytes(), NewlineKey::CsiU, &[]).is_ok());
        }
        let long = c.iter().find(|e| e.id == "long_multiline").unwrap();
        assert!(long.text.chars().count() > 1000 && long.text.len() < 8 * 1024);
        assert!(
            c.iter().filter(|e| e.recorded).all(|e| encode_win_keys(
                e.text.as_bytes(),
                NewlineKey::CsiU,
                &e.allow
            )
            .is_ok())
        );
    }
}
