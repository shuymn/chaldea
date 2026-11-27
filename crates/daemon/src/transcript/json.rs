//! A streaming JSON reader for one transcript line (RFC 8259): it validates the whole value
//! while the caller keeps only what it asks for, string by string, so the line's size never
//! bounds what it holds. Strings reach a [`StrSink`] decoded, in pieces; skipped values are
//! validated and dropped.

use std::io::{self, Read};

/// The reader's buffer.
const BUF_BYTES: usize = 64 * 1024;

/// Nesting deeper than this is refused (serde_json's own reader stops at 128).
pub(crate) const MAX_DEPTH: usize = 128;

/// Why a value could not be read.
#[derive(Debug)]
pub(crate) enum JsonError {
    /// Not JSON: a syntax error, invalid UTF-8, a lone surrogate escape, or nesting deeper
    /// than [`MAX_DEPTH`].
    Syntax,
    /// The source failed.
    Read(io::Error),
    /// A sink failed (a spool write).
    Sink(io::Error),
}

pub(crate) type Result<T> = std::result::Result<T, JsonError>;

/// Receives a string's decoded UTF-8 bytes in order, in pieces that may split a character.
pub(crate) trait StrSink {
    fn push(&mut self, bytes: &[u8]) -> io::Result<()>;
}

/// Drops every byte.
pub(crate) struct Discard;

impl StrSink for Discard {
    fn push(&mut self, _: &[u8]) -> io::Result<()> {
        Ok(())
    }
}

/// Keeps at most `cap` bytes; `over` once more arrived.
pub(crate) struct Capped {
    pub(crate) bytes: Vec<u8>,
    cap: usize,
    pub(crate) over: bool,
}

impl Capped {
    pub(crate) fn new(cap: usize) -> Capped {
        Capped {
            bytes: Vec::new(),
            cap,
            over: false,
        }
    }

    /// The kept text when nothing was cut.
    pub(crate) fn text(&self) -> Option<&str> {
        if self.over {
            return None;
        }
        std::str::from_utf8(&self.bytes).ok()
    }
}

impl StrSink for Capped {
    fn push(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.over || self.bytes.len() + bytes.len() > self.cap {
            self.over = true;
            return Ok(());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
}

/// What starts the next value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Object,
    Array,
    String,
    Other,
}

/// The reader over `R`.
pub(crate) struct Reader<R> {
    r: R,
    buf: Box<[u8]>,
    pos: usize,
    end: usize,
    eof: bool,
}

impl<R: Read> Reader<R> {
    pub(crate) fn new(r: R) -> Reader<R> {
        Reader {
            r,
            buf: vec![0u8; BUF_BYTES].into_boxed_slice(),
            pos: 0,
            end: 0,
            eof: false,
        }
    }

