//! C1-8, line handling: assistant text with its cursor, lines that produce no event, and
//! lines kept verbatim.

use super::{Fx, MODE, assistant, digest, shorts};
use crate::transcript::read::Step;

/// An assistant line becomes `assistant_message` with its position and time; another known
/// type produces no event; an unknown type, garbage, and the wrong shape are kept verbatim
/// with their reasons; the cursor ends after the last line with the digest of every byte.
#[test]
fn each_line_becomes_its_event_with_the_cursor() {
    let fx = Fx::new();
    let one = assistant("ok");
    let lines = [
        one.as_str(),
        MODE,
        "{\"type\":\"brand-new\",\"x\":1}\n",
        "garbage\n",
        "{\"type\":\"assistant\",\"message\":{\"content\":\"x\"}}\n",
        MODE,
    ];
    let body = lines.concat();
    fx.write(body.as_bytes());
    let mut tail = fx.tail();
    assert_eq!(fx.settle(&mut tail), Step::Wait);
    let evs = fx.events();
    assert_eq!(
        shorts(&evs),
        [
            "generation:1",
            "text:ok",
            "raw:unknown_event:{\"type\":\"brand-new\",\"x\":1}",
            "raw:unparseable:garbage",
            "raw:schema_mismatch:{\"type\":\"assistant\",\"message\":{\"content\":\"x\"}}",
        ]
    );
    let ends: Vec<usize> = lines
        .iter()
        .scan(0, |at, l| {
            *at += l.len();
            Some(*at)
        })
        .collect();
    assert_eq!(evs[1].offset_end, Some(ends[0] as u64));
    assert_eq!(evs[1].occurred_at, "2026-11-26T12:30:31.281Z");
    assert_eq!(evs[2].offset_end, Some(ends[2] as u64));
    assert_eq!(evs[4].offset_end, Some(ends[4] as u64));
    assert_eq!(evs[2].payload["content_type"], "application/json");
    assert_eq!(evs[3].payload["content_type"], "application/octet-stream");
    assert_eq!(evs[2].payload["source"], "transcript");
    assert_eq!(evs[2].payload["cli_version"], "1.0.0");
    let c = fx.cursor().unwrap();
    assert_eq!((c.generation, c.read_offset), (1, body.len() as u64));
    assert_eq!(c.read_continuity.0, digest(body.as_bytes()));
    // The event position is the last event's, with the digest of the bytes before it.
    assert_eq!(c.event_offset, ends[4] as u64);
    assert_eq!(c.event_continuity.0, digest(&body.as_bytes()[..ends[4]]));
}

/// A line still being written is read again once its newline arrives, and a second poll
/// with nothing new stores nothing.
#[test]
fn a_line_is_stored_once_its_newline_arrives() {
    let fx = Fx::new();
    let line = assistant("late");
    fx.write(&line.as_bytes()[..line.len() - 1]);
    let mut tail = fx.tail();
    assert_eq!(fx.settle(&mut tail), Step::Wait);
    assert_eq!(shorts(&fx.events()), ["generation:1"]);
    assert_eq!(fx.cursor().unwrap().read_offset, 0);
    fx.append(b"\n");
    assert_eq!(fx.settle(&mut tail), Step::Wait);
    assert_eq!(fx.settle(&mut tail), Step::Wait);
    assert_eq!(shorts(&fx.events()), ["generation:1", "text:late"]);
}

/// Japanese text and emoji arrive intact, escapes decoded.
#[test]
fn text_is_decoded_intact() {
    let fx = Fx::new();
    fx.write(assistant("日本語\\n😀 \\u304b\\u3099").as_bytes());
    let mut tail = fx.tail();
    fx.settle(&mut tail);
    assert_eq!(
        shorts(&fx.events()),
        ["generation:1", "text:日本語\n😀 か\u{3099}"]
    );
}
