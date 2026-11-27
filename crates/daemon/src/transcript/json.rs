//! A push JSON tokenizer for one transcript line (RFC 8259): the caller feeds the line's bytes
//! in pieces of any size and receives tokens, so the line's size never bounds what it holds.
//! It does no I/O and keeps no input: strings reach the sink decoded, in pieces. Nesting has
//! no depth limit: the open containers are one bit each.

/// The line is not one JSON value: a syntax error, invalid UTF-8, or a lone surrogate escape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Syntax;

pub(crate) type Result<T> = std::result::Result<T, Syntax>;

/// One token. A string is `StrBegin`, its decoded bytes in `Chunk`s (pieces may split a
/// character), then `StrEnd`; a number or literal is one `Scalar` once it is complete.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Tok<'a> {
    BeginObject,
    EndObject,
    BeginArray,
    EndArray,
    /// A string starts; `key` for an object member's name.
    StrBegin {
        key: bool,
    },
    Chunk(&'a [u8]),
    StrEnd,
    Scalar,
}

/// Receives tokens in order.
pub(crate) trait Sink {
    fn tok(&mut self, t: Tok<'_>);
}

/// Where the tokenizer is between two bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// A value is expected (the top level, after `:`, after `,` in an array).
    Value,
    /// After `[`: a value or `]`.
    ArrayFirst,
    /// After `{`: a key or `}`.
    ObjectFirst,
    /// After `,` in an object: a key.
    Key,
    /// After a key: `:`.
    Colon,
    /// After a value inside a container: `,` or its close.
    After,
    /// Inside a string.
    Str {
        key: bool,
    },
    Num(Num),
    /// Inside `true`, `false`, or `null`: the bytes still expected.
    Lit {
        word: &'static [u8],
        at: u8,
    },
    /// The top-level value is complete: only whitespace may follow.
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Num {
    Minus,
    Zero,
    Int,
    Dot,
    Frac,
    Exp,
    ExpSign,
    ExpDigits,
}

impl Num {
    fn complete(self) -> bool {
        matches!(self, Num::Zero | Num::Int | Num::Frac | Num::ExpDigits)
    }
}

/// An escape in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Esc {
    #[default]
    None,
    /// After `\`.
    Slash,
    /// `\u` and `n` hex digits so far.
    Hex { n: u8, v: u32 },
    /// A high surrogate read: `\` must follow.
    LowSlash,
    /// A high surrogate and `\` read: `u` must follow.
    LowU,
}

/// The open containers, one bit each (set: an object).
#[derive(Debug, Default)]
struct Stack {
    bits: Vec<u64>,
    depth: usize,
}

impl Stack {
    fn push(&mut self, object: bool) {
        let (word, bit) = (self.depth / 64, self.depth % 64);
        if word == self.bits.len() {
            self.bits.push(0);
        }
        if object {
            self.bits[word] |= 1 << bit;
        } else {
            self.bits[word] &= !(1 << bit);
        }
        self.depth += 1;
    }

    /// Closes the innermost container when it is an object (`object`) or an array.
    fn pop(&mut self, object: bool) -> Result<()> {
        if self.depth == 0 || self.top_is_object() != object {
            return Err(Syntax);
        }
        self.depth -= 1;
        Ok(())
    }

    fn top_is_object(&self) -> bool {
        let d = self.depth - 1;
        self.bits[d / 64] & (1 << (d % 64)) != 0
    }
}

/// The tokenizer's state across pieces.
#[derive(Debug)]
pub(crate) struct Tokenizer {
    state: State,
    stack: Stack,
    utf8: Utf8,
    esc: Esc,
    /// A high surrogate waiting for its low half.
    high: Option<u32>,
    failed: bool,
}

impl Default for Tokenizer {
    fn default() -> Self {
        Tokenizer {
            state: State::Value,
            stack: Stack::default(),
            utf8: Utf8::default(),
            esc: Esc::None,
            high: None,
            failed: false,
        }
    }
}

fn ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

impl Tokenizer {
    /// Feeds the next piece of the input.
    ///
    /// # Errors
    ///
    /// `Syntax` at the first byte that breaks JSON; later calls fail at once.
    pub(crate) fn feed(&mut self, input: &[u8], sink: &mut impl Sink) -> Result<()> {
        if self.failed {
            return Err(Syntax);
        }
        let r = self.run(input, sink);
        self.failed = r.is_err();
        r
    }

