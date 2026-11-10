mod common;

use serde_json::json;
use veneer_resolve::{
    load::load_single,
    overlay::{apply_overlays, to_yaml},
};
use veneer_spec::codes;

const OVERLAY: &str = "overlay: 1.0.0\ninfo: { title: bench, version: \"1\" }\nactions:\n  - target: $.env\n    update: { BENCH: \"1\" }\n  - target: $.services.db\n    remove: true\n  - target: $.nothing\n    remove: true\n";

#[test]
fn applies_actions_in_order_and_keeps_spans() {
    let (dir, entry) = common::tree(&[
        (
            "veneer.yaml",
            "version: 1\nenv:\n  A: \"1\"\nservices:\n  db:\n    image: postgres:16\n",
        ),
        ("o.yaml", OVERLAY),
    ]);
    let tree = load_single(&entry).0.unwrap();
    let mut diags = Vec::new();
    let out = apply_overlays(tree, &[dir.path().join("o.yaml")], &mut diags);
    assert_eq!(
        out.to_json(),
        json!({"version": 1, "env": {"A": "1", "BENCH": "1"}, "services": {}})
    );
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, codes::OVERLAY_UNMATCHED);
    assert!(out.pointer("/env/A").unwrap().span.is_some());
}

#[test]
fn invalid_overlay_is_reported_and_skipped() {
    let (dir, entry) = common::tree(&[
        ("veneer.yaml", "version: 1\n"),
        ("bad.yaml", "not_an_overlay: true\n"),
    ]);
    let tree = load_single(&entry).0.unwrap();
    let mut diags = Vec::new();
    let out = apply_overlays(
        tree,
        &[dir.path().join("bad.yaml"), dir.path().join("missing.yaml")],
        &mut diags,
    );
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
