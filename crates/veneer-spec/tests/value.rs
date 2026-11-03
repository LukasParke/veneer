use serde_json::json;
use veneer_spec::{Kind, Node, Span, escape_pointer_token, split_pointer};

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