    /// Ends the input: the top-level value must be complete.
    ///
    /// # Errors
    ///
    /// `Syntax` for an incomplete or missing value.
    pub(crate) fn finish(&mut self, sink: &mut impl Sink) -> Result<()> {
        if self.failed {
            return Err(Syntax);
        }
        if let State::Num(n) = self.state {
            if !n.complete() {
                return Err(Syntax);
            }
            sink.tok(Tok::Scalar);
            self.value_done();
        }
        if self.state == State::Done {
            Ok(())
        } else {
            self.failed = true;
            Err(Syntax)
        }
    }

    /// After a complete value: the top level is done, or a container continues.
    fn value_done(&mut self) {
        self.state = if self.stack.depth == 0 {
            State::Done
        } else {
            State::After
        };
    }

    fn close(&mut self, object: bool, sink: &mut impl Sink) -> Result<()> {
        self.stack.pop(object)?;
        sink.tok(if object {
            Tok::EndObject
        } else {
            Tok::EndArray
        });
        self.value_done();
        Ok(())
    }

    /// The first byte of a value.
    fn begin_value(&mut self, b: u8, sink: &mut impl Sink) -> Result<()> {
        match b {
            b'{' => {
                self.stack.push(true);
                sink.tok(Tok::BeginObject);
                self.state = State::ObjectFirst;
            }
            b'[' => {
                self.stack.push(false);
                sink.tok(Tok::BeginArray);
                self.state = State::ArrayFirst;
            }
            b'"' => {
                sink.tok(Tok::StrBegin { key: false });
                self.state = State::Str { key: false };
            }
            b't' => {
                self.state = State::Lit {
                    word: b"true",
                    at: 1,
                }
            }
            b'f' => {
                self.state = State::Lit {
                    word: b"false",
                    at: 1,
                }
            }
            b'n' => {
                self.state = State::Lit {
                    word: b"null",
                    at: 1,
                }
            }
            b'-' => self.state = State::Num(Num::Minus),
            b'0' => self.state = State::Num(Num::Zero),
            b'1'..=b'9' => self.state = State::Num(Num::Int),
            _ => return Err(Syntax),
        }
        Ok(())
    }

    fn run(&mut self, input: &[u8], sink: &mut impl Sink) -> Result<()> {
        let mut i = 0;
        while i < input.len() {
            let b = input[i];
            match self.state {
                State::Str { key } => {
                    // A pending high surrogate always has its escape state set.
                    if self.esc == Esc::None {
                        // The run of plain bytes up to a quote, a backslash, or a control byte.
                        let start = i;
                        while i < input.len() {
                            let c = input[i];
                            if c == b'"' || c == b'\\' || c < 0x20 {
                                break;
                            }
                            self.utf8.step(c)?;
                            i += 1;
                        }
                        if i > start {
                            sink.tok(Tok::Chunk(&input[start..i]));
                        }
                        if i == input.len() {
                            break;
                        }
                        match input[i] {
                            b'"' => {
                                self.utf8.finish()?;
                                sink.tok(Tok::StrEnd);
                                if key {
                                    self.state = State::Colon;
                                } else {
                                    self.value_done();
                                }
                            }
                            b'\\' => {
                                self.utf8.finish()?;
                                self.esc = Esc::Slash;
                            }
                            _ => return Err(Syntax),
                        }
                        i += 1;
                        continue;
                    }
                    self.escape(b, sink)?;
                }
                State::Num(n) => {
                    let next = match (n, b) {
                        (Num::Minus, b'0') => Some(Num::Zero),
                        (Num::Minus, b'1'..=b'9') | (Num::Int, b'0'..=b'9') => Some(Num::Int),
                        (Num::Zero | Num::Int, b'.') => Some(Num::Dot),
                        (Num::Dot | Num::Frac, b'0'..=b'9') => Some(Num::Frac),
                        (Num::Zero | Num::Int | Num::Frac, b'e' | b'E') => Some(Num::Exp),
                        (Num::Exp, b'+' | b'-') => Some(Num::ExpSign),
                        (Num::Exp | Num::ExpSign | Num::ExpDigits, b'0'..=b'9') => {
                            Some(Num::ExpDigits)
                        }
                        _ => None,
                    };
                    if let Some(n) = next {
                        self.state = State::Num(n);
                    } else {
                        if !n.complete() {
                            return Err(Syntax);
                        }
                        sink.tok(Tok::Scalar);
                        self.value_done();
                        // The byte belongs to what follows the number.
                        continue;
                    }
                }
                State::Lit { word, at } => {
                    if word[usize::from(at)] != b {
                        return Err(Syntax);
                    }
                    if usize::from(at) + 1 == word.len() {
                        sink.tok(Tok::Scalar);
                        self.value_done();
                    } else {
                        self.state = State::Lit { word, at: at + 1 };
                    }
                }
                _ if ws(b) => {}
                State::Value => self.begin_value(b, sink)?,
                State::ArrayFirst => {
                    if b == b']' {
                        self.close(false, sink)?;
                    } else {
                        self.begin_value(b, sink)?;
                    }
                }
                State::ObjectFirst | State::Key => {
                    if b == b'}' && self.state == State::ObjectFirst {
                        self.close(true, sink)?;
                    } else if b == b'"' {
                        sink.tok(Tok::StrBegin { key: true });
                        self.state = State::Str { key: true };
                    } else {
                        return Err(Syntax);
                    }
                }
                State::Colon => {
                    if b != b':' {
                        return Err(Syntax);
                    }
                    self.state = State::Value;
                }
                State::After => match b {
                    b',' => {
                        self.state = if self.stack.top_is_object() {
                            State::Key
                        } else {
                            State::Value
                        };
                    }
                    b'}' => self.close(true, sink)?,
                    b']' => self.close(false, sink)?,
                    _ => return Err(Syntax),
                },
                State::Done => return Err(Syntax),
            }
            i += 1;
        }
        Ok(())
    }

