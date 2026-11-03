# veneer M1 — Spec Core + `veneer check` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a `veneer` binary whose `check` command loads `.veneer/veneer.yaml`, expands local `$ref`s, `extends` layers and Overlay 1.0 overlays, validates the result against a strict typed schema with source-located diagnostics, and can print the fully resolved spec.

**Architecture:** suspect's generic crates parse YAML losslessly and resolve `$ref` targets with source positions. veneer converts each document into its own span-carrying `Node` tree, merges layers with veneer's merge rules, applies overlays through `suspect-overlay`, checks template phases with minijinja, and deserializes the tree into typed `Spec` structs through a custom serde `Deserializer` over `Node` that reports JSON-Pointer paths. The CLI renders diagnostics with miette.

**Tech Stack:** Rust 2024 (MSRV 1.88), suspect-{source,syntax,low,ref,overlay} (git, pinned rev), serde, indexmap, schemars 1, minijinja 2, clap 4, miette 7, assert_cmd, insta, tempfile.

**Spec:** `docs/superpowers/specs/2026-11-03-veneer-design.md` (milestone 1 of §11; implements §4.1, §4.2 shape, §4.3 local `$ref`, §4.6, §4.7, §4.8, and the phase rule of §3).

## Global Constraints

- Workspace: `edition = "2024"`, `rust-version = "1.88"`, `license = "MIT OR Apache-2.0"`, resolver 3.
- suspect crates are git dependencies pinned to `rev = "f0156145fe4d3f3d138f9dd68cd0f96a977e92e4"` of `https://github.com/LukasParke/suspect` (public).
- **suspect enables `serde_json/arbitrary_precision`** through feature unification. This breaks `#[serde(untagged)]` enums that pass through `serde_json::Value`. **Never deserialize the typed `Spec` via `serde_json`.** Always use `veneer_spec::from_node` (Task 3).
- Unknown keys are errors everywhere in the spec model (`#[serde(deny_unknown_fields)]` on every struct).
- Diagnostic codes are stable strings from `veneer_spec::codes`. Never inline a code literal elsewhere.
- Diagnostics are always sorted with `veneer_spec::sort_diagnostics` before output.
- Spans carry 1-based `line`/`col` and 0-based byte `start`/`end`.
- `check` exit codes: `0` ok (warnings allowed), `2` any error-severity diagnostic.
- Remote sources (`github:`, `git+`, `oci:`, any `scheme://` except `file://`) are rejected with `V1007` in M1. Remote fetch, lockfile (`V1005`, `V1006`), `use:` modules, `from:` adapters, `--set`, and DAG printing are later milestones.
- Commit after every task with a conventional-commit message ending with the trailer `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## File Structure

```
Cargo.toml                                  # workspace + shared deps
.gitignore
schema/veneer.schema.json                   # generated, committed (Task 4)
.github/workflows/ci.yml                    # Task 12
crates/veneer-spec/                         # no suspect deps
  src/lib.rs                                # re-exports
  src/diag.rs                               # Span, Severity, Diagnostic, codes, sorting
  src/value.rs                              # Node/Kind tree, pointers, JSON conversion, span index
  src/de.rs                                 # serde Deserializer over &Node with pointer-tagged errors
  src/model.rs                              # typed Spec + validate()
  src/schema.rs                             # json_schema_string()
  tests/diag.rs  tests/value.rs  tests/de.rs  tests/model.rs  tests/schema.rs
crates/veneer-template/                     # depends on veneer-spec only
  src/lib.rs                                # check_templates(): V2001/V2002/V2003
  tests/phase.rs
crates/veneer-resolve/                      # suspect + spec + template
  src/lib.rs                                # resolve() pipeline, ResolveOptions, Resolved
  src/merge.rs                              # layer merge rules (§4.7)
  src/load.rs                               # suspect doc -> Node, syntax/dup keys, $ref expansion
  src/extends.rs                            # extends layering
  src/discover.rs                           # locate(), is_remote()
  src/overlay.rs                            # Overlay 1.0 application + to_yaml()
  tests/common/mod.rs                       # temp-dir fixture helper
  tests/merge.rs tests/load.rs tests/refs.rs tests/extends.rs tests/discover.rs tests/overlay.rs tests/pipeline.rs
crates/veneer/                              # the binary
  src/main.rs                               # clap CLI
  src/check.rs                              # `check` command
  src/render.rs                             # miette rendering
  tests/cli.rs
  tests/fixtures/{basic,cycle,phase,unknown-key}/...
```

---

### Task 1: Workspace scaffold and diagnostics core

**Files:**
- Create: `Cargo.toml`, `.gitignore`, `crates/veneer-spec/Cargo.toml`, `crates/veneer-spec/src/lib.rs`, `crates/veneer-spec/src/diag.rs`
- Test: `crates/veneer-spec/tests/diag.rs`

**Interfaces:**
- Produces: `veneer_spec::{Span, Severity, Diagnostic, codes, sort_diagnostics, has_errors}` with:
  - `Diagnostic::error(code: &'static str, message: impl Into<String>) -> Diagnostic`
  - `Diagnostic::warning(code: &'static str, message: impl Into<String>) -> Diagnostic`
  - `Diagnostic::at(self, span: Option<Span>) -> Diagnostic`
  - `Diagnostic::note(self, note: impl Into<String>) -> Diagnostic`
  - `sort_diagnostics(&mut [Diagnostic])`, `has_errors(&[Diagnostic]) -> bool`

- [ ] **Step 1: Write workspace manifest and ignore file**

`Cargo.toml`:
```toml
[workspace]
resolver = "3"
members = ["crates/*"]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.88"
license = "MIT OR Apache-2.0"

[workspace.dependencies]
veneer-spec = { path = "crates/veneer-spec" }
veneer-template = { path = "crates/veneer-template" }
veneer-resolve = { path = "crates/veneer-resolve" }

suspect-source = { git = "https://github.com/LukasParke/suspect", rev = "f0156145fe4d3f3d138f9dd68cd0f96a977e92e4" }
suspect-syntax = { git = "https://github.com/LukasParke/suspect", rev = "f0156145fe4d3f3d138f9dd68cd0f96a977e92e4" }
suspect-low = { git = "https://github.com/LukasParke/suspect", rev = "f0156145fe4d3f3d138f9dd68cd0f96a977e92e4" }
suspect-ref = { git = "https://github.com/LukasParke/suspect", rev = "f0156145fe4d3f3d138f9dd68cd0f96a977e92e4" }
suspect-overlay = { git = "https://github.com/LukasParke/suspect", rev = "f0156145fe4d3f3d138f9dd68cd0f96a977e92e4" }

serde = { version = "1", features = ["derive"] }
serde_json = { version = "1", features = ["preserve_order"] }
indexmap = { version = "2", features = ["serde"] }
schemars = { version = "1", features = ["indexmap2"] }
minijinja = "2"
miette = { version = "7", features = ["fancy"] }
clap = { version = "4", features = ["derive"] }
tempfile = "3"
assert_cmd = "2"
predicates = "3"
insta = "1"
```

`.gitignore`:
```
/target
*.snap.new
```

`crates/veneer-spec/Cargo.toml`:
```toml
[package]
name = "veneer-spec"
description = "veneer spec model, value tree, and diagnostics"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
serde.workspace = true
serde_json.workspace = true
indexmap.workspace = true
schemars.workspace = true
```

- [ ] **Step 2: Write the failing test**

`crates/veneer-spec/tests/diag.rs`:
```rust
use veneer_spec::{codes, has_errors, sort_diagnostics, Diagnostic, Severity, Span};

fn span(uri: &str, start: usize) -> Option<Span> {
    Some(Span { uri: uri.into(), start, end: start + 1, line: 1, col: start as u32 + 1 })
}

#[test]
fn builders_set_fields() {
    let d = Diagnostic::warning(codes::OVERLAY_UNMATCHED, "nothing matched")
        .at(span("file:///a.yaml", 3))
        .note("check the target");
    assert_eq!(d.code, "W1102");
    assert_eq!(d.severity, Severity::Warning);
    assert_eq!(d.message, "nothing matched");
    assert_eq!(d.span.as_ref().unwrap().start, 3);
    assert_eq!(d.notes, vec!["check the target".to_string()]);
}

#[test]
fn sorting_is_by_uri_then_range_then_code_with_spanless_first() {
    let mut ds = vec![
        Diagnostic::error(codes::SCHEMA, "b").at(span("file:///b.yaml", 0)),
        Diagnostic::error(codes::CYCLE, "a2").at(span("file:///a.yaml", 9)),
        Diagnostic::error(codes::SPEC_NOT_FOUND, "none"),
        Diagnostic::error(codes::SYNTAX, "a1").at(span("file:///a.yaml", 2)),
    ];
    sort_diagnostics(&mut ds);
    let order: Vec<&str> = ds.iter().map(|d| d.message.as_str()).collect();
    assert_eq!(order, vec!["none", "a1", "a2", "b"]);
}

#[test]
fn has_errors_ignores_warnings() {
    assert!(!has_errors(&[Diagnostic::warning(codes::OVERLAY_HINT, "w")]));
    assert!(has_errors(&[Diagnostic::error(codes::SCHEMA, "e")]));
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p veneer-spec --test diag`
Expected: FAIL to compile (`unresolved import veneer_spec::codes` etc.)

- [ ] **Step 4: Write the implementation**

`crates/veneer-spec/src/diag.rs`:
```rust
//! Source-located diagnostics with stable codes.

use serde::Serialize;

/// Diagnostic severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

/// A source location. `start`/`end` are 0-based byte offsets; `line`/`col` are 1-based.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct Span {
    pub uri: String,
    pub start: usize,
    pub end: usize,
    pub line: u32,
    pub col: u32,
}

/// One finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub code: &'static str,
    pub severity: Severity,
    pub message: String,
    pub span: Option<Span>,
    pub notes: Vec<String>,
}

impl Diagnostic {
    pub fn error(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, severity: Severity::Error, message: message.into(), span: None, notes: Vec::new() }
    }

    pub fn warning(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, severity: Severity::Warning, message: message.into(), span: None, notes: Vec::new() }
    }

    #[must_use]
    pub fn at(mut self, span: Option<Span>) -> Self {
        self.span = span;
        self
    }

    #[must_use]
    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    fn sort_key(&self) -> (Option<(&str, usize, usize)>, &'static str, &str) {
        (
            self.span.as_ref().map(|s| (s.uri.as_str(), s.start, s.end)),
            self.code,
            self.message.as_str(),
        )
    }
}

/// Deterministic order: spanless first, then `(uri, start, end)`, then code, then message.
pub fn sort_diagnostics(diags: &mut [Diagnostic]) {
    diags.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
}

pub fn has_errors(diags: &[Diagnostic]) -> bool {
    diags.iter().any(|d| d.severity == Severity::Error)
}

/// Stable diagnostic codes. `V1xxx` resolve, `V2xxx` template/phase, `V3xxx` adapters,
/// `V4xxx` steps, `W…` warnings.
pub mod codes {
    pub const SYNTAX: &str = "V1000";
    pub const NOT_FOUND: &str = "V1001";
    pub const POINTER_NOT_FOUND: &str = "V1002";
    pub const CYCLE: &str = "V1003";
    pub const REF_SIBLINGS: &str = "V1004";
    pub const REMOTE_UNSUPPORTED: &str = "V1007";
    pub const DUPLICATE_KEY: &str = "V1008";
    pub const SCHEMA: &str = "V1009";
    pub const VERSION: &str = "V1010";
    pub const OVERLAY: &str = "V1011";
    pub const SPEC_NOT_FOUND: &str = "V1012";
    pub const SECRET_SOURCE: &str = "V1013";
    pub const SERVICE_KIND: &str = "V1014";
    pub const PHASE: &str = "V2001";
    pub const TEMPLATE_SYNTAX: &str = "V2002";
    pub const TEMPLATE_UNKNOWN_VAR: &str = "V2003";
    pub const OVERLAY_HINT: &str = "W1101";
    pub const OVERLAY_UNMATCHED: &str = "W1102";
    pub const MODULES_UNSUPPORTED: &str = "W1201";
}
```

`crates/veneer-spec/src/lib.rs`:
```rust
//! veneer-spec: the veneer spec model, value tree, and diagnostics.

mod diag;

pub use diag::{codes, has_errors, sort_diagnostics, Diagnostic, Severity, Span};
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p veneer-spec --test diag`
Expected: 3 passed.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml .gitignore crates/veneer-spec
git commit -m "feat(spec): scaffold workspace and diagnostics core"
```
(Also commit the generated `Cargo.lock`.)

---

### Task 2: Span-carrying value tree

**Files:**
- Create: `crates/veneer-spec/src/value.rs`
- Modify: `crates/veneer-spec/src/lib.rs`
- Test: `crates/veneer-spec/tests/value.rs`

**Interfaces:**
- Consumes: `Span` (Task 1).
- Produces:
  - `pub enum Kind { Null, Bool(bool), Int(i64), Float(f64), Str(String), Array(Vec<Node>), Object(IndexMap<String, Node>) }`
  - `pub struct Node { pub kind: Kind, pub span: Option<Span> }`
  - `Node::new(Kind) -> Node`, `Node::with_span(Kind, Option<Span>) -> Node`, `Node::object() -> Node` (empty object)
  - `Node::get(&self, &str) -> Option<&Node>`, `Node::as_str(&self) -> Option<&str>`, `Node::as_object(&self) -> Option<&IndexMap<String, Node>>`, `Node::is_null(&self) -> bool`
  - `Node::pointer(&self, &str) -> Option<&Node>` (RFC 6901)
  - `Node::nearest_span(&self, &str) -> Option<Span>`: span of the deepest existing node along the pointer
  - `Node::to_json(&self) -> serde_json::Value`, `Node::from_json(&serde_json::Value) -> Node`
  - `Node::span_index(&self) -> HashMap<String, Span>`, `Node::reattach_spans(&mut self, &HashMap<String, Span>)`
  - `pub fn escape_pointer_token(&str) -> String`, `pub fn split_pointer(&str) -> Vec<String>`

- [ ] **Step 1: Write the failing test**

`crates/veneer-spec/tests/value.rs`:
```rust
use serde_json::json;
use veneer_spec::{escape_pointer_token, split_pointer, Kind, Node, Span};

fn sp(start: usize) -> Option<Span> {
    Some(Span { uri: "file:///x.yaml".into(), start, end: start + 1, line: 1, col: 1 })
}

#[test]
fn json_roundtrip_preserves_values() {
    let v = json!({"a": [1, 2.5, "s", true, null], "b": {"c/d": "e~f"}});
    assert_eq!(Node::from_json(&v).to_json(), v);
}

#[test]
fn pointer_tokens_escape_and_split() {
    assert_eq!(escape_pointer_token("a/b~c"), "a~1b~0c");
    assert_eq!(split_pointer("/a~1b~0c/0"), vec!["a/b~c".to_string(), "0".to_string()]);
    assert!(split_pointer("").is_empty());
}

#[test]
fn pointer_lookup_walks_objects_and_arrays() {
    let n = Node::from_json(&json!({"a": [{"b": "x"}], "k/1": 2}));
    assert_eq!(n.pointer("/a/0/b").and_then(Node::as_str), Some("x"));
    assert_eq!(n.pointer("/k~11").map(|n| n.to_json()), Some(json!(2)));
    assert!(n.pointer("/a/5").is_none());
    assert_eq!(n.pointer("").map(|n| n.to_json()), Some(n.to_json()));
}

#[test]
fn span_index_and_reattach_roundtrip() {
    let mut inner = indexmap::IndexMap::new();
    inner.insert("b".to_string(), Node::with_span(Kind::Int(1), sp(5)));
    let mut top = indexmap::IndexMap::new();
    top.insert("a".to_string(), Node::with_span(Kind::Object(inner), sp(2)));
    let tree = Node::with_span(Kind::Object(top), sp(0));

    let index = tree.span_index();
    assert_eq!(index.get("/a/b").map(|s| s.start), Some(5));

    let mut stripped = Node::from_json(&tree.to_json());
    assert!(stripped.span.is_none());
    stripped.reattach_spans(&index);
    assert_eq!(stripped.pointer("/a/b").unwrap().span.as_ref().map(|s| s.start), Some(5));
}

#[test]
fn nearest_span_falls_back_to_deepest_existing_ancestor() {
    let mut top = indexmap::IndexMap::new();
    top.insert("a".to_string(), Node::with_span(Kind::Object(Default::default()), sp(7)));
    let tree = Node::with_span(Kind::Object(top), sp(0));
    assert_eq!(tree.nearest_span("/a/missing/x").map(|s| s.start), Some(7));
    assert_eq!(tree.nearest_span("/nope").map(|s| s.start), Some(0));
}
```

Add to `crates/veneer-spec/Cargo.toml`:
```toml
[dev-dependencies]
serde_json.workspace = true
indexmap.workspace = true
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p veneer-spec --test value`
Expected: FAIL to compile (`Node` not found).

- [ ] **Step 3: Write the implementation**

`crates/veneer-spec/src/value.rs`:
```rust
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
        .trim_start_matches('/')
        .split('/')
        .map(|t| t.replace("~1", "/").replace("~0", "~"))
        .collect()
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
        fn walk(node: &Node, ptr: &mut String, out: &mut HashMap<String, Span>) {
            if let Some(span) = &node.span {
                out.insert(ptr.clone(), span.clone());
            }
            let len = ptr.len();
            match &node.kind {
                Kind::Array(a) => {
                    for (i, child) in a.iter().enumerate() {
                        ptr.push('/');
                        ptr.push_str(&i.to_string());
                        walk(child, ptr, out);
                        ptr.truncate(len);
                    }
                }
                Kind::Object(m) => {
                    for (k, child) in m {
                        ptr.push('/');
                        ptr.push_str(&escape_pointer_token(k));
                        walk(child, ptr, out);
                        ptr.truncate(len);
                    }
                }
                _ => {}
            }
        }
        let mut out = HashMap::new();
        walk(self, &mut String::new(), &mut out);
        out
    }

    /// Fills `span` on every node that lacks one, from `index` (by pointer).
    pub fn reattach_spans(&mut self, index: &HashMap<String, Span>) {
        fn walk(node: &mut Node, ptr: &mut String, index: &HashMap<String, Span>) {
            if node.span.is_none() {
                node.span = index.get(ptr.as_str()).cloned();
            }
            let len = ptr.len();
            match &mut node.kind {
                Kind::Array(a) => {
                    for (i, child) in a.iter_mut().enumerate() {
                        ptr.push('/');
                        ptr.push_str(&i.to_string());
                        walk(child, ptr, index);
                        ptr.truncate(len);
                    }
                }
                Kind::Object(m) => {
                    for (k, child) in m.iter_mut() {
                        ptr.push('/');
                        ptr.push_str(&escape_pointer_token(k));
                        walk(child, ptr, index);
                        ptr.truncate(len);
                    }
                }
                _ => {}
            }
        }
        walk(self, &mut String::new(), index);
    }
}
```

Modify `crates/veneer-spec/src/lib.rs` to:
```rust
//! veneer-spec: the veneer spec model, value tree, and diagnostics.

