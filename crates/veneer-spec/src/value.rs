//! An owned, ordered value tree whose nodes remember where they came from.

use std::collections::HashMap;

use indexmap::IndexMap;

use crate::Span;

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Array(Vec<Node>),
    Object(IndexMap<String, Node>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub kind: Kind,
    pub span: Option<Span>,
}

/// RFC 6901 token escaping: `~` -> `~0`, `/` -> `~1`.
pub fn escape_pointer_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

/// Splits an RFC 6901 pointer into unescaped tokens. `""` is the root (no tokens).
pub fn split_pointer(pointer: &str) -> Vec<String> {
    if pointer.is_empty() {
        return Vec::new();
    }
    pointer
        .strip_prefix('/')
        .unwrap_or(pointer)
        .split('/')
        .map(|t| t.replace("~1", "/").replace("~0", "~"))
        .collect()
}

fn walk_spans(node: &Node, ptr: &mut String, out: &mut HashMap<String, Span>) {
    if let Some(span) = &node.span {
        out.insert(ptr.clone(), span.clone());
    }
    let len = ptr.len();
    match &node.kind {
        Kind::Array(a) => {
            for (i, child) in a.iter().enumerate() {
                ptr.push('/');
                ptr.push_str(&i.to_string());
                walk_spans(child, ptr, out);
                ptr.truncate(len);
            }
        }
        Kind::Object(m) => {
            for (k, child) in m {
                ptr.push('/');
                ptr.push_str(&escape_pointer_token(k));
                walk_spans(child, ptr, out);
                ptr.truncate(len);
            }
        }
        _ => {}
    }
}

fn walk_reattach(node: &mut Node, ptr: &mut String, index: &HashMap<String, Span>) {
    if node.span.is_none() {
        node.span = index.get(ptr.as_str()).cloned();
    }
    let len = ptr.len();
    match &mut node.kind {
        Kind::Array(a) => {
            for (i, child) in a.iter_mut().enumerate() {
                ptr.push('/');
                ptr.push_str(&i.to_string());
                walk_reattach(child, ptr, index);
                ptr.truncate(len);
            }
        }
        Kind::Object(m) => {
            for (k, child) in m.iter_mut() {
                ptr.push('/');
                ptr.push_str(&escape_pointer_token(k));
                walk_reattach(child, ptr, index);
                ptr.truncate(len);
            }
        }
        _ => {}
    }
}

impl Node {
    pub fn new(kind: Kind) -> Self {
        Self { kind, span: None }
    }

    pub fn with_span(kind: Kind, span: Option<Span>) -> Self {
        Self { kind, span }
    }

    pub fn object() -> Self {
        Self::new(Kind::Object(IndexMap::new()))
    }

    pub fn is_null(&self) -> bool {
        matches!(self.kind, Kind::Null)
    }

    pub fn as_str(&self) -> Option<&str> {
        match &self.kind {
            Kind::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&IndexMap<String, Node>> {
        match &self.kind {
            Kind::Object(m) => Some(m),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Node> {
        self.as_object().and_then(|m| m.get(key))
    }

    fn child(&self, token: &str) -> Option<&Node> {
        match &self.kind {
            Kind::Object(m) => m.get(token),
            Kind::Array(a) => token.parse::<usize>().ok().and_then(|i| a.get(i)),
            _ => None,
        }
    }

    pub fn pointer(&self, pointer: &str) -> Option<&Node> {
        split_pointer(pointer).iter().try_fold(self, |node, tok| node.child(tok))
    }

    /// Span of the deepest node that exists along `pointer` (and has a span).
    pub fn nearest_span(&self, pointer: &str) -> Option<Span> {
        let mut current = self;
        let mut best = self.span.clone();
        for tok in split_pointer(pointer) {
            match current.child(&tok) {
                Some(next) => {
                    current = next;
                    if next.span.is_some() {
                        best = next.span.clone();
                    }
                }
                None => break,
            }
        }
        best
    }

    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::Value;
        match &self.kind {
            Kind::Null => Value::Null,
            Kind::Bool(b) => Value::Bool(*b),
            Kind::Int(i) => Value::from(*i),
            Kind::Float(f) => serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number),
            Kind::Str(s) => Value::String(s.clone()),
            Kind::Array(a) => Value::Array(a.iter().map(Node::to_json).collect()),
            Kind::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), v.to_json())).collect()),
        }
    }

    pub fn from_json(value: &serde_json::Value) -> Self {
        use serde_json::Value;
        let kind = match value {
            Value::Null => Kind::Null,
            Value::Bool(b) => Kind::Bool(*b),
            Value::Number(n) => match n.as_i64() {
                Some(i) => Kind::Int(i),
                None => Kind::Float(n.as_f64().unwrap_or(0.0)),
            },
            Value::String(s) => Kind::Str(s.clone()),
            Value::Array(a) => Kind::Array(a.iter().map(Node::from_json).collect()),
            Value::Object(m) => Kind::Object(m.iter().map(|(k, v)| (k.clone(), Node::from_json(v))).collect()),
        };
        Self::new(kind)
    }

    /// Map of JSON Pointer -> span for every node that has one.
    pub fn span_index(&self) -> HashMap<String, Span> {
        let mut out = HashMap::new();
        walk_spans(self, &mut String::new(), &mut out);
        out
    }

    /// Fills `span` on every node that lacks one, from `index` (by pointer).
    pub fn reattach_spans(&mut self, index: &HashMap<String, Span>) {
        walk_reattach(self, &mut String::new(), index);
    }
}
