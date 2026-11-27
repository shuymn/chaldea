//! The Claude transcript line parser: pure parsing (C1 D7), run inside
//! `lifecycle::recoverable`. It reads one line as a stream, keeps the assistant text
//! and the `timestamp`, and validates and drops everything else.
//!
//! A line is one JSON object with a string `type`. The types Claude Code 2.1.331 wrote in the
//! captures are known (`KNOWN_TYPES`); an `assistant` line's `message.content` is an array of
//! blocks, each an object with a string `type`, and a `text` block has a string `text`. The
//! text of an `assistant` line is its `text` blocks' texts concatenated in order; an
//! `assistant` line without a `text` block (thinking, a tool call) and every other known type
//! produce no event.

use std::io::{self, Read};

use chaldea_core::ids::{Timestamp, stamp_millis};
use chaldea_core::payload::RawUnknownReason;

use super::acc::{ByteAcc, SpoolDir};
use super::json::{Capped, JsonError, Kind, Reader};

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

/// What one line is.
pub(crate) enum Line {
    /// An `assistant` line with text.
    Assistant {
        text: Box<ByteAcc>,
        occurred_at: Option<Timestamp>,
    },
    /// A known line that produces no event.
    Skip,
    /// A line kept verbatim as `raw_unknown` with this reason.
    Raw(RawUnknownReason),
}

/// The longest key or `type` value compared; a longer one matches nothing.
const KEY_BYTES: usize = 32;
const VALUE_BYTES: usize = 64;

/// Parses one line read from `r` (its bytes without the newline).
///
/// # Errors
///
/// `r` or a spool in `dir` failed: the line was not classified.
pub(crate) fn parse(r: impl Read, dir: &SpoolDir) -> io::Result<Line> {
    let mut rd = Reader::new(r);
    match line(&mut rd, dir) {
        Ok(l) => Ok(l),
        Err(JsonError::Syntax) => Ok(Line::Raw(RawUnknownReason::Unparseable)),
        Err(JsonError::Read(e) | JsonError::Sink(e)) => Err(e),
    }
}

/// A `type` member as read.
enum TypeField {
    Absent,
    /// Not a string, or repeated.
    Bad,
    /// A string; `None` when longer than `VALUE_BYTES` (no known type is).
    Text(Option<String>),
}

/// What the walk of an `assistant` message found.
struct Shape {
    /// Every rule held so far.
    ok: bool,
    /// The `text` blocks seen.
    text_blocks: usize,
    text: ByteAcc,
}

