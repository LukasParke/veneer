use serde_json::{json, Value};
use veneer_resolve::merge::merge;
use veneer_spec::Node;

fn merged(base: Value, top: Value) -> Value {
    let mut b = Node::from_json(&base);
    merge(&mut b, Node::from_json(&top), &[]);
    b.to_json()
}

#[test]
fn maps_deep_merge_and_later_wins() {
    assert_eq!(
        merged(json!({"env": {"A": "1", "B": "2"}}), json!({"env": {"B": "3", "C": "4"}})),
        json!({"env": {"A": "1", "B": "3", "C": "4"}})
    );
}

#[test]
fn null_deletes_inherited_keys_and_is_dropped_from_new_subtrees() {
    assert_eq!(
        merged(json!({"env": {"A": "1", "B": "2"}}), json!({"env": {"A": null}, "new": {"x": null, "y": 1}})),
        json!({"env": {"B": "2"}, "new": {"y": 1}})
    );
}

#[test]
fn ordinary_lists_replace() {
    assert_eq!(
        merged(json!({"services": {"db": {"ports": ["1"]}}}), json!({"services": {"db": {"ports": ["2"]}}})),
        json!({"services": {"db": {"ports": ["2"]}}})
    );
}

#[test]
fn concat_lists_append_and_dedupe_by_identity() {
    assert_eq!(
        merged(
            json!({"packages": ["git", {"from": "Brewfile"}], "images": ["a"], "deps": [{"from": "p.json"}]}),
            json!({"packages": ["git", "curl", {"from": "Brewfile"}], "images": ["b"], "deps": [{"from": "p.json"}]})
        ),
        json!({"packages": ["git", {"from": "Brewfile"}, "curl"], "images": ["a", "b"], "deps": [{"from": "p.json"}]})
    );
}

#[test]
fn nested_concat_paths_use_their_identity_and_replace_in_place() {
    assert_eq!(
        merged(
            json!({
                "hooks": {"build": ["make", {"run": "x", "creates": "a"}]},
                "workspace": {"repos": [{"path": "app", "url": "u1"}], "files": [{"dest": "d", "src": "s1"}]}
            }),
            json!({
                "hooks": {"build": [{"run": "x", "creates": "b"}, "lint"]},
                "workspace": {"repos": [{"path": "app", "url": "u2"}, {"path": "lib", "url": "u3"}], "files": [{"dest": "d", "src": "s2"}]}
            })
        ),
        json!({
            "hooks": {"build": ["make", {"run": "x", "creates": "b"}, "lint"]},
            "workspace": {"repos": [{"path": "app", "url": "u2"}, {"path": "lib", "url": "u3"}], "files": [{"dest": "d", "src": "s2"}]}
        })
    );
}

#[test]
fn scalar_over_map_replaces() {
    assert_eq!(merged(json!({"base": {"x": 1}}), json!({"base": "ubuntu"})), json!({"base": "ubuntu"}));
}