mod diag;
mod value;

pub use diag::{codes, has_errors, sort_diagnostics, Diagnostic, Severity, Span};
pub use value::{escape_pointer_token, split_pointer, Kind, Node};
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p veneer-spec --test value`
Expected: 5 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/veneer-spec
git commit -m "feat(spec): add span-carrying value tree with JSON pointer support"
```

---

### Task 3: serde Deserializer over `Node`

This is required because suspect enables `serde_json/arbitrary_precision` (see Global Constraints). Errors carry the JSON Pointer where they happened.

**Files:**
- Create: `crates/veneer-spec/src/de.rs`
- Modify: `crates/veneer-spec/src/lib.rs`
- Test: `crates/veneer-spec/tests/de.rs`

**Interfaces:**
- Consumes: `Node`, `Kind`, `escape_pointer_token` (Task 2).
- Produces: `pub fn from_node<T: serde::de::DeserializeOwned>(node: &Node) -> Result<T, DeError>`; `pub struct DeError { pub pointer: Option<String>, pub message: String }` (implements `Display`, `Error`, `serde::de::Error`).

- [ ] **Step 1: Write the failing test**

`crates/veneer-spec/tests/de.rs`:
```rust
use serde::Deserialize;
use serde_json::json;
use veneer_spec::{from_node, Node};

#[derive(Debug, Deserialize, PartialEq)]
#[serde(untagged)]
enum Ver {
    Str(String),
    Int(i64),
    Float(f64),
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Clone,
    Worktree,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Inner {
    name: String,
    #[serde(default)]
    mode: Option<Mode>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Outer {
    version: u32,
    tools: indexmap::IndexMap<String, Ver>,
    items: Vec<Inner>,
    extra: Option<String>,
}

#[test]
fn deserializes_untagged_numbers_enums_options() {
    let n = Node::from_json(&json!({
        "version": 1,
        "tools": {"node": 22, "python": "3.12", "go": 1.5},
        "items": [{"name": "a", "mode": "worktree"}, {"name": "b"}],
        "extra": null
    }));
    let o: Outer = from_node(&n).unwrap();
    assert_eq!(o.version, 1);
    assert_eq!(o.tools["node"], Ver::Int(22));
    assert_eq!(o.tools["python"], Ver::Str("3.12".into()));
    assert_eq!(o.tools["go"], Ver::Float(1.5));
    assert_eq!(o.items[0].mode, Some(Mode::Worktree));
    assert_eq!(o.extra, None);
}

#[test]
fn unknown_field_error_points_at_the_key() {
    let n = Node::from_json(&json!({"version": 1, "tools": {}, "items": [{"name": "a", "nmae": "x"}]}));
    let err = from_node::<Outer>(&n).unwrap_err();
    assert_eq!(err.pointer.as_deref(), Some("/items/0/nmae"));
    assert!(err.message.contains("unknown field `nmae`"), "{}", err.message);
}

#[test]
fn type_error_points_at_the_value() {
    let n = Node::from_json(&json!({"version": "one", "tools": {}, "items": []}));
    let err = from_node::<Outer>(&n).unwrap_err();
    assert_eq!(err.pointer.as_deref(), Some("/version"));
}

#[test]
fn missing_field_points_at_the_object() {
    let n = Node::from_json(&json!({"version": 1, "tools": {}, "items": [{}]}));
    let err = from_node::<Outer>(&n).unwrap_err();
    assert_eq!(err.pointer.as_deref(), Some("/items/0"));
    assert!(err.message.contains("missing field `name`"), "{}", err.message);
}
```

Add to `[dev-dependencies]` in `crates/veneer-spec/Cargo.toml`:
```toml
serde.workspace = true
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p veneer-spec --test de`
Expected: FAIL to compile (`from_node` not found).

- [ ] **Step 3: Write the implementation**

`crates/veneer-spec/src/de.rs`:
```rust
//! A serde `Deserializer` over `&Node` whose errors carry a JSON Pointer.

use std::fmt;

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};

use crate::value::{escape_pointer_token, Kind, Node};

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
        Self { pointer: None, message: msg.to_string() }
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
    T::deserialize(NodeDe { node, pointer: String::new() }).map_err(|e| at(e, ""))
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
            Kind::Array(items) => visitor.visit_seq(Seq { items: items.iter().enumerate(), pointer: self.pointer }),
            Kind::Object(map) => visitor.visit_map(Map { iter: map.iter(), value: None, pointer: self.pointer }),
        };
        result.map_err(|e| at(e, &pointer))
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        if self.node.is_null() { visitor.visit_none() } else { visitor.visit_some(self) }
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

    fn deserialize_newtype_struct<V: Visitor<'de>>(self, _name: &'static str, visitor: V) -> Result<V::Value, DeError> {
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

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>, DeError> {
        let Some((i, node)) = self.items.next() else { return Ok(None) };
        let pointer = format!("{}/{i}", self.pointer);
        seed.deserialize(NodeDe { node, pointer: pointer.clone() })
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

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, DeError> {
        let Some((key, node)) = self.iter.next() else { return Ok(None) };
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
        seed.deserialize(NodeDe { node, pointer: pointer.clone() }).map_err(|e| at(e, &pointer))
    }
}
```

