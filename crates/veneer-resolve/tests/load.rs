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
