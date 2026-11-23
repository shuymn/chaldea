//! `decode`: the pure hook-head decoder (`docs/design/hook-ingest.md` §2.3 step 3), the fuzz
//! target of PR 22.
//!
//! It reads only `hook_event_name`, `session_id`, and `transcript_path`, borrowed from the
//! body when a string has no escape and owned when it has one; every other value is
//! skipped without being built, though its JSON syntax is still checked.

use std::borrow::Cow;
use std::fmt;

use serde::Deserialize;
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};

/// The `hook_event_name` that can bind a launch.
pub const SESSION_START: &str = "SessionStart";

/// What [`decode`] made of a body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoded<'a> {
    /// A JSON object with a non-empty string `hook_event_name` and `session_id`; for a
    /// `SessionStart`, `transcript_path` absent, `null`, or a string.
    Hook(HookHead<'a>),
    /// Not UTF-8, or not JSON (§2.7 row 1).
    Unparseable,
    /// JSON without the hook fields in their form (§2.7 row 2).
    SchemaMismatch,
}

/// The discriminating fields of a hook payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookHead<'a> {
    pub event: Cow<'a, str>,
    pub session_id: Cow<'a, str>,
    /// Read only for a `SessionStart`; `None` when absent or `null`.
    pub transcript_path: Option<Cow<'a, str>>,
}

impl HookHead<'_> {
    /// The fields as owned strings, for a closure that outlives the body.
    #[must_use]
    pub fn into_owned(self) -> HookHead<'static> {
        HookHead {
            event: Cow::Owned(self.event.into_owned()),
            session_id: Cow::Owned(self.session_id.into_owned()),
            transcript_path: self.transcript_path.map(|p| Cow::Owned(p.into_owned())),
        }
    }

    #[must_use]
    pub fn is_session_start(&self) -> bool {
        self.event == SESSION_START
    }
}

/// Classifies `body` as a hook head, `Unparseable`, or `SchemaMismatch`. Pure, no I/O, never
/// panics. A repeated key counts with its last value, as a parsed JSON tree reads it.
#[must_use]
pub fn decode(body: &[u8]) -> Decoded<'_> {
    let Ok(text) = std::str::from_utf8(body) else {
        return Decoded::Unparseable;
    };
    let mut de = serde_json::Deserializer::from_str(text);
    let Ok(top) = Top::deserialize(&mut de) else {
        return Decoded::Unparseable;
    };
    if de.end().is_err() {
        return Decoded::Unparseable;
    }
    let Top::Object {
        event,
        session_id,
        transcript_path,
    } = top
    else {
        return Decoded::SchemaMismatch;
    };
    fn non_empty(v: Option<Val<'_>>) -> Option<Cow<'_, str>> {
        match v {
            Some(Val::Str(s)) if !s.is_empty() => Some(s),
            _ => None,
        }
    }
    let (Some(event), Some(session_id)) = (non_empty(event), non_empty(session_id)) else {
        return Decoded::SchemaMismatch;
    };
    let transcript_path = if event == SESSION_START {
        match transcript_path {
            None | Some(Val::Null) => None,
            Some(Val::Str(s)) => Some(s),
            Some(Val::Other) => return Decoded::SchemaMismatch,
        }
    } else {
        None
    };
    Decoded::Hook(HookHead {
        event,
        session_id,
        transcript_path,
    })
}

/// One field's value: a string, `null`, or anything else (skipped).
enum Val<'a> {
    Str(Cow<'a, str>),
    Null,
    Other,
}

impl<'de> Deserialize<'de> for Val<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(ValVisitor)
    }
}

struct ValVisitor;

impl<'de> Visitor<'de> for ValVisitor {
    type Value = Val<'de>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Val<'de>, E> {
        Ok(Val::Str(Cow::Borrowed(v)))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Val<'de>, E> {
        Ok(Val::Str(Cow::Owned(v.to_owned())))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Val<'de>, E> {
        Ok(Val::Str(Cow::Owned(v)))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Val<'de>, E> {
        Ok(Val::Null)
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Val<'de>, E> {
        Ok(Val::Other)
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Val<'de>, E> {
        Ok(Val::Other)
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Val<'de>, E> {
        Ok(Val::Other)
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Val<'de>, E> {
        Ok(Val::Other)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Val<'de>, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Val::Other)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Val<'de>, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(Val::Other)
    }
}

/// The top-level value: an object's three fields, or anything else.
enum Top<'a> {
    Object {
        event: Option<Val<'a>>,
        session_id: Option<Val<'a>>,
        transcript_path: Option<Val<'a>>,
    },
    NotObject,
}

impl<'de> Deserialize<'de> for Top<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(TopVisitor)
    }
}

struct TopVisitor;