Modify `crates/veneer-spec/src/lib.rs`: add `mod de;` and `pub use de::{from_node, DeError};`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p veneer-spec --test de`
Expected: 4 passed. If `missing_field_points_at_the_object` reports `/items/0/…`, check that `next_key_seed` only annotates errors raised while deserializing the *key*. Missing-field errors come from the struct visitor after the map is exhausted, so they must be annotated by `deserialize_any`'s `map_err` with the object pointer.

- [ ] **Step 5: Commit**

```bash
git add crates/veneer-spec
git commit -m "feat(spec): add pointer-aware serde deserializer over Node"
```

---

### Task 4: Typed spec model, semantic validation, JSON Schema

**Files:**
- Create: `crates/veneer-spec/src/model.rs`, `crates/veneer-spec/src/schema.rs`, `schema/veneer.schema.json` (generated)
- Modify: `crates/veneer-spec/src/lib.rs`
- Test: `crates/veneer-spec/tests/model.rs`, `crates/veneer-spec/tests/schema.rs`

**Interfaces:**
- Consumes: `from_node` (Task 3), `codes`, `escape_pointer_token`.
- Produces:
  - `pub struct Spec { version: u32, base: Option<String>, extends: Vec<String>, modules: Vec<ModuleUse> /* key "use" */, vars: IndexMap<String, serde_json::Value>, secrets: IndexMap<String, Secret>, env: IndexMap<String, Scalar>, packages: Vec<Package>, tools: IndexMap<String, ToolVersion>, deps: Vec<Dep>, images: Vec<String>, workspace: Option<Workspace>, services: IndexMap<String, ServiceEntry>, hooks: Hooks }` (all fields `pub`)
  - Supporting types: `ModuleUse`, `ModuleRef`, `Secret`, `SecretSource`, `Scalar`, `Package`, `FromRef`, `ToolVersion`, `Dep`, `Workspace`, `Repo`, `RepoMode`, `FileSpec`, `ServiceEntry`, `Service`, `Healthcheck`, `Hooks`, `Hook`, `HookSpec`
  - `Spec::validate(&self) -> Vec<Violation>`; `pub struct Violation { pub code: &'static str, pub pointer: String, pub message: String }`
  - `pub fn json_schema_string() -> String`

- [ ] **Step 1: Write the failing tests**

`crates/veneer-spec/tests/model.rs`:
```rust
use serde_json::json;
use veneer_spec::{codes, from_node, Hook, Node, Package, RepoMode, ServiceEntry, Spec, ToolVersion};

fn spec(v: serde_json::Value) -> Result<Spec, veneer_spec::DeError> {
    from_node(&Node::from_json(&v))
}

#[test]
fn parses_the_full_design_example() {
    let s = spec(json!({
        "version": 1,
        "base": "ubuntu:24.04",
        "use": ["veneer/harness/claude-code@1", {"module": "x/y@2", "with": {"a": 1}}],
        "vars": {"python": "3.12"},
        "secrets": {
            "A": {"from": "env"},
            "B": {"from": "file", "path": "/run/secrets/b"},
            "C": {"from": "command", "run": "op read x"}
        },
        "env": {"DATABASE_URL": "postgres://x", "PORT": 5432, "DEBUG": true},
        "packages": ["git", {"from": "../Brewfile"}],
        "tools": {"node": 22, "python": "{{ vars.python }}", "from": "../mise.toml"},
        "deps": [{"from": "../package.json"}],
        "images": ["postgres:16"],
        "workspace": {
            "root": "/w",
            "repos": [{"url": "https://github.com/acme/app", "ref": "main", "path": "app", "mode": "worktree"}],
            "files": [{"src": "templates/.env.j2", "dest": "app/.env", "mode": "0600"}]
        },
        "services": {
            "db": {"image": "postgres:16", "env": {"POSTGRES_DB": "x"}, "healthcheck": {"cmd": "pg_isready", "interval": "1s", "retries": 30}},
            "worker": {"run": "python -m worker", "healthcheck": {"http": "http://localhost:8001/health"}},
            "from": "../compose.yaml"
        },
        "hooks": {
            "build": ["pip install -r r.txt", {"run": "make", "creates": "out"}],
            "workspace": [{"run": "make migrate", "unless": "make migrate-status"}]
        }
    }))
    .unwrap();
    assert_eq!(s.version, 1);
    assert_eq!(s.modules.len(), 2);
    assert!(matches!(s.packages[1], Package::Import(_)));
    assert_eq!(s.tools["node"], ToolVersion::Int(22));
    assert_eq!(s.workspace.as_ref().unwrap().repos[0].mode, RepoMode::Worktree);
    assert!(matches!(s.services["from"], ServiceEntry::Import(_)));
    assert!(matches!(s.hooks.build[0], Hook::Cmd(_)));
    assert!(s.validate().is_empty());
}

#[test]
fn minimal_spec_only_needs_version() {
    let s = spec(json!({"version": 1})).unwrap();
    assert!(s.packages.is_empty() && s.services.is_empty() && s.workspace.is_none());
}

#[test]
fn unknown_top_level_key_is_rejected() {
    let err = spec(json!({"version": 1, "pakages": ["git"]})).unwrap_err();
    assert_eq!(err.pointer.as_deref(), Some("/pakages"));
}

#[test]
fn validate_reports_version_secret_and_service_problems() {
    let s = spec(json!({
        "version": 2,
        "secrets": {"F": {"from": "file"}, "C": {"from": "command"}},
        "services": {"both": {"image": "x", "run": "y"}, "neither": {}}
    }))
    .unwrap();
    let got: Vec<(&str, String)> = s.validate().into_iter().map(|v| (v.code, v.pointer)).collect();
    assert_eq!(
        got,
        vec![
            (codes::VERSION, "/version".to_string()),
            (codes::SECRET_SOURCE, "/secrets/F".to_string()),
            (codes::SECRET_SOURCE, "/secrets/C".to_string()),
            (codes::SERVICE_KIND, "/services/both".to_string()),
            (codes::SERVICE_KIND, "/services/neither".to_string()),
        ]
    );
}
```

`crates/veneer-spec/tests/schema.rs`:
```rust
#[test]
fn committed_schema_is_up_to_date() {
    let generated = veneer_spec::json_schema_string();
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../schema/veneer.schema.json");
    if std::env::var_os("VENEER_BLESS").is_some() {
        std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap()).unwrap();
        std::fs::write(path, &generated).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(path).unwrap_or_default();
    assert!(
        committed == generated,
        "schema/veneer.schema.json is stale: run `VENEER_BLESS=1 cargo test -p veneer-spec --test schema`"
    );
}

#[test]
fn schema_forbids_unknown_top_level_keys() {
    let schema: serde_json::Value = serde_json::from_str(&veneer_spec::json_schema_string()).unwrap();
    assert_eq!(schema["additionalProperties"], serde_json::json!(false));
    assert!(schema["properties"]["services"].is_object());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p veneer-spec --test model --test schema`
Expected: FAIL to compile (`Spec` not found).

- [ ] **Step 3: Write the model**

`crates/veneer-spec/src/model.rs`:
```rust
//! The typed veneer spec (version 1).

use indexmap::IndexMap;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{codes, escape_pointer_token};

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// Spec format version. Must be `1`.
    pub version: u32,
    /// Base image used by `veneer bake`.
    #[serde(default)]
    pub base: Option<String>,
    /// Specs to layer underneath this one (consumed during resolution).
    #[serde(default)]
    pub extends: Vec<String>,
    /// Reusable spec fragments.
    #[serde(default, rename = "use")]
    pub modules: Vec<ModuleUse>,
    /// Template variables.
    #[serde(default)]
    pub vars: IndexMap<String, serde_json::Value>,
    /// Secrets, resolved at workspace time only.
    #[serde(default)]
    pub secrets: IndexMap<String, Secret>,
    /// Environment variables.
    #[serde(default)]
    pub env: IndexMap<String, Scalar>,
    /// OS packages.
    #[serde(default)]
    pub packages: Vec<Package>,
    /// Tools installed through mise. The key `from` names a mise.toml/.tool-versions file.
    #[serde(default)]
    pub tools: IndexMap<String, ToolVersion>,
    /// Project dependency installs, detected from lockfiles.
    #[serde(default)]
    pub deps: Vec<Dep>,
    /// Container images pre-pulled at build time.
    #[serde(default)]
    pub images: Vec<String>,
    /// Workspace folder layout.
    #[serde(default)]
    pub workspace: Option<Workspace>,
    /// Services. The key `from` names a compose file.
    #[serde(default)]
    pub services: IndexMap<String, ServiceEntry>,
    /// Lifecycle hooks.
    #[serde(default)]
    pub hooks: Hooks,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ModuleUse {
    Ref(String),
    Full(ModuleRef),
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModuleRef {
    pub module: String,
    #[serde(default)]
    pub with: IndexMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Secret {
    pub from: SecretSource,
    /// Path for `from: file`.
    #[serde(default)]
    pub path: Option<String>,
    /// Command for `from: command`.
    #[serde(default)]
    pub run: Option<String>,
    /// Host variable name for `from: env` (defaults to the secret's name).
    #[serde(default)]
    pub var: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SecretSource {
    Env,
    File,
    Command,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Scalar {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Package {
    Name(String),
    Import(FromRef),
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FromRef {
    pub from: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ToolVersion {
    Str(String),
    Int(i64),
    Float(f64),
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Dep {
    pub from: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default)]
    pub repos: Vec<Repo>,
    #[serde(default)]
    pub files: Vec<FileSpec>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Repo {
    pub url: String,
    #[serde(default, rename = "ref")]
    pub git_ref: Option<String>,
    pub path: String,
    #[serde(default)]
    pub mode: RepoMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RepoMode {
    #[default]
    Clone,
    Worktree,
    Copy,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileSpec {
    pub src: String,
    pub dest: String,
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ServiceEntry {
    Def(Service),
    Import(String),
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Service {
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub env: IndexMap<String, Scalar>,
    #[serde(default)]
    pub ports: Vec<String>,
    #[serde(default)]
    pub healthcheck: Option<Healthcheck>,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Healthcheck {
    #[serde(default)]
    pub cmd: Option<String>,
    #[serde(default)]
    pub http: Option<String>,
    #[serde(default)]
    pub interval: Option<String>,
    #[serde(default)]
    pub retries: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    #[serde(default)]
    pub build: Vec<Hook>,
    #[serde(default)]
    pub workspace: Vec<Hook>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Hook {
    Cmd(String),
    Full(HookSpec),
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HookSpec {
    pub run: String,
    #[serde(default)]
    pub creates: Option<String>,
    #[serde(default)]
    pub unless: Option<String>,
}

/// A semantic rule violation found after successful deserialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub code: &'static str,
    pub pointer: String,
    pub message: String,
}

impl Spec {
    /// Rules the type system cannot express. Order: version, secrets, services (declaration order).
    pub fn validate(&self) -> Vec<Violation> {
        let mut out = Vec::new();
        if self.version != 1 {
            out.push(Violation {
                code: codes::VERSION,
                pointer: "/version".into(),
                message: format!("unsupported spec version {}; this veneer supports version 1", self.version),
            });
        }
        for (name, secret) in &self.secrets {
            let missing = match secret.from {
                SecretSource::File if secret.path.is_none() => Some("path"),
                SecretSource::Command if secret.run.is_none() => Some("run"),
                _ => None,
            };
            if let Some(field) = missing {
                out.push(Violation {
                    code: codes::SECRET_SOURCE,
                    pointer: format!("/secrets/{}", escape_pointer_token(name)),
                    message: format!("secret `{name}` needs `{field}` for this `from` source"),
                });
            }
        }
        for (name, entry) in &self.services {
            if let ServiceEntry::Def(svc) = entry {
                if svc.image.is_some() == svc.run.is_some() {
                    out.push(Violation {
                        code: codes::SERVICE_KIND,
                        pointer: format!("/services/{}", escape_pointer_token(name)),
                        message: format!("service `{name}` must set exactly one of `image` or `run`"),
                    });
                }
            }
        }
        out
    }
}
```

`crates/veneer-spec/src/schema.rs`:
```rust
//! JSON Schema for `veneer.yaml`, generated from the Rust types.

use crate::Spec;

/// Pretty JSON Schema text with a trailing newline (the committed file's exact bytes).
pub fn json_schema_string() -> String {
    let schema = schemars::schema_for!(Spec);
    let mut text = serde_json::to_string_pretty(&schema).expect("schema serializes");
    text.push('\n');
    text
}
```

Final `crates/veneer-spec/src/lib.rs`:
```rust
//! veneer-spec: the veneer spec model, value tree, and diagnostics.

mod de;
mod diag;
mod model;
mod schema;
mod value;

pub use de::{from_node, DeError};
pub use diag::{codes, has_errors, sort_diagnostics, Diagnostic, Severity, Span};
pub use model::*;
pub use schema::json_schema_string;
pub use value::{escape_pointer_token, split_pointer, Kind, Node};
```

- [ ] **Step 4: Generate the schema, then run the tests**

Run: `VENEER_BLESS=1 cargo test -p veneer-spec --test schema && cargo test -p veneer-spec`
Expected: all veneer-spec tests pass, and `schema/veneer.schema.json` now exists. Open it and confirm that the top level has `"additionalProperties": false` and that `properties` includes `use` (not `modules`).

- [ ] **Step 5: Commit**

```bash
git add crates/veneer-spec schema/veneer.schema.json
git commit -m "feat(spec): add typed spec model, semantic validation, and JSON Schema"
```

---

### Task 5: Layer merge rules

**Files:**
- Create: `crates/veneer-resolve/Cargo.toml`, `crates/veneer-resolve/src/lib.rs`, `crates/veneer-resolve/src/merge.rs`
- Test: `crates/veneer-resolve/tests/merge.rs`

**Interfaces:**
- Consumes: `Node`, `Kind` (Task 2).
- Produces: `veneer_resolve::merge::merge(base: &mut Node, top: Node, path: &[String])`. It merges `top` over `base` at logical location `path` (empty slice = spec root).

Rules (spec §4.7): maps deep-merge (later wins); a `null` value deletes the key; lists replace, except these paths, which concatenate and de-duplicate by identity (a later item with the same identity replaces the earlier one in place):

| Path | Identity |
|---|---|
| `packages`, `images` | the string, or `from:<path>` for `{from}` items |
| `deps` | `from` |
| `hooks.build`, `hooks.workspace` | the string, or `run` |
| `workspace.repos` | `path` |
| `workspace.files` | `dest` |

- [ ] **Step 1: Create the crate manifest**

`crates/veneer-resolve/Cargo.toml`:
```toml
[package]
name = "veneer-resolve"
description = "Load, layer, and resolve veneer specs"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
veneer-spec.workspace = true
indexmap.workspace = true
serde_json.workspace = true

[dev-dependencies]
serde_json.workspace = true
tempfile.workspace = true
```

`crates/veneer-resolve/src/lib.rs`:
```rust
//! veneer-resolve: load, layer, and resolve veneer specs.

pub mod merge;
```

- [ ] **Step 2: Write the failing test**

`crates/veneer-resolve/tests/merge.rs`:
```rust
use serde_json::{json, Value};
use veneer_resolve::merge::merge;
use veneer_spec::Node;

fn merged(base: Value, top: Value) -> Value {
    let mut b = Node::from_json(&base);
    merge(&mut b, Node::from_json(&top), &[]);
    b.to_json()
}

#[test]
fn maps_deep_merge_and_later_wins() {
    assert_eq!(
        merged(json!({"env": {"A": "1", "B": "2"}}), json!({"env": {"B": "3", "C": "4"}})),
        json!({"env": {"A": "1", "B": "3", "C": "4"}})
    );
}

#[test]
fn null_deletes_inherited_keys_and_is_dropped_from_new_subtrees() {
    assert_eq!(
        merged(json!({"env": {"A": "1", "B": "2"}}), json!({"env": {"A": null}, "new": {"x": null, "y": 1}})),
        json!({"env": {"B": "2"}, "new": {"y": 1}})
    );
}

#[test]
fn ordinary_lists_replace() {
    assert_eq!(
        merged(json!({"services": {"db": {"ports": ["1"]}}}), json!({"services": {"db": {"ports": ["2"]}}})),
        json!({"services": {"db": {"ports": ["2"]}}})
    );
}

#[test]
fn concat_lists_append_and_dedupe_by_identity() {
    assert_eq!(
        merged(
            json!({"packages": ["git", {"from": "Brewfile"}], "images": ["a"], "deps": [{"from": "p.json"}]}),
            json!({"packages": ["git", "curl", {"from": "Brewfile"}], "images": ["b"], "deps": [{"from": "p.json"}]})
        ),
        json!({"packages": ["git", {"from": "Brewfile"}, "curl"], "images": ["a", "b"], "deps": [{"from": "p.json"}]})
    );
}

#[test]
fn nested_concat_paths_use_their_identity_and_replace_in_place() {
    assert_eq!(
        merged(
            json!({
                "hooks": {"build": ["make", {"run": "x", "creates": "a"}]},
                "workspace": {"repos": [{"path": "app", "url": "u1"}], "files": [{"dest": "d", "src": "s1"}]}
            }),
            json!({
                "hooks": {"build": [{"run": "x", "creates": "b"}, "lint"]},
                "workspace": {"repos": [{"path": "app", "url": "u2"}, {"path": "lib", "url": "u3"}], "files": [{"dest": "d", "src": "s2"}]}
            })
        ),
        json!({
            "hooks": {"build": ["make", {"run": "x", "creates": "b"}, "lint"]},
            "workspace": {"repos": [{"path": "app", "url": "u2"}, {"path": "lib", "url": "u3"}], "files": [{"dest": "d", "src": "s2"}]}
        })
    );
}

#[test]
fn scalar_over_map_replaces() {
    assert_eq!(merged(json!({"base": {"x": 1}}), json!({"base": "ubuntu"})), json!({"base": "ubuntu"}));
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p veneer-resolve --test merge`
Expected: FAIL to compile (`merge` not found in `veneer_resolve::merge`).

- [ ] **Step 4: Write the implementation**

`crates/veneer-resolve/src/merge.rs`:
```rust
//! Layer merge rules (design spec §4.7).

use veneer_spec::{Kind, Node};

const CONCAT_PATHS: &[&[&str]] = &[
    &["packages"],
    &["images"],
    &["deps"],
    &["hooks", "build"],
    &["hooks", "workspace"],
    &["workspace", "repos"],
    &["workspace", "files"],
];

fn is_concat(path: &[String]) -> bool {
    CONCAT_PATHS
        .iter()
        .any(|p| p.len() == path.len() && p.iter().zip(path).all(|(a, b)| *a == b.as_str()))
}

fn identity(path: &[String], item: &Node) -> Option<String> {
    let field = |name: &str| item.get(name).and_then(Node::as_str).map(str::to_owned);
    let path: Vec<&str> = path.iter().map(String::as_str).collect();
    match path.as_slice() {
        ["packages"] | ["images"] => item
            .as_str()
            .map(str::to_owned)
            .or_else(|| field("from").map(|f| format!("from:{f}"))),
        ["deps"] => field("from"),
        ["hooks", _] => item.as_str().map(str::to_owned).or_else(|| field("run")),
        ["workspace", "repos"] => field("path"),
        ["workspace", "files"] => field("dest"),
        _ => None,
    }
}

/// Removes `null`-valued keys from objects, recursively (a null over nothing deletes nothing).
fn strip_nulls(mut node: Node) -> Node {
    match &mut node.kind {
        Kind::Object(map) => {
            let taken = std::mem::take(map);
            *map = taken
                .into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, strip_nulls(v)))
                .collect();
        }
        Kind::Array(items) => {
            let taken = std::mem::take(items);
            *items = taken.into_iter().map(strip_nulls).collect();
        }
        _ => {}
    }
    node
}

/// Merges `top` over `base`. `path` is the logical location of `base` in the spec.
pub fn merge(base: &mut Node, top: Node, path: &[String]) {
    let Node { kind: top_kind, span: top_span } = top;
    match top_kind {
        Kind::Object(top_map) if matches!(base.kind, Kind::Object(_)) => {
            let Kind::Object(base_map) = &mut base.kind else { unreachable!() };
            for (key, value) in top_map {
                if value.is_null() {
                    base_map.shift_remove(&key);
                    continue;
                }
                let mut child_path = path.to_vec();
                child_path.push(key.clone());
                match base_map.get_mut(&key) {
                    Some(existing) => merge(existing, value, &child_path),
                    None => {
                        base_map.insert(key, strip_nulls(value));
                    }
                }
            }
        }
        Kind::Array(top_items) if is_concat(path) && matches!(base.kind, Kind::Array(_)) => {
            let Kind::Array(base_items) = &mut base.kind else { unreachable!() };
            for item in top_items {
                let existing = identity(path, &item).and_then(|id| {
                    base_items.iter().position(|e| identity(path, e).as_deref() == Some(id.as_str()))
                });
                match existing {
                    Some(i) => base_items[i] = item,
                    None => base_items.push(item),
                }
            }
        }
        other => *base = strip_nulls(Node::with_span(other, top_span)),
    }
}
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p veneer-resolve --test merge`
Expected: 6 passed.

- [ ] **Step 6: Commit**

```bash
git add crates/veneer-resolve Cargo.lock
git commit -m "feat(resolve): add layer merge rules"
```

---

### Task 6: Document loader (suspect → `Node`), syntax and duplicate-key diagnostics

**Files:**
- Create: `crates/veneer-resolve/src/load.rs`, `crates/veneer-resolve/tests/common/mod.rs`
- Modify: `crates/veneer-resolve/Cargo.toml`, `crates/veneer-resolve/src/lib.rs`
- Test: `crates/veneer-resolve/tests/load.rs`

**Interfaces:**
- Consumes: `Node`, `Kind`, `Span`, `Diagnostic`, `codes`.
- Produces:
  - `pub struct Loader<'ws> { pub diagnostics: Vec<Diagnostic>, pub documents: BTreeSet<String>, /* private: ws, stack, checked */ }`
  - `Loader::new(ws: &'ws suspect_ref::Workspace) -> Loader<'ws>`
  - `Loader::load_file(&mut self, path: &Path) -> Option<Node>`: `path` must be absolute and canonical. Returns `None` if the file can't be opened or has syntax errors.
  - `pub fn load_single(path: &Path) -> (Option<Node>, Vec<Diagnostic>)`: a convenience wrapper that builds its own workspace.
  - Test helper `common::tree(&[(&str, &str)]) -> (TempDir, PathBuf)`: writes files and returns the canonical path of the first one.

suspect facts this task relies on (verified against rev `f015614`):
- `WorkspaceBuilder::new().build() -> Result<Workspace, WorkspaceError>`; `ws.open(uri_str) -> Result<DocHandle, WorkspaceError>` accepts `file://` URIs from `Uri::from_path`.
- `DocHandle::{id() -> usize, uri() -> &Uri, doc() -> &LowDoc}`; `LowDoc::{root() -> NodeRef, syntax_errors() -> &[SyntaxError { range, message }]}`.
- `NodeRef::{kind() -> ValueKind, as_bool, as_i64, as_f64, decoded_scalar() -> Cow<[u8]>, items() -> Vec<NodeRef>, entries() -> Vec<Entry { key: &str, key_node, value: Option<NodeRef> }>, get(&str), byte_range(), line_col() /* 0-based */}`.
- `suspect_source::LineIndex::new(&bytes).line_col(&bytes, offset) -> (u32, u32)` (0-based).

- [ ] **Step 1: Add dependencies**

Add to `[dependencies]` in `crates/veneer-resolve/Cargo.toml`:
```toml
suspect-source.workspace = true
suspect-syntax.workspace = true
suspect-low.workspace = true
suspect-ref.workspace = true
```

- [ ] **Step 2: Write the test helper and failing test**

`crates/veneer-resolve/tests/common/mod.rs`:
```rust
#![allow(dead_code)]
use std::path::PathBuf;

/// Writes `files` (relative path, contents) into a temp dir. Returns the dir guard and the
/// canonical path of the first file.
pub fn tree(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    for (rel, body) in files {
        let path = dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
    }
    let entry = dir.path().join(files[0].0).canonicalize().unwrap();
    (dir, entry)
}
```

`crates/veneer-resolve/tests/load.rs`:
```rust
mod common;

use serde_json::json;
use veneer_resolve::load::load_single;
use veneer_spec::codes;

#[test]
fn converts_scalars_maps_and_lists_with_spans() {
    let (_d, entry) = common::tree(&[(
        "veneer.yaml",
        "version: 1\nenv:\n  A: \"x\\ty\"\n  B: 2.5\n  C: true\n  D:\npackages: [git, curl]\n",
    )]);
    let (node, diags) = load_single(&entry);
    assert!(diags.is_empty(), "{diags:?}");
    let node = node.unwrap();
    assert_eq!(
        node.to_json(),
        json!({"version": 1, "env": {"A": "x\ty", "B": 2.5, "C": true, "D": null}, "packages": ["git", "curl"]})
    );
    let span = node.pointer("/env/B").unwrap().span.clone().unwrap();
    assert_eq!((span.line, span.col), (4, 6));
    assert!(span.uri.starts_with("file://") && span.uri.ends_with("veneer.yaml"));
}

#[test]
fn syntax_errors_are_reported_with_location() {
    let (_d, entry) = common::tree(&[("veneer.yaml", "version: 1\npackages: [git, curl\n")]);
    let (node, diags) = load_single(&entry);
    assert!(node.is_none());
    assert_eq!(diags[0].code, codes::SYNTAX);
    assert!(diags[0].span.is_some());
}

#[test]
fn duplicate_keys_are_errors() {
    let (_d, entry) = common::tree(&[("veneer.yaml", "version: 1\nbase: a\nbase: b\n")]);
    let (_, diags) = load_single(&entry);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].code, codes::DUPLICATE_KEY);
    assert_eq!(diags[0].span.as_ref().unwrap().line, 3);
}

#[test]
fn missing_file_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let (node, diags) = load_single(&dir.path().canonicalize().unwrap().join("nope.yaml"));
    assert!(node.is_none());
    assert_eq!(diags[0].code, codes::NOT_FOUND);
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p veneer-resolve --test load`
Expected: FAIL to compile (`load` module missing).

- [ ] **Step 4: Write the implementation (no `$ref` yet)**

`crates/veneer-resolve/src/load.rs`:
```rust
//! Converts suspect documents into veneer `Node` trees.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use indexmap::IndexMap;
use suspect_low::{NodeRef, ValueKind};
use suspect_ref::{DocHandle, Workspace, WorkspaceBuilder};
use suspect_source::{LineIndex, Uri};
use veneer_spec::{codes, Diagnostic, Kind, Node, Span};

pub struct Loader<'ws> {
    ws: &'ws Workspace,
    /// `$ref` targets currently being expanded, as (doc id, pointer).
    stack: Vec<(usize, String)>,
    /// Per-document syntax verdict, so each document is reported once.
    checked: HashMap<usize, bool>,
    pub diagnostics: Vec<Diagnostic>,
    pub documents: BTreeSet<String>,
}

pub(crate) fn span_of(uri: &Uri, node: NodeRef<'_>) -> Span {
    let range = node.byte_range();
    let (line, col) = node.line_col();
    Span { uri: uri.to_string(), start: range.start, end: range.end, line: line + 1, col: col + 1 }
}

impl<'ws> Loader<'ws> {
    pub fn new(ws: &'ws Workspace) -> Self {
        Self { ws, stack: Vec::new(), checked: HashMap::new(), diagnostics: Vec::new(), documents: BTreeSet::new() }
    }

    /// Loads an absolute, canonical path. `None` when unreadable or syntactically invalid.
    pub fn load_file(&mut self, path: &Path) -> Option<Node> {
        let uri = match Uri::from_path(path) {
            Ok(uri) => uri,
            Err(e) => {
                self.diagnostics.push(Diagnostic::error(codes::NOT_FOUND, format!("invalid path {}: {e}", path.display())));
                return None;
            }
        };
        let handle = match self.ws.open(uri.as_str()) {
            Ok(h) => h,
            Err(e) => {
                self.diagnostics.push(Diagnostic::error(codes::NOT_FOUND, format!("cannot read {}: {e}", path.display())));
                return None;
            }
        };
        if !self.ensure_valid(&handle) {
            return None;
        }
        let root = handle.doc().root();
        Some(self.convert(&handle, root, &[]))
    }

    /// Records the document and reports its syntax errors once. `true` if it parsed cleanly.
    fn ensure_valid(&mut self, h: &DocHandle<'ws>) -> bool {
        if let Some(ok) = self.checked.get(&h.id()) {
            return *ok;
        }
        self.documents.insert(h.uri().to_string());
        let errors = h.doc().syntax_errors();
        if !errors.is_empty() {
            let bytes = h.uri().as_path().and_then(|p| std::fs::read(p).ok()).unwrap_or_default();
            let index = LineIndex::new(&bytes);
            for e in errors {
                let (line, col) = index.line_col(&bytes, e.range.start);
                let span = Span { uri: h.uri().to_string(), start: e.range.start, end: e.range.end, line: line + 1, col: col + 1 };
                self.diagnostics.push(Diagnostic::error(codes::SYNTAX, e.message.clone()).at(Some(span)));
            }
        }
        let ok = errors.is_empty();
        self.checked.insert(h.id(), ok);
        ok
    }

    /// `path` is the logical location in the spec (used for merge rules on `$ref` siblings).
    fn convert(&mut self, h: &DocHandle<'ws>, node: NodeRef<'_>, path: &[String]) -> Node {
        let span = Some(span_of(h.uri(), node));
        let kind = match node.kind() {
            ValueKind::Null => Kind::Null,
            ValueKind::Bool => Kind::Bool(node.as_bool().unwrap_or(false)),
            ValueKind::Int => match node.as_i64() {
                Some(i) => Kind::Int(i),
                None => Kind::Float(node.as_f64().unwrap_or(0.0)),
            },
            ValueKind::Float => Kind::Float(node.as_f64().unwrap_or(0.0)),
            ValueKind::Str => Kind::Str(String::from_utf8_lossy(&node.decoded_scalar()).into_owned()),
            ValueKind::Array => Kind::Array(
                node.items()
                    .into_iter()
                    .enumerate()
                    .map(|(i, item)| {
                        let mut p = path.to_vec();
                        p.push(i.to_string());
                        self.convert(h, item, &p)
                    })
                    .collect(),
            ),
            ValueKind::Object => Kind::Object(self.convert_entries(h, node, path, false)),
        };
        Node::with_span(kind, span)
    }

    /// Converts mapping entries. With `skip_ref`, the `$ref` key is omitted (used for siblings).
    fn convert_entries(&mut self, h: &DocHandle<'ws>, node: NodeRef<'_>, path: &[String], skip_ref: bool) -> IndexMap<String, Node> {
        let mut map = IndexMap::new();
        for entry in node.entries() {
            if skip_ref && entry.key == "$ref" {
                continue;
            }
            let mut p = path.to_vec();
            p.push(entry.key.to_owned());
            let value = match entry.value {
                Some(v) => self.convert(h, v, &p),
                None => Node::with_span(Kind::Null, Some(span_of(h.uri(), entry.key_node))),
            };
            if map.contains_key(entry.key) {
                self.diagnostics.push(
                    Diagnostic::error(codes::DUPLICATE_KEY, format!("duplicate key `{}`", entry.key))
                        .at(Some(span_of(h.uri(), entry.key_node))),
                );
            }
            map.insert(entry.key.to_owned(), value);
        }
        map
    }
}

/// Loads one file with a fresh workspace. `$ref`s are expanded (Task 7).
pub fn load_single(path: &Path) -> (Option<Node>, Vec<Diagnostic>) {
    let ws = match WorkspaceBuilder::new().build() {
        Ok(ws) => ws,
        Err(e) => return (None, vec![Diagnostic::error(codes::NOT_FOUND, format!("cannot create workspace: {e}"))]),
    };
    let mut loader = Loader::new(&ws);
    let node = loader.load_file(path);
    (node, loader.diagnostics)
}
```

Modify `crates/veneer-resolve/src/lib.rs`:
```rust
//! veneer-resolve: load, layer, and resolve veneer specs.

pub mod load;
pub mod merge;
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p veneer-resolve --test load`
Expected: 4 passed. The first build compiles the vendored tree-sitter grammars and takes about 1 minute. If `converts_…` fails only on `(line, col)`, print the span. suspect's `line_col()` is 0-based, so the expected 1-based value for `B: 2.5` is line 4, col 6.

- [ ] **Step 6: Commit**

```bash
git add crates/veneer-resolve Cargo.lock
git commit -m "feat(resolve): load suspect documents into span-carrying nodes"
```

---

### Task 7: `$ref` expansion

**Files:**
- Modify: `crates/veneer-resolve/src/load.rs`, `crates/veneer-resolve/src/lib.rs`
- Create: `crates/veneer-resolve/src/discover.rs` (only `is_remote` in this task)
- Test: `crates/veneer-resolve/tests/refs.rs`

**Interfaces:**
- Consumes: `Loader` (Task 6), `merge::merge` (Task 5).
- Produces: `Loader::load_file` now expands every mapping that has a `$ref` key. `pub(crate) fn discover::is_remote(&str) -> bool`.

Semantics (spec §4.3):
- The target is the node named by the reference (relative to the containing document; `#/ptr` fragments; a whole document when there is no fragment).
- Sibling keys deep-merge over the target, using merge rules at the `$ref`'s logical path. If siblings exist and the target is not a mapping → `V1004`.
- A target that is already being expanded → `V1003`, with a `chain:` note.
- Missing document → `V1001`. Missing pointer → `V1002`. Remote → `V1007`.
- On error, the `$ref` object becomes `null`, which merges away as a deletion. Resolution continues so that all errors are reported.

suspect facts: `DocHandle::ref_target(ref_value_node) -> Result<ReferenceTarget { doc: usize, pointer: Pointer }, RefError>`; `ws.get_by_id(usize) -> Option<DocHandle>`; `handle.node_at_pointer(&Pointer) -> Result<NodeRef, RefError>`; `Pointer::to_path() -> String` (`""` for root). `RefError::{MissingPointer{..}, MissingDoc{..}, Io(_), RemoteDenied{..}, …}`.

- [ ] **Step 1: Write the failing test**

`crates/veneer-resolve/tests/refs.rs`:
```rust
mod common;

use serde_json::json;
use veneer_resolve::load::load_single;
use veneer_spec::codes;

#[test]
fn whole_document_ref_with_sibling_merge() {
    let (_d, entry) = common::tree(&[
        (".veneer/veneer.yaml", "version: 1\ntools:\n  $ref: ./tools.yaml\n  python: \"3.12\"\n"),
        (".veneer/tools.yaml", "node: 22\npython: \"3.11\"\n"),
    ]);
    let (node, diags) = load_single(&entry);
    assert!(diags.is_empty(), "{diags:?}");
    let node = node.unwrap();
    assert_eq!(node.to_json(), json!({"version": 1, "tools": {"node": 22, "python": "3.12"}}));
    // spans of expanded nodes point into the referenced file
    assert!(node.pointer("/tools/node").unwrap().span.as_ref().unwrap().uri.ends_with("tools.yaml"));
}

#[test]
fn pointer_fragments_local_and_nested_refs() {
    let (_d, entry) = common::tree(&[
        ("veneer.yaml", "version: 1\nenv:\n  $ref: ./parts/all.yaml#/env\nimages:\n  $ref: '#/defs/imgs'\ndefs:\n  imgs: [redis:7]\n"),
        ("parts/all.yaml", "env:\n  $ref: ./env.yaml\n"),
        ("parts/env.yaml", "A: \"1\"\n"),
    ]);
    let (node, diags) = load_single(&entry);
    assert!(diags.is_empty(), "{diags:?}");
    let v = node.unwrap().to_json();
    assert_eq!(v["env"], json!({"A": "1"}));
    assert_eq!(v["images"], json!(["redis:7"]));
}

#[test]
fn cycles_are_reported_with_chain() {
    let (_d, entry) = common::tree(&[
        ("veneer.yaml", "version: 1\ntools:\n  $ref: ./a.yaml\n"),
        ("a.yaml", "$ref: ./b.yaml\n"),
        ("b.yaml", "$ref: ./a.yaml\n"),
    ]);
    let (_, diags) = load_single(&entry);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::CYCLE);
    assert!(diags[0].notes[0].starts_with("chain: "), "{:?}", diags[0].notes);
}

#[test]
fn self_reference_is_a_cycle() {
    let (_d, entry) = common::tree(&[("veneer.yaml", "version: 1\nx:\n  y:\n    $ref: '#/x'\n")]);
    let (_, diags) = load_single(&entry);
    assert_eq!(diags[0].code, codes::CYCLE);
}

#[test]
fn missing_targets_and_bad_siblings() {
    let (_d, entry) = common::tree(&[
        (
            "veneer.yaml",
            "version: 1\na:\n  $ref: ./gone.yaml\nb:\n  $ref: '#/nope'\nc:\n  $ref: '#/version'\n  extra: 1\nd:\n  $ref: https://example.com/x.yaml\n",
        ),
    ]);
    let (_, diags) = load_single(&entry);
    let codes_seen: Vec<&str> = diags.iter().map(|d| d.code).collect();
    assert_eq!(codes_seen, vec![codes::NOT_FOUND, codes::POINTER_NOT_FOUND, codes::REF_SIBLINGS, codes::REMOTE_UNSUPPORTED]);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p veneer-resolve --test refs`
Expected: FAIL. `$ref` keys are currently kept verbatim, so the JSON assertions and diagnostic counts don't match.

- [ ] **Step 3: Implement `is_remote`**

`crates/veneer-resolve/src/discover.rs`:
```rust
//! Spec discovery and source classification.

/// True for sources M1 cannot fetch: `github:`, `git+…`, `oci:`, or any `scheme://` but `file://`.
pub(crate) fn is_remote(source: &str) -> bool {
    source.starts_with("github:")
        || source.starts_with("git+")
        || source.starts_with("oci:")
        || (source.contains("://") && !source.starts_with("file://"))
}
```

Add `pub mod discover;` to `crates/veneer-resolve/src/lib.rs`.

- [ ] **Step 4: Implement expansion in `load.rs`**

In `Loader::convert`, replace the `ValueKind::Object` arm with:
```rust
            ValueKind::Object => {
                if let Some(ref_value) = node.get("$ref") {
                    return self.expand_ref(h, node, ref_value, path);
                }
                Kind::Object(self.convert_entries(h, node, path, false))
            }
```

Add these imports at the top of `load.rs`:
```rust
use suspect_ref::RefError;

use crate::discover::is_remote;
use crate::merge::merge;
```

Add these methods inside `impl<'ws> Loader<'ws>`:
```rust
    fn uri_of(&self, doc: usize) -> String {
        self.ws.get_by_id(doc).map(|h| h.uri().to_string()).unwrap_or_default()
    }

    fn expand_ref(&mut self, h: &DocHandle<'ws>, node: NodeRef<'_>, ref_value: NodeRef<'_>, path: &[String]) -> Node {
        let ref_span = Some(span_of(h.uri(), ref_value));
        let null = || Node::with_span(Kind::Null, Some(span_of(h.uri(), ref_value)));
        let raw = String::from_utf8_lossy(&ref_value.decoded_scalar()).into_owned();

        if is_remote(&raw) {
            self.diagnostics.push(
                Diagnostic::error(codes::REMOTE_UNSUPPORTED, format!("remote `$ref` `{raw}` is not supported yet"))
                    .at(ref_span),
            );
            return null();
        }
        let target = match h.ref_target(ref_value) {
            Ok(t) => t,
            Err(e) => {
                let code = match &e {
                    RefError::MissingPointer { .. } => codes::POINTER_NOT_FOUND,
                    RefError::RemoteDenied { .. } => codes::REMOTE_UNSUPPORTED,
                    _ => codes::NOT_FOUND,
                };
                self.diagnostics.push(Diagnostic::error(code, format!("cannot resolve `$ref` `{raw}`: {e}")).at(ref_span));
                return null();
            }
        };
        let key = (target.doc, target.pointer.to_path());
        if self.stack.contains(&key) {
            let mut chain: Vec<String> = self.stack.iter().map(|(d, p)| format!("{}#{p}", self.uri_of(*d))).collect();
            chain.push(format!("{}#{}", self.uri_of(key.0), key.1));
            self.diagnostics.push(
                Diagnostic::error(codes::CYCLE, format!("`$ref` cycle: `{raw}` leads back to a location already being expanded"))
                    .at(ref_span)
                    .note(format!("chain: {}", chain.join(" → "))),
            );
            return null();
        }
        let Some(target_doc) = self.ws.get_by_id(target.doc) else {
            self.diagnostics.push(Diagnostic::error(codes::NOT_FOUND, format!("cannot resolve `$ref` `{raw}`")).at(ref_span));
            return null();
        };
        if !self.ensure_valid(&target_doc) {
            return null();
        }
        let target_node = match target_doc.node_at_pointer(&target.pointer) {
            Ok(n) => n,
            Err(e) => {
                self.diagnostics.push(
                    Diagnostic::error(codes::POINTER_NOT_FOUND, format!("cannot resolve `$ref` `{raw}`: {e}")).at(ref_span),
                );
                return null();
            }
        };

        self.stack.push(key);
        let mut expanded = self.convert(&target_doc, target_node, path);
        self.stack.pop();

        let siblings = self.convert_entries(h, node, path, true);
        if siblings.is_empty() {
            return expanded;
        }
        if !matches!(expanded.kind, Kind::Object(_)) {
            self.diagnostics.push(
                Diagnostic::error(codes::REF_SIBLINGS, format!("`$ref` `{raw}` has sibling keys, but its target is not a mapping"))
                    .at(ref_span),
            );
            return expanded;
        }
        merge(&mut expanded, Node::with_span(Kind::Object(siblings), Some(span_of(h.uri(), node))), path);
        expanded
    }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p veneer-resolve`
Expected: the `refs` tests (5) pass, and the `load` and `merge` tests still pass. If `missing_targets_and_bad_siblings` shows a different order, sort `diags` with `veneer_spec::sort_diagnostics` in the test. The intended order is document order, which already matches sorting by span.

- [ ] **Step 6: Commit**

```bash
git add crates/veneer-resolve
git commit -m "feat(resolve): expand \$ref with sibling merge, cycle and missing-target diagnostics"
```

---

### Task 8: `extends` layering and spec discovery

**Files:**
- Create: `crates/veneer-resolve/src/extends.rs`
- Modify: `crates/veneer-resolve/src/discover.rs`, `crates/veneer-resolve/src/lib.rs`
- Test: `crates/veneer-resolve/tests/extends.rs`, `crates/veneer-resolve/tests/discover.rs`

**Interfaces:**
- Consumes: `Loader` (Tasks 6–7), `merge` (Task 5), `is_remote` (Task 7).
- Produces:
  - `pub fn extends::load_layered(loader: &mut Loader<'_>, path: &Path, chain: &mut Vec<PathBuf>) -> Option<Node>`: returns the layered tree with `extends` removed.
  - `pub fn discover::locate(arg: Option<&str>, cwd: &Path) -> Result<PathBuf, Diagnostic>`: returns the canonical entry path.

Rules: `extends` is a string or a list of strings. Entries are local paths relative to the declaring file. Layers merge in list order, then the declaring document merges on top. Extended specs may themselves `extends`. A cycle → `V1003`. A remote entry → `V1007`. An unreadable entry → `V1001`. A non-string entry → `V1009`.

`locate`:
- No arg: walk up from `cwd` to the first `<dir>/.veneer/veneer.yaml`. If none → `V1012`.
- Remote arg → `V1007`.
- A file arg is used as-is.
- A dir arg uses `<dir>/.veneer/veneer.yaml`, else `<dir>/veneer.yaml`, else `V1012`.
- A nonexistent arg → `V1012`.

- [ ] **Step 1: Write the failing tests**

`crates/veneer-resolve/tests/extends.rs`:
```rust
mod common;

use serde_json::json;
use suspect_ref::WorkspaceBuilder;
use veneer_resolve::{extends::load_layered, load::Loader};
use veneer_spec::{codes, Diagnostic, Node};

fn layered(entry: &std::path::Path) -> (Option<Node>, Vec<Diagnostic>) {
    let ws = WorkspaceBuilder::new().build().unwrap();
    let mut loader = Loader::new(&ws);
    let node = load_layered(&mut loader, entry, &mut Vec::new());
    (node, loader.diagnostics)
}

#[test]
fn layers_merge_in_order_and_extends_is_consumed() {
    let (_d, entry) = common::tree(&[
        (".veneer/veneer.yaml", "version: 1\nextends: [./base.yaml, ./team.yaml]\nenv:\n  DEBUG: ~\n  APP: \"1\"\npackages: [libpq-dev]\n"),
        (".veneer/base.yaml", "version: 1\nbase: ubuntu:24.04\npackages: [git]\nenv:\n  DEBUG: \"1\"\n  LOG: info\n"),
        (".veneer/team.yaml", "extends: ./org/root.yaml\nenv:\n  LOG: debug\n"),
        (".veneer/org/root.yaml", "images: [redis:7]\n"),
    ]);
    let (node, diags) = layered(&entry);
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(
        node.unwrap().to_json(),
        json!({
            "version": 1, "base": "ubuntu:24.04", "packages": ["git", "libpq-dev"],
            "env": {"LOG": "debug", "APP": "1"}, "images": ["redis:7"]
        })
    );
}

#[test]
fn extends_cycles_remote_and_missing_are_reported() {
    let (_d, entry) = common::tree(&[
        ("veneer.yaml", "version: 1\nextends: [./a.yaml, github:acme/envs, ./gone.yaml, 5]\n"),
        ("a.yaml", "extends: [./veneer.yaml]\n"),
    ]);
    let (_, diags) = layered(&entry);
    let got: Vec<&str> = diags.iter().map(|d| d.code).collect();
    assert_eq!(got, vec![codes::CYCLE, codes::REMOTE_UNSUPPORTED, codes::NOT_FOUND, codes::SCHEMA]);
}
```

`crates/veneer-resolve/tests/discover.rs`:
```rust
mod common;

use veneer_resolve::discover::locate;
use veneer_spec::codes;

#[test]
fn walks_up_to_dot_veneer() {
    let (dir, entry) = common::tree(&[(".veneer/veneer.yaml", "version: 1\n"), ("app/src/.keep", "")]);
    let cwd = dir.path().join("app/src").canonicalize().unwrap();
    assert_eq!(locate(None, &cwd).unwrap(), entry);
}

#[test]
fn explicit_file_and_dir_arguments() {
    let (dir, entry) = common::tree(&[(".veneer/veneer.yaml", "version: 1\n"), ("other/veneer.yaml", "version: 1\n")]);
    let root = dir.path().canonicalize().unwrap();
    assert_eq!(locate(Some("."), &root).unwrap(), entry);
    assert_eq!(locate(Some("other"), &root).unwrap(), root.join("other/veneer.yaml"));
    assert_eq!(locate(Some(".veneer/veneer.yaml"), &root).unwrap(), entry);
}

#[test]
fn missing_and_remote_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    assert_eq!(locate(None, &root).unwrap_err().code, codes::SPEC_NOT_FOUND);
    assert_eq!(locate(Some("nope"), &root).unwrap_err().code, codes::SPEC_NOT_FOUND);
    assert_eq!(locate(Some("github:acme/app@main"), &root).unwrap_err().code, codes::REMOTE_UNSUPPORTED);
}
```

Add to `[dev-dependencies]` in `crates/veneer-resolve/Cargo.toml`:
```toml
suspect-ref.workspace = true
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p veneer-resolve --test extends --test discover`
Expected: FAIL to compile (`extends` module and `locate` missing).

- [ ] **Step 3: Implement `extends.rs`**

`crates/veneer-resolve/src/extends.rs`:
```rust
//! `extends` layering (design spec §4.7 resolution order).

use std::path::{Path, PathBuf};

use veneer_spec::{codes, Diagnostic, Kind, Node};

use crate::discover::is_remote;
use crate::load::Loader;
use crate::merge::merge;

/// Loads `path` and everything it extends. `chain` holds the files currently being layered.
pub fn load_layered(loader: &mut Loader<'_>, path: &Path, chain: &mut Vec<PathBuf>) -> Option<Node> {
    let mut doc = loader.load_file(path)?;
    let extends = match &mut doc.kind {
        Kind::Object(map) => map.shift_remove("extends"),
        _ => None,
    };
    let mut acc = Node::object();
    chain.push(path.to_path_buf());
    if let Some(ext) = extends {
        let items = match ext.kind {
            Kind::Array(items) => items,
            Kind::Str(_) => vec![ext],
            Kind::Null => Vec::new(),
            _ => {
                loader.diagnostics.push(Diagnostic::error(codes::SCHEMA, "`extends` must be a path or a list of paths").at(ext.span));
                Vec::new()
            }
        };
        let base_dir = path.parent().unwrap_or(Path::new("/"));
        for item in items {
            let Some(raw) = item.as_str() else {
                loader.diagnostics.push(Diagnostic::error(codes::SCHEMA, "`extends` entries must be strings").at(item.span.clone()));
                continue;
            };
            if is_remote(raw) {
                loader.diagnostics.push(
                    Diagnostic::error(codes::REMOTE_UNSUPPORTED, format!("remote `extends` source `{raw}` is not supported yet"))
                        .at(item.span.clone()),
                );
                continue;
            }
            let target = match base_dir.join(raw).canonicalize() {
                Ok(p) => p,
                Err(e) => {
                    loader.diagnostics.push(
                        Diagnostic::error(codes::NOT_FOUND, format!("cannot read extended spec `{raw}`: {e}")).at(item.span.clone()),
                    );
                    continue;
                }
            };
            if chain.contains(&target) {
                let mut names: Vec<String> = chain.iter().map(|p| p.display().to_string()).collect();
                names.push(target.display().to_string());
                loader.diagnostics.push(
                    Diagnostic::error(codes::CYCLE, format!("`extends` cycle through `{raw}`"))
                        .at(item.span.clone())
                        .note(format!("chain: {}", names.join(" → "))),
                );
                continue;
            }
            if let Some(layer) = load_layered(loader, &target, chain) {
                merge(&mut acc, layer, &[]);
            }
        }
    }
    chain.pop();
    merge(&mut acc, doc, &[]);
    Some(acc)
}
```

Add `pub mod extends;` to `lib.rs`.

- [ ] **Step 4: Implement `locate`**

Append to `crates/veneer-resolve/src/discover.rs`:
```rust
use std::path::{Path, PathBuf};

use veneer_spec::{codes, Diagnostic};

fn canonical(p: &Path) -> Result<PathBuf, Diagnostic> {
    p.canonicalize()
        .map_err(|e| Diagnostic::error(codes::SPEC_NOT_FOUND, format!("cannot open {}: {e}", p.display())))
}

/// Finds the entry spec for `arg` (or by walking up from `cwd` when `arg` is `None`).
pub fn locate(arg: Option<&str>, cwd: &Path) -> Result<PathBuf, Diagnostic> {
    let Some(arg) = arg else {
        for dir in cwd.ancestors() {
            let candidate = dir.join(".veneer/veneer.yaml");
            if candidate.is_file() {
                return canonical(&candidate);
            }
        }
        return Err(Diagnostic::error(
            codes::SPEC_NOT_FOUND,
            format!("no .veneer/veneer.yaml found in {} or any parent directory", cwd.display()),
        )
        .note("run `veneer check <path>` or create .veneer/veneer.yaml"));
    };
    if is_remote(arg) {
        return Err(Diagnostic::error(codes::REMOTE_UNSUPPORTED, format!("remote spec source `{arg}` is not supported yet")));
    }
    let path = cwd.join(arg);
    if path.is_file() {
        return canonical(&path);
    }
    if path.is_dir() {
        for candidate in [path.join(".veneer/veneer.yaml"), path.join("veneer.yaml")] {
            if candidate.is_file() {
                return canonical(&candidate);
            }
        }
    }
    Err(Diagnostic::error(codes::SPEC_NOT_FOUND, format!("no veneer spec at {}", path.display())))
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p veneer-resolve`
Expected: all pass (`extends` 2, `discover` 3, plus earlier tests).

- [ ] **Step 6: Commit**

```bash
git add crates/veneer-resolve
git commit -m "feat(resolve): add extends layering and spec discovery"
```

---

### Task 9: Overlays and YAML emit

**Files:**
- Create: `crates/veneer-resolve/src/overlay.rs`
- Modify: `crates/veneer-resolve/Cargo.toml`, `crates/veneer-resolve/src/lib.rs`
- Test: `crates/veneer-resolve/tests/overlay.rs`

**Interfaces:**
- Consumes: `Node::{to_json, from_json, span_index, reattach_spans}`.
- Produces:
  - `pub fn overlay::apply_overlays(tree: Node, overlays: &[PathBuf], diags: &mut Vec<Diagnostic>) -> Node`
  - `pub fn overlay::to_yaml(node: &Node) -> String`

Behaviour:
- Each overlay applies in order to the current tree.
- Overlay syntax errors → `V1000`. Unreadable → `V1001`. Invalid structure or a failed apply → `V1011`. In each of these cases the overlay is skipped.
- `validate_overlay` hints → `W1101`, except code `overlay-action-missing-description`, which is ignored.
- Each unmatched target → `W1102`.
- Spans are re-attached by pointer after applying.

suspect facts:
- `LowDoc::with_format(Uri, Source, suspect_syntax::Format::Json)`, `Source::from_vec(Vec<u8>)`, `Source::from_path(&Path) -> io::Result<Source>`, `LowDoc::parse(Uri, Source)`.
- `suspect_overlay::{OverlayDoc::parse(&LowDoc) -> Result<OverlayDoc, OverlayError>, validate_overlay(&OverlayDoc) -> Vec<OverlayDiagnostic { code: &'static str, message: String }>, apply(&OverlayDoc, NodeRef) -> Result<Applied { output: Value, applied_actions, unmatched_targets: Vec<String> }, OverlayError>}`.
- `suspect_overlay::Value::{Null, Bool, Int, Float, Str(Box<str>), Array(Vec<Value>), Object(Vec<(Box<str>, Value)>)}` with `to_json()`, `to_yaml()`.

- [ ] **Step 1: Add dependency**

Add `suspect-overlay.workspace = true` to `[dependencies]` in `crates/veneer-resolve/Cargo.toml`.

- [ ] **Step 2: Write the failing test**

`crates/veneer-resolve/tests/overlay.rs`:
```rust
mod common;

use serde_json::json;
use veneer_resolve::{load::load_single, overlay::{apply_overlays, to_yaml}};
use veneer_spec::codes;

const OVERLAY: &str = "overlay: 1.0.0\ninfo: { title: bench, version: \"1\" }\nactions:\n  - target: $.env\n    update: { BENCH: \"1\" }\n  - target: $.services.db\n    remove: true\n  - target: $.nothing\n    remove: true\n";

#[test]
fn applies_actions_in_order_and_keeps_spans() {
    let (dir, entry) = common::tree(&[
        ("veneer.yaml", "version: 1\nenv:\n  A: \"1\"\nservices:\n  db:\n    image: postgres:16\n"),
        ("o.yaml", OVERLAY),
    ]);
    let tree = load_single(&entry).0.unwrap();
    let mut diags = Vec::new();
    let out = apply_overlays(tree, &[dir.path().join("o.yaml")], &mut diags);
    assert_eq!(out.to_json(), json!({"version": 1, "env": {"A": "1", "BENCH": "1"}, "services": {}}));
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::OVERLAY_UNMATCHED);
    assert!(out.pointer("/env/A").unwrap().span.is_some());
}

#[test]
fn invalid_overlay_is_reported_and_skipped() {
    let (dir, entry) = common::tree(&[("veneer.yaml", "version: 1\n"), ("bad.yaml", "not_an_overlay: true\n")]);
    let tree = load_single(&entry).0.unwrap();
    let mut diags = Vec::new();
    let out = apply_overlays(tree, &[dir.path().join("bad.yaml"), dir.path().join("missing.yaml")], &mut diags);
    assert_eq!(out.to_json(), json!({"version": 1}));
    let got: Vec<&str> = diags.iter().map(|d| d.code).collect();
    assert_eq!(got, vec![codes::OVERLAY, codes::NOT_FOUND]);
}

#[test]
fn yaml_emit_contains_keys_and_values() {
    let (_d, entry) = common::tree(&[("veneer.yaml", "version: 1\npackages: [git]\n")]);
    let yaml = to_yaml(&load_single(&entry).0.unwrap());
    assert!(yaml.contains("version: 1"), "{yaml}");
    assert!(yaml.contains("git"), "{yaml}");
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p veneer-resolve --test overlay`
Expected: FAIL to compile (`overlay` module missing).

- [ ] **Step 4: Write the implementation**

`crates/veneer-resolve/src/overlay.rs`:
```rust
//! Overlay 1.0 application (design spec §4.6) and YAML emit.

use std::path::{Path, PathBuf};

use suspect_low::LowDoc;
use suspect_source::{LineIndex, Source, Uri};
use veneer_spec::{codes, Diagnostic, Kind, Node, Span};

const IGNORED_HINTS: &[&str] = &["overlay-action-missing-description"];

pub fn apply_overlays(tree: Node, overlays: &[PathBuf], diags: &mut Vec<Diagnostic>) -> Node {
    let index = tree.span_index();
    let mut current = tree.to_json();
    for path in overlays {
        if let Some(next) = apply_one(&current, path, diags) {
            current = next;
        }
    }
    let mut out = Node::from_json(&current);
    out.reattach_spans(&index);
    out
}

fn file_start(uri: &Uri) -> Option<Span> {
    Some(Span { uri: uri.to_string(), start: 0, end: 0, line: 1, col: 1 })
}

fn apply_one(current: &serde_json::Value, path: &Path, diags: &mut Vec<Diagnostic>) -> Option<serde_json::Value> {
    let shown = path.display().to_string();
    let source = match Source::from_path(path) {
        Ok(s) => s,
        Err(e) => {
            diags.push(Diagnostic::error(codes::NOT_FOUND, format!("cannot read overlay {shown}: {e}")));
            return None;
        }
    };
    let bytes = source.bytes().to_vec();
    let uri = match path.canonicalize().ok().and_then(|p| Uri::from_path(&p).ok()) {
        Some(u) => u,
        None => Uri::from(shown.as_str()),
    };
    let doc = LowDoc::parse(uri.clone(), source);
    if !doc.syntax_errors().is_empty() {
        let index = LineIndex::new(&bytes);
        for e in doc.syntax_errors() {
            let (line, col) = index.line_col(&bytes, e.range.start);
            let span = Span { uri: uri.to_string(), start: e.range.start, end: e.range.end, line: line + 1, col: col + 1 };
            diags.push(Diagnostic::error(codes::SYNTAX, e.message.clone()).at(Some(span)));
        }
        return None;
    }
    let overlay = match suspect_overlay::OverlayDoc::parse(&doc) {
        Ok(o) => o,
        Err(e) => {
            diags.push(Diagnostic::error(codes::OVERLAY, format!("invalid overlay {shown}: {e}")).at(file_start(&uri)));
            return None;
        }
    };
    for hint in suspect_overlay::validate_overlay(&overlay) {
        if !IGNORED_HINTS.contains(&hint.code) {
            diags.push(
                Diagnostic::warning(codes::OVERLAY_HINT, format!("overlay {shown}: {} ({})", hint.message, hint.code))
                    .at(file_start(&uri)),
            );
        }
    }
    let target_bytes = serde_json::to_vec(current).expect("JSON values always serialize");
    let target = LowDoc::with_format(
        Uri::from("mem:///resolved.json"),
        Source::from_vec(target_bytes),
        suspect_syntax::Format::Json,
    );
    match suspect_overlay::apply(&overlay, target.root()) {
        Ok(applied) => {
            for t in &applied.unmatched_targets {
                diags.push(
                    Diagnostic::warning(codes::OVERLAY_UNMATCHED, format!("overlay {shown}: target `{t}` matched nothing"))
                        .at(file_start(&uri)),
                );
            }
            serde_json::from_str(&applied.output.to_json()).ok()
        }
        Err(e) => {
            diags.push(Diagnostic::error(codes::OVERLAY, format!("overlay {shown} failed: {e}")).at(file_start(&uri)));
            None
        }
    }
}

fn to_overlay_value(node: &Node) -> suspect_overlay::Value {
    use suspect_overlay::Value;
    match &node.kind {
        Kind::Null => Value::Null,
        Kind::Bool(b) => Value::Bool(*b),
        Kind::Int(i) => Value::Int(*i),
        Kind::Float(f) => Value::Float(*f),
        Kind::Str(s) => Value::Str(s.as_str().into()),
        Kind::Array(items) => Value::Array(items.iter().map(to_overlay_value).collect()),
        Kind::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.as_str().into(), to_overlay_value(v))).collect()),
    }
}

/// Canonical YAML for a resolved tree.
pub fn to_yaml(node: &Node) -> String {
    to_overlay_value(node).to_yaml()
}
```

Add `pub mod overlay;` to `lib.rs`.

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p veneer-resolve --test overlay`
Expected: 3 passed. If `invalid_overlay_is_reported_and_skipped` sees `V1011` *and* hint warnings, the overlay parsed but failed validation. In that case, change the fixture body to `- not a mapping\n`, which `OverlayDoc::parse` rejects.

- [ ] **Step 6: Commit**

```bash
git add crates/veneer-resolve Cargo.lock
git commit -m "feat(resolve): apply Overlay 1.0 overlays and emit YAML"
```

---

### Task 10: Template phase checks

**Files:**
- Create: `crates/veneer-template/Cargo.toml`, `crates/veneer-template/src/lib.rs`
- Test: `crates/veneer-template/tests/phase.rs`

**Interfaces:**
- Consumes: `Node`, `Kind`, `Diagnostic`, `codes`.
- Produces: `pub fn veneer_template::check_templates(tree: &Node) -> Vec<Diagnostic>`.

Rules (spec §3, §4.2):
- A template is any string value containing `{{` or `{%`.
- Allowed roots: `vars`, `env`, `workspace`, `secrets`, `os`, `veneer`. Any other root → `V2003`.
- Parse failure → `V2002`.
- Build-phase locations (`base`, `packages`, `tools`, `deps`, `images`, `hooks.build`) must not reference `workspace.*` or `secrets.*` (→ `V2001`), directly or through a `vars.X` whose own templates reach them transitively (→ `V2001`, naming the cause).

- [ ] **Step 1: Create the crate and write the failing test**

`crates/veneer-template/Cargo.toml`:
```toml
[package]
name = "veneer-template"
description = "Template validation for veneer specs"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
veneer-spec.workspace = true
minijinja.workspace = true

[dev-dependencies]
serde_json.workspace = true
```

`crates/veneer-template/tests/phase.rs`:
```rust
use serde_json::json;
use veneer_spec::{codes, Node};
use veneer_template::check_templates;

fn codes_of(v: serde_json::Value) -> Vec<(&'static str, String)> {
    check_templates(&Node::from_json(&v)).into_iter().map(|d| (d.code, d.message)).collect()
}

#[test]
fn workspace_values_are_fine_in_workspace_blocks() {
    assert!(codes_of(json!({
        "vars": {"db": "bench_{{ workspace.id }}"},
        "env": {"URL": "pg://{{ vars.db }}", "T": "{{ secrets.TOKEN }}", "H": "{{ env.HOME }}"},
        "tools": {"python": "{{ vars.py }}", "node": "{{ os.arch }}"},
        "hooks": {"workspace": ["echo {{ workspace.root }}"]}
    }))
    .is_empty());
}

#[test]
fn direct_workspace_reference_in_build_block_is_v2001() {
    let got = codes_of(json!({"packages": ["lib-{{ workspace.id }}"], "hooks": {"build": [{"run": "echo {{ secrets.X }}"}]}}));
    assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), vec![codes::PHASE, codes::PHASE]);
}

#[test]
fn transitive_taint_through_vars_is_v2001() {
    let got = codes_of(json!({
        "vars": {"a": "{{ secrets.T }}", "b": "x-{{ vars.a }}", "c": "plain"},
        "tools": {"x": "{{ vars.b }}", "y": "{{ vars.c }}"}
    }));
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].0, codes::PHASE);
    assert!(got[0].1.contains("vars.b") && got[0].1.contains("secrets.T"), "{}", got[0].1);
}

#[test]
fn unknown_roots_and_syntax_errors() {
    let got = codes_of(json!({"env": {"A": "{{ nope.x }}", "B": "{{ vars.a "}}));
    let mut c: Vec<_> = got.iter().map(|g| g.0).collect();
    c.sort();
    assert_eq!(c, vec![codes::TEMPLATE_SYNTAX, codes::TEMPLATE_UNKNOWN_VAR]);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p veneer-template`
Expected: FAIL to compile (`check_templates` missing).

- [ ] **Step 3: Write the implementation**

`crates/veneer-template/src/lib.rs`:
```rust
//! veneer-template: validates minijinja templates in a resolved spec tree.

use std::collections::{HashMap, HashSet};

use minijinja::Environment;
use veneer_spec::{codes, Diagnostic, Kind, Node};

const ROOTS: &[&str] = &["vars", "env", "workspace", "secrets", "os", "veneer"];
const BUILD_KEYS: &[&str] = &["base", "packages", "tools", "deps", "images"];
const WORKSPACE_ONLY: &[&str] = &["workspace", "secrets"];

fn is_template(s: &str) -> bool {
    s.contains("{{") || s.contains("{%")
}

fn references(env: &Environment<'_>, s: &str) -> Result<HashSet<String>, minijinja::Error> {
    Ok(env.template_from_str(s)?.undeclared_variables(true))
}

fn is_build_phase(path: &[String]) -> bool {
    match path {
        [first, ..] if BUILD_KEYS.contains(&first.as_str()) => true,
        [hooks, build, ..] => hooks == "hooks" && build == "build",
        _ => false,
    }
}

fn walk<'a>(node: &'a Node, path: &mut Vec<String>, f: &mut dyn FnMut(&[String], &'a Node)) {
    f(path, node);
    match &node.kind {
        Kind::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                path.push(i.to_string());
                walk(child, path, f);
                path.pop();
            }
        }
        Kind::Object(map) => {
            for (k, child) in map {
                path.push(k.clone());
                walk(child, path, f);
                path.pop();
            }
        }
        _ => {}
    }
}

