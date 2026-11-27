//! The Claude transcript line parser: pure parsing (C1 D7), fed one line's bytes in pieces
//! inside `lifecycle::recoverable`. It does no I/O and takes no lock: the assistant text it
//! finds leaves it as [`Emit`]s, which the caller stores outside the recoverable region. It
//! keeps the `timestamp` and the `promptId`, and validates and drops everything else.
//!
//! A line is one JSON object with a string `type`. The types Claude Code 2.1.331 wrote in the
//! captures are known (`KNOWN_TYPES`); an `assistant` line's `message.content` is an array of
//! blocks, each an object with a string `type`, and a `text` block has a string `text`. The
//! text of an `assistant` line is its `text` blocks' texts concatenated in order; an
//! `assistant` line without a `text` block (thinking, a tool call) and every other known type
//! produce no event. Values the parser does not use are skipped at any depth.

use chaldea_core::ids::{Timestamp, stamp_millis};
use chaldea_core::payload::RawUnknownReason;

use super::json::{Sink, Tok, Tokenizer};

/// The line types of the 2.1.331 captures (`fixtures/claude/2.1.331/`).
pub(crate) const KNOWN_TYPES: &[&str] = &[
    "ai-title",
    "api-request",
    "api-request-blob",
    "api-request-shape",
    "assistant",
    "atis-latch",
    "attachment",
    "cost-state",
    "data-flow-incomplete",
    "data-flow-records",
    "file-history-snapshot",
    "last-prompt",
    "mode",
    "permission-mode",
    "queue-operation",
    "system",
    "tools-used",
    "user",
];

/// Assistant text found while feeding, in order.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Emit {
    /// Bytes of the line's text.
    Text(Vec<u8>),
    /// Bytes of a block's `text` read before the block's `type`: held aside.
    Aside(Vec<u8>),
    /// The block is a `text` block: the aside bytes follow the line's text.
    KeepAside,
    /// The block is not a `text` block: the aside bytes are dropped.
    DropAside,
}

/// What one line is.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// An `assistant` line with text (the emitted text).
    Assistant {
        occurred_at: Option<Timestamp>,
        turn_ref: Option<String>,
    },
    /// A known line that produces no event.
    Skip,
    /// A line kept verbatim as `raw_unknown` with this reason.
    Raw(RawUnknownReason),
}

/// The longest key or `type` value compared; a longer one matches nothing.
const KEY_BYTES: usize = 32;
const VALUE_BYTES: usize = 64;
/// The longest `promptId` kept; a longer one gives no `turn_ref`.
const PROMPT_BYTES: usize = 128;

/// A string kept up to a cap.
#[derive(Debug, Default)]
struct Capped {
    cap: usize,
    bytes: Vec<u8>,
    over: bool,
}

impl Capped {
    fn new(cap: usize) -> Capped {
        Capped {
            cap,
            bytes: Vec::new(),
            over: false,
        }
    }

    /// Empties the string for one capped at `cap`, keeping its buffer.
    fn reset(&mut self, cap: usize) {
        self.cap = cap;
        self.bytes.clear();
        self.over = false;
    }

    fn push(&mut self, b: &[u8]) {
        if self.over || self.bytes.len() + b.len() > self.cap {
            self.over = true;
            self.bytes.clear();
        } else {
            self.bytes.extend_from_slice(b);
        }
    }

    /// The string, unless it passed the cap.
    fn text(&self) -> Option<&str> {
        if self.over {
            None
        } else {
            std::str::from_utf8(&self.bytes).ok()
        }
    }
}

/// A `type` member as read.
#[derive(Debug)]
enum TypeField {
    Absent,
    /// Not a string, or repeated.
    Bad,
    /// A string; `None` when longer than `VALUE_BYTES` (no known type is).
    Text(Option<String>),
}

/// A block's `type` as read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockType {
    Unseen,
    Text,
    Other,
    Bad,
}

/// The containers the parser reads into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Frame {
    /// The line's object.
    Top,
    /// `message`.
    Message,
    /// `message.content`.
    Content,
    /// One content block.
    Block,
}

/// What the next value is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Root,
    Type,
    Stamp,
    Prompt,
    Message,
    Content,
    Block,
    BlockType,
    BlockText,
    Ignore,
}

/// Where a string's pieces go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StrTo {
    Key,
    Capped(Slot),
    Text,
    Aside,
    Discard,
}