    /// One byte of an escape (or of the `\u` that must follow a high surrogate).
    fn escape(&mut self, b: u8, sink: &mut impl Sink) -> Result<()> {
        match self.esc {
            // `None` is not reached: the caller handles plain bytes itself.
            Esc::None | Esc::LowSlash => {
                if b != b'\\' {
                    return Err(Syntax);
                }
                self.esc = Esc::LowU;
            }
            Esc::LowU => {
                if b != b'u' {
                    return Err(Syntax);
                }
                self.esc = Esc::Hex { n: 0, v: 0 };
            }
            Esc::Slash => {
                let byte = match b {
                    b'"' => b'"',
                    b'\\' => b'\\',
                    b'/' => b'/',
                    b'b' => 0x08,
                    b'f' => 0x0c,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'u' => {
                        self.esc = Esc::Hex { n: 0, v: 0 };
                        return Ok(());
                    }
                    _ => return Err(Syntax),
                };
                sink.tok(Tok::Chunk(&[byte]));
                self.esc = Esc::None;
            }
            Esc::Hex { n, v } => {
                let d = match b {
                    b'0'..=b'9' => b - b'0',
                    b'a'..=b'f' => b - b'a' + 10,
                    b'A'..=b'F' => b - b'A' + 10,
                    _ => return Err(Syntax),
                };
                let v = v * 16 + u32::from(d);
                if n < 3 {
                    self.esc = Esc::Hex { n: n + 1, v };
                    return Ok(());
                }
                self.esc = Esc::None;
                let c = match self.high.take() {
                    Some(hi) => {
                        if !(0xDC00..0xE000).contains(&v) {
                            return Err(Syntax);
                        }
                        0x10000 + ((hi - 0xD800) << 10) + (v - 0xDC00)
                    }
                    None if (0xD800..0xDC00).contains(&v) => {
                        self.high = Some(v);
                        self.esc = Esc::LowSlash;
                        return Ok(());
                    }
                    None if (0xDC00..0xE000).contains(&v) => return Err(Syntax),
                    None => v,
                };
                let c = char::from_u32(c).ok_or(Syntax)?;
                let mut tmp = [0u8; 4];
                sink.tok(Tok::Chunk(c.encode_utf8(&mut tmp).as_bytes()));
            }
        }
        Ok(())
    }
}

/// An incremental UTF-8 check (RFC 3629: no overlong forms, surrogates, or values above
/// U+10FFFF).
#[derive(Debug, Default)]
struct Utf8 {
    /// Continuation bytes still expected.
    need: u8,
    /// The allowed range of the next continuation byte.
    lo: u8,
    hi: u8,
}

impl Utf8 {
    fn step(&mut self, b: u8) -> Result<()> {
        if self.need > 0 {
            if b < self.lo || b > self.hi {
                return Err(Syntax);
            }
            self.need -= 1;
            self.lo = 0x80;
            self.hi = 0xBF;
            return Ok(());
        }
        let (need, lo, hi) = match b {
            0x00..=0x7F => return Ok(()),
            0xC2..=0xDF => (1, 0x80, 0xBF),
            0xE0 => (2, 0xA0, 0xBF),
            0xE1..=0xEC | 0xEE..=0xEF => (2, 0x80, 0xBF),
            0xED => (2, 0x80, 0x9F),
            0xF0 => (3, 0x90, 0xBF),
            0xF1..=0xF3 => (3, 0x80, 0xBF),
            0xF4 => (3, 0x80, 0x8F),
            _ => return Err(Syntax),
        };
        self.need = need;
        self.lo = lo;
        self.hi = hi;
        Ok(())
    }

