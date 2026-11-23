//! Canonical JSON for command request identity (`docs/design/http-surface.md` §2.8
//! "Request identity").
//!
//! [`CanonicalJson::of`] serializes any `Serialize` value directly, without going through
//! `serde_json::Value`, and writes every object's members sorted by the UTF-8 bytes of their
//! keys, at every level, with no whitespace. The order is decided here rather than taken
//! from the order a type or a map serializes its members in: a struct's declaration order,
//! a `HashMap`'s hasher, and a `serde_json::Map` built with the `preserve_order` feature
//! would each give another order. Scalars are written as `serde_json` writes them, so a
//! float keeps its `serde_json` spelling (`1.0`, with `-0.0` written as `0.0`) and strings
//! use its escapes. Raw JSON text (`RawValue`) is refused: its members would keep their
//! sender's order.

use std::fmt;

use serde::ser::{self, Serialize};

/// The canonical JSON text of one value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CanonicalJson(String);

impl CanonicalJson {
    /// The canonical text of `value`.
    ///
    /// # Errors
    ///
    /// When `value` has a non-finite float, a map key that is not a string, integer, `bool`,
    /// or unit variant, an object with two members of one key, raw JSON text (a
    /// `serde_json::value::RawValue`, which the caller converts to a typed value first), or
    /// its `Serialize` fails.
    pub fn of<T: Serialize + ?Sized>(value: &T) -> Result<CanonicalJson, CanonicalError> {
        let node = value.serialize(NodeSerializer)?;
        let mut out = String::new();
        write(&node, &mut out)?;
        Ok(CanonicalJson(out))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a value has no canonical form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalError(String);

impl fmt::Display for CanonicalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CanonicalError {}

impl ser::Error for CanonicalError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        CanonicalError(msg.to_string())
    }
}

/// A serialized value with its members in serialization order (sorted only when written).
enum Node {
    Null,
    Bool(bool),
    /// The number as `serde_json` writes it.
    Number(String),
    String(String),
    Array(Vec<Node>),
    Object(Vec<(String, Node)>),
}

