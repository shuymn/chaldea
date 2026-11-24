//! The input encoders against goldens, a reference model of the CLIs' input handling, and
//! the size bound (`docs/design/input-encoders.md` §5.2, §6).
//!
//! Goldens are of two kinds: **measured** sequences the spike wrote and observed (Claude's
//! bracketed paste of the A-6 corpus with `cr`, the record format of the measured scalars,
//! the `w32` Shift+Enter pair, Claude's recorded prompts under `tests/data/`), and **derived**
//! sequences computed from those formats (every other record, the padding, Codex's three
//! pastes). The corpus table marks which kind each entry's Claude golden is;
//! `win_keys_measured` holds the measured records apart.
//!
//! The reference model decodes `Send` bytes the way the CLIs' documented input handling
//! does, independently of the encoder's code, and must give `expected_prompt`.

use chaldea_core::adapter::{CAPABILITIES, Capabilities, HostOs};
use chaldea_core::ids::Agent;
use chaldea_core::input::MAX_PROMPT_BYTES;
use chaldea_core::input::RejectReason::{
    self, ControlCharacter, EmptyPrompt, LeadingTrailingWhitespace, LossyText, Tab,
};
use chaldea_core::input::encode::{
    Encoded, MAX_EXPANSION, MAX_OVERHEAD, Purpose, Rejection, SUBMIT, check, encode,
    expected_prompt, prompt_matches,
};
use chaldea_core::payload::InputEncoding::{self, BracketedPaste, Positional, WinKeys};
use proptest::prelude::*;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::{canonical_combining_class, compose, decompose_canonical};

// ---- golden notation ---------------------------------------------------------------------

/// One bracketed paste of an already newline-mapped body.
macro_rules! p {
    ($s:expr) => {
        concat!("\x1b[200~", $s, "\x1b[201~")
    };
}
/// Codex's three pastes: a space, the body, a space.
macro_rules! p3 {
    ($s:expr) => {
        concat!(p!(" "), p!($s), p!(" "))
    };
}
/// A key-down and key-up record pair carrying one UTF-16 unit, in decimal.
macro_rules! r {
    ($u:literal) => {
        concat!("\x1b[0;0;", $u, ";1;0;1_\x1b[0;0;", $u, ";0;0;1_")
    };
}
/// The `w32` Shift+Enter key-down and key-up pair (measured, spike `NewlineKey::W32`).
macro_rules! nl {
    () => {
        "\x1b[13;28;13;1;16;1_\x1b[13;28;13;0;16;1_"
    };
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
const SHIFT_ENTER: &[u8] = nl!().as_bytes();

// ---- the corpus --------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Golden {
    /// The Claude bracketed-paste golden was written and observed by the spike (A-6).
    Measured,
    /// Every golden of the entry is computed from the measured formats.
    Derived,
}
use Golden::{Derived, Measured};

