//! Layer merge rules (design spec §4.7).

use veneer_spec::{Kind, Node};

const CONCAT_PATHS: &[&[&str]] = &[
    &["packages"],
    &["images"],
    &["deps"],
    &["hooks", "build"],
    &["hooks", "workspace"],
    &["workspace", "repos"],
    &["workspace", "files"],
];

fn is_concat(path: &[String]) -> bool {
    CONCAT_PATHS
        .iter()
        .any(|p| p.len() == path.len() && p.iter().zip(path).all(|(a, b)| *a == b.as_str()))
}

fn identity(path: &[String], item: &Node) -> Option<String> {
    let field = |name: &str| item.get(name).and_then(Node::as_str).map(str::to_owned);
    let path: Vec<&str> = path.iter().map(String::as_str).collect();
    match path.as_slice() {
        ["packages"] | ["images"] => item
            .as_str()
            .map(str::to_owned)
            .or_else(|| field("from").map(|f| format!("from:{f}"))),
        ["deps"] => field("from"),
        ["hooks", _] => item.as_str().map(str::to_owned).or_else(|| field("run")),
        ["workspace", "repos"] => field("path"),
        ["workspace", "files"] => field("dest"),
        _ => None,
    }
}

/// Removes `null`-valued keys from objects, recursively (a null over nothing deletes nothing).
fn strip_nulls(mut node: Node) -> Node {
    match &mut node.kind {
        Kind::Object(map) => {
            let taken = std::mem::take(map);
            *map = taken
                .into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, strip_nulls(v)))
                .collect();
        }
        Kind::Array(items) => {
            let taken = std::mem::take(items);
            *items = taken.into_iter().map(strip_nulls).collect();
        }
        _ => {}
    }
    node
}

/// Merges `top` over `base`. `path` is the logical location of `base` in the spec.
pub fn merge(base: &mut Node, top: Node, path: &[String]) {
    let Node { kind: top_kind, span: top_span } = top;
    match top_kind {
        Kind::Object(top_map) if matches!(base.kind, Kind::Object(_)) => {
            let Kind::Object(base_map) = &mut base.kind else { unreachable!() };
            for (key, value) in top_map {
                if value.is_null() {
                    base_map.shift_remove(&key);
                    continue;
                }
                let mut child_path = path.to_vec();
                child_path.push(key.clone());
                match base_map.get_mut(&key) {
                    Some(existing) => merge(existing, value, &child_path),
                    None => {
                        base_map.insert(key, strip_nulls(value));
                    }
                }
            }
        }
        Kind::Array(top_items) if is_concat(path) && matches!(base.kind, Kind::Array(_)) => {
            let Kind::Array(base_items) = &mut base.kind else { unreachable!() };
            for item in top_items {
                let existing = identity(path, &item).and_then(|id| {
                    base_items.iter().position(|e| identity(path, e).as_deref() == Some(id.as_str()))
                });
                match existing {
                    Some(i) => base_items[i] = item,
                    None => base_items.push(item),
                }
            }
        }
        other => *base = strip_nulls(Node::with_span(other, top_span)),
    }
}