/// The parse state of one line.
#[derive(Debug)]
struct Walk {
    frames: Vec<Frame>,
    /// The depth inside a skipped container (0: not skipping).
    skip: usize,
    /// The slot of the value after the last key.
    slot: Slot,
    str_to: StrTo,
    key: Capped,
    value: Capped,
    not_object: bool,
    ty: TypeField,
    stamp: Option<String>,
    prompt: Option<String>,
    message: bool,
    /// Every rule of an `assistant` message held so far.
    ok: bool,
    content: bool,
    text_blocks: usize,
    block_ty: BlockType,
    /// The block's `text` was seen; `text_string`: it was a string.
    block_text: bool,
    text_string: bool,
    /// Aside bytes are held for this block.
    aside: bool,
}

/// Collects the walk's emits during one feed.
struct Feed<'a> {
    w: &'a mut Walk,
    out: &'a mut Vec<Emit>,
}

impl Feed<'_> {
    fn emit_bytes(&mut self, aside: bool, b: &[u8]) {
        match (self.out.last_mut(), aside) {
            (Some(Emit::Text(v)), false) | (Some(Emit::Aside(v)), true) => v.extend_from_slice(b),
            (_, false) => self.out.push(Emit::Text(b.to_vec())),
            (_, true) => self.out.push(Emit::Aside(b.to_vec())),
        }
    }

    /// The slot of a value starting now.
    fn slot(&self) -> Slot {
        match self.w.frames.last() {
            None => Slot::Root,
            Some(Frame::Content) => Slot::Block,
            Some(_) => self.w.slot,
        }
    }

    /// A value of the wrong kind in `slot`.
    fn mismatch(&mut self, slot: Slot) {
        let w = &mut *self.w;
        match slot {
            Slot::Root => w.not_object = true,
            Slot::Type => w.ty = TypeField::Bad,
            Slot::Message | Slot::Content | Slot::Block => w.ok = false,
            Slot::BlockType => w.block_ty = BlockType::Bad,
            Slot::BlockText => w.block_text = true,
            Slot::Stamp | Slot::Prompt | Slot::Ignore => {}
        }
    }

    /// The slot of the value after the key just read.
    fn key_slot(&mut self) -> Slot {
        let w = &mut *self.w;
        let key = w.key.text();
        match (w.frames.last(), key) {
            (Some(Frame::Top), Some("type")) => {
                if matches!(w.ty, TypeField::Absent) {
                    Slot::Type
                } else {
                    w.ty = TypeField::Bad;
                    Slot::Ignore
                }
            }
            (Some(Frame::Top), Some("message")) => {
                let not_assistant =
                    matches!(&w.ty, TypeField::Text(t) if t.as_deref() != Some("assistant"));
                let slot = if w.message || not_assistant {
                    w.ok &= !w.message;
                    Slot::Ignore
                } else {
                    Slot::Message
                };
                w.message = true;
                slot
            }
            (Some(Frame::Top), Some("timestamp")) => Slot::Stamp,
            (Some(Frame::Top), Some("promptId" | "prompt_id")) => Slot::Prompt,
            (Some(Frame::Message), Some("content")) => {
                let slot = if w.content {
                    w.ok = false;
                    Slot::Ignore
                } else {
                    Slot::Content
                };
                w.content = true;
                slot
            }
            (Some(Frame::Block), Some("type")) => {
                if w.block_ty == BlockType::Unseen {
                    Slot::BlockType
                } else {
                    w.block_ty = BlockType::Bad;
                    Slot::Ignore
                }
            }
            (Some(Frame::Block), Some("text")) => {
                if w.block_text {
                    w.ok = false;
                    Slot::Ignore
                } else {
                    Slot::BlockText
                }
            }
            _ => Slot::Ignore,
        }
    }

    fn begin_string(&mut self) {
        let slot = self.slot();
        self.w.str_to = match slot {
            Slot::Type | Slot::Stamp => {
                self.w.value.reset(VALUE_BYTES);
                StrTo::Capped(slot)
            }
            Slot::Prompt => {
                self.w.value.reset(PROMPT_BYTES);
                StrTo::Capped(slot)
            }
            Slot::BlockType => {
                self.w.value.reset(KEY_BYTES);
                StrTo::Capped(slot)
            }
            Slot::BlockText => {
                self.w.block_text = true;
                self.w.text_string = true;
                match self.w.block_ty {
                    BlockType::Text => StrTo::Text,
                    BlockType::Unseen => {
                        self.w.aside = true;
                        StrTo::Aside
                    }
                    BlockType::Other | BlockType::Bad => StrTo::Discard,
                }
            }
            Slot::Ignore => StrTo::Discard,
            Slot::Root | Slot::Message | Slot::Content | Slot::Block => {
                self.mismatch(slot);
                StrTo::Discard
            }
        };
    }

    fn end_string(&mut self) {
        let w = &mut *self.w;
        match w.str_to {
            StrTo::Key => {}
            StrTo::Capped(Slot::Type) => w.ty = TypeField::Text(w.value.text().map(str::to_owned)),
            StrTo::Capped(Slot::Stamp) => w.stamp = w.value.text().map(str::to_owned),
            StrTo::Capped(Slot::Prompt) => w.prompt = w.value.text().map(str::to_owned),
            StrTo::Capped(_) => {
                w.block_ty = if w.value.text() == Some("text") {
                    BlockType::Text
                } else {
                    BlockType::Other
                };
            }
            StrTo::Text | StrTo::Aside | StrTo::Discard => {}
        }
    }

    fn begin_container(&mut self, object: bool) {
        let slot = self.slot();
        let frame = match (slot, object) {
            (Slot::Root, true) => Some(Frame::Top),
            (Slot::Message, true) => {
                self.w.content = false;
                Some(Frame::Message)
            }
            (Slot::Content, false) => Some(Frame::Content),
            (Slot::Block, true) => {
                let w = &mut *self.w;
                w.block_ty = BlockType::Unseen;
                w.block_text = false;
                w.text_string = false;
                w.aside = false;
                Some(Frame::Block)
            }
            _ => None,
        };
        match frame {
            Some(f) => self.w.frames.push(f),
            None => {
                self.mismatch(slot);
                self.w.skip = 1;
            }
        }
    }

    fn end_container(&mut self) {
        match self.w.frames.pop() {
            Some(Frame::Message) => self.w.ok &= self.w.content,
            Some(Frame::Block) => self.end_block(),
            Some(Frame::Top | Frame::Content) | None => {}
        }
    }

    fn end_block(&mut self) {
        let w = &mut *self.w;
        let mut keep = false;
        match w.block_ty {
            BlockType::Text => {
                if w.text_string {
                    keep = true;
                    w.text_blocks += 1;
                } else {
                    w.ok = false;
                }
            }
            BlockType::Other => {}
            BlockType::Unseen | BlockType::Bad => w.ok = false,
        }
        if w.aside {
            self.out.push(if keep {
                Emit::KeepAside
            } else {
                Emit::DropAside
            });
        }
    }
}

