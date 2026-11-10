use serde_json::json;
use veneer_spec::{Node, codes};
use veneer_template::check_templates;

fn codes_of(v: serde_json::Value) -> Vec<(&'static str, String)> {
    check_templates(&Node::from_json(&v))
        .into_iter()
        .map(|d| (d.code, d.message))
        .collect()
}

#[test]
fn workspace_values_are_fine_in_workspace_blocks() {
    assert!(
        codes_of(json!({
            "vars": {"db": "bench_{{ workspace.id }}"},
            "env": {"URL": "pg://{{ vars.db }}", "T": "{{ secrets.TOKEN }}", "H": "{{ env.HOME }}"},
            "tools": {"python": "{{ vars.py }}", "node": "{{ os.arch }}"},
            "hooks": {"workspace": ["echo {{ workspace.root }}"]}
        }))
        .is_empty()
    );
}

#[test]
fn direct_workspace_reference_in_build_block_is_v2001() {
    let got = codes_of(
        json!({"packages": ["lib-{{ workspace.id }}"], "hooks": {"build": [{"run": "echo {{ secrets.X }}"}]}}),
    );
    assert_eq!(
        got.iter().map(|g| g.0).collect::<Vec<_>>(),
        vec![codes::PHASE, codes::PHASE]
    );
}

#[test]
fn transitive_taint_through_vars_is_v2001() {
    let got = codes_of(json!({
        "vars": {"a": "{{ secrets.T }}", "b": "x-{{ vars.a }}", "c": "plain"},
        "tools": {"x": "{{ vars.b }}", "y": "{{ vars.c }}"}
    }));
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].0, codes::PHASE);
    assert!(
        got[0].1.contains("vars.b") && got[0].1.contains("secrets.T"),
        "{}",
        got[0].1
    );
}

#[test]
fn unknown_roots_and_syntax_errors() {
    let got = codes_of(json!({"env": {"A": "{{ nope.x }}", "B": "{{ vars.a "}}));
    let mut c: Vec<_> = got.iter().map(|g| g.0).collect();
    c.sort();
    assert_eq!(c, vec![codes::TEMPLATE_SYNTAX, codes::TEMPLATE_UNKNOWN_VAR]);
}