    fn finish(&self) -> Result<()> {
        if self.need == 0 { Ok(()) } else { Err(Syntax) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collects the decoded strings and counts the other tokens.
    #[derive(Default)]
    struct Collect {
        strings: Vec<Vec<u8>>,
        tokens: usize,
    }

    impl Sink for Collect {
        fn tok(&mut self, t: Tok<'_>) {
            self.tokens += 1;
            match t {
                Tok::StrBegin { .. } => self.strings.push(Vec::new()),
                Tok::Chunk(b) => self.strings.last_mut().unwrap().extend_from_slice(b),
                _ => {}
            }
        }
    }

    /// Feeds `src` in pieces of `piece` bytes.
    fn tokens(src: &[u8], piece: usize) -> Result<Collect> {
        let mut t = Tokenizer::default();
        let mut c = Collect::default();
        for p in src.chunks(piece.max(1)) {
            t.feed(p, &mut c)?;
        }
        t.finish(&mut c)?;
        Ok(c)
    }

    fn valid(src: &[u8]) -> bool {
        let whole = tokens(src, src.len()).is_ok();
        let trickled = tokens(src, 1).is_ok();
        assert_eq!(whole, trickled, "{}", String::from_utf8_lossy(src));
        whole
    }

    /// The tokenizer accepts exactly what serde_json accepts as a `Value` (strings decoded),
    /// for these inputs, fed whole and byte by byte.
    #[test]
    fn agrees_with_serde_json() {
        let cases: &[&[u8]] = &[
            b"{}",
            b"[]",
            b" {\"a\" : [1, -2.5e+3, true, false, null, \"x\"] } ",
            b"\"\\u00e9\\ud83d\\ude00\\n\"",
            b"0",
            b"-0.0e0",
            b"12",
            b"{\"a\":{\"b\":[[]]}}",
            "\"日本語😀\"".as_bytes(),
            b"",
            b"   ",
            b"{",
            b"{\"a\"}",
            b"{\"a\":1,}",
            b"[1,]",
            b"[,1]",
            b"]",
            b"{]",
            b"[}",
            b"01",
            b"1.",
            b"1e",
            b"-",
            b"tru",
            b"nul",
            b"\"\\x\"",
            b"\"\\ud800\"",
            b"\"\\udc00\"",
            b"\"\\ud800\\u0041\"",
            b"\"\\ud800x\"",
            b"\"a\x01\"",
            b"\"\xff\"",
            b"\"\xc0\x80\"",
            b"\"\xed\xa0\x80\"",
            b"\"\xf4\x90\x80\x80\"",
            b"\"\xe3\x81\"",
            b"{} {}",
            b"{\"a\":1}x",
            b"[1 2]",
            b"{\"a\" 1}",
            b"{1:2}",
            b"{\"a\":1 \"b\":2}",
        ];
        for c in cases {
            let serde = serde_json::from_slice::<serde_json::Value>(c).is_ok();
            assert_eq!(valid(c), serde, "{}", String::from_utf8_lossy(c));
        }
    }

    #[test]
    fn strings_decode_in_pieces() {
        let src = "\"a\\\"b\\\\c\\/d\\b\\f\\n\\r\\t\\u3042\\ud83d\\ude00日\"".as_bytes();
        for piece in [1, 2, 3, 7, src.len()] {
            let c = tokens(src, piece).unwrap();
            assert_eq!(
                String::from_utf8(c.strings[0].clone()).unwrap(),
                "a\"b\\c/d\u{8}\u{c}\n\r\tあ😀日"
            );
        }
    }

    /// Nesting has no depth limit (serde_json stops at 128).
    #[test]
    fn nesting_is_not_bounded() {
        let n = 100_000;
        let mut s = "[{\"a\":".repeat(n);
        s.push('0');
        s.push_str(&"}]".repeat(n));
        assert!(valid(s.as_bytes()));
        let mut bad = "[".repeat(n);
        bad.push_str(&"]".repeat(n - 1));
        assert!(!valid(bad.as_bytes()));
    }
}
