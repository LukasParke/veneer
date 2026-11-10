//! Overlay 1.0 application (design spec §4.6) and YAML emit.

use std::path::{Path, PathBuf};

use suspect_low::LowDoc;
use suspect_source::{LineIndex, Source, Uri};
use veneer_spec::{Diagnostic, Kind, Node, Span, codes};

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
    Some(Span {
        uri: uri.to_string(),
        start: 0,
        end: 0,
        line: 1,
        col: 1,
    })
}

fn apply_one(
    current: &serde_json::Value,
    path: &Path,
    diags: &mut Vec<Diagnostic>,
) -> Option<serde_json::Value> {
    let shown = path.display().to_string();
    let source = match Source::from_path(path) {
        Ok(s) => s,
        Err(e) => {
            diags.push(Diagnostic::error(
                codes::NOT_FOUND,
                format!("cannot read overlay {shown}: {e}"),
            ));
            return None;
        }
    };
    let bytes = source.bytes().to_vec();
    let uri = match path
        .canonicalize()
        .ok()
        .and_then(|p| Uri::from_path(&p).ok())
    {
        Some(u) => u,
        None => Uri::from(shown.as_str()),
    };
    let doc = LowDoc::parse(uri.clone(), source);
    if !doc.syntax_errors().is_empty() {
        let index = LineIndex::new(&bytes);
        for e in doc.syntax_errors() {
            let (line, col) = index.line_col(&bytes, e.range.start);
            let span = Span {
                uri: uri.to_string(),
                start: e.range.start,
                end: e.range.end,
                line: line + 1,
                col: col + 1,
            };
            diags.push(Diagnostic::error(codes::SYNTAX, e.message.clone()).at(Some(span)));
        }
        return None;
    }
    let overlay = match suspect_overlay::OverlayDoc::parse(&doc) {
        Ok(o) => o,
        Err(e) => {
            diags.push(
                Diagnostic::error(codes::OVERLAY, format!("invalid overlay {shown}: {e}"))
                    .at(file_start(&uri)),
            );
            return None;
        }
    };
    for hint in suspect_overlay::validate_overlay(&overlay) {
        if !IGNORED_HINTS.contains(&hint.code) {
            diags.push(
                Diagnostic::warning(
                    codes::OVERLAY_HINT,
                    format!("overlay {shown}: {} ({})", hint.message, hint.code),
                )
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
                    Diagnostic::warning(
                        codes::OVERLAY_UNMATCHED,
                        format!("overlay {shown}: target `{t}` matched nothing"),
                    )
                    .at(file_start(&uri)),
                );
            }
            serde_json::from_str(&applied.output.to_json()).ok()
        }
        Err(e) => {
            diags.push(
                Diagnostic::error(codes::OVERLAY, format!("overlay {shown} failed: {e}"))
                    .at(file_start(&uri)),
            );
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
        Kind::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.as_str().into(), to_overlay_value(v)))
                .collect(),
        ),
    }
}

/// Canonical YAML for a resolved tree.
pub fn to_yaml(node: &Node) -> String {
    to_overlay_value(node).to_yaml()
}
