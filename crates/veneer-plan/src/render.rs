//! Template rendering context (design spec §4.2).

use std::collections::BTreeMap;

use minijinja::{Environment, Value};
use veneer_spec::{Kind, Node};

pub fn is_template(s: &str) -> bool {
    s.contains("{{") || s.contains("{%")
}

/// Converts a spec node into a template value, rendering nothing.
pub fn node_value(node: &Node) -> Value {
    match &node.kind {
        Kind::Null => Value::from(()),
        Kind::Bool(b) => Value::from(*b),
        Kind::Int(i) => Value::from(*i),
        Kind::Float(f) => Value::from(*f),
        Kind::Str(s) => Value::from(s.as_str()),
        Kind::Array(items) => Value::from(items.iter().map(node_value).collect::<Vec<_>>()),
        Kind::Object(map) => Value::from_iter(map.iter().map(|(k, v)| (k.clone(), node_value(v)))),
    }
}

pub struct Renderer {
    env: Environment<'static>,
    pub roots: BTreeMap<String, Value>,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer {
    pub fn new() -> Self {
        let mut env = Environment::new();
        env.set_keep_trailing_newline(true);
        Self {
            env,
            roots: BTreeMap::new(),
        }
    }

    pub fn set(&mut self, root: &str, value: Value) {
        self.roots.insert(root.to_string(), value);
    }

    /// Renders `s` if it contains template syntax; otherwise returns it unchanged.
    pub fn render(&self, s: &str) -> Result<String, String> {
        if !is_template(s) {
            return Ok(s.to_string());
        }
        self.render_always(s)
    }

    pub fn render_always(&self, s: &str) -> Result<String, String> {
        let ctx = Value::from_iter(self.roots.iter().map(|(k, v)| (k.clone(), v.clone())));
        self.env
            .render_str(s, ctx)
            .map_err(|e| format!("cannot render template `{s}`: {e}"))
    }

    /// Renders every string leaf of `node` (used for `vars`), returning a template value.
    pub fn render_value(&self, node: &Node) -> Result<Value, String> {
        Ok(match &node.kind {
            Kind::Str(s) => Value::from(self.render(s)?),
            Kind::Array(items) => Value::from(
                items
                    .iter()
                    .map(|n| self.render_value(n))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            Kind::Object(map) => {
                let mut out = Vec::with_capacity(map.len());
                for (k, v) in map {
                    out.push((k.clone(), self.render_value(v)?));
                }
                Value::from_iter(out)
            }
            _ => node_value(node),
        })
    }
}
