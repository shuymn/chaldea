//! Chaldea's Sans-IO core: the public wire types and the pure rules every other crate
//! applies (`docs/mvp-design.md` §2.1–§2.5, §5).
//!
//! No async runtime, filesystem, network, process, or clock access: `crates/core/clippy.toml`
//! disallows them, and CI checks the dependency tree. Ids and times come in from the caller.
//!
//! Generated TypeScript and JSON Schema for the public types live under
//! `packages/contracts/`; `tests/contracts_export.rs` keeps them equal to these types.

use std::fmt;

pub mod adapter;
pub mod auth;
pub mod canonical;
pub mod command;
pub mod event;
pub mod http;
pub mod ids;
pub mod input;
pub mod launch;
pub mod payload;
pub mod state;
pub mod sync;

/// The version of the whole event schema set. Additive changes (an optional field, a new
/// kind) keep it; a change of meaning or type of an existing field bumps it, except before
/// any producer has persisted events, as for the `vt_snapshot_taken` grid encoding (§2.1).
pub const SCHEMA_VERSION: u32 = 1;

/// The largest integer every wire reader keeps exactly: 2^53 - 1, JavaScript's
/// `Number.MAX_SAFE_INTEGER`. Every `u64` on the wire (sequence numbers, offsets, sizes)
/// is decoded and serialized only up to it (either direction errors above it), and the
/// generated JSON Schema states the same `maximum`, so the TypeScript `number` type never
/// loses precision and no writer emits a value its readers refuse.
pub const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// `serialize_with` / `deserialize_with` helpers that bound wire integers to
/// [`MAX_SAFE_INTEGER`].
pub(crate) mod wire {
    use serde::de::{self, Deserialize, Deserializer, Unexpected};
    use serde::ser::{self, Serialize, Serializer};

    use crate::MAX_SAFE_INTEGER;

    const EXPECTED: &str = "an integer no greater than 2^53 - 1";

    fn check<E: de::Error>(v: u64) -> Result<u64, E> {
        if v > MAX_SAFE_INTEGER {
            return Err(E::invalid_value(Unexpected::Unsigned(v), &EXPECTED));
        }
        Ok(v)
    }

    fn check_out<E: ser::Error>(v: u64) -> Result<u64, E> {
        if v > MAX_SAFE_INTEGER {
            return Err(E::custom(format_args!("{v} is not {EXPECTED}")));
        }
        Ok(v)
    }

    pub(crate) fn safe_u64<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        check(u64::deserialize(d)?)
    }

    pub(crate) fn opt_safe_u64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
        Option::<u64>::deserialize(d)?.map(check).transpose()
    }

    /// An optional member that, when present, holds a value: `null` is refused (the member is
    /// omitted when absent, and `#[serde(default)]` gives `None` then).
    pub(crate) fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
        d: D,
    ) -> Result<Option<T>, D::Error> {
        T::deserialize(d).map(Some)
    }

    /// A struct that must be a JSON object: serde would read a struct from an array too,
    /// which the schema refuses. `T` is the struct's field shape.
    pub(crate) fn object<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
        d: D,
    ) -> Result<T, D::Error> {
        struct Object<T>(std::marker::PhantomData<T>);
        impl<'de, T: Deserialize<'de>> de::Visitor<'de> for Object<T> {
            type Value = T;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: de::MapAccess<'de>>(self, map: A) -> Result<T, A::Error> {
                T::deserialize(de::value::MapAccessDeserializer::new(map))
            }
        }
        d.deserialize_map(Object(std::marker::PhantomData))
    }

    #[allow(
        clippy::trivially_copy_pass_by_ref,
        reason = "serialize_with passes a reference"
    )]
    pub(crate) fn ser_safe_u64<S: Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
        check_out(*v)?.serialize(s)
    }

    #[allow(clippy::ref_option, reason = "serialize_with passes a reference")]
    pub(crate) fn ser_opt_safe_u64<S: Serializer>(
        v: &Option<u64>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        v.map(check_out).transpose()?.serialize(s)
    }
}

/// An input that does not apply to the current state of a state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidTransition {
    /// The input's name.
    pub input: &'static str,
    /// The rule it broke.
    pub rule: &'static str,
}

impl fmt::Display for InvalidTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid transition on {}: {}", self.input, self.rule)
    }
}

impl std::error::Error for InvalidTransition {}
