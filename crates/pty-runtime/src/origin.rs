//! Who wrote a unit (`docs/design/pty-runtime.md` §2.3): the origin every write record
//! carries, its class for the `last_started` counters, and its `human_write` mapping.

use chaldea_core::ids::{ApprovalId, InputId};
use chaldea_core::payload::HumanWriteOrigin;

/// The origin of a write unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOrigin {
    /// The send path: text, pause, submit as one unit.
    Input { input_id: InputId },
    /// `chaldea attach`.
    Attach,
    /// Owner `key`, keypad, `surface_action send_key`.
    Key,
    /// A key from an open approval card.
    CardKey { approval_id: ApprovalId },
    /// The VT responder; never submitted by callers.
    VtReply { kind: ReplyKind },
}

/// The class a write counts under; `Attach`, `Key`, and `CardKey` are `Human`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OriginClass {
    Input,
    Human,
    Reply,
}

impl OriginClass {
    /// Every class, in counter order.
    pub const ALL: [OriginClass; 3] = [OriginClass::Input, OriginClass::Human, OriginClass::Reply];

    /// The index of this class in `WriteContext::last_started`.
    #[must_use]
    pub fn index(self) -> usize {
        match self {
            OriginClass::Input => 0,
            OriginClass::Human => 1,
            OriginClass::Reply => 2,
        }
    }

    fn bit(self) -> u8 {
        1 << self.index()
    }
}

/// A set of origin classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClassSet(u8);

impl ClassSet {
    /// Every class: the default `Basis::counted`.
    pub const ALL: ClassSet = ClassSet(0b111);
    /// No class.
    pub const NONE: ClassSet = ClassSet(0);

    #[must_use]
    pub fn of(classes: &[OriginClass]) -> ClassSet {
        ClassSet(classes.iter().fold(0, |acc, c| acc | c.bit()))
    }

    #[must_use]
    pub fn contains(self, class: OriginClass) -> bool {
        self.0 & class.bit() != 0
    }

    /// The classes in the set, in counter order.
    pub fn iter(self) -> impl Iterator<Item = OriginClass> {
        OriginClass::ALL
            .into_iter()
            .filter(move |c| self.contains(*c))
    }
}

/// What a VT reply answers (§2.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReplyKind {
    Da1,
    Da2,
    Dsr5,
    Dsr6,
    KittyKeyboard,
    ModeReport,
    TextAreaChars,
    TextAreaPixels,
    /// OSC 4 palette (0–255) or OSC 10 / 11 / 12 (alacritty's indexes 256–258).
    Color {
        index: usize,
    },
    /// A reply the emulator wrote that none of the patterns above matches. The pinned
    /// emulator emits none; it is kept so an unknown reply is still written and recorded.
    Other,
}

impl WriteOrigin {
    #[must_use]
    pub fn class(&self) -> OriginClass {
        match self {
            WriteOrigin::Input { .. } => OriginClass::Input,
            WriteOrigin::Attach | WriteOrigin::Key | WriteOrigin::CardKey { .. } => {
                OriginClass::Human
            }
            WriteOrigin::VtReply { .. } => OriginClass::Reply,
        }
    }

    /// `Attach` → `attach`, `Key` → `key`, `CardKey` → `card_key`; `Input` and `VtReply`
    /// → `None`.
    #[must_use]
    pub fn human_write(&self) -> Option<HumanWriteOrigin> {
        match self {
            WriteOrigin::Attach => Some(HumanWriteOrigin::Attach),
            WriteOrigin::Key => Some(HumanWriteOrigin::Key),
            WriteOrigin::CardKey { .. } => Some(HumanWriteOrigin::CardKey),
            WriteOrigin::Input { .. } | WriteOrigin::VtReply { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PR2-CL-22.
    #[test]
    fn maps_to_human_write_origins_and_classes() {
        let input = WriteOrigin::Input {
            input_id: InputId::from_u128(1),
        };
        let card = WriteOrigin::CardKey {
            approval_id: ApprovalId::from_u128(2),
        };
        let reply = WriteOrigin::VtReply {
            kind: ReplyKind::Dsr5,
        };
        let cases = [
            (&input, OriginClass::Input, None),
            (
                &WriteOrigin::Attach,
                OriginClass::Human,
                Some(HumanWriteOrigin::Attach),
            ),
            (
                &WriteOrigin::Key,
                OriginClass::Human,
                Some(HumanWriteOrigin::Key),
            ),
            (&card, OriginClass::Human, Some(HumanWriteOrigin::CardKey)),
            (&reply, OriginClass::Reply, None),
        ];
        for (origin, class, human) in cases {
            assert_eq!(origin.class(), class, "{origin:?}");
            assert_eq!(origin.human_write(), human, "{origin:?}");
        }
        let set = ClassSet::of(&[OriginClass::Human, OriginClass::Reply]);
        assert!(!set.contains(OriginClass::Input));
        assert!(set.contains(OriginClass::Human) && set.contains(OriginClass::Reply));
        assert_eq!(ClassSet::of(&OriginClass::ALL), ClassSet::ALL);
        assert_eq!(
            ClassSet::ALL.iter().collect::<Vec<_>>(),
            OriginClass::ALL.to_vec()
        );
    }
}
