mod common;

use veneer_resolve::{ResolveOptions, resolve};
use veneer_spec::codes;

#[test]
fn resolves_layers_refs_and_types() {
    let (_d, entry) = common::tree(&[
        (
            ".veneer/veneer.yaml",
            "version: 1\nextends: [./base.yaml]\ntools:\n  $ref: ./tools.yaml\nservices:\n  db:\n    image: postgres:16\n",
        ),
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
    assert!(
        d.message.contains("unknown field `pakages`"),
        "{}",
        d.message
    );
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
    assert_eq!(
        got,
        vec![codes::VERSION, codes::MODULES_UNSUPPORTED, codes::PHASE]
    );
}

#[test]
fn resolution_errors_skip_typing() {
    let (_d, entry) = common::tree(&[("veneer.yaml", "version: 1\ntools:\n  $ref: ./gone.yaml\n")]);
    let r = resolve(&entry, &ResolveOptions::default());
    assert_eq!(
        r.diagnostics.iter().map(|d| d.code).collect::<Vec<_>>(),
        vec![codes::NOT_FOUND]
    );
    assert!(r.spec.is_none());
    assert!(r.tree.is_some());
}
