use serde::Deserialize;
use serde_json::json;
use veneer_spec::{Node, from_node};

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
    let n =
        Node::from_json(&json!({"version": 1, "tools": {}, "items": [{"name": "a", "nmae": "x"}]}));
    let err = from_node::<Outer>(&n).unwrap_err();
    assert_eq!(err.pointer.as_deref(), Some("/items/0/nmae"));
    assert!(
        err.message.contains("unknown field `nmae`"),
        "{}",
        err.message
    );
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
    assert!(
        err.message.contains("missing field `name`"),
        "{}",
        err.message
    );
}