impl Sink for Feed<'_> {
    fn tok(&mut self, t: Tok<'_>) {
        if self.w.skip > 0 {
            match t {
                Tok::BeginObject | Tok::BeginArray => self.w.skip += 1,
                Tok::EndObject | Tok::EndArray => self.w.skip -= 1,
                _ => {}
            }
            return;
        }
        match t {
            Tok::StrBegin { key: true } => {
                self.w.key.reset(KEY_BYTES);
                self.w.str_to = StrTo::Key;
            }
            Tok::StrBegin { key: false } => self.begin_string(),
            Tok::Chunk(b) => match self.w.str_to {
                StrTo::Key => self.w.key.push(b),
                StrTo::Capped(_) => self.w.value.push(b),
                StrTo::Text => self.emit_bytes(false, b),
                StrTo::Aside => self.emit_bytes(true, b),
                StrTo::Discard => {}
            },
            Tok::StrEnd => {
                if self.w.str_to == StrTo::Key {
                    self.w.slot = self.key_slot();
                } else {
                    self.end_string();
                }
            }
            Tok::Scalar => {
                let slot = self.slot();
                if slot != Slot::Ignore {
                    self.mismatch(slot);
                }
            }
            Tok::BeginObject => self.begin_container(true),
            Tok::BeginArray => self.begin_container(false),
            Tok::EndObject | Tok::EndArray => self.end_container(),
        }
    }
}

/// One line's parser: [`feed`](Self::feed) its bytes (without the newline) in pieces, then
/// [`finish`](Self::finish).
#[derive(Debug)]
pub(crate) struct LineParser {
    tok: Tokenizer,
    walk: Walk,
    syntax: bool,
}

impl Default for LineParser {
    fn default() -> Self {
        LineParser {
            tok: Tokenizer::default(),
            walk: Walk {
                frames: Vec::with_capacity(4),
                skip: 0,
                slot: Slot::Ignore,
                str_to: StrTo::Discard,
                key: Capped::new(KEY_BYTES),
                value: Capped::new(VALUE_BYTES),
                not_object: false,
                ty: TypeField::Absent,
                stamp: None,
                prompt: None,
                message: false,
                ok: true,
                content: false,
                text_blocks: 0,
                block_ty: BlockType::Unseen,
                block_text: false,
                text_string: false,
                aside: false,
            },
            syntax: false,
        }
    }
}

