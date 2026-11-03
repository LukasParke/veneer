//! `use:` modules and `extends` layering (design spec §4.5, §4.7 resolution order:
//! `use` modules → `extends` in order → the declaring document).

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use veneer_spec::{Diagnostic, Kind, Node, codes};

use crate::discover::is_remote;
use crate::load::Loader;
use crate::merge::merge;
use crate::remote;

fn take(doc: &mut Node, key: &str) -> Option<Node> {
    match &mut doc.kind {
        Kind::Object(map) => map.shift_remove(key),
        _ => None,
    }
}

fn as_list(node: Node, what: &str, loader: &mut Loader<'_>) -> Vec<Node> {
    match node.kind {
        Kind::Array(items) => items,
        Kind::Str(_) | Kind::Object(_) => vec![node],
        Kind::Null => Vec::new(),
        _ => {
            loader.diagnostics.push(
                Diagnostic::error(codes::SCHEMA, format!("`{what}` must be a list")).at(node.span),
            );
            Vec::new()
        }
    }
}

/// Resolves one source string (local path, remote source, or built-in module) to a file.
fn locate_source(
    loader: &mut Loader<'_>,
    raw: &str,
    base_dir: &Path,
    item: &Node,
    builtin_ok: bool,
) -> Option<PathBuf> {
    if builtin_ok && raw.starts_with("veneer/") {
        let cache = loader
            .remote
            .as_ref()
            .map(|r| r.cache.clone())
            .unwrap_or_else(|| std::env::temp_dir().join("veneer-cache"));
        return match remote::builtin_path(&cache, raw) {
            Some(p) => Some(p),
            None => {
                loader.diagnostics.push(
                    Diagnostic::error(codes::NOT_FOUND, format!("unknown built-in module `{raw}`"))
                        .at(item.span.clone())
                        .note("built-in modules: veneer/harness/claude-code@1, veneer/harness/codex@1"),
                );
                None
            }
        };
    }
    if is_remote(raw) {
        let Some(r) = loader.remote.as_mut() else {
            loader.diagnostics.push(
                Diagnostic::error(
                    codes::REMOTE_UNSUPPORTED,
                    format!("remote source `{raw}` is not enabled here"),
                )
                .at(item.span.clone()),
            );
            return None;
        };
        return match remote::fetch(r, raw) {
            Ok(p) => Some(p),
            Err(d) => {
                loader.diagnostics.push(d.at(item.span.clone()));
                None
            }
        };
    }
    match base_dir.join(raw).canonicalize() {
        Ok(p) => Some(p),
        Err(e) => {
            loader.diagnostics.push(
                Diagnostic::error(codes::NOT_FOUND, format!("cannot read `{raw}`: {e}"))
                    .at(item.span.clone()),
            );
            None
        }
    }
}

fn layer(
    loader: &mut Loader<'_>,
    target: &Path,
    raw: &str,
    item: &Node,
    chain: &mut Vec<PathBuf>,
) -> Option<Node> {
    if chain.iter().any(|p| p == target) {
        let mut names: Vec<String> = chain.iter().map(|p| p.display().to_string()).collect();
        names.push(target.display().to_string());
        loader.diagnostics.push(
            Diagnostic::error(codes::CYCLE, format!("`extends` cycle through `{raw}`"))
                .at(item.span.clone())
                .note(format!("chain: {}", names.join(" → "))),
        );
        return None;
    }
    load_layered(loader, target, chain)
}

/// Loads `path` with its modules and everything it extends. `chain` holds the files
/// currently being layered.
pub fn load_layered(
    loader: &mut Loader<'_>,
    path: &Path,
    chain: &mut Vec<PathBuf>,
) -> Option<Node> {
    let mut doc = loader.load_file(path)?;
    let modules = take(&mut doc, "use");
    let extends = take(&mut doc, "extends");
    let mut acc = Node::object();
    chain.push(path.to_path_buf());
    let base_dir = path.parent().unwrap_or(Path::new("/")).to_path_buf();

    for item in modules
        .map(|m| as_list(m, "use", loader))
        .unwrap_or_default()
    {
        let (name, with) = match &item.kind {
            Kind::Str(s) => (s.clone(), None),
            Kind::Object(map) => match map.get("module").and_then(Node::as_str) {
                Some(m) => (m.to_string(), map.get("with").cloned()),
                None => {
                    loader.diagnostics.push(
                        Diagnostic::error(codes::SCHEMA, "a `use` entry needs `module:`")
                            .at(item.span.clone()),
                    );
                    continue;
                }
            },
            _ => {
                loader.diagnostics.push(
                    Diagnostic::error(
                        codes::SCHEMA,
                        "`use` entries must be strings or {module, with}",
                    )
                    .at(item.span.clone()),
                );
                continue;
            }
        };
        let Some(target) = locate_source(loader, &name, &base_dir, &item, true) else {
            continue;
        };
        if let Some(mut module) = layer(loader, &target, &name, &item, chain) {
            if let Some(with) = with {
                let mut vars = IndexMap::new();
                vars.insert("vars".to_string(), with);
                merge(&mut module, Node::new(Kind::Object(vars)), &[]);
            }
            merge(&mut acc, module, &[]);
        }
    }

    for item in extends
        .map(|e| as_list(e, "extends", loader))
        .unwrap_or_default()
    {
        let Some(raw) = item.as_str().map(str::to_owned) else {
            loader.diagnostics.push(
                Diagnostic::error(codes::SCHEMA, "`extends` entries must be strings")
                    .at(item.span.clone()),
            );
            continue;
        };
        if is_remote(&raw) && loader.remote.is_none() {
            loader.diagnostics.push(
                Diagnostic::error(
                    codes::REMOTE_UNSUPPORTED,
                    format!("remote `extends` source `{raw}` is not supported here"),
                )
                .at(item.span.clone()),
            );
            continue;
        }
        let Some(target) = locate_source(loader, &raw, &base_dir, &item, false) else {
            continue;
        };
        if crate::devcontainer::is_devcontainer(&target.to_string_lossy()) {
            let uri = suspect_source::Uri::from_path(&target)
                .map(|u| u.to_string())
                .unwrap_or_default();
            match crate::devcontainer::to_node(&target, &uri) {
                Ok((l, warnings)) => {
                    loader.documents.insert(uri);
                    loader.diagnostics.extend(warnings);
                    merge(&mut acc, l, &[]);
                }
                Err(d) => loader.diagnostics.push(d.at(item.span.clone())),
            }
            continue;
        }
        if let Some(l) = layer(loader, &target, &raw, &item, chain) {
            merge(&mut acc, l, &[]);
        }
    }
    chain.pop();
    let span = doc.span.clone();
    merge(&mut acc, doc, &[]);
    acc.span = span;
    Some(acc)
}