impl<'de> Visitor<'de> for TopVisitor {
    type Value = Top<'de>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Top<'de>, A::Error> {
        let (mut event, mut session_id, mut transcript_path) = (None, None, None);
        while let Some(key) = map.next_key::<Val<'de>>()? {
            let slot = match &key {
                Val::Str(k) if k == "hook_event_name" => &mut event,
                Val::Str(k) if k == "session_id" => &mut session_id,
                Val::Str(k) if k == "transcript_path" => &mut transcript_path,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                    continue;
                }
            };
            *slot = Some(map.next_value::<Val<'de>>()?);
        }
        Ok(Top::Object {
            event,
            session_id,
            transcript_path,
        })
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Top<'de>, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Top::NotObject)
    }

    fn visit_borrowed_str<E: de::Error>(self, _: &'de str) -> Result<Top<'de>, E> {
        Ok(Top::NotObject)
    }

    fn visit_str<E: de::Error>(self, _: &str) -> Result<Top<'de>, E> {
        Ok(Top::NotObject)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Top<'de>, E> {
        Ok(Top::NotObject)
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Top<'de>, E> {
        Ok(Top::NotObject)
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Top<'de>, E> {
        Ok(Top::NotObject)
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Top<'de>, E> {
        Ok(Top::NotObject)
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Top<'de>, E> {
        Ok(Top::NotObject)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook<'a>(event: &'a str, session: &'a str, path: Option<&'a str>) -> Decoded<'a> {
        Decoded::Hook(HookHead {
            event: Cow::Borrowed(event),
            session_id: Cow::Borrowed(session),
            transcript_path: path.map(Cow::Borrowed),
        })
    }

    /// Bodies → `Decoded`.
    #[test]
    fn table() {
        let cases: &[(&[u8], Decoded<'_>)] = &[
            (
                br#"{"hook_event_name":"Stop","session_id":"s","x":[1,{"y":null}]}"#,
                hook("Stop", "s", None),
            ),
            (
                br#"{"hook_event_name":"SessionStart","session_id":"s","transcript_path":"/work/t.jsonl"}"#,
                hook("SessionStart", "s", Some("/work/t.jsonl")),
            ),
            (
                br#"{"hook_event_name":"SessionStart","session_id":"s","transcript_path":null}"#,
                hook("SessionStart", "s", None),
            ),
            (
                br#"{"hook_event_name":"SessionStart","session_id":"s"}"#,
                hook("SessionStart", "s", None),
            ),
            // Another event's transcript_path is not read.
            (
                br#"{"hook_event_name":"Stop","session_id":"s","transcript_path":5}"#,
                hook("Stop", "s", None),
            ),
            (
                br#"{"hook_event_name":"SessionStart","session_id":"s","transcript_path":5}"#,
                Decoded::SchemaMismatch,
            ),
            // The last of a repeated key counts.
            (
                br#"{"session_id":"a","hook_event_name":"Stop","session_id":"b"}"#,
                hook("Stop", "b", None),
            ),
            (br#"{"hook_event_name":"Stop"}"#, Decoded::SchemaMismatch),
            (
                br#"{"hook_event_name":"","session_id":"s"}"#,
                Decoded::SchemaMismatch,
            ),
            (
                br#"{"hook_event_name":"Stop","session_id":7}"#,
                Decoded::SchemaMismatch,
            ),
            (br#"{"x":1}"#, Decoded::SchemaMismatch),
            (b"[1,2]", Decoded::SchemaMismatch),
            (b"\"str\"", Decoded::SchemaMismatch),
            (b"null", Decoded::SchemaMismatch),
            (b"12", Decoded::SchemaMismatch),
            (b"not json", Decoded::Unparseable),
            (b"", Decoded::Unparseable),
            (br#"{"hook_event_name":"Stop","session_id":"s"} x"#, Decoded::Unparseable),
            (br#"{"hook_event_name":"Stop","session_id":"s""#, Decoded::Unparseable),
            (b"{\"a\":\"\xff\"}", Decoded::Unparseable),
            (b"\xff", Decoded::Unparseable),
            // A lone surrogate in a field it reads fails; in a skipped string it may pass
            // here, and the job's expansion refuses it (row 1).
            (
                br#"{"hook_event_name":"Stop","session_id":"\uD800"}"#,
                Decoded::Unparseable,
            ),
        ];
        for (body, want) in cases {
            assert_eq!(&decode(body), want, "{}", String::from_utf8_lossy(body));
        }
    }

    /// A string with an escape is owned and unescaped; one without is borrowed.
    #[test]
    fn escaped_strings_are_owned() {
        let body = br#"{"hook_event_name":"SessionStart","session_id":"s","transcript_path":"C:\\work\\t.jsonl"}"#;
        let Decoded::Hook(h) = decode(body) else {
            panic!("not a hook");
        };
        assert!(matches!(h.event, Cow::Borrowed(_)));
        assert!(matches!(h.transcript_path, Some(Cow::Owned(_))));
        assert_eq!(h.transcript_path.as_deref(), Some(r"C:\work\t.jsonl"));
    }
}