fn write(node: &Node, out: &mut String) -> Result<(), CanonicalError> {
    match node {
        Node::Null => out.push_str("null"),
        Node::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Node::Number(n) => out.push_str(n),
        Node::String(s) => write_str(s, out)?,
        Node::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(item, out)?;
            }
            out.push(']');
        }
        Node::Object(members) => {
            // The explicit order: keys compared as UTF-8 bytes.
            let mut sorted: Vec<&(String, Node)> = members.iter().collect();
            sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            out.push('{');
            for (i, (key, value)) in sorted.iter().enumerate() {
                if i > 0 {
                    if sorted[i - 1].0 == *key {
                        return Err(CanonicalError(format!("duplicate member {key:?}")));
                    }
                    out.push(',');
                }
                write_str(key, out)?;
                out.push(':');
                write(value, out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

fn write_str(s: &str, out: &mut String) -> Result<(), CanonicalError> {
    out.push_str(&serde_json::to_string(s).map_err(|e| CanonicalError(e.to_string()))?);
    Ok(())
}

fn number(n: &impl ToString) -> Result<Node, CanonicalError> {
    Ok(Node::Number(n.to_string()))
}

/// A float as `serde_json` writes it; `-0.0` is written as `0.0`, the value JSON gives both.
fn float(f: f64) -> Result<Node, CanonicalError> {
    if !f.is_finite() {
        return Err(CanonicalError(format!("{f} is not a JSON number")));
    }
    let f = if f == 0.0 { 0.0 } else { f };
    let s = serde_json::to_string(&f).map_err(|e| CanonicalError(e.to_string()))?;
    Ok(Node::Number(s))
}

/// The struct names `serde_json` uses to pass raw JSON text through a serializer
/// (`RawValue`, and `Number` with `arbitrary_precision`): text this module cannot order.
const RAW_PREFIX: &str = "$serde_json::private::";

/// Serializes into a [`Node`], with `serde_json`'s data model (enums externally tagged).
struct NodeSerializer;

impl ser::Serializer for NodeSerializer {
    type Ok = Node;
    type Error = CanonicalError;
    type SerializeSeq = SeqNode;
    type SerializeTuple = SeqNode;
    type SerializeTupleStruct = SeqNode;
    type SerializeTupleVariant = VariantSeqNode;
    type SerializeMap = MapNode;
    type SerializeStruct = MapNode;
    type SerializeStructVariant = VariantMapNode;

    fn serialize_bool(self, v: bool) -> Result<Node, CanonicalError> {
        Ok(Node::Bool(v))
    }
    fn serialize_i8(self, v: i8) -> Result<Node, CanonicalError> {
        number(&v)
    }
    fn serialize_i16(self, v: i16) -> Result<Node, CanonicalError> {
        number(&v)
    }
    fn serialize_i32(self, v: i32) -> Result<Node, CanonicalError> {
        number(&v)
    }
    fn serialize_i64(self, v: i64) -> Result<Node, CanonicalError> {
        number(&v)
    }
    fn serialize_i128(self, v: i128) -> Result<Node, CanonicalError> {
        number(&v)
    }
    fn serialize_u8(self, v: u8) -> Result<Node, CanonicalError> {
        number(&v)
    }
    fn serialize_u16(self, v: u16) -> Result<Node, CanonicalError> {
        number(&v)
    }
    fn serialize_u32(self, v: u32) -> Result<Node, CanonicalError> {
        number(&v)
    }
    fn serialize_u64(self, v: u64) -> Result<Node, CanonicalError> {
        number(&v)
    }
    fn serialize_u128(self, v: u128) -> Result<Node, CanonicalError> {
        number(&v)
    }
    fn serialize_f32(self, v: f32) -> Result<Node, CanonicalError> {
        if !v.is_finite() {
            return Err(CanonicalError(format!("{v} is not a JSON number")));
        }
        let v = if v == 0.0 { 0.0 } else { v };
        let s = serde_json::to_string(&v).map_err(|e| CanonicalError(e.to_string()))?;
        Ok(Node::Number(s))
    }
    fn serialize_f64(self, v: f64) -> Result<Node, CanonicalError> {
        float(v)
    }
    fn serialize_char(self, v: char) -> Result<Node, CanonicalError> {
        Ok(Node::String(v.to_string()))
    }
    fn serialize_str(self, v: &str) -> Result<Node, CanonicalError> {
        Ok(Node::String(v.to_owned()))
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<Node, CanonicalError> {
        Ok(Node::Array(
            v.iter().map(|b| Node::Number(b.to_string())).collect(),
        ))
    }
    fn serialize_none(self) -> Result<Node, CanonicalError> {
        Ok(Node::Null)
    }
    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<Node, CanonicalError> {
        value.serialize(self)
    }
    fn serialize_unit(self) -> Result<Node, CanonicalError> {
        Ok(Node::Null)
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<Node, CanonicalError> {
        Ok(Node::Null)
    }
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<Node, CanonicalError> {
        Ok(Node::String(variant.to_owned()))
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Node, CanonicalError> {
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Node, CanonicalError> {
        Ok(Node::Object(vec![(
            variant.to_owned(),
            value.serialize(NodeSerializer)?,
        )]))
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<SeqNode, CanonicalError> {
        Ok(SeqNode(Vec::with_capacity(len.unwrap_or(0))))
    }
    fn serialize_tuple(self, len: usize) -> Result<SeqNode, CanonicalError> {
        Ok(SeqNode(Vec::with_capacity(len)))
    }
    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<SeqNode, CanonicalError> {
        Ok(SeqNode(Vec::with_capacity(len)))
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<VariantSeqNode, CanonicalError> {
        Ok(VariantSeqNode(variant, Vec::with_capacity(len)))
    }
    fn serialize_map(self, len: Option<usize>) -> Result<MapNode, CanonicalError> {
        Ok(MapNode {
            members: Vec::with_capacity(len.unwrap_or(0)),
            key: None,
        })
    }
    fn serialize_struct(self, name: &'static str, len: usize) -> Result<MapNode, CanonicalError> {
        if name.starts_with(RAW_PREFIX) {
            return Err(CanonicalError(
                "raw JSON text has no canonical form; convert it to a typed value".into(),
            ));
        }
        Ok(MapNode {
            members: Vec::with_capacity(len),
            key: None,
        })
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<VariantMapNode, CanonicalError> {
        Ok(VariantMapNode(variant, Vec::with_capacity(len)))
    }
}

struct SeqNode(Vec<Node>);

impl ser::SerializeSeq for SeqNode {
    type Ok = Node;
    type Error = CanonicalError;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), CanonicalError> {
        self.0.push(v.serialize(NodeSerializer)?);
        Ok(())
    }
    fn end(self) -> Result<Node, CanonicalError> {
        Ok(Node::Array(self.0))
    }
}

impl ser::SerializeTuple for SeqNode {
    type Ok = Node;
    type Error = CanonicalError;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), CanonicalError> {
        ser::SerializeSeq::serialize_element(self, v)
    }
    fn end(self) -> Result<Node, CanonicalError> {
        ser::SerializeSeq::end(self)
    }
}

impl ser::SerializeTupleStruct for SeqNode {
    type Ok = Node;
    type Error = CanonicalError;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), CanonicalError> {
        ser::SerializeSeq::serialize_element(self, v)
    }
    fn end(self) -> Result<Node, CanonicalError> {
        ser::SerializeSeq::end(self)
    }
}

struct VariantSeqNode(&'static str, Vec<Node>);

impl ser::SerializeTupleVariant for VariantSeqNode {
    type Ok = Node;
    type Error = CanonicalError;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), CanonicalError> {
        self.1.push(v.serialize(NodeSerializer)?);
        Ok(())
    }
    fn end(self) -> Result<Node, CanonicalError> {
        Ok(Node::Object(vec![(self.0.to_owned(), Node::Array(self.1))]))
    }
}

struct MapNode {
    members: Vec<(String, Node)>,
    key: Option<String>,
}

impl ser::SerializeMap for MapNode {
    type Ok = Node;
    type Error = CanonicalError;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), CanonicalError> {
        self.key = Some(key.serialize(KeySerializer)?);
        Ok(())
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), CanonicalError> {
        let key = self
            .key
            .take()
            .ok_or_else(|| CanonicalError("a map value without a key".into()))?;
        self.members.push((key, v.serialize(NodeSerializer)?));
        Ok(())
    }
    fn end(self) -> Result<Node, CanonicalError> {
        Ok(Node::Object(self.members))
    }
}

impl ser::SerializeStruct for MapNode {
    type Ok = Node;
    type Error = CanonicalError;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        v: &T,
    ) -> Result<(), CanonicalError> {
        self.members
            .push((key.to_owned(), v.serialize(NodeSerializer)?));
        Ok(())
    }
    fn end(self) -> Result<Node, CanonicalError> {
        Ok(Node::Object(self.members))
    }
}

struct VariantMapNode(&'static str, Vec<(String, Node)>);

impl ser::SerializeStructVariant for VariantMapNode {
    type Ok = Node;
    type Error = CanonicalError;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        v: &T,
    ) -> Result<(), CanonicalError> {
        self.1.push((key.to_owned(), v.serialize(NodeSerializer)?));
        Ok(())
    }
    fn end(self) -> Result<Node, CanonicalError> {
        Ok(Node::Object(vec![(
            self.0.to_owned(),
            Node::Object(self.1),
        )]))
    }
}

/// Map keys, as `serde_json` accepts them: strings, and integers, `bool`s, and unit
/// variants written as strings.
struct KeySerializer;

fn key_error() -> CanonicalError {
    CanonicalError("a map key must be a string, an integer, a bool, or a unit variant".into())
}

impl ser::Serializer for KeySerializer {
    type Ok = String;
    type Error = CanonicalError;
    type SerializeSeq = ser::Impossible<String, CanonicalError>;
    type SerializeTuple = ser::Impossible<String, CanonicalError>;
    type SerializeTupleStruct = ser::Impossible<String, CanonicalError>;
    type SerializeTupleVariant = ser::Impossible<String, CanonicalError>;
    type SerializeMap = ser::Impossible<String, CanonicalError>;
    type SerializeStruct = ser::Impossible<String, CanonicalError>;
    type SerializeStructVariant = ser::Impossible<String, CanonicalError>;

    fn serialize_bool(self, v: bool) -> Result<String, CanonicalError> {
        Ok(v.to_string())
    }
    fn serialize_i8(self, v: i8) -> Result<String, CanonicalError> {
        Ok(v.to_string())
    }
    fn serialize_i16(self, v: i16) -> Result<String, CanonicalError> {
        Ok(v.to_string())
    }
    fn serialize_i32(self, v: i32) -> Result<String, CanonicalError> {
        Ok(v.to_string())
    }
    fn serialize_i64(self, v: i64) -> Result<String, CanonicalError> {
        Ok(v.to_string())
    }
    fn serialize_i128(self, v: i128) -> Result<String, CanonicalError> {
        Ok(v.to_string())
    }
    fn serialize_u8(self, v: u8) -> Result<String, CanonicalError> {
        Ok(v.to_string())
    }
    fn serialize_u16(self, v: u16) -> Result<String, CanonicalError> {
        Ok(v.to_string())
    }
    fn serialize_u32(self, v: u32) -> Result<String, CanonicalError> {
        Ok(v.to_string())
    }
    fn serialize_u64(self, v: u64) -> Result<String, CanonicalError> {
        Ok(v.to_string())
    }
    fn serialize_u128(self, v: u128) -> Result<String, CanonicalError> {
        Ok(v.to_string())
    }
    fn serialize_f32(self, _v: f32) -> Result<String, CanonicalError> {
        Err(key_error())
    }
    fn serialize_f64(self, _v: f64) -> Result<String, CanonicalError> {
        Err(key_error())
    }
    fn serialize_char(self, v: char) -> Result<String, CanonicalError> {
        Ok(v.to_string())
    }
    fn serialize_str(self, v: &str) -> Result<String, CanonicalError> {
        Ok(v.to_owned())
    }
    fn serialize_bytes(self, _v: &[u8]) -> Result<String, CanonicalError> {
        Err(key_error())
    }
    fn serialize_none(self) -> Result<String, CanonicalError> {
        Err(key_error())
    }
    fn serialize_some<T: Serialize + ?Sized>(self, _v: &T) -> Result<String, CanonicalError> {
        Err(key_error())
    }
    fn serialize_unit(self) -> Result<String, CanonicalError> {
        Err(key_error())
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<String, CanonicalError> {
        Err(key_error())
    }
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<String, CanonicalError> {
        Ok(variant.to_owned())
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<String, CanonicalError> {
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<String, CanonicalError> {
        Err(key_error())
    }
    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, CanonicalError> {
        Err(key_error())
    }
    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, CanonicalError> {
        Err(key_error())
    }
    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, CanonicalError> {
        Err(key_error())
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, CanonicalError> {
        Err(key_error())
    }
    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, CanonicalError> {
        Err(key_error())
    }
    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, CanonicalError> {
        Err(key_error())
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, CanonicalError> {
        Err(key_error())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use serde::Serialize;

    use super::*;

    fn canon<T: Serialize + ?Sized>(v: &T) -> String {
        CanonicalJson::of(v).unwrap().as_str().to_owned()
    }

    #[derive(Serialize)]
    struct Declared {
        zeta: u8,
        alpha: Inner,
        #[serde(rename = "Mid")]
        mid: Vec<Inner>,
    }

    #[derive(Serialize)]
    struct Inner {
        y: bool,
        x: Option<f64>,
    }

    #[test]
    fn members_are_sorted_at_every_level_without_whitespace() {
        let v = Declared {
            zeta: 1,
            alpha: Inner {
                y: true,
                x: Some(1.0),
            },
            mid: vec![Inner { y: false, x: None }],
        };
        assert_eq!(
            canon(&v),
            r#"{"Mid":[{"x":null,"y":false}],"alpha":{"x":1.0,"y":true},"zeta":1}"#
        );
    }

    #[test]
    fn keys_sort_by_utf8_bytes() {
        // 'Z' (5A) < 'a' (61) < 'z' (7A) < 'é' (C3 A9) < 'ｚ' (EF BD 9A) < '😀' (F0 9F 98 80);
        // UTF-16 code units would put '😀' (D83D) before 'ｚ' (FF5A).
        let mut m = HashMap::new();
        for k in ["😀", "é", "ｚ", "z", "a", "Z"] {
            m.insert(k, 0);
        }
        assert_eq!(canon(&m), r#"{"Z":0,"a":0,"z":0,"é":0,"ｚ":0,"😀":0}"#);
    }

    #[test]
    fn equal_maps_in_any_order_are_equal() {
        let mut a = HashMap::new();
        let mut b = HashMap::new();
        for i in 0..64 {
            a.insert(format!("k{i}"), i);
        }
        for i in (0..64).rev() {
            b.insert(format!("k{i}"), i);
        }
        let sorted: BTreeMap<_, _> = a.iter().collect();
        assert_eq!(canon(&a), canon(&b));
        assert_eq!(canon(&a), serde_json::to_string(&sorted).unwrap());
    }

    #[test]
    fn scalars_are_serde_json_spellings() {
        assert_eq!(canon(&1.0_f64), "1.0");
        assert_eq!(canon(&-0.5_f64), "-0.5");
        assert_eq!(canon(&u64::MAX), "18446744073709551615");
        assert_eq!(
            canon("a\"\\\n\u{1}é"),
            serde_json::to_string("a\"\\\n\u{1}é").unwrap()
        );
        assert!(CanonicalJson::of(&f64::NAN).is_err());
        assert!(CanonicalJson::of(&f64::INFINITY).is_err());
    }

    #[test]
    fn enums_follow_serde_json() {
        #[derive(Serialize)]
        enum E {
            Unit,
            New(u8),
            Tup(u8, u8),
            Rec { b: u8, a: u8 },
        }
        assert_eq!(canon(&E::Unit), r#""Unit""#);
        assert_eq!(canon(&E::New(1)), r#"{"New":1}"#);
        assert_eq!(canon(&E::Tup(1, 2)), r#"{"Tup":[1,2]}"#);
        assert_eq!(canon(&E::Rec { b: 1, a: 2 }), r#"{"Rec":{"a":2,"b":1}}"#);
    }

    #[test]
    fn duplicate_and_bad_keys_are_refused() {
        struct Dup;
        impl Serialize for Dup {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                use serde::ser::SerializeMap;
                let mut m = s.serialize_map(Some(2))?;
                m.serialize_entry("a", &1)?;
                m.serialize_entry("a", &2)?;
                m.end()
            }
        }
        assert!(CanonicalJson::of(&Dup).is_err());
        let mut by_int = BTreeMap::new();
        by_int.insert(10_u32, true);
        assert_eq!(canon(&by_int), r#"{"10":true}"#);
        let by_float = vec![(1.5_f64, 1)];
        struct FloatKeys(Vec<(f64, i32)>);
        impl Serialize for FloatKeys {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.collect_map(self.0.iter().map(|(k, v)| (k, v)))
            }
        }
        assert!(CanonicalJson::of(&FloatKeys(by_float)).is_err());
    }

    #[test]
    fn negative_zero_is_zero() {
        assert_eq!(canon(&-0.0_f64), "0.0");
        assert_eq!(canon(&-0.0_f32), "0.0");
        let parsed: f64 = serde_json::from_str("-0.0").unwrap();
        assert_eq!(canon(&parsed), canon(&0.0_f64));
    }

    /// A `RawValue` carries JSON text that the canonical form cannot order: refused, at the
    /// top and inside a typed value.
    #[test]
    fn raw_value_is_refused() {
        #[derive(Serialize)]
        struct Args {
            n: u8,
            inner: Box<serde_json::value::RawValue>,
        }
        let raw = |s: &str| serde_json::value::RawValue::from_string(s.to_owned()).unwrap();
        assert!(CanonicalJson::of(raw(r#"{"b":2,"a":1}"#).as_ref()).is_err());
        let typed = Args {
            n: 1,
            inner: raw(r#"{"b":2,"a":1}"#),
        };
        assert!(CanonicalJson::of(&typed).is_err());
    }
}