#[derive(Debug, Clone, Copy)]
enum T {
    Lit(&'static str),
    Gen(fn() -> String),
}

/// The outcome for one row and purpose.
#[derive(Debug, Clone, Copy)]
enum W {
    /// Accepted with these text bytes.
    Ok(&'static str),
    /// Accepted; the bytes are pinned by the model and the size tests.
    Large,
    /// Accepted as a positional argument (no PTY bytes).
    Argv,
    Err(RejectReason, Option<usize>),
}

/// The expected prompt for one encoding.
#[derive(Debug, Clone, Copy)]
enum E {
    /// The text itself.
    Text,
    Lit(&'static str),
    Fn(fn() -> String),
    No,
}

struct Entry {
    id: &'static str,
    text: T,
    golden: Golden,
    /// `[Send, AttachPaste]` on both Claude rows.
    claude: [W; 2],
    /// `[Send, AttachPaste]` on Codex macOS.
    codex_paste: [W; 2],
    /// `[Send, AttachPaste]` on Codex Windows.
    win_keys: [W; 2],
    /// `check(Codex, Positional, Send, _)`.
    positional: W,
    /// `expected_prompt` for Claude `BracketedPaste`, Codex `BracketedPaste`, `WinKeys`,
    /// `Positional`.
    expected: [E; 4],
}

impl Entry {
    fn text(&self) -> String {
        match self.text {
            T::Lit(s) => s.to_owned(),
            T::Gen(f) => f(),
        }
    }
}

/// `"日本語テキスト𠮷😀abc\n"` repeated, then `x` up to `target` bytes (spike `sized`).
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

fn sized_4k_plus_1() -> String {
    sized(4097)
}

/// More than 1,000 characters on 60 lines (spike `long_japanese`).
fn long_japanese() -> String {
    (1..=60)
        .map(|i| format!("{i:03}行目：日本語の長い入力テストです。"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `/zzexample ` and `x`s: `lf(t)` is 1,000 characters.
fn zzexample_1000() -> String {
    format!("/zzexample {}", "x".repeat(989))
}

fn zzexample_1001() -> String {
    format!("/zzexample {}", "x".repeat(990))
}

/// `U+0344 + "\n" × (MAX_PROMPT_BYTES − 4) + U+0344`: `MAX_PROMPT_BYTES` bytes.
fn near_bound() -> String {
    format!("\u{344}{}\u{344}", "\n".repeat(MAX_PROMPT_BYTES - 4))
}

fn near_bound_composed() -> String {
    format!(
        "\u{308}\u{301}{}\u{308}\u{301}",
        "\n".repeat(MAX_PROMPT_BYTES - 4)
    )
}

/// `MAX_PROMPT_BYTES` of mixed BMP text on many lines, ending in `x`.
fn max_prompt_mixed() -> String {
    let unit = "日本語テキスト\u{e9}あいう、ｶﾅ\n";
    let mut s = String::new();
    while s.len() + unit.len() <= MAX_PROMPT_BYTES {
        s.push_str(unit);
    }
    while s.len() < MAX_PROMPT_BYTES {
        s.push('x');
    }
    s
}

#[rustfmt::skip]
static CORPUS: &[Entry] = &[
    Entry {
        id: "a6_ascii",
        text: T::Lit("Hello, world! 0123456789 ~!@#$%^&*()_+-=[]{}|;':\",./<>?` tab\there"),
        golden: Measured,
        claude: [W::Ok(p!("Hello, world! 0123456789 ~!@#$%^&*()_+-=[]{}|;':\",./<>?` tab\there")), W::Ok(p!("Hello, world! 0123456789 ~!@#$%^&*()_+-=[]{}|;':\",./<>?` tab\there"))],
        codex_paste: [W::Ok(p3!("Hello, world! 0123456789 ~!@#$%^&*()_+-=[]{}|;':\",./<>?` tab\there")), W::Ok(p!("Hello, world! 0123456789 ~!@#$%^&*()_+-=[]{}|;':\",./<>?` tab\there"))],
        win_keys: [W::Err(Tab, Some(60)), W::Err(Tab, Some(60))],
        positional: W::Argv,
        expected: [E::Lit("Hello, world! 0123456789 ~!@#$%^&*()_+-=[]{}|;':\",./<>?` tab    here"), E::Text, E::No, E::Text],
    },
    Entry {
        id: "a6_japanese",
        text: T::Lit("日本語の入力テスト、句読点。「括弧」"),
        golden: Measured,
        claude: [W::Ok(p!("日本語の入力テスト、句読点。「括弧」")), W::Ok(p!("日本語の入力テスト、句読点。「括弧」"))],
        codex_paste: [W::Ok(p3!("日本語の入力テスト、句読点。「括弧」")), W::Ok(p!("日本語の入力テスト、句読点。「括弧」"))],
        win_keys: [W::Ok(concat!(" ", r!(26085), r!(26412), r!(35486), r!(12398), r!(20837), r!(21147), r!(12486), r!(12473), r!(12488), r!(12289), r!(21477), r!(35501), r!(28857), r!(12290), r!(12300), r!(25324), r!(24359), r!(12301), " ")), W::Ok(concat!(r!(26085), r!(26412), r!(35486), r!(12398), r!(20837), r!(21147), r!(12486), r!(12473), r!(12488), r!(12289), r!(21477), r!(35501), r!(28857), r!(12290), r!(12300), r!(25324), r!(24359), r!(12301)))],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "a6_halfwidth_kana",
        text: T::Lit("ｶﾀｶﾅ"),
        golden: Measured,
        claude: [W::Ok(p!("ｶﾀｶﾅ")), W::Ok(p!("ｶﾀｶﾅ"))],
        codex_paste: [W::Ok(p3!("ｶﾀｶﾅ")), W::Ok(p!("ｶﾀｶﾅ"))],
        win_keys: [W::Ok(concat!(" ", r!(65398), r!(65408), r!(65398), r!(65413), " ")), W::Ok(concat!(r!(65398), r!(65408), r!(65398), r!(65413)))],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "a6_astral",
        text: T::Lit("𠮷野家"),
        golden: Measured,
        claude: [W::Ok(p!("𠮷野家")), W::Ok(p!("𠮷野家"))],
        codex_paste: [W::Ok(p3!("𠮷野家")), W::Ok(p!("𠮷野家"))],
        win_keys: [W::Ok(concat!(" 𠮷", r!(37326), r!(23478), " ")), W::Ok(concat!("𠮷", r!(37326), r!(23478)))],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "a6_emoji",
        text: T::Lit("😀"),
        golden: Measured,
        claude: [W::Ok(p!("😀")), W::Ok(p!("😀"))],
        codex_paste: [W::Ok(p3!("😀")), W::Ok(p!("😀"))],
        win_keys: [W::Ok(" 😀 "), W::Ok("😀")],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "a6_emoji_zwj",
        text: T::Lit("👨\u{200d}👩\u{200d}👧\u{200d}👦"),
        golden: Measured,
        claude: [W::Ok(p!("👨\u{200d}👩\u{200d}👧\u{200d}👦")), W::Ok(p!("👨\u{200d}👩\u{200d}👧\u{200d}👦"))],
        codex_paste: [W::Ok(p3!("👨\u{200d}👩\u{200d}👧\u{200d}👦")), W::Ok(p!("👨\u{200d}👩\u{200d}👧\u{200d}👦"))],
        win_keys: [W::Ok(concat!(" 👨", r!(8205), "👩", r!(8205), "👧", r!(8205), "👦 ")), W::Ok(concat!("👨", r!(8205), "👩", r!(8205), "👧", r!(8205), "👦"))],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "a6_emoji_flag",
        text: T::Lit("🇯🇵"),
        golden: Measured,
        claude: [W::Ok(p!("🇯🇵")), W::Ok(p!("🇯🇵"))],
        codex_paste: [W::Ok(p3!("🇯🇵")), W::Ok(p!("🇯🇵"))],
        win_keys: [W::Ok(" 🇯🇵 "), W::Ok("🇯🇵")],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "a6_emoji_vs16",
        text: T::Lit("❤\u{fe0f}"),
        golden: Measured,
        claude: [W::Ok(p!("❤\u{fe0f}")), W::Ok(p!("❤\u{fe0f}"))],
        codex_paste: [W::Ok(p3!("❤\u{fe0f}")), W::Ok(p!("❤\u{fe0f}"))],
        win_keys: [W::Ok(concat!(" ", r!(10084), r!(65039), " ")), W::Ok(concat!(r!(10084), r!(65039)))],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "a6_decomposed_kana",
        text: T::Lit("か\u{3099}"),
        golden: Measured,
        claude: [W::Ok(p!("か\u{3099}")), W::Ok(p!("か\u{3099}"))],
        codex_paste: [W::Ok(p3!("か\u{3099}")), W::Ok(p!("か\u{3099}"))],
        win_keys: [W::Ok(concat!(" ", r!(12364), " ")), W::Ok(r!(12364))],
        positional: W::Argv,
        expected: [E::Lit("\u{304c}"), E::Text, E::Lit("\u{304c}"), E::Text],
    },
    Entry {
        id: "a6_decomposed_latin",
        text: T::Lit("e\u{301}"),
        golden: Measured,
        claude: [W::Ok(p!("e\u{301}")), W::Ok(p!("e\u{301}"))],
        codex_paste: [W::Ok(p3!("e\u{301}")), W::Ok(p!("e\u{301}"))],
        win_keys: [W::Ok(concat!(" ", r!(233), " ")), W::Ok(r!(233))],
        positional: W::Argv,
        expected: [E::Lit("\u{e9}"), E::Text, E::Lit("\u{e9}"), E::Text],
    },
    Entry {
        id: "a6_multiline_lf",
        text: T::Lit("一行目の日本語\n二行目、句読点。\n三行目「括弧」"),
        golden: Measured,
        claude: [W::Ok(p!("一行目の日本語\r二行目、句読点。\r三行目「括弧」")), W::Ok(p!("一行目の日本語\r二行目、句読点。\r三行目「括弧」"))],
        codex_paste: [W::Ok(p3!("一行目の日本語\r二行目、句読点。\r三行目「括弧」")), W::Ok(p!("一行目の日本語\r二行目、句読点。\r三行目「括弧」"))],
        win_keys: [W::Ok(concat!(" ", r!(19968), r!(34892), r!(30446), r!(12398), r!(26085), r!(26412), r!(35486), nl!(), r!(20108), r!(34892), r!(30446), r!(12289), r!(21477), r!(35501), r!(28857), r!(12290), nl!(), r!(19977), r!(34892), r!(30446), r!(12300), r!(25324), r!(24359), r!(12301), " ")), W::Ok(concat!(r!(19968), r!(34892), r!(30446), r!(12398), r!(26085), r!(26412), r!(35486), nl!(), r!(20108), r!(34892), r!(30446), r!(12289), r!(21477), r!(35501), r!(28857), r!(12290), nl!(), r!(19977), r!(34892), r!(30446), r!(12300), r!(25324), r!(24359), r!(12301)))],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "a6_multiline_crlf",
        text: T::Lit("一行目の日本語\r\n二行目、句読点。\r\n三行目「括弧」"),
        golden: Measured,
        claude: [W::Ok(p!("一行目の日本語\r二行目、句読点。\r三行目「括弧」")), W::Ok(p!("一行目の日本語\r二行目、句読点。\r三行目「括弧」"))],
        codex_paste: [W::Ok(p3!("一行目の日本語\r二行目、句読点。\r三行目「括弧」")), W::Ok(p!("一行目の日本語\r二行目、句読点。\r三行目「括弧」"))],
        win_keys: [W::Ok(concat!(" ", r!(19968), r!(34892), r!(30446), r!(12398), r!(26085), r!(26412), r!(35486), nl!(), r!(20108), r!(34892), r!(30446), r!(12289), r!(21477), r!(35501), r!(28857), r!(12290), nl!(), r!(19977), r!(34892), r!(30446), r!(12300), r!(25324), r!(24359), r!(12301), " ")), W::Ok(concat!(r!(19968), r!(34892), r!(30446), r!(12398), r!(26085), r!(26412), r!(35486), nl!(), r!(20108), r!(34892), r!(30446), r!(12289), r!(21477), r!(35501), r!(28857), r!(12290), nl!(), r!(19977), r!(34892), r!(30446), r!(12300), r!(25324), r!(24359), r!(12301)))],
        positional: W::Argv,
        expected: [E::Lit("一行目の日本語\n二行目、句読点。\n三行目「括弧」"), E::Lit("一行目の日本語\n二行目、句読点。\n三行目「括弧」"), E::Lit("一行目の日本語\n二行目、句読点。\n三行目「括弧」"), E::Lit("一行目の日本語\n二行目、句読点。\n三行目「括弧」")],
    },
    Entry {
        id: "a6_size_4k_plus_1",
        text: T::Gen(sized_4k_plus_1),
        golden: Measured,
        claude: [W::Large, W::Large],
        codex_paste: [W::Large, W::Large],
        win_keys: [W::Large, W::Large],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "f_ascii",
        text: T::Lit("Hello, world! 0123456789 ~#$%^&*()_+-=[]{}|;':\",./<>"),
        golden: Derived,
        claude: [W::Ok(p!("Hello, world! 0123456789 ~#$%^&*()_+-=[]{}|;':\",./<>")), W::Ok(p!("Hello, world! 0123456789 ~#$%^&*()_+-=[]{}|;':\",./<>"))],
        codex_paste: [W::Ok(p3!("Hello, world! 0123456789 ~#$%^&*()_+-=[]{}|;':\",./<>")), W::Ok(p!("Hello, world! 0123456789 ~#$%^&*()_+-=[]{}|;':\",./<>"))],
        win_keys: [W::Ok(" Hello, world! 0123456789 ~#$%^&*()_+-=[]{}|;':\",./<> "), W::Ok("Hello, world! 0123456789 ~#$%^&*()_+-=[]{}|;':\",./<>")],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "f_japanese",
        text: T::Lit("日本語の入力テスト、句読点。「括弧」"),
        golden: Derived,
        claude: [W::Ok(p!("日本語の入力テスト、句読点。「括弧」")), W::Ok(p!("日本語の入力テスト、句読点。「括弧」"))],
        codex_paste: [W::Ok(p3!("日本語の入力テスト、句読点。「括弧」")), W::Ok(p!("日本語の入力テスト、句読点。「括弧」"))],
        win_keys: [W::Ok(concat!(" ", r!(26085), r!(26412), r!(35486), r!(12398), r!(20837), r!(21147), r!(12486), r!(12473), r!(12488), r!(12289), r!(21477), r!(35501), r!(28857), r!(12290), r!(12300), r!(25324), r!(24359), r!(12301), " ")), W::Ok(concat!(r!(26085), r!(26412), r!(35486), r!(12398), r!(20837), r!(21147), r!(12486), r!(12473), r!(12488), r!(12289), r!(21477), r!(35501), r!(28857), r!(12290), r!(12300), r!(25324), r!(24359), r!(12301)))],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "f_astral",
        text: T::Lit("𠮷野家"),
        golden: Derived,
        claude: [W::Ok(p!("𠮷野家")), W::Ok(p!("𠮷野家"))],
        codex_paste: [W::Ok(p3!("𠮷野家")), W::Ok(p!("𠮷野家"))],
        win_keys: [W::Ok(concat!(" 𠮷", r!(37326), r!(23478), " ")), W::Ok(concat!("𠮷", r!(37326), r!(23478)))],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "f_emoji_zwj",
        text: T::Lit("👨\u{200d}👩\u{200d}👧\u{200d}👦"),
        golden: Derived,
        claude: [W::Ok(p!("👨\u{200d}👩\u{200d}👧\u{200d}👦")), W::Ok(p!("👨\u{200d}👩\u{200d}👧\u{200d}👦"))],
        codex_paste: [W::Ok(p3!("👨\u{200d}👩\u{200d}👧\u{200d}👦")), W::Ok(p!("👨\u{200d}👩\u{200d}👧\u{200d}👦"))],
        win_keys: [W::Ok(concat!(" 👨", r!(8205), "👩", r!(8205), "👧", r!(8205), "👦 ")), W::Ok(concat!("👨", r!(8205), "👩", r!(8205), "👧", r!(8205), "👦"))],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "f_emoji_flag",
        text: T::Lit("🇯🇵"),
        golden: Derived,
        claude: [W::Ok(p!("🇯🇵")), W::Ok(p!("🇯🇵"))],
        codex_paste: [W::Ok(p3!("🇯🇵")), W::Ok(p!("🇯🇵"))],
        win_keys: [W::Ok(" 🇯🇵 "), W::Ok("🇯🇵")],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "f_emoji_vs16",
        text: T::Lit("❤\u{fe0f}"),
        golden: Derived,
        claude: [W::Ok(p!("❤\u{fe0f}")), W::Ok(p!("❤\u{fe0f}"))],
        codex_paste: [W::Ok(p3!("❤\u{fe0f}")), W::Ok(p!("❤\u{fe0f}"))],
        win_keys: [W::Ok(concat!(" ", r!(10084), r!(65039), " ")), W::Ok(concat!(r!(10084), r!(65039)))],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "f_decomposed_kana",
        text: T::Lit("か\u{3099}"),
        golden: Derived,
        claude: [W::Ok(p!("か\u{3099}")), W::Ok(p!("か\u{3099}"))],
        codex_paste: [W::Ok(p3!("か\u{3099}")), W::Ok(p!("か\u{3099}"))],
        win_keys: [W::Ok(concat!(" ", r!(12364), " ")), W::Ok(r!(12364))],
        positional: W::Argv,
        expected: [E::Lit("\u{304c}"), E::Text, E::Lit("\u{304c}"), E::Text],
    },
    Entry {
        id: "f_decomposed_latin",
        text: T::Lit("e\u{301}"),
        golden: Derived,
        claude: [W::Ok(p!("e\u{301}")), W::Ok(p!("e\u{301}"))],
        codex_paste: [W::Ok(p3!("e\u{301}")), W::Ok(p!("e\u{301}"))],
        win_keys: [W::Ok(concat!(" ", r!(233), " ")), W::Ok(r!(233))],
        positional: W::Argv,
        expected: [E::Lit("\u{e9}"), E::Text, E::Lit("\u{e9}"), E::Text],
    },
    Entry {
        id: "f_cjk_compat",
        text: T::Lit("\u{fa19}"),
        golden: Derived,
        claude: [W::Ok(p!("\u{fa19}")), W::Ok(p!("\u{fa19}"))],
        codex_paste: [W::Ok(p3!("\u{fa19}")), W::Ok(p!("\u{fa19}"))],
        win_keys: [W::Ok(concat!(" ", r!(64025), " ")), W::Ok(r!(64025))],
        positional: W::Argv,
        expected: [E::Lit("神"), E::Text, E::Text, E::Text],
    },
    Entry {
        id: "f_multiline_lf",
        text: T::Lit("一行目の日本語\n二行目、句読点。\n三行目「括弧」"),
        golden: Derived,
        claude: [W::Ok(p!("一行目の日本語\r二行目、句読点。\r三行目「括弧」")), W::Ok(p!("一行目の日本語\r二行目、句読点。\r三行目「括弧」"))],
        codex_paste: [W::Ok(p3!("一行目の日本語\r二行目、句読点。\r三行目「括弧」")), W::Ok(p!("一行目の日本語\r二行目、句読点。\r三行目「括弧」"))],
        win_keys: [W::Ok(concat!(" ", r!(19968), r!(34892), r!(30446), r!(12398), r!(26085), r!(26412), r!(35486), nl!(), r!(20108), r!(34892), r!(30446), r!(12289), r!(21477), r!(35501), r!(28857), r!(12290), nl!(), r!(19977), r!(34892), r!(30446), r!(12300), r!(25324), r!(24359), r!(12301), " ")), W::Ok(concat!(r!(19968), r!(34892), r!(30446), r!(12398), r!(26085), r!(26412), r!(35486), nl!(), r!(20108), r!(34892), r!(30446), r!(12289), r!(21477), r!(35501), r!(28857), r!(12290), nl!(), r!(19977), r!(34892), r!(30446), r!(12300), r!(25324), r!(24359), r!(12301)))],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "f_multiline_crlf",
        text: T::Lit("一行目の日本語\r\n二行目、句読点。\r\n三行目「括弧」"),
        golden: Derived,
        claude: [W::Ok(p!("一行目の日本語\r二行目、句読点。\r三行目「括弧」")), W::Ok(p!("一行目の日本語\r二行目、句読点。\r三行目「括弧」"))],
        codex_paste: [W::Ok(p3!("一行目の日本語\r二行目、句読点。\r三行目「括弧」")), W::Ok(p!("一行目の日本語\r二行目、句読点。\r三行目「括弧」"))],
        win_keys: [W::Ok(concat!(" ", r!(19968), r!(34892), r!(30446), r!(12398), r!(26085), r!(26412), r!(35486), nl!(), r!(20108), r!(34892), r!(30446), r!(12289), r!(21477), r!(35501), r!(28857), r!(12290), nl!(), r!(19977), r!(34892), r!(30446), r!(12300), r!(25324), r!(24359), r!(12301), " ")), W::Ok(concat!(r!(19968), r!(34892), r!(30446), r!(12398), r!(26085), r!(26412), r!(35486), nl!(), r!(20108), r!(34892), r!(30446), r!(12289), r!(21477), r!(35501), r!(28857), r!(12290), nl!(), r!(19977), r!(34892), r!(30446), r!(12300), r!(25324), r!(24359), r!(12301)))],
        positional: W::Argv,
        expected: [E::Lit("一行目の日本語\n二行目、句読点。\n三行目「括弧」"), E::Lit("一行目の日本語\n二行目、句読点。\n三行目「括弧」"), E::Lit("一行目の日本語\n二行目、句読点。\n三行目「括弧」"), E::Lit("一行目の日本語\n二行目、句読点。\n三行目「括弧」")],
    },
    Entry {
        id: "f_multiline_blank",
        text: T::Lit("一行目の日本語\n\n三行目「括弧」"),
        golden: Derived,
        claude: [W::Ok(p!("一行目の日本語\r\r三行目「括弧」")), W::Ok(p!("一行目の日本語\r\r三行目「括弧」"))],
        codex_paste: [W::Ok(p3!("一行目の日本語\r\r三行目「括弧」")), W::Ok(p!("一行目の日本語\r\r三行目「括弧」"))],
        win_keys: [W::Ok(concat!(" ", r!(19968), r!(34892), r!(30446), r!(12398), r!(26085), r!(26412), r!(35486), nl!(), nl!(), r!(19977), r!(34892), r!(30446), r!(12300), r!(25324), r!(24359), r!(12301), " ")), W::Ok(concat!(r!(19968), r!(34892), r!(30446), r!(12398), r!(26085), r!(26412), r!(35486), nl!(), nl!(), r!(19977), r!(34892), r!(30446), r!(12300), r!(25324), r!(24359), r!(12301)))],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "f_slash_first",
        text: T::Lit("/review の件"),
        golden: Derived,
        claude: [W::Ok(p!("/review の件")), W::Ok(p!("/review の件"))],
        codex_paste: [W::Ok(p3!("/review の件")), W::Ok(p!("/review の件"))],
        win_keys: [W::Ok(concat!(" /review ", r!(12398), r!(20214), " ")), W::Ok(concat!("/review ", r!(12398), r!(20214)))],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "f_bang_first",
        text: T::Lit("!ls は実行しない"),
        golden: Derived,
        claude: [W::Ok(p!("!ls は実行しない")), W::Ok(p!("!ls は実行しない"))],
        codex_paste: [W::Ok(p3!("!ls は実行しない")), W::Ok(p!("!ls は実行しない"))],
        win_keys: [W::Ok(concat!(" !ls ", r!(12399), r!(23455), r!(34892), r!(12375), r!(12394), r!(12356), " ")), W::Ok(concat!("!ls ", r!(12399), r!(23455), r!(34892), r!(12375), r!(12394), r!(12356)))],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "f_question_first",
        text: T::Lit("?の意味"),
        golden: Derived,
        claude: [W::Ok(p!("?の意味")), W::Ok(p!("?の意味"))],
        codex_paste: [W::Ok(p3!("?の意味")), W::Ok(p!("?の意味"))],
        win_keys: [W::Ok(concat!(" ?", r!(12398), r!(24847), r!(21619), " ")), W::Ok(concat!("?", r!(12398), r!(24847), r!(21619)))],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "f_at_end",
        text: T::Lit("参照してく\u{3060}さい @README.md"),
        golden: Derived,
        claude: [W::Ok(p!("参照してく\u{3060}さい @README.md")), W::Ok(p!("参照してく\u{3060}さい @README.md"))],
        codex_paste: [W::Ok(p3!("参照してく\u{3060}さい @README.md")), W::Ok(p!("参照してく\u{3060}さい @README.md"))],
        win_keys: [W::Ok(concat!(" ", r!(21442), r!(29031), r!(12375), r!(12390), r!(12367), r!(12384), r!(12373), r!(12356), " @README.md ")), W::Ok(concat!(r!(21442), r!(29031), r!(12375), r!(12390), r!(12367), r!(12384), r!(12373), r!(12356), " @README.md"))],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "f_slash_second_line",
        text: T::Lit("一行目\n/二行目"),
        golden: Derived,
        claude: [W::Ok(p!("一行目\r/二行目")), W::Ok(p!("一行目\r/二行目"))],
        codex_paste: [W::Ok(p3!("一行目\r/二行目")), W::Ok(p!("一行目\r/二行目"))],
        win_keys: [W::Ok(concat!(" ", r!(19968), r!(34892), r!(30446), nl!(), "/", r!(20108), r!(34892), r!(30446), " ")), W::Ok(concat!(r!(19968), r!(34892), r!(30446), nl!(), "/", r!(20108), r!(34892), r!(30446)))],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "f_bang_second_line",
        text: T::Lit("一行目\n!二行目"),
        golden: Derived,
        claude: [W::Ok(p!("一行目\r!二行目")), W::Ok(p!("一行目\r!二行目"))],
        codex_paste: [W::Ok(p3!("一行目\r!二行目")), W::Ok(p!("一行目\r!二行目"))],
        win_keys: [W::Ok(concat!(" ", r!(19968), r!(34892), r!(30446), nl!(), "!", r!(20108), r!(34892), r!(30446), " ")), W::Ok(concat!(r!(19968), r!(34892), r!(30446), nl!(), "!", r!(20108), r!(34892), r!(30446)))],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "f_long_multiline",
        text: T::Gen(long_japanese),
        golden: Derived,
        claude: [W::Large, W::Large],
        codex_paste: [W::Large, W::Large],
        win_keys: [W::Large, W::Large],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "f_small_kana_mark",
        text: T::Lit("ㇷ\u{309a}"),
        golden: Derived,
        claude: [W::Ok(p!("ㇷ\u{309a}")), W::Ok(p!("ㇷ\u{309a}"))],
        codex_paste: [W::Ok(p3!("ㇷ\u{309a}")), W::Ok(p!("ㇷ\u{309a}"))],
        win_keys: [W::Ok(concat!(" ", r!(12791), r!(12442), " ")), W::Ok(concat!(r!(12791), r!(12442)))],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "f_lossy_trim",
        text: T::Lit("\u{3000}一行目\n二行目\n"),
        golden: Derived,
        claude: [W::Ok(p!("\u{3000}一行目\r二行目\r")), W::Ok(p!("\u{3000}一行目\r二行目\r"))],
        codex_paste: [W::Ok(p3!("\u{3000}一行目\r二行目\r")), W::Ok(p!("\u{3000}一行目\r二行目\r"))],
        win_keys: [W::Err(LeadingTrailingWhitespace, Some(0)), W::Err(LeadingTrailingWhitespace, Some(0))],
        positional: W::Argv,
        expected: [E::Text, E::Lit("一行目\n二行目"), E::No, E::Text],
    },
    Entry {
        id: "f_lossy_tab",
        text: T::Lit("列1\t列2"),
        golden: Derived,
        claude: [W::Ok(p!("列1\t列2")), W::Ok(p!("列1\t列2"))],
        codex_paste: [W::Ok(p3!("列1\t列2")), W::Ok(p!("列1\t列2"))],
        win_keys: [W::Err(Tab, Some(4)), W::Err(Tab, Some(4))],
        positional: W::Argv,
        expected: [E::Lit("列1    列2"), E::Text, E::No, E::Text],
    },
    Entry {
        id: "bengali_vowel_sign",
        text: T::Lit("\u{9c7}\u{9be}"),
        golden: Derived,
        claude: [W::Ok(p!("\u{9c7}\u{9be}")), W::Ok(p!("\u{9c7}\u{9be}"))],
        codex_paste: [W::Ok(p3!("\u{9c7}\u{9be}")), W::Ok(p!("\u{9c7}\u{9be}"))],
        win_keys: [W::Ok(concat!(" ", r!(2507), " ")), W::Ok(r!(2507))],
        positional: W::Argv,
        expected: [E::Lit("\u{9cb}"), E::Text, E::Lit("\u{9cb}"), E::Text],
    },
    Entry {
        id: "compat_with_acute",
        text: T::Lit("\u{fa19}\u{301}"),
        golden: Derived,
        claude: [W::Ok(p!("\u{fa19}\u{301}")), W::Ok(p!("\u{fa19}\u{301}"))],
        codex_paste: [W::Ok(p3!("\u{fa19}\u{301}")), W::Ok(p!("\u{fa19}\u{301}"))],
        win_keys: [W::Ok(concat!(" ", r!(64025), r!(769), " ")), W::Ok(concat!(r!(64025), r!(769)))],
        positional: W::Argv,
        expected: [E::Lit("神\u{301}"), E::Text, E::Text, E::Text],
    },
    Entry {
        id: "hangul_l_v",
        text: T::Lit("가"),
        golden: Derived,
        claude: [W::Ok(p!("가")), W::Ok(p!("가"))],
        codex_paste: [W::Ok(p3!("가")), W::Ok(p!("가"))],
        win_keys: [W::Ok(concat!(" ", r!(4352), r!(4449), " ")), W::Ok(concat!(r!(4352), r!(4449)))],
        positional: W::Argv,
        expected: [E::Lit("\u{ac00}"), E::Text, E::Text, E::Text],
    },
    Entry {
        id: "u0344",
        text: T::Lit("\u{344}"),
        golden: Derived,
        claude: [W::Ok(p!("\u{344}")), W::Ok(p!("\u{344}"))],
        codex_paste: [W::Ok(p3!("\u{344}")), W::Ok(p!("\u{344}"))],
        win_keys: [W::Ok(concat!(" ", r!(776), r!(769), " ")), W::Ok(concat!(r!(776), r!(769)))],
        positional: W::Argv,
        expected: [E::Lit("\u{308}\u{301}"), E::Text, E::Lit("\u{308}\u{301}"), E::Text],
    },
    Entry {
        id: "fb2c_05b0",
        text: T::Lit("\u{fb2c}\u{5b0}"),
        golden: Derived,
        claude: [W::Ok(p!("\u{fb2c}\u{5b0}")), W::Ok(p!("\u{fb2c}\u{5b0}"))],
        codex_paste: [W::Ok(p3!("\u{fb2c}\u{5b0}")), W::Ok(p!("\u{fb2c}\u{5b0}"))],
        win_keys: [W::Ok(concat!(" ", r!(1513), r!(1456), r!(1468), r!(1473), " ")), W::Ok(concat!(r!(1513), r!(1456), r!(1468), r!(1473)))],
        positional: W::Argv,
        expected: [E::Lit("ש\u{5b0}\u{5bc}\u{5c1}"), E::Text, E::Lit("ש\u{5b0}\u{5bc}\u{5c1}"), E::Text],
    },
    Entry {
        id: "keycap",
        text: T::Lit("1\u{fe0f}\u{20e3}"),
        golden: Derived,
        claude: [W::Ok(p!("1\u{fe0f}\u{20e3}")), W::Ok(p!("1\u{fe0f}\u{20e3}"))],
        codex_paste: [W::Ok(p3!("1\u{fe0f}\u{20e3}")), W::Ok(p!("1\u{fe0f}\u{20e3}"))],
        win_keys: [W::Ok(concat!(" 1", r!(65039), r!(8419), " ")), W::Ok(concat!("1", r!(65039), r!(8419)))],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "astral_variation_selector",
        text: T::Lit("葛\u{e0100}"),
        golden: Derived,
        claude: [W::Ok(p!("葛\u{e0100}")), W::Ok(p!("葛\u{e0100}"))],
        codex_paste: [W::Ok(p3!("葛\u{e0100}")), W::Ok(p!("葛\u{e0100}"))],
        win_keys: [W::Ok(concat!(" ", r!(33883), "\u{e0100} ")), W::Ok(concat!(r!(33883), "\u{e0100}"))],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "u1d165",
        text: T::Lit("\u{1d165}"),
        golden: Derived,
        claude: [W::Ok(p!("\u{1d165}")), W::Ok(p!("\u{1d165}"))],
        codex_paste: [W::Ok(p3!("\u{1d165}")), W::Ok(p!("\u{1d165}"))],
        win_keys: [W::Ok(" \u{1d165} "), W::Ok("\u{1d165}")],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "c0_00",
        text: T::Lit("a\u{0}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_01",
        text: T::Lit("a\u{1}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_02",
        text: T::Lit("a\u{2}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_03",
        text: T::Lit("a\u{3}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_04",
        text: T::Lit("a\u{4}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_05",
        text: T::Lit("a\u{5}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_06",
        text: T::Lit("a\u{6}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_07",
        text: T::Lit("a\u{7}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_08",
        text: T::Lit("a\u{8}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_0b",
        text: T::Lit("a\u{b}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_0c",
        text: T::Lit("a\u{c}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_0e",
        text: T::Lit("a\u{e}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_0f",
        text: T::Lit("a\u{f}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_10",
        text: T::Lit("a\u{10}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_11",
        text: T::Lit("a\u{11}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_12",
        text: T::Lit("a\u{12}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_13",
        text: T::Lit("a\u{13}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_14",
        text: T::Lit("a\u{14}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_15",
        text: T::Lit("a\u{15}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_16",
        text: T::Lit("a\u{16}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_17",
        text: T::Lit("a\u{17}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_18",
        text: T::Lit("a\u{18}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_19",
        text: T::Lit("a\u{19}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_1a",
        text: T::Lit("a\u{1a}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_1b",
        text: T::Lit("a\u{1b}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_1c",
        text: T::Lit("a\u{1c}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_1d",
        text: T::Lit("a\u{1d}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_1e",
        text: T::Lit("a\u{1e}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "c0_1f",
        text: T::Lit("a\u{1f}b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "esc_alone",
        text: T::Lit("\u{1b}"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(0)), W::Err(ControlCharacter, Some(0))],
        codex_paste: [W::Err(ControlCharacter, Some(0)), W::Err(ControlCharacter, Some(0))],
        win_keys: [W::Err(ControlCharacter, Some(0)), W::Err(ControlCharacter, Some(0))],
        positional: W::Err(ControlCharacter, Some(0)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "paste_end_inside",
        text: T::Lit("a\u{1b}[201~b"),
        golden: Derived,
        claude: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        codex_paste: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        win_keys: [W::Err(ControlCharacter, Some(1)), W::Err(ControlCharacter, Some(1))],
        positional: W::Err(ControlCharacter, Some(1)),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "del_c1_7f",
        text: T::Lit("a\u{7f}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{7f}b")), W::Ok(p!("a\u{7f}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::No, E::Text],
    },
    Entry {
        id: "del_c1_80",
        text: T::Lit("a\u{80}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{80}b")), W::Ok(p!("a\u{80}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(128), "b ")), W::Ok(concat!("a", r!(128), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_81",
        text: T::Lit("a\u{81}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{81}b")), W::Ok(p!("a\u{81}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(129), "b ")), W::Ok(concat!("a", r!(129), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_82",
        text: T::Lit("a\u{82}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{82}b")), W::Ok(p!("a\u{82}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(130), "b ")), W::Ok(concat!("a", r!(130), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_83",
        text: T::Lit("a\u{83}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{83}b")), W::Ok(p!("a\u{83}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(131), "b ")), W::Ok(concat!("a", r!(131), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_84",
        text: T::Lit("a\u{84}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{84}b")), W::Ok(p!("a\u{84}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(132), "b ")), W::Ok(concat!("a", r!(132), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_85",
        text: T::Lit("a\u{85}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{85}b")), W::Ok(p!("a\u{85}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(133), "b ")), W::Ok(concat!("a", r!(133), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_86",
        text: T::Lit("a\u{86}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{86}b")), W::Ok(p!("a\u{86}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(134), "b ")), W::Ok(concat!("a", r!(134), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_87",
        text: T::Lit("a\u{87}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{87}b")), W::Ok(p!("a\u{87}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(135), "b ")), W::Ok(concat!("a", r!(135), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_88",
        text: T::Lit("a\u{88}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{88}b")), W::Ok(p!("a\u{88}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(136), "b ")), W::Ok(concat!("a", r!(136), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_89",
        text: T::Lit("a\u{89}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{89}b")), W::Ok(p!("a\u{89}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(137), "b ")), W::Ok(concat!("a", r!(137), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_8a",
        text: T::Lit("a\u{8a}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{8a}b")), W::Ok(p!("a\u{8a}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(138), "b ")), W::Ok(concat!("a", r!(138), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_8b",
        text: T::Lit("a\u{8b}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{8b}b")), W::Ok(p!("a\u{8b}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(139), "b ")), W::Ok(concat!("a", r!(139), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_8c",
        text: T::Lit("a\u{8c}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{8c}b")), W::Ok(p!("a\u{8c}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(140), "b ")), W::Ok(concat!("a", r!(140), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_8d",
        text: T::Lit("a\u{8d}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{8d}b")), W::Ok(p!("a\u{8d}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(141), "b ")), W::Ok(concat!("a", r!(141), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_8e",
        text: T::Lit("a\u{8e}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{8e}b")), W::Ok(p!("a\u{8e}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(142), "b ")), W::Ok(concat!("a", r!(142), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_8f",
        text: T::Lit("a\u{8f}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{8f}b")), W::Ok(p!("a\u{8f}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(143), "b ")), W::Ok(concat!("a", r!(143), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_90",
        text: T::Lit("a\u{90}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{90}b")), W::Ok(p!("a\u{90}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(144), "b ")), W::Ok(concat!("a", r!(144), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_91",
        text: T::Lit("a\u{91}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{91}b")), W::Ok(p!("a\u{91}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(145), "b ")), W::Ok(concat!("a", r!(145), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_92",
        text: T::Lit("a\u{92}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{92}b")), W::Ok(p!("a\u{92}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(146), "b ")), W::Ok(concat!("a", r!(146), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_93",
        text: T::Lit("a\u{93}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{93}b")), W::Ok(p!("a\u{93}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(147), "b ")), W::Ok(concat!("a", r!(147), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_94",
        text: T::Lit("a\u{94}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{94}b")), W::Ok(p!("a\u{94}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(148), "b ")), W::Ok(concat!("a", r!(148), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_95",
        text: T::Lit("a\u{95}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{95}b")), W::Ok(p!("a\u{95}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(149), "b ")), W::Ok(concat!("a", r!(149), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_96",
        text: T::Lit("a\u{96}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{96}b")), W::Ok(p!("a\u{96}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(150), "b ")), W::Ok(concat!("a", r!(150), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_97",
        text: T::Lit("a\u{97}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{97}b")), W::Ok(p!("a\u{97}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(151), "b ")), W::Ok(concat!("a", r!(151), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_98",
        text: T::Lit("a\u{98}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{98}b")), W::Ok(p!("a\u{98}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(152), "b ")), W::Ok(concat!("a", r!(152), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_99",
        text: T::Lit("a\u{99}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{99}b")), W::Ok(p!("a\u{99}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(153), "b ")), W::Ok(concat!("a", r!(153), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_9a",
        text: T::Lit("a\u{9a}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{9a}b")), W::Ok(p!("a\u{9a}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(154), "b ")), W::Ok(concat!("a", r!(154), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_9b",
        text: T::Lit("a\u{9b}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{9b}b")), W::Ok(p!("a\u{9b}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(155), "b ")), W::Ok(concat!("a", r!(155), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_9c",
        text: T::Lit("a\u{9c}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{9c}b")), W::Ok(p!("a\u{9c}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(156), "b ")), W::Ok(concat!("a", r!(156), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_9d",
        text: T::Lit("a\u{9d}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{9d}b")), W::Ok(p!("a\u{9d}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(157), "b ")), W::Ok(concat!("a", r!(157), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_9e",
        text: T::Lit("a\u{9e}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{9e}b")), W::Ok(p!("a\u{9e}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(158), "b ")), W::Ok(concat!("a", r!(158), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "del_c1_9f",
        text: T::Lit("a\u{9f}b"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{9f}b")), W::Ok(p!("a\u{9f}b"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Ok(concat!(" a", r!(159), "b ")), W::Ok(concat!("a", r!(159), "b"))],
        positional: W::Argv,
        expected: [E::Text, E::No, E::Text, E::Text],
    },
    Entry {
        id: "empty",
        text: T::Lit(""),
        golden: Derived,
        claude: [W::Err(EmptyPrompt, None), W::Err(EmptyPrompt, None)],
        codex_paste: [W::Err(EmptyPrompt, None), W::Err(EmptyPrompt, None)],
        win_keys: [W::Err(EmptyPrompt, None), W::Err(EmptyPrompt, None)],
        positional: W::Err(EmptyPrompt, None),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "space",
        text: T::Lit(" "),
        golden: Derived,
        claude: [W::Err(EmptyPrompt, None), W::Ok(p!(" "))],
        codex_paste: [W::Err(EmptyPrompt, None), W::Ok(p!(" "))],
        win_keys: [W::Err(EmptyPrompt, None), W::Err(LeadingTrailingWhitespace, Some(0))],
        positional: W::Err(EmptyPrompt, None),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "ideographic_space",
        text: T::Lit("\u{3000}"),
        golden: Derived,
        claude: [W::Err(EmptyPrompt, None), W::Ok(p!("\u{3000}"))],
        codex_paste: [W::Err(EmptyPrompt, None), W::Ok(p!("\u{3000}"))],
        win_keys: [W::Err(EmptyPrompt, None), W::Err(LeadingTrailingWhitespace, Some(0))],
        positional: W::Err(EmptyPrompt, None),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "lf_only",
        text: T::Lit("\n"),
        golden: Derived,
        claude: [W::Err(EmptyPrompt, None), W::Ok(p!("\r"))],
        codex_paste: [W::Err(EmptyPrompt, None), W::Ok(p!("\r"))],
        win_keys: [W::Err(EmptyPrompt, None), W::Err(LeadingTrailingWhitespace, Some(0))],
        positional: W::Err(EmptyPrompt, None),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "tab_only",
        text: T::Lit("\t"),
        golden: Derived,
        claude: [W::Err(EmptyPrompt, None), W::Ok(p!("\t"))],
        codex_paste: [W::Err(EmptyPrompt, None), W::Ok(p!("\t"))],
        win_keys: [W::Err(EmptyPrompt, None), W::Err(LeadingTrailingWhitespace, Some(0))],
        positional: W::Err(EmptyPrompt, None),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "four_spaces",
        text: T::Lit("    "),
        golden: Derived,
        claude: [W::Err(EmptyPrompt, None), W::Ok(p!("    "))],
        codex_paste: [W::Err(EmptyPrompt, None), W::Ok(p!("    "))],
        win_keys: [W::Err(EmptyPrompt, None), W::Err(LeadingTrailingWhitespace, Some(0))],
        positional: W::Err(EmptyPrompt, None),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "two_lf",
        text: T::Lit("\n\n"),
        golden: Derived,
        claude: [W::Err(EmptyPrompt, None), W::Ok(p!("\r\r"))],
        codex_paste: [W::Err(EmptyPrompt, None), W::Ok(p!("\r\r"))],
        win_keys: [W::Err(EmptyPrompt, None), W::Err(LeadingTrailingWhitespace, Some(0))],
        positional: W::Err(EmptyPrompt, None),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "leading_space",
        text: T::Lit(" a"),
        golden: Derived,
        claude: [W::Ok(p!(" a")), W::Ok(p!(" a"))],
        codex_paste: [W::Ok(p3!(" a")), W::Ok(p!(" a"))],
        win_keys: [W::Err(LeadingTrailingWhitespace, Some(0)), W::Err(LeadingTrailingWhitespace, Some(0))],
        positional: W::Argv,
        expected: [E::Text, E::Lit("a"), E::No, E::Text],
    },
    Entry {
        id: "trailing_space",
        text: T::Lit("a "),
        golden: Derived,
        claude: [W::Ok(p!("a ")), W::Ok(p!("a "))],
        codex_paste: [W::Ok(p3!("a ")), W::Ok(p!("a "))],
        win_keys: [W::Err(LeadingTrailingWhitespace, Some(1)), W::Err(LeadingTrailingWhitespace, Some(1))],
        positional: W::Argv,
        expected: [E::Text, E::Lit("a"), E::No, E::Text],
    },
    Entry {
        id: "leading_ideographic_space",
        text: T::Lit("\u{3000}a"),
        golden: Derived,
        claude: [W::Ok(p!("\u{3000}a")), W::Ok(p!("\u{3000}a"))],
        codex_paste: [W::Ok(p3!("\u{3000}a")), W::Ok(p!("\u{3000}a"))],
        win_keys: [W::Err(LeadingTrailingWhitespace, Some(0)), W::Err(LeadingTrailingWhitespace, Some(0))],
        positional: W::Argv,
        expected: [E::Text, E::Lit("a"), E::No, E::Text],
    },
    Entry {
        id: "trailing_ideographic_space",
        text: T::Lit("a\u{3000}"),
        golden: Derived,
        claude: [W::Ok(p!("a\u{3000}")), W::Ok(p!("a\u{3000}"))],
        codex_paste: [W::Ok(p3!("a\u{3000}")), W::Ok(p!("a\u{3000}"))],
        win_keys: [W::Err(LeadingTrailingWhitespace, Some(1)), W::Err(LeadingTrailingWhitespace, Some(1))],
        positional: W::Argv,
        expected: [E::Text, E::Lit("a"), E::No, E::Text],
    },
    Entry {
        id: "leading_lf",
        text: T::Lit("\na"),
        golden: Derived,
        claude: [W::Ok(p!("\ra")), W::Ok(p!("\ra"))],
        codex_paste: [W::Ok(p3!("\ra")), W::Ok(p!("\ra"))],
        win_keys: [W::Err(LeadingTrailingWhitespace, Some(0)), W::Err(LeadingTrailingWhitespace, Some(0))],
        positional: W::Argv,
        expected: [E::Text, E::Lit("a"), E::No, E::Text],
    },
    Entry {
        id: "trailing_lf",
        text: T::Lit("a\n"),
        golden: Derived,
        claude: [W::Ok(p!("a\r")), W::Ok(p!("a\r"))],
        codex_paste: [W::Ok(p3!("a\r")), W::Ok(p!("a\r"))],
        win_keys: [W::Err(LeadingTrailingWhitespace, Some(1)), W::Err(LeadingTrailingWhitespace, Some(1))],
        positional: W::Argv,
        expected: [E::Text, E::Lit("a"), E::No, E::Text],
    },
    Entry {
        id: "tab_middle",
        text: T::Lit("a\tb"),
        golden: Derived,
        claude: [W::Ok(p!("a\tb")), W::Ok(p!("a\tb"))],
        codex_paste: [W::Ok(p3!("a\tb")), W::Ok(p!("a\tb"))],
        win_keys: [W::Err(Tab, Some(1)), W::Err(Tab, Some(1))],
        positional: W::Argv,
        expected: [E::Lit("a    b"), E::Text, E::No, E::Text],
    },
    Entry {
        id: "nel_alone",
        text: T::Lit("\u{85}"),
        golden: Derived,
        claude: [W::Err(EmptyPrompt, None), W::Ok(p!("\u{85}"))],
        codex_paste: [W::Err(LossyText, Some(0)), W::Err(LossyText, Some(0))],
        win_keys: [W::Err(EmptyPrompt, None), W::Err(LeadingTrailingWhitespace, Some(0))],
        positional: W::Err(EmptyPrompt, None),
        expected: [E::No, E::No, E::No, E::No],
    },
    Entry {
        id: "tab_then_x",
        text: T::Lit("\tx"),
        golden: Derived,
        claude: [W::Ok(p!("\tx")), W::Ok(p!("\tx"))],
        codex_paste: [W::Ok(p3!("\tx")), W::Ok(p!("\tx"))],
        win_keys: [W::Err(LeadingTrailingWhitespace, Some(0)), W::Err(LeadingTrailingWhitespace, Some(0))],
        positional: W::Argv,
        expected: [E::Lit("    x"), E::Lit("x"), E::No, E::Text],
    },
    Entry {
        id: "space_a_tab_b",
        text: T::Lit(" a\tb"),
        golden: Derived,
        claude: [W::Ok(p!(" a\tb")), W::Ok(p!(" a\tb"))],
        codex_paste: [W::Ok(p3!(" a\tb")), W::Ok(p!(" a\tb"))],
        win_keys: [W::Err(LeadingTrailingWhitespace, Some(0)), W::Err(LeadingTrailingWhitespace, Some(0))],
        positional: W::Argv,
        expected: [E::Lit(" a    b"), E::Lit("a\tb"), E::No, E::Text],
    },
    Entry {
        id: "tab_del",
        text: T::Lit("\t\u{7f}"),
        golden: Derived,
        claude: [W::Ok(p!("\t\u{7f}")), W::Ok(p!("\t\u{7f}"))],
        codex_paste: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        win_keys: [W::Err(LossyText, Some(1)), W::Err(LossyText, Some(1))],
        positional: W::Argv,
        expected: [E::Lit("    \u{7f}"), E::No, E::No, E::Text],
    },
    Entry {
        id: "lone_cr_end",
        text: T::Lit("a\r"),
        golden: Derived,
        claude: [W::Ok(p!("a\r")), W::Ok(p!("a\r"))],
        codex_paste: [W::Ok(p3!("a\r")), W::Ok(p!("a\r"))],
        win_keys: [W::Err(LeadingTrailingWhitespace, Some(1)), W::Err(LeadingTrailingWhitespace, Some(1))],
        positional: W::Argv,
        expected: [E::Lit("a\n"), E::Lit("a"), E::No, E::Lit("a\n")],
    },
    Entry {
        id: "mixed_newlines",
        text: T::Lit("a\rb\nc\r\nd\r\re"),
        golden: Derived,
        claude: [W::Ok(p!("a\rb\rc\rd\r\re")), W::Ok(p!("a\rb\rc\rd\r\re"))],
        codex_paste: [W::Ok(p3!("a\rb\rc\rd\r\re")), W::Ok(p!("a\rb\rc\rd\r\re"))],
        win_keys: [W::Ok(concat!(" a", nl!(), "b", nl!(), "c", nl!(), "d", nl!(), nl!(), "e ")), W::Ok(concat!("a", nl!(), "b", nl!(), "c", nl!(), "d", nl!(), nl!(), "e"))],
        positional: W::Argv,
        expected: [E::Lit("a\nb\nc\nd\n\ne"), E::Lit("a\nb\nc\nd\n\ne"), E::Lit("a\nb\nc\nd\n\ne"), E::Lit("a\nb\nc\nd\n\ne")],
    },
    Entry {
        id: "zzexample_1000",
        text: T::Gen(zzexample_1000),
        golden: Derived,
        claude: [W::Large, W::Large],
        codex_paste: [W::Large, W::Large],
        win_keys: [W::Large, W::Large],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "zzexample_1001",
        text: T::Gen(zzexample_1001),
        golden: Derived,
        claude: [W::Large, W::Large],
        codex_paste: [W::Large, W::Large],
        win_keys: [W::Large, W::Large],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "image_relative",
        text: T::Lit("./diagram.png"),
        golden: Derived,
        claude: [W::Ok(p!("./diagram.png")), W::Ok(p!("./diagram.png"))],
        codex_paste: [W::Ok(p3!("./diagram.png")), W::Ok(p!("./diagram.png"))],
        win_keys: [W::Ok(" ./diagram.png "), W::Ok("./diagram.png")],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "image_absolute",
        text: T::Lit("/work/diagram.png"),
        golden: Derived,
        claude: [W::Ok(p!("/work/diagram.png")), W::Ok(p!("/work/diagram.png"))],
        codex_paste: [W::Ok(p3!("/work/diagram.png")), W::Ok(p!("/work/diagram.png"))],
        win_keys: [W::Ok(" /work/diagram.png "), W::Ok("/work/diagram.png")],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "image_file_url",
        text: T::Lit("file:///work/diagram.png"),
        golden: Derived,
        claude: [W::Ok(p!("file:///work/diagram.png")), W::Ok(p!("file:///work/diagram.png"))],
        codex_paste: [W::Ok(p3!("file:///work/diagram.png")), W::Ok(p!("file:///work/diagram.png"))],
        win_keys: [W::Ok(" file:///work/diagram.png "), W::Ok("file:///work/diagram.png")],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
    Entry {
        id: "max_prompt_near_bound",
        text: T::Gen(near_bound),
        golden: Derived,
        claude: [W::Large, W::Large],
        codex_paste: [W::Large, W::Large],
        win_keys: [W::Large, W::Large],
        positional: W::Argv,
        expected: [E::Fn(near_bound_composed), E::Text, E::Fn(near_bound_composed), E::Text],
    },
    Entry {
        id: "needy_composition",
        text: T::Lit("\u{390}\u{344}\n\u{390}\u{344}\n\u{390}\u{344}"),
        golden: Derived,
        claude: [W::Ok(p!("\u{390}\u{344}\r\u{390}\u{344}\r\u{390}\u{344}")), W::Ok(p!("\u{390}\u{344}\r\u{390}\u{344}\r\u{390}\u{344}"))],
        codex_paste: [W::Ok(p3!("\u{390}\u{344}\r\u{390}\u{344}\r\u{390}\u{344}")), W::Ok(p!("\u{390}\u{344}\r\u{390}\u{344}\r\u{390}\u{344}"))],
        win_keys: [W::Ok(concat!(" ", r!(912), r!(776), r!(769), nl!(), r!(912), r!(776), r!(769), nl!(), r!(912), r!(776), r!(769), " ")), W::Ok(concat!(r!(912), r!(776), r!(769), nl!(), r!(912), r!(776), r!(769), nl!(), r!(912), r!(776), r!(769)))],
        positional: W::Argv,
        expected: [E::Lit("\u{390}\u{308}\u{301}\n\u{390}\u{308}\u{301}\n\u{390}\u{308}\u{301}"), E::Text, E::Lit("\u{390}\u{308}\u{301}\n\u{390}\u{308}\u{301}\n\u{390}\u{308}\u{301}"), E::Text],
    },
    Entry {
        id: "max_prompt_mixed",
        text: T::Gen(max_prompt_mixed),
        golden: Derived,
        claude: [W::Large, W::Large],
        codex_paste: [W::Large, W::Large],
        win_keys: [W::Large, W::Large],
        positional: W::Argv,
        expected: [E::Text, E::Text, E::Text, E::Text],
    },
];

// ---- the reference model -----------------------------------------------------------------

fn lf(t: &str) -> String {
    t.replace("\r\n", "\n").replace('\r', "\n")
}

fn nfc(t: &str) -> String {
    t.nfc().collect()
}

fn tab4(t: &str) -> String {
    t.replace('\t', "    ")
}

/// The bodies of consecutive bracketed pastes making up all of `bytes`; `None` when the bytes
/// are anything else or a body holds ESC or LF.
fn pastes(bytes: &[u8]) -> Option<Vec<&[u8]>> {
    let mut rest = bytes;
    let mut bodies = Vec::new();
    while !rest.is_empty() {
        rest = rest.strip_prefix(PASTE_START)?;
        let end = rest.windows(PASTE_END.len()).position(|w| w == PASTE_END)?;
        let body = &rest[..end];
        if body.contains(&0x1b) || body.contains(&b'\n') {
            return None;
        }
        bodies.push(body);
        rest = &rest[end + PASTE_END.len()..];
    }
    Some(bodies)
}

/// What the CLI records from a bracketed-paste `Send`: exactly one paste (Claude) or three
/// with a space first and last (Codex); CR becomes LF; then Claude's `tab4 ∘ nfc` or Codex's
/// `trim`.
fn model_paste(agent: Agent, bytes: &[u8]) -> Option<String> {
    let bodies = pastes(bytes)?;
    let body = match (agent, bodies.as_slice()) {
        (Agent::Claude, [body]) => *body,
        (Agent::Codex, [b" ", body, b" "]) => *body,
        _ => return None,
    };
    let s = std::str::from_utf8(body).ok()?.replace('\r', "\n");
    Some(match agent {
        Agent::Claude => tab4(&nfc(&s)),
        Agent::Codex => format!(" {s} ").trim().to_owned(),
    })
}

/// One `ESC[Vk;Sc;Uc;Kd;Cs;Rc_` record at the start of `b`: its fields and length.
fn parse_record(b: &[u8]) -> Option<([u32; 6], usize)> {
    let rest = b.strip_prefix(b"\x1b[")?;
    let end = rest.iter().position(|&x| x == b'_')?;
    let fields: Vec<u32> = std::str::from_utf8(&rest[..end])
        .ok()?
        .split(';')
        .map(|f| {
            (!f.is_empty()
                && f.bytes().all(|d| d.is_ascii_digit())
                && (f == "0" || !f.starts_with('0')))
            .then(|| f.parse().ok())
            .flatten()
        })
        .collect::<Option<_>>()?;
    Some((fields.try_into().ok()?, 2 + end + 1))
}

/// What Codex on Windows reads from `win_keys` bytes: printable ASCII bytes, astral UTF-8,
/// and key-down records each followed directly by the matching key-up; Shift+Enter is LF and
/// a unit record its scalar. A 2- or 3-byte UTF-8 sequence, 0x7F, or any other byte is
/// refused. Untrimmed.
fn model_keys_raw(bytes: &[u8]) -> Option<String> {
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x1b => {
                let (down, n) = parse_record(&bytes[i..])?;
                let (up, m) = parse_record(&bytes[i + n..])?;
                let [vk, sc, uc, kd, cs, rc] = down;
                if kd != 1 || up != [vk, sc, uc, 0, cs, rc] || rc != 1 {
                    return None;
                }
                match (vk, sc, cs) {
                    (13, 28, 16) if uc == 13 => out.push('\n'),
                    (0, 0, 0) if uc <= 0xFFFF => out.push(char::from_u32(uc)?),
                    _ => return None,
                }
                i += n + m;
            }
            b @ 0x20..=0x7e => {
                out.push(char::from(b));
                i += 1;
            }
            0xf0..=0xf4 => {
                let c = std::str::from_utf8(bytes.get(i..i + 4)?).ok()?;
                out.push_str(c);
                i += 4;
            }
            _ => return None,
        }
    }
    Some(out)
}

fn model_keys(bytes: &[u8]) -> Option<String> {
    model_keys_raw(bytes).map(|s| s.trim().to_owned())
}

/// The model's reading of a `Send` encoded for `row`.
fn model(row: &Capabilities, e: &Encoded) -> Option<String> {
    match row.pty_encoding() {
        BracketedPaste => model_paste(row.agent(), &e.text),
        WinKeys => model_keys(&e.text),
        Positional => None,
    }
}

// ---- strategies --------------------------------------------------------------------------

fn pick(s: &'static [&'static str]) -> impl Strategy<Value = String> {
    proptest::sample::select(s).prop_map(str::to_owned)
}

fn chars(lo: char, hi: char) -> impl Strategy<Value = String> {
    proptest::collection::vec(proptest::char::range(lo, hi), 1..4)
        .prop_map(|v| v.into_iter().collect())
}

/// A piece of text weighted over the classes of PR5-CL-11.
fn piece() -> impl Strategy<Value = String> {
    prop_oneof![
        6 => chars(' ', '~'),
        3 => chars('\u{3041}', '\u{30ff}'),
        3 => chars('\u{4e00}', '\u{9fff}'),
        1 => Just("\u{3000}".to_owned()),
        3 => pick(&["\u{301}", "\u{308}", "\u{3099}", "\u{309a}", "\u{9be}", "\u{5b0}", "\u{20e3}", "\u{1d165}", "\u{344}"]),
        1 => Just("\u{200d}".to_owned()),
        1 => pick(&["\u{fe0e}", "\u{fe0f}"]),
        1 => pick(&["1\u{fe0f}\u{20e3}", "#\u{20e3}"]),
        1 => pick(&["\u{1f1ef}\u{1f1f5}", "\u{1f1fa}\u{1f1f8}"]),
        2 => chars('\u{1f300}', '\u{1faff}'),
        1 => chars('\u{20000}', '\u{2a6df}'),
        1 => pick(&["\u{fa19}", "\u{f900}", "\u{2f800}"]),
        2 => pick(&["\r", "\n", "\r\n"]),
        1 => Just("\t".to_owned()),
        1 => Just("\u{7f}".to_owned()),
        1 => chars('\u{80}', '\u{9f}'),
        1 => pick(&["\u{344}", "\u{390}", "\u{1d5}", "\u{fb2c}", "\u{e9}", "e"]),
        1 => chars('\u{80}', '\u{ffff}'),
    ]
}

fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => proptest::collection::vec(piece(), 0..12).prop_map(|v| v.concat()),
        1 => any::<String>(),
    ]
}

/// A scalar `check` refuses somewhere.
fn forbidden() -> impl Strategy<Value = char> {
    prop_oneof![
        proptest::char::range('\u{0}', '\u{1f}'),
        proptest::char::range('\u{7f}', '\u{9f}'),
        proptest::sample::select(&[' ', '\u{3000}', '\u{a0}', '\n', '\t'][..]),
    ]
}

/// `s` with `c` inserted at the char boundary nearest `at` (a fraction of its length).
fn insert(mut s: String, at: prop::sample::Index, c: char) -> String {
    let bounds: Vec<usize> = s.char_indices().map(|(i, _)| i).chain([s.len()]).collect();
    s.insert(bounds[at.index(bounds.len())], c);
    s
}

const PURPOSES: [Purpose; 2] = [Purpose::Send, Purpose::AttachPaste];
const AGENTS: [Agent; 2] = [Agent::Claude, Agent::Codex];
const ENCODINGS: [InputEncoding; 3] = [BracketedPaste, WinKeys, Positional];
const OSES: [HostOs; 2] = [HostOs::Macos, HostOs::Windows];

fn enc(row: &Capabilities, t: &str, purpose: Purpose) -> Result<Encoded, Rejection> {
    encode(row.agent(), row.os(), t, purpose)
}

fn bound(t: &str) -> usize {
    MAX_EXPANSION * t.len() + MAX_OVERHEAD
}

fn total_len(e: &Encoded) -> usize {
    e.text.len() + e.submit.map_or(0, |s| s.bytes.len())
}

// ---- corpus tests ------------------------------------------------------------------------

fn assert_want(id: &str, what: &str, got: Result<&Encoded, Rejection>, want: W) {
    match (want, got) {
        (W::Ok(bytes), Ok(e)) => assert!(
            e.text == bytes.as_bytes(),
            "{id} {what}: got {:?}, want {:?}",
            String::from_utf8_lossy(&e.text),
            bytes
        ),
        (W::Large, Ok(_)) => {}
        (W::Err(reason, at), Err(r)) => assert_eq!(r, Rejection { reason, at }, "{id} {what}"),
        (want, got) => panic!("{id} {what}: got {got:?}, want {want:?}"),
    }
}

/// Every corpus entry on every row and purpose: the golden bytes or the rejection, `check`
/// agreeing with `encode`, the expected prompts, and `prompt_matches` on them.
#[test]
fn corpus_goldens() {
    for e in CORPUS {
        let t = e.text();
        let rows: [(Agent, HostOs, [W; 2]); 4] = [
            (Agent::Claude, HostOs::Macos, e.claude),
            (Agent::Claude, HostOs::Windows, e.claude),
            (Agent::Codex, HostOs::Macos, e.codex_paste),
            (Agent::Codex, HostOs::Windows, e.win_keys),
        ];
        for (agent, os, wants) in rows {
            let row = chaldea_core::adapter::capabilities(agent, os);
            for (purpose, want) in PURPOSES.into_iter().zip(wants) {
                let got = encode(agent, os, &t, purpose);
                assert_want(
                    e.id,
                    &format!("{agent:?} {os:?} {purpose:?}"),
                    got.as_ref().map_err(|r| *r),
                    want,
                );
                assert_eq!(
                    check(agent, row.pty_encoding(), purpose, &t),
                    got.map(|_| ()),
                    "{} check {agent:?} {os:?} {purpose:?}",
                    e.id
                );
            }
        }
        let positional = check(Agent::Codex, Positional, Purpose::Send, &t);
        match e.positional {
            W::Argv => assert_eq!(positional, Ok(()), "{} positional", e.id),
            W::Err(reason, at) => {
                assert_eq!(
                    positional,
                    Err(Rejection { reason, at }),
                    "{} positional",
                    e.id
                );
            }
            w => panic!("{}: positional {w:?}", e.id),
        }
        let encodings = [
            (Agent::Claude, BracketedPaste),
            (Agent::Codex, BracketedPaste),
            (Agent::Codex, WinKeys),
            (Agent::Codex, Positional),
        ];
        for ((agent, encoding), want) in encodings.into_iter().zip(e.expected) {
            let want = match want {
                E::Text => Some(t.clone()),
                E::Lit(s) => Some(s.to_owned()),
                E::Fn(f) => Some(f()),
                E::No => None,
            };
            let got = expected_prompt(agent, encoding, &t);
            assert!(
                got == want,
                "{} expected {agent:?} {encoding:?}: {got:?}",
                e.id
            );
            if let Some(form) = got {
                assert!(
                    prompt_matches(agent, encoding, &t, &form),
                    "{} matches {agent:?} {encoding:?}",
                    e.id
                );
            }
        }
    }
}

/// The corpus run of the model: every accepted `Send` decodes to its expected prompt.
#[test]
fn corpus_model() {
    for e in CORPUS {
        let t = e.text();
        for row in &CAPABILITIES {
            let Ok(sent) = enc(row, &t, Purpose::Send) else {
                continue;
            };
            assert_eq!(
                model(row, &sent),
                expected_prompt(row.agent(), row.pty_encoding(), &t),
                "{} {row:?}",
                e.id
            );
        }
    }
}

/// Claude's bracketed paste of the spike's A-6 corpus with `cr`, as measured.
#[test]
fn bracketed_measured() {
    let measured: Vec<&Entry> = CORPUS.iter().filter(|e| e.golden == Measured).collect();
    assert_eq!(measured.len(), 13);
    for e in measured {
        let t = e.text();
        let want = match e.claude {
            [W::Ok(b), _] => b.as_bytes().to_vec(),
            // The 4 KiB + 1 entry: its golden is the measured framing of its newline-mapped text.
            [W::Large, _] => [PASTE_START, t.replace('\n', "\r").as_bytes(), PASTE_END].concat(),
            w => panic!("{}: {w:?}", e.id),
        };
        for os in OSES {
            for purpose in PURPOSES {
                assert_eq!(
                    encode(Agent::Claude, os, &t, purpose).unwrap().text,
                    want,
                    "{}",
                    e.id
                );
            }
        }
    }
}

#[test]
fn bracketed_derived() {
    let mapped = p!("a\rb\rc\rd").as_bytes();
    for os in OSES {
        for purpose in PURPOSES {
            assert_eq!(
                encode(Agent::Claude, os, "a\nb\r\nc\rd", purpose)
                    .unwrap()
                    .text,
                mapped
            );
        }
    }
    assert_eq!(
        encode(
            Agent::Codex,
            HostOs::Macos,
            "a\nb\r\nc\rd",
            Purpose::AttachPaste
        )
        .unwrap()
        .text,
        mapped
    );
    assert_eq!(
        encode(Agent::Codex, HostOs::Macos, "/review の件", Purpose::Send)
            .unwrap()
            .text,
        "\x1b[200~ \x1b[201~\x1b[200~/review の件\x1b[201~\x1b[200~ \x1b[201~".as_bytes()
    );
    for (t, n) in [(zzexample_1000(), 1000), (zzexample_1001(), 1001)] {
        let sent = encode(Agent::Codex, HostOs::Macos, &t, Purpose::Send).unwrap();
        let want = [
            p!(" ").as_bytes(),
            PASTE_START,
            t.as_bytes(),
            PASTE_END,
            p!(" ").as_bytes(),
        ]
        .concat();
        assert_eq!(sent.text, want);
        let bodies = pastes(&sent.text).unwrap();
        assert_eq!(bodies.len(), 3);
        assert_eq!(std::str::from_utf8(bodies[1]).unwrap().chars().count(), n);
        let pasted = encode(Agent::Codex, HostOs::Macos, &t, Purpose::AttachPaste).unwrap();
        assert_eq!(pasted.text, [PASTE_START, t.as_bytes(), PASTE_END].concat());
    }
}

/// The record format of the measured scalars and the `w32` Shift+Enter pair, as the spike
/// wrote them (`paste.rs:427`, `:595-597`, phase 3c).
#[test]
fn win_keys_measured() {
    let measured: [(&str, &[u8]); 6] = [
        ("\u{200d}", b"\x1b[0;0;8205;1;0;1_\x1b[0;0;8205;0;0;1_"),
        ("\u{fe0f}", b"\x1b[0;0;65039;1;0;1_\x1b[0;0;65039;0;0;1_"),
        ("\u{2764}", b"\x1b[0;0;10084;1;0;1_\x1b[0;0;10084;0;0;1_"),
        ("\u{3099}", b"\x1b[0;0;12441;1;0;1_\x1b[0;0;12441;0;0;1_"),
        ("\u{309a}", b"\x1b[0;0;12442;1;0;1_\x1b[0;0;12442;0;0;1_"),
        ("a\nb", b"a\x1b[13;28;13;1;16;1_\x1b[13;28;13;0;16;1_b"),
    ];
    for (t, want) in measured {
        let pasted = encode(Agent::Codex, HostOs::Windows, t, Purpose::AttachPaste).unwrap();
        assert_eq!(pasted.text, want, "{t:?}");
        let sent = encode(Agent::Codex, HostOs::Windows, t, Purpose::Send).unwrap();
        assert_eq!(sent.text, [b" ", want, b" "].concat(), "{t:?}");
    }
}

/// `win_keys` text for an entry without marks: §2.7 restated for the derived goldens of
/// generated entries.
fn keys_golden(t: &str, pad: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if pad {
        out.push(b' ');
    }
    for c in lf(t).chars() {
        match c {
            '\n' => out.extend_from_slice(SHIFT_ENTER),
            ' '..='~' | '\u{10000}'.. => out.extend_from_slice(c.to_string().as_bytes()),
            _ => {
                out.extend(format!("\x1b[0;0;{0};1;0;1_\x1b[0;0;{0};0;0;1_", u32::from(c)).bytes());
            }
        }
    }
    if pad {
        out.push(b' ');
    }
    out
}

/// Corpus F on `win_keys` as whole byte strings, both purposes, and the single-scalar cases.
#[test]
fn win_keys_derived() {
    let f: Vec<&Entry> = CORPUS.iter().filter(|e| e.id.starts_with("f_")).collect();
    assert_eq!(f.len(), 22);
    for e in f {
        let t = e.text();
        for (purpose, want) in PURPOSES.into_iter().zip(e.win_keys) {
            let got = encode(Agent::Codex, HostOs::Windows, &t, purpose);
            match want {
                W::Large => assert_eq!(
                    got.unwrap().text,
                    keys_golden(&t, purpose == Purpose::Send),
                    "{}",
                    e.id
                ),
                w => assert_want(
                    e.id,
                    &format!("{purpose:?}"),
                    got.as_ref().map_err(|r| *r),
                    w,
                ),
            }
        }
    }
    let cases: [(&str, &str, &str); 6] = [
        ("日", concat!(" ", r!(26085), " "), r!(26085)),
        ("\u{fa19}", concat!(" ", r!(64025), " "), r!(64025)),
        ("\u{e9}", concat!(" ", r!(233), " "), r!(233)),
        (
            "\u{31f7}\u{309a}",
            concat!(" ", r!(12791), r!(12442), " "),
            concat!(r!(12791), r!(12442)),
        ),
        (
            "𠮷野家",
            concat!(" 𠮷", r!(37326), r!(23478), " "),
            concat!("𠮷", r!(37326), r!(23478)),
        ),
        (
            "/review の件",
            concat!(" /review ", r!(12398), r!(20214), " "),
            concat!("/review ", r!(12398), r!(20214)),
        ),
    ];
    for (t, send, attach) in cases {
        assert_eq!(
            encode(Agent::Codex, HostOs::Windows, t, Purpose::Send)
                .unwrap()
                .text,
            send.as_bytes(),
            "{t:?}"
        );
        assert_eq!(
            encode(Agent::Codex, HostOs::Windows, t, Purpose::AttachPaste)
                .unwrap()
                .text,
            attach.as_bytes(),
            "{t:?}"
        );
    }
}

/// Claude Code 2.1.322's recorded prompts (spike A-6): each matches its sent text.
#[test]
fn claude_recorded_prompts() {
    let cases: [(&str, &str); 4] = [
        (
            "a6_ascii",
            include_str!("data/claude-2.1.322/ups-ascii.prompt.txt"),
        ),
        (
            "a6_decomposed_kana",
            include_str!("data/claude-2.1.322/ups-decomposed_kana.prompt.txt"),
        ),
        (
            "a6_decomposed_latin",
            include_str!("data/claude-2.1.322/ups-decomposed_latin.prompt.txt"),
        ),
        (
            "a6_size_4k_plus_1",
            include_str!("data/claude-2.1.322/ups-size_4k_plus_1.prompt.txt"),
        ),
    ];
    for (id, recorded) in cases {
        let t = CORPUS.iter().find(|e| e.id == id).unwrap().text();
        assert_ne!(
            t, recorded,
            "{id}: the recorded form differs from the sent text"
        );
        assert!(
            prompt_matches(Agent::Claude, BracketedPaste, &t, recorded),
            "{id}"
        );
        assert!(
            !prompt_matches(Agent::Claude, BracketedPaste, &t, &format!("{recorded}x")),
            "{id}"
        );
    }
}

// ---- size bound --------------------------------------------------------------------------

/// The facts of §2.8 about the dependency's tables, over every scalar: a scalar whose
/// canonical decomposition has more scalars than its UTF-8 bytes has exactly one more, is an
/// NFC-stable starter, and its decomposition starts with a starter that composes with the
/// next scalar.
#[test]
fn needy_scalars_compose() {
    let mut needy = 0;
    for c in (0..=0x10_ffff).filter_map(char::from_u32) {
        let mut d = Vec::new();
        decompose_canonical(c, |x| d.push(x));
        if d.len() <= c.len_utf8() {
            continue;
        }
        needy += 1;
        assert_eq!(d.len(), c.len_utf8() + 1, "{c:?}");
        assert_eq!(canonical_combining_class(c), 0, "{c:?}");
        assert_eq!(canonical_combining_class(d[0]), 0, "{c:?}");
        assert_eq!(nfc(&c.to_string()), c.to_string(), "{c:?}");
        assert!(compose(d[0], d[1]).is_some(), "{c:?}");
    }
    assert_eq!(needy, 60);
}

fn assert_bound(t: &str) {
    if let Some(composed) = expected_prompt(Agent::Codex, WinKeys, t) {
        assert!(composed.chars().count() <= t.len(), "{t:?}");
    }
    for row in &CAPABILITIES {
        for purpose in PURPOSES {
            if let Ok(e) = enc(row, t, purpose) {
                assert!(total_len(&e) <= bound(t), "{t:?} {row:?} {purpose:?}");
            }
        }
    }
}

/// Every scalar alone, between `a` and `b`, and after `a` with U+0301 following. The scalar
/// range is split across the available cores to keep the debug run short.
#[test]
fn size_bound_every_scalar() {
    const END: u32 = 0x11_0000;
    let workers = std::thread::available_parallelism().map_or(1, usize::from);
    let step = END.div_ceil(u32::try_from(workers).unwrap());
    std::thread::scope(|s| {
        for lo in (0..END).step_by(step as usize) {
            s.spawn(move || {
                let mut buf = String::with_capacity(16);
                for c in (lo..(lo + step).min(END)).filter_map(char::from_u32) {
                    for (pre, post) in [("", ""), ("a", "b"), ("a", "\u{301}")] {
                        buf.clear();
                        buf.push_str(pre);
                        buf.push(c);
                        buf.push_str(post);
                        assert_bound(&buf);
                    }
                }
            });
        }
    });
}

#[test]
fn needy_composition_case() {
    let t = ["\u{390}\u{344}"; 3].join("\n");
    assert_eq!(t.len(), 14);
    let e = encode(Agent::Codex, HostOs::Windows, &t, Purpose::Send).unwrap();
    assert_eq!(total_len(&e), 363);
    assert_eq!(bound(&t), 543);
}

#[test]
fn max_prompt_near_bound() {
    let t = near_bound();
    assert_eq!(t.len(), MAX_PROMPT_BYTES);
    let e = encode(Agent::Codex, HostOs::Windows, &t, Purpose::Send).unwrap();
    assert_eq!(total_len(&e), 36 * MAX_PROMPT_BYTES - 13);
    assert!(total_len(&e) <= bound(&t));
}

/// The two `MAX_PROMPT_BYTES` fixtures as attach pastes on every row, compared whole against
/// bytes built here from §2.6 and §2.7.
#[test]
fn max_prompt_attach_paste_bodies() {
    for (t, composed) in [
        (near_bound(), near_bound_composed()),
        (max_prompt_mixed(), max_prompt_mixed()),
    ] {
        assert_eq!(t.len(), MAX_PROMPT_BYTES);
        let paste = [
            PASTE_START,
            lf(&t).replace('\n', "\r").as_bytes(),
            PASTE_END,
        ]
        .concat();
        for row in &CAPABILITIES {
            let want = match row.pty_encoding() {
                BracketedPaste => paste.clone(),
                WinKeys => keys_golden(&composed, false),
                Positional => unreachable!("no table row has a positional PTY encoding"),
            };
            let got = enc(row, &t, Purpose::AttachPaste).unwrap();
            assert!(
                got.text == want,
                "{row:?}: {} bytes, want {}",
                got.text.len(),
                want.len()
            );
            assert_eq!(got.submit, None);
        }
    }
}

// ---- properties --------------------------------------------------------------------------

/// The shape of a `win_keys` text: the two record shapes, printable ASCII, astral UTF-8.
fn assert_keys_bytes(text: &[u8]) {
    let mut i = 0;
    while i < text.len() {
        let b = text[i];
        assert!(b >= 0x20 || b == 0x1b, "byte {b:#x} at {i}");
        assert_ne!(b, 0x7f);
        if b == 0x1b {
            let (down, n) = parse_record(&text[i..]).expect("a whole record");
            let (up, m) = parse_record(&text[i + n..]).expect("its key-up");
            let unit = matches!(down, [0, 0, u, 1, 0, 1] if (0x80..=0xffff).contains(&u) && !(0xd800..=0xdfff).contains(&u));
            assert!(unit || down == [13, 28, 13, 1, 16, 1], "{down:?}");
            assert_eq!(up, [down[0], down[1], down[2], 0, down[4], 1]);
            i += n + m;
        } else if b >= 0x80 {
            assert!((0xf0..=0xf4).contains(&b), "a 2- or 3-byte sequence at {i}");
            i += 4;
        } else {
            i += 1;
        }
    }
}

/// Every escape sequence lies whole inside `text`, and the submit is exactly one CR.
fn assert_escapes_complete(row: &Capabilities, e: &Encoded, purpose: Purpose) {
    assert!(!e.text.is_empty());
    match row.pty_encoding() {
        BracketedPaste => {
            let bodies = pastes(&e.text).expect("only whole pastes");
            let n = match (row.agent(), purpose) {
                (Agent::Codex, Purpose::Send) => 3,
                _ => 1,
            };
            assert_eq!(bodies.len(), n);
        }
        WinKeys => assert_keys_bytes(&e.text),
        Positional => panic!("no PTY bytes for a positional row"),
    }
    if let Some(s) = e.submit {
        assert_eq!(s.bytes, b"\r");
    }
}

proptest! {
    #[test]
    fn encode_err_iff_check_err(base in text(), at in any::<prop::sample::Index>(), c in forbidden()) {
        let t = insert(base, at, c);
        for row in &CAPABILITIES {
            for purpose in PURPOSES {
                let checked = check(row.agent(), row.pty_encoding(), purpose, &t);
                let got = enc(row, &t, purpose);
                prop_assert_eq!(got.as_ref().err().copied(), checked.err());
                prop_assert_eq!(got.is_ok(), checked.is_ok());
            }
        }
    }

    #[test]
    fn control_anywhere_rejected(base in text(), at in any::<prop::sample::Index>(), c in proptest::char::range('\u{0}', '\u{1f}')) {
        prop_assume!(!matches!(c, '\t' | '\r' | '\n'));
        let t = insert(base, at, c);
        let first = t.char_indices().find(|&(_, x)| x < ' ' && !matches!(x, '\t' | '\r' | '\n')).unwrap().0;
        for agent in AGENTS {
            for encoding in ENCODINGS {
                for purpose in PURPOSES {
                    if encoding == Positional && purpose == Purpose::AttachPaste
                        || agent == Agent::Claude && encoding != BracketedPaste
                    {
                        continue;
                    }
                    prop_assert_eq!(
                        check(agent, encoding, purpose, &t),
                        Err(Rejection { reason: ControlCharacter, at: Some(first) })
                    );
                }
            }
        }
    }

    #[test]
    fn bracketed_shape(t in text()) {
        for row in CAPABILITIES.iter().filter(|r| r.pty_encoding() == BracketedPaste) {
            for purpose in PURPOSES {
                let Ok(e) = enc(row, &t, purpose) else { continue };
                let mapped = lf(&t).replace('\n', "\r");
                let one = [PASTE_START, mapped.as_bytes(), PASTE_END].concat();
                let want = match (row.agent(), purpose) {
                    (Agent::Codex, Purpose::Send) => [p!(" ").as_bytes(), &one, p!(" ").as_bytes()].concat(),
                    _ => one,
                };
                prop_assert_eq!(&e.text, &want);
                prop_assert!(!e.text.contains(&b'\n'));
                prop_assert_eq!(e.text.iter().filter(|&&b| b == 0x1b).count(), want.iter().filter(|&&b| b == 0x1b).count());
            }
        }
    }

    #[test]
    fn win_keys_byte_classes(t in text()) {
        for purpose in PURPOSES {
            if let Ok(e) = encode(Agent::Codex, HostOs::Windows, &t, purpose) {
                assert_keys_bytes(&e.text);
            }
        }
    }

    #[test]
    fn escapes_complete(t in text()) {
        for row in &CAPABILITIES {
            for purpose in PURPOSES {
                if let Ok(e) = enc(row, &t, purpose) {
                    assert_escapes_complete(row, &e, purpose);
                }
            }
        }
    }

    #[test]
    fn win_keys_text_has_no_cr(t in text()) {
        for purpose in PURPOSES {
            if let Ok(e) = encode(Agent::Codex, HostOs::Windows, &t, purpose) {
                prop_assert!(!e.text.contains(&b'\r'));
            }
        }
    }

    #[test]
    fn submit_only_for_send(t in text()) {
        for row in &CAPABILITIES {
            if let Ok(e) = enc(row, &t, Purpose::Send) {
                prop_assert_eq!(e.submit.map(|s| (s.delay, s.bytes)), Some((row.submit_delay(), SUBMIT)));
            }
            if let Ok(e) = enc(row, &t, Purpose::AttachPaste) {
                prop_assert_eq!(e.submit, None);
            }
        }
    }

    #[test]
    fn model_decode_equals_expected(t in text()) {
        for row in &CAPABILITIES {
            let Ok(sent) = enc(row, &t, Purpose::Send) else { continue };
            let expected = expected_prompt(row.agent(), row.pty_encoding(), &t);
            prop_assert!(expected.is_some());
            prop_assert_eq!(model(row, &sent), expected.clone());
            prop_assert!(prompt_matches(row.agent(), row.pty_encoding(), &t, &expected.unwrap()));
        }
        if check(Agent::Codex, Positional, Purpose::Send, &t).is_ok() {
            prop_assert_eq!(expected_prompt(Agent::Codex, Positional, &t), Some(lf(&t)));
        }
    }

    #[test]
    fn size_bound(t in text()) {
        assert_bound(&t);
    }

    #[test]
    fn total_functions(t in text(), recorded in text()) {
        for agent in AGENTS {
            for encoding in ENCODINGS {
                let expected = expected_prompt(agent, encoding, &t);
                prop_assert_eq!(expected.is_some(), check(agent, encoding, Purpose::Send, &t).is_ok());
                let _ = prompt_matches(agent, encoding, &t, &recorded);
                if let Some(form) = expected {
                    prop_assert!(prompt_matches(agent, encoding, &t, &form));
                } else {
                    prop_assert!(!prompt_matches(agent, encoding, &t, &recorded));
                }
                for purpose in PURPOSES {
                    let _ = check(agent, encoding, purpose, &t);
                }
            }
            for os in OSES {
                for purpose in PURPOSES {
                    let _ = encode(agent, os, &t, purpose);
                }
            }
        }
    }
}