    fn fill(&mut self) -> Result<bool> {
        if self.pos < self.end {
            return Ok(true);
        }
        if self.eof {
            return Ok(false);
        }
        loop {
            match self.r.read(&mut self.buf) {
                Ok(0) => {
                    self.eof = true;
                    return Ok(false);
                }
                Ok(n) => {
                    self.pos = 0;
                    self.end = n;
                    return Ok(true);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(JsonError::Read(e)),
            }
        }
    }

    fn peek(&mut self) -> Result<Option<u8>> {
        Ok(if self.fill()? {
            Some(self.buf[self.pos])
        } else {
            None
        })
    }

    fn next(&mut self) -> Result<u8> {
        if !self.fill()? {
            return Err(JsonError::Syntax);
        }
        let b = self.buf[self.pos];
        self.pos += 1;
        Ok(b)
    }

    fn expect(&mut self, want: u8) -> Result<()> {
        if self.next()? == want {
            Ok(())
        } else {
            Err(JsonError::Syntax)
        }
    }

    fn skip_ws(&mut self) -> Result<()> {
        while self.fill()? {
            while self.pos < self.end {
                if !matches!(self.buf[self.pos], b' ' | b'\t' | b'\n' | b'\r') {
                    return Ok(());
                }
                self.pos += 1;
            }
        }
        Ok(())
    }

    /// The kind of the next value, after whitespace.
    pub(crate) fn kind(&mut self) -> Result<Kind> {
        self.skip_ws()?;
        Ok(match self.peek()? {
            Some(b'{') => Kind::Object,
            Some(b'[') => Kind::Array,
            Some(b'"') => Kind::String,
            Some(_) => Kind::Other,
            None => return Err(JsonError::Syntax),
        })
    }

    /// Checks that only whitespace follows the value.
    pub(crate) fn end(&mut self) -> Result<()> {
        self.skip_ws()?;
        match self.peek()? {
            None => Ok(()),
            Some(_) => Err(JsonError::Syntax),
        }
    }

    // ---- objects and arrays ----------------------------------------------------------------

    /// Enters an object: `{` (after [`Reader::kind`] said `Object`).
    pub(crate) fn begin_object(&mut self) -> Result<()> {
        self.skip_ws()?;
        self.expect(b'{')
    }

    /// The next key of an object into `key`; `false` at its `}`. `first` is true for the
    /// first call after [`Reader::begin_object`].
    pub(crate) fn next_key(&mut self, first: bool, key: &mut dyn StrSink) -> Result<bool> {
        self.skip_ws()?;
        match self.peek()? {
            Some(b'}') => {
                self.pos += 1;
                return Ok(false);
            }
            Some(b',') if !first => {
                self.pos += 1;
                self.skip_ws()?;
            }
            Some(b'"') if first => {}
            _ => return Err(JsonError::Syntax),
        }
        self.string(key)?;
        self.skip_ws()?;
        self.expect(b':')?;
        Ok(true)
    }

    /// Enters an array: `[`.
    pub(crate) fn begin_array(&mut self) -> Result<()> {
        self.skip_ws()?;
        self.expect(b'[')
    }

    /// Whether another element follows; `false` at the array's `]`.
    pub(crate) fn next_element(&mut self, first: bool) -> Result<bool> {
        self.skip_ws()?;
        match self.peek()? {
            Some(b']') => {
                self.pos += 1;
                Ok(false)
            }
            Some(b',') if !first => {
                self.pos += 1;
                Ok(true)
            }
            Some(_) if first => Ok(true),
            _ => Err(JsonError::Syntax),
        }
    }

    // ---- values ----------------------------------------------------------------------------

    /// Validates and drops the next value, at nesting `depth` (1 for a top-level value's
    /// members).
    pub(crate) fn skip(&mut self, depth: usize) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(JsonError::Syntax);
        }
        match self.kind()? {
            Kind::Object => {
                self.begin_object()?;
                let mut first = true;
                while self.next_key(first, &mut Discard)? {
                    first = false;
                    self.skip(depth + 1)?;
                }
                Ok(())
            }
            Kind::Array => {
                self.begin_array()?;
                let mut first = true;
                while self.next_element(first)? {
                    first = false;
                    self.skip(depth + 1)?;
                }
                Ok(())
            }
            Kind::String => self.string(&mut Discard),
            Kind::Other => self.scalar(),
        }
    }

    /// A number, `true`, `false`, or `null`.
    fn scalar(&mut self) -> Result<()> {
        match self.peek()? {
            Some(b't') => self.literal(b"true"),
            Some(b'f') => self.literal(b"false"),
            Some(b'n') => self.literal(b"null"),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(JsonError::Syntax),
        }
    }

    fn literal(&mut self, word: &[u8]) -> Result<()> {
        for &b in word {
            self.expect(b)?;
        }
        Ok(())
    }

    fn digits(&mut self) -> Result<usize> {
        let mut n = 0;
        while let Some(b'0'..=b'9') = self.peek()? {
            self.pos += 1;
            n += 1;
        }
        Ok(n)
    }

    fn number(&mut self) -> Result<()> {
        if self.peek()? == Some(b'-') {
            self.pos += 1;
        }
        match self.next()? {
            b'0' => {}
            b'1'..=b'9' => {
                self.digits()?;
            }
            _ => return Err(JsonError::Syntax),
        }
        if self.peek()? == Some(b'.') {
            self.pos += 1;
            if self.digits()? == 0 {
                return Err(JsonError::Syntax);
            }
        }
        if let Some(b'e' | b'E') = self.peek()? {
            self.pos += 1;
            if let Some(b'+' | b'-') = self.peek()? {
                self.pos += 1;
            }
            if self.digits()? == 0 {
                return Err(JsonError::Syntax);
            }
        }
        Ok(())
    }

    /// A string, its decoded bytes to `out`: escapes decoded, raw bytes checked as UTF-8.
    pub(crate) fn string(&mut self, out: &mut dyn StrSink) -> Result<()> {
        self.skip_ws()?;
        self.expect(b'"')?;
        let mut utf8 = Utf8::default();
        loop {
            if !self.fill()? {
                return Err(JsonError::Syntax);
            }
            // The run of plain bytes up to a quote, a backslash, or a control byte.
            let start = self.pos;
            let mut at = start;
            while at < self.end {
                let b = self.buf[at];
                if b == b'"' || b == b'\\' || b < 0x20 {
                    break;
                }
                utf8.step(b)?;
                at += 1;
            }
            if at > start {
                out.push(&self.buf[start..at]).map_err(JsonError::Sink)?;
            }
            self.pos = at;
            if at == self.end {
                continue;
            }
            match self.buf[at] {
                b'"' => {
                    self.pos += 1;
                    utf8.finish()?;
                    return Ok(());
                }
                b'\\' => {
                    utf8.finish()?;
                    self.pos += 1;
                    self.escape(out)?;
                }
                _ => return Err(JsonError::Syntax),
            }
        }
    }

