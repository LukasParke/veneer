mod common;

use serde_json::json;
use veneer_resolve::load::load_single;
use veneer_spec::codes;

#[test]
fn whole_document_ref_with_sibling_merge() {
    let (_d, entry) = common::tree(&[
        (
            ".veneer/veneer.yaml",
            "version: 1\ntools:\n  $ref: ./tools.yaml\n  python: \"3.12\"\n",
        ),
        (".veneer/tools.yaml", "node: 22\npython: \"3.11\"\n"),
    ]);
    let (node, diags) = load_single(&entry);
    assert!(diags.is_empty(), "{diags:?}");
    let node = node.unwrap();
    assert_eq!(
        node.to_json(),
        json!({"version": 1, "tools": {"node": 22, "python": "3.12"}})
    );
    // spans of expanded nodes point into the referenced file
    assert!(
        node.pointer("/tools/node")
            .unwrap()
            .span
            .as_ref()
            .unwrap()
            .uri
            .ends_with("tools.yaml")
    );
}

#[test]
fn pointer_fragments_local_and_nested_refs() {
    let (_d, entry) = common::tree(&[
        (
            "veneer.yaml",
            "version: 1\nenv:\n  $ref: ./parts/all.yaml#/env\nimages:\n  $ref: '#/defs/imgs'\ndefs:\n  imgs: [redis:7]\n",
        ),
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
    assert!(
        diags[0].notes[0].starts_with("chain: "),
        "{:?}",
        diags[0].notes
    );
}

#[test]
fn self_reference_is_a_cycle() {
    let (_d, entry) = common::tree(&[("veneer.yaml", "version: 1\nx:\n  y:\n    $ref: '#/x'\n")]);
    let (_, diags) = load_single(&entry);
    assert_eq!(diags[0].code, codes::CYCLE);
}

#[test]
fn missing_targets_and_bad_siblings() {
    let (_d, entry) = common::tree(&[(
        "veneer.yaml",
        "version: 1\na:\n  $ref: ./gone.yaml\nb:\n  $ref: '#/nope'\nc:\n  $ref: '#/version'\n  extra: 1\nd:\n  $ref: https://example.com/x.yaml\n",
    )]);
    let (_, diags) = load_single(&entry);
    let codes_seen: Vec<&str> = diags.iter().map(|d| d.code).collect();
    assert_eq!(
        codes_seen,
        vec![
            codes::NOT_FOUND,
            codes::POINTER_NOT_FOUND,
            codes::REF_SIBLINGS,
            codes::REMOTE_UNSUPPORTED
        ]
    );
}