fn line<R: Read>(rd: &mut Reader<R>, dir: &SpoolDir) -> super::json::Result<Line> {
    if rd.kind()? != Kind::Object {
        rd.skip(1)?;
        rd.end()?;
        return Ok(Line::Raw(RawUnknownReason::SchemaMismatch));
    }
    rd.begin_object()?;
    let mut ty = TypeField::Absent;
    let mut stamp: Option<String> = None;
    let mut message = false;
    let mut shape = Shape {
        ok: true,
        text_blocks: 0,
        text: ByteAcc::new(dir.clone()),
    };
    let mut first = true;
    loop {
        let mut key = Capped::new(KEY_BYTES);
        if !rd.next_key(first, &mut key)? {
            break;
        }
        first = false;
        match key.text() {
            Some("type") => {
                let repeated = !matches!(ty, TypeField::Absent);
                ty = if rd.kind()? == Kind::String {
                    let mut v = Capped::new(VALUE_BYTES);
                    rd.string(&mut v)?;
                    TypeField::Text(v.text().map(str::to_owned))
                } else {
                    rd.skip(2)?;
                    TypeField::Bad
                };
                if repeated {
                    ty = TypeField::Bad;
                }
            }
            Some("message") => {
                let not_assistant =
                    matches!(&ty, TypeField::Text(t) if t.as_deref() != Some("assistant"));
                if message || not_assistant {
                    shape.ok &= !message;
                    rd.skip(2)?;
                } else {
                    assistant_message(rd, &mut shape, dir)?;
                }
                message = true;
            }
            Some("timestamp") => {
                if rd.kind()? == Kind::String {
                    let mut v = Capped::new(VALUE_BYTES);
                    rd.string(&mut v)?;
                    stamp = v.text().map(str::to_owned);
                } else {
                    rd.skip(2)?;
                }
            }
            _ => rd.skip(2)?,
        }
    }
    rd.end()?;
    let ty = match ty {
        TypeField::Absent | TypeField::Bad => {
            return Ok(Line::Raw(RawUnknownReason::SchemaMismatch));
        }
        TypeField::Text(None) => return Ok(Line::Raw(RawUnknownReason::UnknownEvent)),
        TypeField::Text(Some(t)) => t,
    };
    #[cfg(test)]
    if ty == tests::PANIC_TYPE {
        panic!("the test's line parser panic");
    }
    if ty == "assistant" {
        if !message || !shape.ok {
            return Ok(Line::Raw(RawUnknownReason::SchemaMismatch));
        }
        if shape.text_blocks == 0 {
            return Ok(Line::Skip);
        }
        let occurred_at = stamp.filter(|s| stamp_millis(s).is_some()).map(Timestamp);
        return Ok(Line::Assistant {
            text: Box::new(shape.text),
            occurred_at,
        });
    }
    if KNOWN_TYPES.contains(&ty.as_str()) {
        Ok(Line::Skip)
    } else {
        Ok(Line::Raw(RawUnknownReason::UnknownEvent))
    }
}

/// `message`, read while the line may be an `assistant` one: an object whose `content` is an
/// array of blocks.
fn assistant_message<R: Read>(
    rd: &mut Reader<R>,
    shape: &mut Shape,
    dir: &SpoolDir,
) -> super::json::Result<()> {
    if rd.kind()? != Kind::Object {
        shape.ok = false;
        return rd.skip(2);
    }
    rd.begin_object()?;
    let mut content = false;
    let mut first = true;
    loop {
        let mut key = Capped::new(KEY_BYTES);
        if !rd.next_key(first, &mut key)? {
            break;
        }
        first = false;
        if key.text() != Some("content") {
            rd.skip(3)?;
            continue;
        }
        if content || rd.kind()? != Kind::Array {
            shape.ok = false;
            rd.skip(3)?;
            content = true;
            continue;
        }
        content = true;
        rd.begin_array()?;
        let mut first_block = true;
        while rd.next_element(first_block)? {
            first_block = false;
            block(rd, shape, dir)?;
        }
    }
    shape.ok &= content;
    Ok(())
}

/// The block's `type` as read.
#[derive(PartialEq, Eq)]
enum BlockType {
    Unseen,
    Text,
    Other,
    Bad,
}