impl LineParser {
    /// Feeds the next piece of the line; text found is appended to `out`.
    pub(crate) fn feed(&mut self, bytes: &[u8], out: &mut Vec<Emit>) {
        if self.syntax {
            return;
        }
        let mut f = Feed {
            w: &mut self.walk,
            out,
        };
        self.syntax = self.tok.feed(bytes, &mut f).is_err();
    }

    /// Classifies the line once all its bytes were fed.
    pub(crate) fn finish(mut self) -> Outcome {
        let mut out = Vec::new();
        let mut f = Feed {
            w: &mut self.walk,
            out: &mut out,
        };
        if self.syntax || self.tok.finish(&mut f).is_err() {
            return Outcome::Raw(RawUnknownReason::Unparseable);
        }
        let w = self.walk;
        if w.not_object {
            return Outcome::Raw(RawUnknownReason::SchemaMismatch);
        }
        let ty = match w.ty {
            TypeField::Absent | TypeField::Bad => {
                return Outcome::Raw(RawUnknownReason::SchemaMismatch);
            }
            TypeField::Text(None) => return Outcome::Raw(RawUnknownReason::UnknownEvent),
            TypeField::Text(Some(t)) => t,
        };
        #[cfg(test)]
        if ty == tests::PANIC_TYPE {
            panic!("the test's line parser panic");
        }
        if ty == "assistant" {
            if !w.message || !w.ok {
                return Outcome::Raw(RawUnknownReason::SchemaMismatch);
            }
            if w.text_blocks == 0 {
                return Outcome::Skip;
            }
            return Outcome::Assistant {
                occurred_at: w.stamp.filter(|s| stamp_millis(s).is_some()).map(Timestamp),
                turn_ref: w.prompt,
            };
        }
        if KNOWN_TYPES.contains(&ty.as_str()) {
            Outcome::Skip
        } else {
            Outcome::Raw(RawUnknownReason::UnknownEvent)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A line of this type makes the parser panic (unit tests only).
    pub(crate) const PANIC_TYPE: &str = "chaldea-test-panic";

    /// Parses `line` fed in pieces of `piece` bytes and applies the emits.
    fn parse(line: &[u8], piece: usize) -> (Outcome, Vec<u8>) {
        let mut p = LineParser::default();
        let mut out = Vec::new();
        for c in line.chunks(piece.max(1)) {
            p.feed(c, &mut out);
        }
        let mut text = Vec::new();
        let mut aside: Option<Vec<u8>> = None;
        for e in out {
            match e {
                Emit::Text(b) => text.extend_from_slice(&b),
                Emit::Aside(b) => aside.get_or_insert_default().extend_from_slice(&b),
                Emit::KeepAside => text.extend_from_slice(&aside.take().unwrap()),
                Emit::DropAside => aside = None,
            }
        }
        (p.finish(), text)
    }

    fn text_of(line: &str) -> Result<String, String> {
        let mut got = None;
        for piece in [1, 3, line.len()] {
            let (o, text) = parse(line.as_bytes(), piece);
            let r = match o {
                Outcome::Assistant { .. } => Ok(String::from_utf8(text).unwrap()),
                Outcome::Skip => Err("skip".to_owned()),
                Outcome::Raw(r) => Err(format!("{r:?}")),
            };
            if let Some(g) = &got {
                assert_eq!(&r, g, "{line} in pieces of {piece}");
            }
            got = Some(r);
        }
        got.unwrap()
    }

    #[test]
    fn classifies_lines() {
        let a = r#"{"type":"assistant","timestamp":"2026-11-26T12:29:40.123Z","message":{"role":"assistant","content":[{"type":"text","text":"ok"}]}}"#;
        assert_eq!(text_of(a), Ok("ok".into()));
        let cases = [
            // Text blocks concatenate; other blocks are skipped; key order does not matter.
            (
                r#"{"message":{"content":[{"text":"a","type":"text"},{"type":"thinking","thinking":"x","text":1},{"text":"no","type":"thinking"},{"type":"text","text":"b"}]},"type":"assistant"}"#,
                Ok("ab".to_owned()),
            ),
            (
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t","input":{}}]}}"#,
                Err("skip".to_owned()),
            ),
            (
                r#"{"type":"user","message":{"content":"hi"}}"#,
                Err("skip".into()),
            ),
            (r#"{"type":"mode","mode":"normal"}"#, Err("skip".into())),
            (r#"{"type":"new-kind"}"#, Err("UnknownEvent".into())),
            (r#"{"type":"assistant"}"#, Err("SchemaMismatch".into())),
            (
                r#"{"type":"assistant","message":{"content":"text"}}"#,
                Err("SchemaMismatch".into()),
            ),
            (
                r#"{"type":"assistant","message":{"content":[{"type":"text"}]}}"#,
                Err("SchemaMismatch".into()),
            ),
            (
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":3}]}}"#,
                Err("SchemaMismatch".into()),
            ),
            (
                r#"{"type":"assistant","message":{"content":[{"text":"x"}]}}"#,
                Err("SchemaMismatch".into()),
            ),
            (
                r#"{"type":"assistant","message":{"content":[1]}}"#,
                Err("SchemaMismatch".into()),
            ),
            (
                r#"{"type":"assistant","message":{"content":[]},"message":{}}"#,
                Err("SchemaMismatch".into()),
            ),
            (
                r#"{"type":"assistant","message":{"content":[],"content":[]}}"#,
                Err("SchemaMismatch".into()),
            ),
            (
                r#"{"type":"assistant","message":{}}"#,
                Err("SchemaMismatch".into()),
            ),
            (
                r#"{"type":"assistant","message":[]}"#,
                Err("SchemaMismatch".into()),
            ),
            (r#"{"type":1}"#, Err("SchemaMismatch".into())),
            (r#"{"type":{"a":[1]}}"#, Err("SchemaMismatch".into())),
            (
                r#"{"type":"mode","type":"mode"}"#,
                Err("SchemaMismatch".into()),
            ),
            (r#"{"mode":"normal"}"#, Err("SchemaMismatch".into())),
            (r#"["assistant"]"#, Err("SchemaMismatch".into())),
            (r#""assistant""#, Err("SchemaMismatch".into())),
            (
                r#"{"type":"assistant","message":{"content":[]}"#,
                Err("Unparseable".into()),
            ),
            ("not json", Err("Unparseable".into())),
            ("", Err("Unparseable".into())),
            (r#"{"type":"mode"} {}"#, Err("Unparseable".into())),
        ];
        for (line, want) in cases {
            assert_eq!(text_of(line), want, "{line}");
        }
    }

    /// A value the parser does not use is skipped at any depth.
    #[test]
    fn deep_unused_values_are_skipped() {
        let deep = |n: usize| format!("{}0{}", "[".repeat(n), "]".repeat(n));
        let line = format!(
            r#"{{"type":"assistant","unused":{},"message":{{"x":{},"content":[{{"type":"text","y":{},"text":"ok"}}]}}}}"#,
            deep(100_000),
            deep(200),
            deep(129)
        );
        assert_eq!(text_of(&line), Ok("ok".into()));
    }

    /// The recoverable region does no I/O and takes no lock by construction: the parser and
    /// the tokenizer use no file, spool, or lock type, so a line over 64 MiB reaches its
    /// spool only through the caller.
    #[test]
    fn the_parser_does_no_io() {
        let code = |src: &'static str, tests: &str| src.split_once(tests).unwrap().0;
        for (name, src) in [
            (
                "claude.rs",
                code(
                    include_str!("claude.rs"),
                    "\n#[cfg(test)]\npub(crate) mod tests",
                ),
            ),
            (
                "json.rs",
                code(include_str!("json.rs"), "\n#[cfg(test)]\nmod tests"),
            ),
        ] {
            for banned in [
                "std::io",
                "std::fs",
                "File",
                "Mutex",
                "RwLock",
                "ByteAcc",
                "SpoolDir",
                "BlobSpool",
            ] {
                assert!(!src.contains(banned), "{name} uses {banned}");
            }
        }
    }

    #[test]
    fn keeps_a_valid_timestamp_and_the_prompt_id() {
        let at = |extra: &str| {
            let l = format!(
                r#"{{"type":"assistant",{extra}"message":{{"content":[{{"type":"text","text":"t"}}]}}}}"#
            );
            parse(l.as_bytes(), l.len()).0
        };
        assert_eq!(
            at(r#""timestamp":"2026-11-26T12:29:40.123Z","promptId":"p1","#),
            Outcome::Assistant {
                occurred_at: Some(Timestamp("2026-11-26T12:29:40.123Z".into())),
                turn_ref: Some("p1".into()),
            }
        );
        assert_eq!(
            at(r#""timestamp":"yesterday","prompt_id":"p2","#),
            Outcome::Assistant {
                occurred_at: None,
                turn_ref: Some("p2".into()),
            }
        );
        assert_eq!(
            at(r#""promptId":7,"#),
            Outcome::Assistant {
                occurred_at: None,
                turn_ref: None,
            }
        );
    }
}
