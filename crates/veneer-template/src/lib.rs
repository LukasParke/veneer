//! veneer-template: validates minijinja templates in a resolved spec tree.

use std::collections::{HashMap, HashSet};

use minijinja::Environment;
use veneer_spec::{Diagnostic, Kind, Node, codes};

const ROOTS: &[&str] = &["vars", "env", "workspace", "secrets", "os", "veneer"];
const BUILD_KEYS: &[&str] = &["base", "packages", "tools", "deps", "images"];
const WORKSPACE_ONLY: &[&str] = &["workspace", "secrets"];

fn is_template(s: &str) -> bool {
    s.contains("{{") || s.contains("{%")
}

fn references(env: &Environment<'_>, s: &str) -> Result<HashSet<String>, minijinja::Error> {
    Ok(env.template_from_str(s)?.undeclared_variables(true))
}

fn is_build_phase(path: &[String]) -> bool {
    match path {
        [first, ..] if BUILD_KEYS.contains(&first.as_str()) => true,
        [hooks, build, ..] => hooks == "hooks" && build == "build",
        _ => false,
    }
}

fn walk<'a>(node: &'a Node, path: &mut Vec<String>, f: &mut dyn FnMut(&[String], &'a Node)) {
    f(path, node);
    match &node.kind {
        Kind::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                path.push(i.to_string());
                walk(child, path, f);
                path.pop();
            }
        }
        Kind::Object(map) => {
            for (k, child) in map {
                path.push(k.clone());
                walk(child, path, f);
                path.pop();
            }
        }
        _ => {}
    }
}

/// var name -> the workspace-only reference that taints it (e.g. "secrets.T").
fn tainted_vars(tree: &Node, env: &Environment<'_>) -> HashMap<String, String> {
    let Some(vars) = tree.get("vars").and_then(Node::as_object) else {
        return HashMap::new();
    };
    let direct: Vec<(String, HashSet<String>)> = vars
        .iter()
        .map(|(name, value)| {
            let mut refs = HashSet::new();
            walk(value, &mut Vec::new(), &mut |_, n| {
                if let Some(s) = n.as_str().filter(|s| is_template(s)) {
                    refs.extend(references(env, s).unwrap_or_default());
                }
            });
            (name.clone(), refs)
        })
        .collect();
    let mut tainted: HashMap<String, String> = HashMap::new();
    loop {
        let mut changed = false;
        for (name, refs) in &direct {
            if tainted.contains_key(name) {
                continue;
            }
            let cause = refs.iter().find_map(|r| {
                let mut parts = r.split('.');
                match parts.next() {
                    Some(root) if WORKSPACE_ONLY.contains(&root) => Some(r.clone()),
                    Some("vars") => parts.next().and_then(|v| tainted.get(v)).cloned(),
                    _ => None,
                }
            });
            if let Some(cause) = cause {
                tainted.insert(name.clone(), cause);
                changed = true;
            }
        }
        if !changed {
            return tainted;
        }
    }
}

pub fn check_templates(tree: &Node) -> Vec<Diagnostic> {
    let env = Environment::new();
    let tainted = tainted_vars(tree, &env);
    let mut diags = Vec::new();
    walk(tree, &mut Vec::new(), &mut |path, node| {
        let Some(s) = node.as_str().filter(|s| is_template(s)) else {
            return;
        };
        let refs = match references(&env, s) {
            Ok(r) => r,
            Err(e) => {
                diags.push(
                    Diagnostic::error(codes::TEMPLATE_SYNTAX, format!("invalid template: {e}"))
                        .at(node.span.clone()),
                );
                return;
            }
        };
        let build = is_build_phase(path);
        let mut refs: Vec<String> = refs.into_iter().collect();
        refs.sort();
        for r in refs {
            let mut parts = r.split('.');
            let root = parts.next().unwrap_or_default();
            if !ROOTS.contains(&root) {
                diags.push(
                    Diagnostic::error(
                        codes::TEMPLATE_UNKNOWN_VAR,
                        format!("unknown template variable `{r}`"),
                    )
                    .at(node.span.clone())
                    .note("available roots: vars, env, workspace, secrets, os, veneer"),
                );
                continue;
            }
            if !build {
                continue;
            }
            if WORKSPACE_ONLY.contains(&root) {
                diags.push(
                    Diagnostic::error(codes::PHASE, format!("`{r}` is only available at workspace time, but this value is used at build time"))
                        .at(node.span.clone())
                        .note("move the value to a workspace-phase block (env, workspace, services, hooks.workspace)"),
                );
            } else if root == "vars"
                && let Some((name, cause)) =
                    parts.next().and_then(|v| tainted.get(v).map(|c| (v, c)))
            {
                {
                    diags.push(
                        Diagnostic::error(
                            codes::PHASE,
                            format!("`vars.{name}` depends on `{cause}`, which is only available at workspace time, but this value is used at build time"),
                        )
                        .at(node.span.clone()),
                    );
                }
            }
        }
    });
    diags
}