/// var name -> the workspace-only reference that taints it (e.g. "secrets.T").
fn tainted_vars(tree: &Node, env: &Environment<'_>) -> HashMap<String, String> {
    let Some(vars) = tree.get("vars").and_then(Node::as_object) else { return HashMap::new() };
    let direct: Vec<(String, HashSet<String>)> = vars
        .iter()
        .map(|(name, value)| {
            let mut refs = HashSet::new();
            walk(value, &mut Vec::new(), &mut |_, n| {
                if let Some(s) = n.as_str().filter(|s| is_template(s)) {
                    refs.extend(references(env, s).unwrap_or_default());
                }
            });
            (name.clone(), refs)
        })
        .collect();
    let mut tainted: HashMap<String, String> = HashMap::new();
    loop {
        let mut changed = false;
        for (name, refs) in &direct {
            if tainted.contains_key(name) {
                continue;
            }
            let cause = refs.iter().find_map(|r| {
                let mut parts = r.split('.');
                match parts.next() {
                    Some(root) if WORKSPACE_ONLY.contains(&root) => Some(r.clone()),
                    Some("vars") => parts.next().and_then(|v| tainted.get(v)).cloned(),
                    _ => None,
                }
            });
            if let Some(cause) = cause {
                tainted.insert(name.clone(), cause);
                changed = true;
            }
        }
        if !changed {
            return tainted;
        }
    }
}