/// One content block. A `text` value read before the block's `type` is kept aside and
/// moved to the line's text if the block turns out to be a `text` block.
fn block<R: Read>(
    rd: &mut Reader<R>,
    shape: &mut Shape,
    dir: &SpoolDir,
) -> super::json::Result<()> {
    if rd.kind()? != Kind::Object {
        shape.ok = false;
        return rd.skip(4);
    }
    rd.begin_object()?;
    let mut ty = BlockType::Unseen;
    // `None`: no `text` yet; `Some(None)`: a non-string `text` or one streamed to the line.
    let mut text: Option<Option<ByteAcc>> = None;
    let mut text_string = false;
    let mut first = true;
    loop {
        let mut key = Capped::new(KEY_BYTES);
        if !rd.next_key(first, &mut key)? {
            break;
        }
        first = false;
        match key.text() {
            Some("type") => {
                if ty != BlockType::Unseen {
                    ty = BlockType::Bad;
                    rd.skip(5)?;
                } else if rd.kind()? == Kind::String {
                    let mut v = Capped::new(KEY_BYTES);
                    rd.string(&mut v)?;
                    ty = if v.text() == Some("text") {
                        BlockType::Text
                    } else {
                        BlockType::Other
                    };
                } else {
                    ty = BlockType::Bad;
                    rd.skip(5)?;
                }
            }
            Some("text") => {
                if text.is_some() {
                    shape.ok = false;
                    rd.skip(5)?;
                    continue;
                }
                if rd.kind()? != Kind::String {
                    text = Some(None);
                    rd.skip(5)?;
                    continue;
                }
                text_string = true;
                match ty {
                    BlockType::Text => {
                        rd.string(&mut shape.text)?;
                        text = Some(None);
                    }
                    BlockType::Unseen => {
                        let mut aside = ByteAcc::new(dir.clone());
                        rd.string(&mut aside)?;
                        text = Some(Some(aside));
                    }
                    BlockType::Other | BlockType::Bad => {
                        rd.skip(5)?;
                        text = Some(None);
                    }
                }
            }
            _ => rd.skip(5)?,
        }
    }
    match ty {
        BlockType::Text => {
            if !text_string {
                shape.ok = false;
            } else {
                if let Some(Some(aside)) = text {
                    shape.text.append(aside).map_err(JsonError::Sink)?;
                }
                shape.text_blocks += 1;
            }
        }
        BlockType::Other => {}
        BlockType::Unseen | BlockType::Bad => shape.ok = false,
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A line of this type makes the parser panic (unit tests only).
    pub(crate) const PANIC_TYPE: &str = "chaldea-test-panic";

    fn dir() -> (tempfile::TempDir, SpoolDir) {
        let d = tempfile::tempdir().unwrap();
        let s = SpoolDir(d.path().to_owned());
        (d, s)
    }

    fn text_of(line: &str) -> Result<String, String> {
        let (_d, dir) = dir();
        match parse(line.as_bytes(), &dir).unwrap() {
            Line::Assistant { text, .. } => {
                let p = text.finish("text/plain; charset=utf-8").unwrap();
                match p.content {
                    chaldea_core::payload::Content::Inline { inline, .. } => Ok(inline),
                    c @ chaldea_core::payload::Content::Blob { .. } => Err(format!("{c:?}")),
                }
            }
            Line::Skip => Err("skip".into()),
            Line::Raw(r) => Err(format!("{r:?}")),
        }
    }

    #[test]
    fn classifies_lines() {
        let a = r#"{"type":"assistant","timestamp":"2026-11-26T12:29:40.123Z","message":{"role":"assistant","content":[{"type":"text","text":"ok"}]}}"#;
        assert_eq!(text_of(a), Ok("ok".into()));
        let cases = [
            // Text blocks concatenate; other blocks are skipped; key order does not matter.
            (
                r#"{"message":{"content":[{"text":"a","type":"text"},{"type":"thinking","thinking":"x","text":1},{"type":"text","text":"b"}]},"type":"assistant"}"#,
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
            (r#"{"type":1}"#, Err("SchemaMismatch".into())),
            (
                r#"{"type":"mode","type":"mode"}"#,
                Err("SchemaMismatch".into()),
            ),
            (r#"{"mode":"normal"}"#, Err("SchemaMismatch".into())),
            (r#"["assistant"]"#, Err("SchemaMismatch".into())),
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

    #[test]
    fn keeps_a_valid_timestamp_only() {
        let (_d, dir) = dir();
        let at = |stamp: &str| {
            let l = format!(
                r#"{{"type":"assistant","timestamp":"{stamp}","message":{{"content":[{{"type":"text","text":"t"}}]}}}}"#
            );
            match parse(l.as_bytes(), &dir).unwrap() {
                Line::Assistant { occurred_at, .. } => occurred_at,
                _ => panic!("assistant"),
            }
        };
        assert_eq!(
            at("2026-11-26T12:29:40.123Z"),
            Some(Timestamp("2026-11-26T12:29:40.123Z".into()))
        );
        assert_eq!(at("yesterday"), None);
    }
}