    fn escape(&mut self, out: &mut dyn StrSink) -> Result<()> {
        let byte = match self.next()? {
            b'"' => b'"',
            b'\\' => b'\\',
            b'/' => b'/',
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'u' => {
                let hi = self.hex4()?;
                let c = if (0xD800..0xDC00).contains(&hi) {
                    self.expect(b'\\')?;
                    self.expect(b'u')?;
                    let lo = self.hex4()?;
                    if !(0xDC00..0xE000).contains(&lo) {
                        return Err(JsonError::Syntax);
                    }
                    0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                } else if (0xDC00..0xE000).contains(&hi) {
                    return Err(JsonError::Syntax);
                } else {
                    hi
                };
                let c = char::from_u32(c).ok_or(JsonError::Syntax)?;
                let mut tmp = [0u8; 4];
                out.push(c.encode_utf8(&mut tmp).as_bytes())
                    .map_err(JsonError::Sink)?;
                return Ok(());
            }
            _ => return Err(JsonError::Syntax),
        };
        out.push(&[byte]).map_err(JsonError::Sink)
    }

    fn hex4(&mut self) -> Result<u32> {
        let mut v = 0u32;
        for _ in 0..4 {
            let d = match self.next()? {
                b @ b'0'..=b'9' => b - b'0',
                b @ b'a'..=b'f' => b - b'a' + 10,
                b @ b'A'..=b'F' => b - b'A' + 10,
                _ => return Err(JsonError::Syntax),
            };
            v = v * 16 + u32::from(d);
        }
        Ok(v)
    }
}

/// An incremental UTF-8 check (RFC 3629: no overlong forms, surrogates, or values above
/// U+10FFFF).
#[derive(Default)]
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
                return Err(JsonError::Syntax);
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
            _ => return Err(JsonError::Syntax),
        };
        self.need = need;
        self.lo = lo;
        self.hi = hi;
        Ok(())
    }

    fn finish(&self) -> Result<()> {
        if self.need == 0 {
            Ok(())
        } else {
            Err(JsonError::Syntax)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads `src` byte by byte (the worst split for every check).
    struct Trickle<'a>(&'a [u8]);

    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0.is_empty() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.0[0];
            self.0 = &self.0[1..];
            Ok(1)
        }
    }

    fn valid(src: &[u8]) -> bool {
        let whole = {
            let mut r = Reader::new(src);
            r.skip(1).and_then(|()| r.end()).is_ok()
        };
        let trickled = {
            let mut r = Reader::new(Trickle(src));
            r.skip(1).and_then(|()| r.end()).is_ok()
        };
        assert_eq!(whole, trickled, "{}", String::from_utf8_lossy(src));
        whole
    }

    /// The reader accepts exactly what serde_json accepts as a `Value` (strings decoded), for
    /// these inputs.
    #[test]
    fn agrees_with_serde_json() {
        let cases: &[&[u8]] = &[
            b"{}",
            b"[]",
            b" {\"a\" : [1, -2.5e+3, true, false, null, \"x\"] } ",
            b"\"\\u00e9\\ud83d\\ude00\\n\"",
            b"0",
            b"-0.0e0",
            b"{\"a\":{\"b\":[[]]}}",
            "\"日本語😀\"".as_bytes(),
            b"",
            b"{",
            b"{\"a\"}",
            b"{\"a\":1,}",
            b"[1,]",
            b"[,1]",
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
        ];
        for c in cases {
            let serde = serde_json::from_slice::<serde_json::Value>(c).is_ok();
            assert_eq!(valid(c), serde, "{}", String::from_utf8_lossy(c));
        }
    }

    #[test]
    fn strings_decode_in_pieces() {
        let src = "\"a\\\"b\\\\c\\/d\\b\\f\\n\\r\\t\\u3042\\ud83d\\ude00日\"".as_bytes();
        let mut out = Capped::new(1024);
        Reader::new(Trickle(src)).string(&mut out).unwrap();
        assert_eq!(out.text().unwrap(), "a\"b\\c/d\u{8}\u{c}\n\r\tあ😀日");
        let mut small = Capped::new(2);
        Reader::new(&b"\"abc\""[..]).string(&mut small).unwrap();
        assert!(small.over);
        assert_eq!(small.text(), None);
    }

    #[test]
    fn nesting_is_bounded() {
        let deep = |n: usize| {
            let mut s = "[".repeat(n);
            s.push_str(&"]".repeat(n));
            s
        };
        assert!(valid(deep(MAX_DEPTH).as_bytes()));
        assert!(!valid(deep(MAX_DEPTH + 1).as_bytes()));
    }
}
