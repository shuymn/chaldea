//! The event annex: internal values projections need that no public payload carries
//! (`docs/design/event-store.md` §2.6). Stored in `events.annex`, immutable with the event,
//! never served.

use std::fmt;

use chaldea_core::ids::LaunchId;
use serde::de::{self, SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The persisted JSON is serde's externally tagged form:
/// `{"readiness":{"launch_id":…,"position":42,"counted":["input","human","reply"]}}` or
/// `{"transcript":"<continuity>"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum EventAnnex {
    /// Required on `readiness_observed`.
    Readiness(WriteBasis),
    /// On every transcript-sourced event.
    Transcript(Continuity),
}

/// The PTY writer's basis captured at the readiness evidence's intake: `position` is
/// `last_started(counted)` of `pty_runtime::writer`, over the origin classes `counted`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteBasis {
    pub launch_id: LaunchId,
    pub position: u64,
    pub counted: CountedClasses,
}

/// Transcript continuity evidence, opaque to the store (PR 13 / 14 define it).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Continuity(pub String);

/// One origin class the writer basis counts; mirrors `pty_runtime::origin::OriginClass`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CountedClass {
    Input,
    Human,
    Reply,
}

impl CountedClass {
    const ALL: [CountedClass; 3] = [
        CountedClass::Input,
        CountedClass::Human,
        CountedClass::Reply,
    ];

    fn bit(self) -> u8 {
        match self {
            CountedClass::Input => 1,
            CountedClass::Human => 2,
            CountedClass::Reply => 4,
        }
    }

    fn name(self) -> &'static str {
        match self {
            CountedClass::Input => "input",
            CountedClass::Human => "human",
            CountedClass::Reply => "reply",
        }
    }
}

/// A set of [`CountedClass`], serialized as a JSON array in the fixed order input, human,
/// reply, without repeats; deserializing any other array (unknown class, other order,
/// repeat) fails, so a set has one spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CountedClasses(u8);

impl CountedClasses {
    /// Every class: the PTY writer's default basis.
    pub const ALL: CountedClasses = CountedClasses(7);

    #[must_use]
    pub fn of(classes: &[CountedClass]) -> Self {
        CountedClasses(classes.iter().fold(0, |acc, c| acc | c.bit()))
    }

    #[must_use]
    pub fn contains(self, c: CountedClass) -> bool {
        self.0 & c.bit() != 0
    }

    pub fn iter(self) -> impl Iterator<Item = CountedClass> {
        CountedClass::ALL
            .into_iter()
            .filter(move |c| self.contains(*c))
    }
}

impl Serialize for CountedClasses {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(None)?;
        for c in self.iter() {
            seq.serialize_element(c.name())?;
        }
        seq.end()
    }
}

impl<'de> Deserialize<'de> for CountedClasses {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = CountedClasses;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an array of input, human, reply in that order without repeats")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut set = 0u8;
                let mut last = 0u8;
                while let Some(name) = seq.next_element::<String>()? {
                    let class = CountedClass::ALL
                        .into_iter()
                        .find(|c| c.name() == name)
                        .ok_or_else(|| de::Error::custom(format!("unknown class {name}")))?;
                    if class.bit() <= last {
                        return Err(de::Error::custom("classes out of order or repeated"));
                    }
                    last = class.bit();
                    set |= class.bit();
                }
                Ok(CountedClasses(set))
            }
        }
        d.deserialize_seq(V)
    }
}

/// An annex that does not have the form its event requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnnexError {
    /// The event's kind or source requires an annex it lacks (`readiness_observed`, or a
    /// transcript-sourced event).
    Missing(&'static str),
    /// An annex of a form the event does not take.
    Unexpected(&'static str),
    /// The write basis names another launch than the event's.
    LaunchMismatch,
    /// The position is above SQLite's integer range.
    PositionOutOfRange,
}

impl fmt::Display for AnnexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AnnexError::Missing(what) => write!(f, "missing {what} annex"),
            AnnexError::Unexpected(what) => write!(f, "unexpected annex on {what}"),
            AnnexError::LaunchMismatch => f.write_str("the write basis names another launch"),
            AnnexError::PositionOutOfRange => f.write_str("the write basis position is too large"),
        }
    }
}

impl std::error::Error for AnnexError {}