pub fn check_templates(tree: &Node) -> Vec<Diagnostic> {
    let env = Environment::new();
    let tainted = tainted_vars(tree, &env);
    let mut diags = Vec::new();
    walk(tree, &mut Vec::new(), &mut |path, node| {
        let Some(s) = node.as_str().filter(|s| is_template(s)) else { return };
        let refs = match references(&env, s) {
            Ok(r) => r,
            Err(e) => {
                diags.push(Diagnostic::error(codes::TEMPLATE_SYNTAX, format!("invalid template: {e}")).at(node.span.clone()));
                return;
            }
        };
        let build = is_build_phase(path);
        let mut refs: Vec<String> = refs.into_iter().collect();
        refs.sort();
        for r in refs {
            let mut parts = r.split('.');
            let root = parts.next().unwrap_or_default();
            if !ROOTS.contains(&root) {
                diags.push(
                    Diagnostic::error(codes::TEMPLATE_UNKNOWN_VAR, format!("unknown template variable `{r}`"))
                        .at(node.span.clone())
                        .note("available roots: vars, env, workspace, secrets, os, veneer"),
                );
                continue;
            }
            if !build {
                continue;
            }
            if WORKSPACE_ONLY.contains(&root) {
                diags.push(
                    Diagnostic::error(codes::PHASE, format!("`{r}` is only available at workspace time, but this value is used at build time"))
                        .at(node.span.clone())
                        .note("move the value to a workspace-phase block (env, workspace, services, hooks.workspace)"),
                );
            } else if root == "vars" {
                if let Some((name, cause)) = parts.next().and_then(|v| tainted.get(v).map(|c| (v, c))) {
                    diags.push(
                        Diagnostic::error(
                            codes::PHASE,
                            format!("`vars.{name}` depends on `{cause}`, which is only available at workspace time, but this value is used at build time"),
                        )
                        .at(node.span.clone()),
                    );
                }
            }
        }
    });
    diags
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p veneer-template`
Expected: 4 passed. If `undeclared_variables` doesn't exist, check that `minijinja` resolved to 2.x (`cargo tree -p minijinja`). The method is `Template::undeclared_variables(&self, nested: bool) -> HashSet<String>`.

- [ ] **Step 5: Commit**

```bash
git add crates/veneer-template Cargo.lock
git commit -m "feat(template): add build/workspace phase and template validation"
```

---

### Task 11: `resolve()` pipeline

**Files:**
- Modify: `crates/veneer-resolve/Cargo.toml`, `crates/veneer-resolve/src/lib.rs`
- Test: `crates/veneer-resolve/tests/pipeline.rs`

**Interfaces:**
- Consumes: `extends::load_layered`, `overlay::apply_overlays`, `veneer_template::check_templates`, `veneer_spec::{from_node, Spec}`.
- Produces:
  - `#[derive(Debug, Clone, Default)] pub struct ResolveOptions { pub overlays: Vec<PathBuf> }`
  - `#[derive(Debug, Default)] pub struct Resolved { pub tree: Option<Node>, pub spec: Option<Spec>, pub documents: Vec<String>, pub diagnostics: Vec<Diagnostic> }`
  - `pub fn resolve(entry: &Path, opts: &ResolveOptions) -> Resolved`: diagnostics are sorted.
  - Re-export `pub use discover::locate;`

Pipeline:
1. Load the entry with `extends` layering (which also expands `$ref`).
2. Apply overlays.
3. If there are no errors so far: check templates, deserialize `Spec`, then `validate()`. Warn `W1201` if `use:` is non-empty.
4. Sort the diagnostics.

- [ ] **Step 1: Add dependency and write the failing test**

Add `veneer-template.workspace = true` to `[dependencies]` in `crates/veneer-resolve/Cargo.toml`.

`crates/veneer-resolve/tests/pipeline.rs`:
```rust
mod common;

use veneer_resolve::{resolve, ResolveOptions};
use veneer_spec::codes;

#[test]
fn resolves_layers_refs_and_types() {
    let (_d, entry) = common::tree(&[
        (".veneer/veneer.yaml", "version: 1\nextends: [./base.yaml]\ntools:\n  $ref: ./tools.yaml\nservices:\n  db:\n    image: postgres:16\n"),
        (".veneer/base.yaml", "version: 1\npackages: [git]\n"),
        (".veneer/tools.yaml", "node: 22\n"),
    ]);
    let r = resolve(&entry, &ResolveOptions::default());
    assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics);
    assert_eq!(r.documents.len(), 3);
    let spec = r.spec.unwrap();
    assert_eq!(spec.packages.len(), 1);
    assert!(spec.services.contains_key("db"));
}

#[test]
fn schema_errors_point_at_the_offending_key() {
    let (_d, entry) = common::tree(&[("veneer.yaml", "version: 1\npakages: [git]\n")]);
    let r = resolve(&entry, &ResolveOptions::default());
    assert_eq!(r.diagnostics.len(), 1);
    let d = &r.diagnostics[0];
    assert_eq!(d.code, codes::SCHEMA);
    assert!(d.message.contains("unknown field `pakages`"), "{}", d.message);
    assert_eq!(d.span.as_ref().unwrap().line, 2);
    assert!(r.spec.is_none());
}

#[test]
fn semantic_phase_and_module_findings_are_combined_and_sorted() {
    let (_d, entry) = common::tree(&[(
        "veneer.yaml",
        "version: 2\nuse: [veneer/harness/claude-code@1]\npackages: [\"lib-{{ workspace.id }}\"]\n",
    )]);
    let r = resolve(&entry, &ResolveOptions::default());
    let got: Vec<&str> = r.diagnostics.iter().map(|d| d.code).collect();
    assert_eq!(got, vec![codes::VERSION, codes::MODULES_UNSUPPORTED, codes::PHASE]);
}

#[test]
fn resolution_errors_skip_typing() {
    let (_d, entry) = common::tree(&[("veneer.yaml", "version: 1\ntools:\n  $ref: ./gone.yaml\n")]);
    let r = resolve(&entry, &ResolveOptions::default());
    assert_eq!(r.diagnostics.iter().map(|d| d.code).collect::<Vec<_>>(), vec![codes::NOT_FOUND]);
    assert!(r.spec.is_none());
    assert!(r.tree.is_some());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p veneer-resolve --test pipeline`
Expected: FAIL to compile (`resolve` missing).

- [ ] **Step 3: Write the implementation**

Replace `crates/veneer-resolve/src/lib.rs` with:
```rust
//! veneer-resolve: load, layer, and resolve veneer specs.

use std::path::{Path, PathBuf};

use suspect_ref::WorkspaceBuilder;
use veneer_spec::{codes, from_node, has_errors, sort_diagnostics, Diagnostic, Node, Spec};

pub mod discover;
pub mod extends;
pub mod load;
pub mod merge;
pub mod overlay;

pub use discover::locate;

#[derive(Debug, Clone, Default)]
pub struct ResolveOptions {
    /// Overlay files, applied in order after layering.
    pub overlays: Vec<PathBuf>,
}

#[derive(Debug, Default)]
pub struct Resolved {
    /// The merged tree (present unless the entry itself failed to load).
    pub tree: Option<Node>,
    /// The typed spec (present only when resolution and typing succeeded).
    pub spec: Option<Spec>,
    /// URIs of every document read.
    pub documents: Vec<String>,
    /// Sorted diagnostics.
    pub diagnostics: Vec<Diagnostic>,
}

pub fn resolve(entry: &Path, opts: &ResolveOptions) -> Resolved {
    let mut out = Resolved::default();
    let ws = match WorkspaceBuilder::new().build() {
        Ok(ws) => ws,
        Err(e) => {
            out.diagnostics.push(Diagnostic::error(codes::NOT_FOUND, format!("cannot create workspace: {e}")));
            return out;
        }
    };
    let mut loader = load::Loader::new(&ws);
    let tree = extends::load_layered(&mut loader, entry, &mut Vec::new());
    out.documents = loader.documents.iter().cloned().collect();
    out.diagnostics.append(&mut loader.diagnostics);

    if let Some(mut tree) = tree {
        if !opts.overlays.is_empty() {
            tree = overlay::apply_overlays(tree, &opts.overlays, &mut out.diagnostics);
        }
        if !has_errors(&out.diagnostics) {
            out.diagnostics.extend(veneer_template::check_templates(&tree));
            match from_node::<Spec>(&tree) {
                Ok(spec) => {
                    for v in spec.validate() {
                        out.diagnostics.push(Diagnostic::error(v.code, v.message).at(tree.nearest_span(&v.pointer)));
                    }
                    if !spec.modules.is_empty() {
                        out.diagnostics.push(
                            Diagnostic::warning(codes::MODULES_UNSUPPORTED, "`use:` modules are not supported yet and were ignored")
                                .at(tree.nearest_span("/use")),
                        );
                    }
                    out.spec = Some(spec);
                }
                Err(e) => {
                    let pointer = e.pointer.clone().unwrap_or_default();
                    out.diagnostics.push(
                        Diagnostic::error(codes::SCHEMA, e.message)
                            .at(tree.nearest_span(&pointer))
                            .note(format!("at `{}`", if pointer.is_empty() { "/" } else { &pointer })),
                    );
                }
            }
        }
        out.tree = Some(tree);
    }
    sort_diagnostics(&mut out.diagnostics);
    out
}
```

- [ ] **Step 4: Run all resolve tests**

Run: `cargo test -p veneer-resolve`
Expected: all pass. For `semantic_phase_…`, the sorted order follows the spans: `version` (line 1), `use` (line 2), `packages` item (line 3).

- [ ] **Step 5: Commit**

```bash
git add crates/veneer-resolve Cargo.lock
git commit -m "feat(resolve): add resolve() pipeline with typing, validation, and phase checks"
```

---

### Task 12: `veneer` CLI: `check`, `schema`, diagnostics rendering, CI

**Files:**
- Create: `crates/veneer/Cargo.toml`, `crates/veneer/src/main.rs`, `crates/veneer/src/check.rs`, `crates/veneer/src/render.rs`
- Create fixtures: `crates/veneer/tests/fixtures/basic/.veneer/{veneer.yaml,base.yaml,tools.yaml,overlays/bench.overlay.yaml}`, `crates/veneer/tests/fixtures/basic/app/src/.keep`, `crates/veneer/tests/fixtures/cycle/.veneer/{veneer.yaml,a.yaml,b.yaml}`, `crates/veneer/tests/fixtures/unknown-key/.veneer/veneer.yaml`
- Create: `.github/workflows/ci.yml`
- Modify: `README.md`
- Test: `crates/veneer/tests/cli.rs`

**Interfaces:**
- Consumes: `veneer_resolve::{locate, resolve, ResolveOptions, Resolved, overlay::to_yaml}`, `veneer_spec::{Diagnostic, Severity, has_errors, json_schema_string}`.
- Produces: the binary `veneer` with these commands:
  - `veneer check [SPEC] [--resolved] [--format yaml|json] [--overlay FILE]... [--json]`
  - `veneer schema`

Output contract:
- Human diagnostics go to stderr, rendered by miette (no color when stderr is not a terminal or `NO_COLOR` is set). Source names are relative to the cwd when the file is under it.
- On success without `--resolved`, stdout is `ok <entry> (<n> documents, <w> warnings)`, where `<entry>` is relative to the cwd when possible.
- `--resolved` prints the tree as YAML (default) or pretty JSON.
- `--json` prints exactly one JSON object to stdout: `{"ok": bool, "entry": string|null, "documents": [..], "diagnostics": [..], "resolved": <tree>|null}`. With `--json`, nothing human-readable is printed.
- Exit `0` with no errors, `2` with errors.

- [ ] **Step 1: Crate manifest and fixtures**

`crates/veneer/Cargo.toml`:
```toml
[package]
name = "veneer"
description = "Define agent workspace environments once; stage them anywhere."
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[[bin]]
name = "veneer"
path = "src/main.rs"

[dependencies]
veneer-spec.workspace = true
veneer-resolve.workspace = true
suspect-source.workspace = true
clap.workspace = true
miette.workspace = true
serde_json.workspace = true

[dev-dependencies]
assert_cmd.workspace = true
predicates.workspace = true
insta.workspace = true
serde_json.workspace = true
tempfile.workspace = true
```

`crates/veneer/tests/fixtures/basic/.veneer/veneer.yaml`:
```yaml
version: 1
extends: [./base.yaml]
vars:
  python: "3.12"
  db_name: "bench_{{ workspace.id }}"
env:
  DEBUG: ~
  DATABASE_URL: "postgres://localhost/{{ vars.db_name }}"
packages: [libpq-dev]
tools:
  $ref: ./tools.yaml
  python: "{{ vars.python }}"
services:
  db:
    image: postgres:16
    env: { POSTGRES_DB: "{{ vars.db_name }}" }
```

`crates/veneer/tests/fixtures/basic/.veneer/base.yaml`:
```yaml
version: 1
base: ubuntu:24.04
packages: [git, libpq-dev]
env:
  LOG_LEVEL: info
  DEBUG: "1"
```

`crates/veneer/tests/fixtures/basic/.veneer/tools.yaml`:
```yaml
node: 22
"aqua:BurntSushi/ripgrep": latest
```

`crates/veneer/tests/fixtures/basic/.veneer/overlays/bench.overlay.yaml`:
```yaml
overlay: 1.0.0
info: { title: bench, version: "1" }
actions:
  - target: $.env
    description: enable bench mode
    update: { BENCH_MODE: "1" }
  - target: $.services.db
    description: no database in bench runs
    remove: true
```

`crates/veneer/tests/fixtures/basic/app/src/.keep`: empty file.

`crates/veneer/tests/fixtures/cycle/.veneer/veneer.yaml`:
```yaml
version: 1
tools:
  $ref: ./a.yaml
```
`crates/veneer/tests/fixtures/cycle/.veneer/a.yaml`: `$ref: ./b.yaml`
`crates/veneer/tests/fixtures/cycle/.veneer/b.yaml`: `$ref: ./a.yaml`

`crates/veneer/tests/fixtures/unknown-key/.veneer/veneer.yaml`:
```yaml
version: 1
pakages: [git]
```

- [ ] **Step 2: Write the failing tests**

`crates/veneer/tests/cli.rs`:
```rust
use std::path::PathBuf;

use assert_cmd::Command;
use predicates::str::contains;
use serde_json::{json, Value};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn veneer() -> Command {
    let mut cmd = Command::cargo_bin("veneer").unwrap();
    cmd.env("NO_COLOR", "1");
    cmd
}

#[test]
fn check_discovers_spec_from_a_subdirectory() {
    veneer()
        .current_dir(fixture("basic/app/src"))
        .arg("check")
        .assert()
        .success()
        .stdout(contains("ok ").and(contains("veneer.yaml")).and(contains("(3 documents, 0 warnings)")));
}

#[test]
fn check_resolved_json_is_the_merged_spec() {
    let out = veneer()
        .current_dir(fixture("basic"))
        .args(["check", "--resolved", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        v,
        json!({
            "version": 1,
            "base": "ubuntu:24.04",
            "packages": ["git", "libpq-dev"],
            "env": {"LOG_LEVEL": "info", "DATABASE_URL": "postgres://localhost/{{ vars.db_name }}"},
            "vars": {"python": "3.12", "db_name": "bench_{{ workspace.id }}"},
            "tools": {"node": 22, "aqua:BurntSushi/ripgrep": "latest", "python": "{{ vars.python }}"},
            "services": {"db": {"image": "postgres:16", "env": {"POSTGRES_DB": "{{ vars.db_name }}"}}}
        })
    );
}

#[test]
fn check_applies_overlays() {
    let out = veneer()
        .current_dir(fixture("basic"))
        .args(["check", "--resolved", "--format", "json", "--overlay", ".veneer/overlays/bench.overlay.yaml"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["env"]["BENCH_MODE"], json!("1"));
    assert_eq!(v["services"], json!({}));
}

#[test]
fn check_resolved_yaml_snapshot() {
    let out = veneer().current_dir(fixture("basic")).args(["check", "--resolved"]).assert().success().get_output().stdout.clone();
    insta::assert_snapshot!("resolved_basic_yaml", String::from_utf8(out).unwrap());
}

#[test]
fn check_json_reports_cycle_and_exits_2() {
    let out = veneer().current_dir(fixture("cycle")).args(["check", "--json"]).assert().code(2).get_output().stdout.clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["ok"], json!(false));
    assert_eq!(v["diagnostics"][0]["code"], json!("V1003"));
    assert_eq!(v["diagnostics"][0]["severity"], json!("error"));
}

#[test]
fn human_diagnostics_snapshot() {
    let out = veneer().current_dir(fixture("unknown-key")).arg("check").assert().code(2).get_output().stderr.clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("V1009") && text.contains("pakages") && text.contains(".veneer/veneer.yaml"), "{text}");
    insta::assert_snapshot!("unknown_key_human", text);
}

#[test]
fn missing_spec_is_v1012() {
    let dir = tempfile::tempdir().unwrap();
    veneer().current_dir(dir.path()).arg("check").assert().code(2).stderr(contains("V1012"));
}

#[test]
fn schema_prints_json_schema() {
    let out = veneer().arg("schema").assert().success().get_output().stdout.clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["additionalProperties"], json!(false));
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p veneer`
Expected: FAIL to compile (no `src/main.rs`).

- [ ] **Step 4: Implement rendering**

`crates/veneer/src/render.rs`:
```rust
//! Human diagnostic rendering through miette.

use std::fmt;
use std::io::IsTerminal;
use std::path::Path;

use miette::{GraphicalReportHandler, GraphicalTheme, LabeledSpan, NamedSource, SourceCode};
use suspect_source::Uri;
use veneer_spec::{Diagnostic, Severity};

struct Rendered {
    diag: Diagnostic,
    source: Option<NamedSource<String>>,
}

impl fmt::Debug for Rendered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.diag.message)
    }
}

impl fmt::Display for Rendered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.diag.message)
    }
}

impl std::error::Error for Rendered {}

impl miette::Diagnostic for Rendered {
    fn code<'a>(&'a self) -> Option<Box<dyn fmt::Display + 'a>> {
        Some(Box::new(self.diag.code))
    }

    fn severity(&self) -> Option<miette::Severity> {
        Some(match self.diag.severity {
            Severity::Error => miette::Severity::Error,
            Severity::Warning => miette::Severity::Warning,
        })
    }

    fn help<'a>(&'a self) -> Option<Box<dyn fmt::Display + 'a>> {
        (!self.diag.notes.is_empty()).then(|| Box::new(self.diag.notes.join("\n")) as Box<dyn fmt::Display>)
    }

    fn source_code(&self) -> Option<&dyn SourceCode> {
        self.source.as_ref().map(|s| s as &dyn SourceCode)
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        let span = self.diag.span.as_ref()?;
        self.source.as_ref()?;
        let len = span.end.saturating_sub(span.start);
        Some(Box::new(std::iter::once(LabeledSpan::at(span.start..span.start + len, "here"))))
    }
}

/// `path` relative to `cwd` when it is inside it.
pub fn display_path(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

pub fn render(diags: &[Diagnostic], cwd: &Path) -> String {
    let color = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let theme = if color { GraphicalTheme::unicode() } else { GraphicalTheme::unicode_nocolor() };
    let handler = GraphicalReportHandler::new_themed(theme).with_width(100);
    let mut out = String::new();
    for diag in diags {
        let source = diag.span.as_ref().and_then(|span| {
            let path = Uri::parse(&span.uri).ok()?.as_path()?;
            let text = std::fs::read_to_string(&path).ok()?;
            Some(NamedSource::new(display_path(&path, cwd), text))
        });
        let rendered = Rendered { diag: diag.clone(), source };
        handler.render_report(&mut out, &rendered).expect("writing to a String cannot fail");
    }
    out
}
```

- [ ] **Step 5: Implement `check` and `main`**

`crates/veneer/src/check.rs`:
```rust
//! `veneer check`.

use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::json;
use veneer_resolve::{locate, overlay::to_yaml, resolve, ResolveOptions};
use veneer_spec::{has_errors, Severity};

use crate::render::{display_path, render};

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Format {
    Yaml,
    Json,
}

#[derive(Debug, clap::Args)]
pub struct CheckArgs {
    /// Spec path or directory (default: nearest .veneer/veneer.yaml).
    pub spec: Option<String>,
    /// Print the fully resolved spec.
    #[arg(long)]
    pub resolved: bool,
    /// Output format for --resolved.
    #[arg(long, value_enum, default_value_t = Format::Yaml)]
    pub format: Format,
    /// Overlay file to apply (repeatable, in order).
    #[arg(long = "overlay", value_name = "FILE")]
    pub overlays: Vec<PathBuf>,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: CheckArgs) -> ExitCode {
    let cwd = std::env::current_dir().and_then(|p| p.canonicalize()).expect("current directory is readable");
    let entry = match locate(args.spec.as_deref(), &cwd) {
        Ok(p) => p,
        Err(diag) => {
            if args.json {
                println!("{}", json!({"ok": false, "entry": null, "documents": [], "diagnostics": [diag], "resolved": null}));
            } else {
                eprint!("{}", render(&[diag], &cwd));
            }
            return ExitCode::from(2);
        }
    };
    let overlays = args.overlays.iter().map(|p| cwd.join(p)).collect();
    let resolved = resolve(&entry, &ResolveOptions { overlays });
    let failed = has_errors(&resolved.diagnostics);
    let entry_shown = display_path(&entry, &cwd);

    if args.json {
        let tree = if args.resolved { resolved.tree.as_ref().map(|t| t.to_json()) } else { None };
        println!(
            "{}",
            json!({
                "ok": !failed,
                "entry": entry_shown,
                "documents": resolved.documents,
                "diagnostics": resolved.diagnostics,
                "resolved": tree,
            })
        );
    } else {
        eprint!("{}", render(&resolved.diagnostics, &cwd));
        if !failed {
            if args.resolved {
                let tree = resolved.tree.as_ref().expect("a tree exists when there are no errors");
                match args.format {
                    Format::Yaml => print!("{}", to_yaml(tree)),
                    Format::Json => println!("{}", serde_json::to_string_pretty(&tree.to_json()).expect("serializes")),
                }
            } else {
                let warnings = resolved.diagnostics.iter().filter(|d| d.severity == Severity::Warning).count();
                println!("ok {entry_shown} ({} documents, {warnings} warnings)", resolved.documents.len());
            }
        }
    }
    if failed { ExitCode::from(2) } else { ExitCode::SUCCESS }
}
```

`crates/veneer/src/main.rs`:
```rust
//! veneer: define agent workspace environments once; stage them anywhere.

mod check;
mod render;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "veneer", version, about = "Define agent workspace environments once; stage them anywhere.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Validate a spec and report diagnostics.
    Check(check::CheckArgs),
    /// Print the JSON Schema for veneer.yaml.
    Schema,
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Check(args) => check::run(args),
        Command::Schema => {
            print!("{}", veneer_spec::json_schema_string());
            ExitCode::SUCCESS
        }
    }
}
```

- [ ] **Step 6: Run tests, then review and accept snapshots**

Run: `INSTA_UPDATE=always cargo test -p veneer`
Expected: all 8 tests pass, and two new files appear under `crates/veneer/tests/snapshots/`. Open both and check them:
- `cli__resolved_basic_yaml.snap`: YAML containing `base: ubuntu:24.04`, `packages:` with `git` and `libpq-dev`, `tools:` with `node: 22`, no `extends`, no `DEBUG`.
- `cli__unknown_key_human.snap`: contains `V1009`, `unknown field \`pakages\``, the `.veneer/veneer.yaml` source name, line `2`, and a `here` label.

If either is wrong, fix the code (not the snapshot) and rerun. Then run `cargo test -p veneer` without `INSTA_UPDATE` and confirm it passes.

- [ ] **Step 7: Add CI and README usage**

`.github/workflows/ci.yml`:
```yaml
name: ci
on:
  push:
    branches: [main]
  pull_request:
jobs:
  test:
    strategy:
      matrix:
        os: [ubuntu-latest, macos-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all --check
      - run: cargo clippy --workspace --all-targets -- -D warnings
      - run: cargo test --workspace
```

Append to `README.md`:
````markdown

Define an agent workspace environment once in `.veneer/veneer.yaml`; veneer stages it
consistently in every workspace. See the [design spec](docs/superpowers/specs/2026-11-03-veneer-design.md).

## Status

Milestone 1: spec core. `veneer check` validates and resolves specs (`$ref`, `extends`,
overlays, strict schema, template phase checks).

```sh
cargo run -p veneer -- check                       # nearest .veneer/veneer.yaml
cargo run -p veneer -- check --resolved            # print the merged spec
cargo run -p veneer -- check --overlay .veneer/overlays/bench.overlay.yaml --resolved
cargo run -p veneer -- schema > veneer.schema.json # JSON Schema for editors
```
````

- [ ] **Step 8: Full verification**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: no clippy warnings and all tests pass. Fix any clippy findings inline. Typical ones are `needless_pass_by_value` (not enabled by default) and `manual_let_else`.

- [ ] **Step 9: Commit**

```bash
git add crates/veneer .github README.md Cargo.lock
git commit -m "feat(cli): add veneer check and schema commands with miette diagnostics"
```

---

## Out of scope for M1 (tracked for later plans)

- M2: `veneer-plan` / `veneer-exec`, `apply`, `diff`, `exec`, `env`, `--set`, DAG output in `check --resolved`.
- M3: `packages`, mise `tools`, `deps`, git `repos` steps, plus the `from:` adapters for those blocks.
- M4: services (compose and native), image pre-pull, `down`.
- M5: remote sources, `.veneer/veneer.lock` (`V1005`/`V1006`), `use:` modules.
- M6: bake (`oci`, `rootfs`), `entrypoint`.
- M7: cargo-dist distribution, one-command installer, `init`.
