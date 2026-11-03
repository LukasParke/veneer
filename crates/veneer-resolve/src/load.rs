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
