mod common;

use serde_json::json;
use suspect_ref::WorkspaceBuilder;
use veneer_resolve::{extends::load_layered, load::Loader};
use veneer_spec::{Diagnostic, Node, codes};

fn layered(entry: &std::path::Path) -> (Option<Node>, Vec<Diagnostic>) {
    let ws = WorkspaceBuilder::new().build().unwrap();
    let mut loader = Loader::new(&ws);
    let node = load_layered(&mut loader, entry, &mut Vec::new());
    (node, loader.diagnostics)
}

#[test]
fn layers_merge_in_order_and_extends_is_consumed() {
    let (_d, entry) = common::tree(&[
        (
            ".veneer/veneer.yaml",
            "version: 1\nextends: [./base.yaml, ./team.yaml]\nenv:\n  DEBUG: ~\n  APP: \"1\"\npackages: [libpq-dev]\n",
        ),
        (
            ".veneer/base.yaml",
            "version: 1\nbase: ubuntu:24.04\npackages: [git]\nenv:\n  DEBUG: \"1\"\n  LOG: info\n",
        ),
        (
            ".veneer/team.yaml",
            "extends: ./org/root.yaml\nenv:\n  LOG: debug\n",
        ),
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
        (
            "veneer.yaml",
            "version: 1\nextends: [./a.yaml, github:acme/envs, ./gone.yaml, 5]\n",
        ),
        ("a.yaml", "extends: [./veneer.yaml]\n"),
    ]);
    let (_, diags) = layered(&entry);
    let got: Vec<&str> = diags.iter().map(|d| d.code).collect();
    assert_eq!(
        got,
        vec![
            codes::CYCLE,
            codes::REMOTE_UNSUPPORTED,
            codes::NOT_FOUND,
            codes::SCHEMA
        ]
    );
}
