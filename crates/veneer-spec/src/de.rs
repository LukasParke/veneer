//! A serde `Deserializer` over `&Node` whose errors carry a JSON Pointer.
//!
//! Needed because suspect enables `serde_json/arbitrary_precision`, which breaks
//! untagged enums deserialized through `serde_json::Value`.

use std::fmt;

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};

use crate::value::{Kind, Node, escape_pointer_token};

#[derive(Debug, Clone, PartialEq)]
pub struct DeError {
    /// Where the error happened; `None` only before annotation.
    pub pointer: Option<String>,
    pub message: String,
}

impl fmt::Display for DeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DeError {}

impl de::Error for DeError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self {
            pointer: None,
            message: msg.to_string(),
        }
    }
}

/// Annotates an error with `pointer` unless a deeper location already did.
fn at(mut err: DeError, pointer: &str) -> DeError {
    if err.pointer.is_none() {
        err.pointer = Some(pointer.to_owned());
    }
    err
}

pub fn from_node<T: de::DeserializeOwned>(node: &Node) -> Result<T, DeError> {
    T::deserialize(NodeDe {
        node,
        pointer: String::new(),
    })
    .map_err(|e| at(e, ""))
}

struct NodeDe<'a> {
    node: &'a Node,
    pointer: String,
}

impl<'de> de::Deserializer<'de> for NodeDe<'_> {
    type Error = DeError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        let pointer = self.pointer.clone();
        let result = match &self.node.kind {
            Kind::Null => visitor.visit_unit(),
            Kind::Bool(b) => visitor.visit_bool(*b),
            Kind::Int(i) => visitor.visit_i64(*i),
            Kind::Float(f) => visitor.visit_f64(*f),
            Kind::Str(s) => visitor.visit_str(s),
            Kind::Array(items) => visitor.visit_seq(Seq {
                items: items.iter().enumerate(),
                pointer: self.pointer,
            }),
            Kind::Object(map) => visitor.visit_map(Map {
                iter: map.iter(),
                value: None,
                pointer: self.pointer,
            }),
        };
        result.map_err(|e| at(e, &pointer))
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        if self.node.is_null() {
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DeError> {
        match &self.node.kind {
            Kind::Str(s) => visitor
                .visit_enum(de::value::StrDeserializer::<DeError>::new(s))
                .map_err(|e| at(e, &self.pointer)),
            _ => Err(at(de::Error::custom("expected a string"), &self.pointer)),
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        visitor.visit_newtype_struct(self)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf unit unit_struct seq tuple tuple_struct map struct
        identifier ignored_any
    }
}

struct Seq<'a> {
    items: std::iter::Enumerate<std::slice::Iter<'a, Node>>,
    pointer: String,
}

impl<'de> SeqAccess<'de> for Seq<'_> {
    type Error = DeError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, DeError> {
        let Some((i, node)) = self.items.next() else {
            return Ok(None);
        };
        let pointer = format!("{}/{i}", self.pointer);
        seed.deserialize(NodeDe {
            node,
            pointer: pointer.clone(),
        })
        .map(Some)
        .map_err(|e| at(e, &pointer))
    }
}

struct Map<'a> {
    iter: indexmap::map::Iter<'a, String, Node>,
    value: Option<(&'a Node, String)>,
    pointer: String,
}

impl<'de> MapAccess<'de> for Map<'_> {
    type Error = DeError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, DeError> {
        let Some((key, node)) = self.iter.next() else {
            return Ok(None);
        };
        let pointer = format!("{}/{}", self.pointer, escape_pointer_token(key));
        self.value = Some((node, pointer.clone()));
        seed.deserialize(de::value::StrDeserializer::<DeError>::new(key))
            .map(Some)
            .map_err(|e| at(e, &pointer))
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, DeError> {
        let (node, pointer) = self
            .value
            .take()
            .ok_or_else(|| de::Error::custom("map value requested before its key"))?;
        seed.deserialize(NodeDe {
            node,
            pointer: pointer.clone(),
        })
        .map_err(|e| at(e, &pointer))
    }
}
