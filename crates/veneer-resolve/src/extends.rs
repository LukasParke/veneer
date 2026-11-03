//! `extends` layering (design spec §4.7 resolution order).

use std::path::{Path, PathBuf};

use veneer_spec::{Diagnostic, Kind, Node, codes};

use crate::discover::is_remote;
use crate::load::Loader;
use crate::merge::merge;

/// Loads `path` and everything it extends. `chain` holds the files currently being layered.
pub fn load_layered(
    loader: &mut Loader<'_>,
    path: &Path,
    chain: &mut Vec<PathBuf>,
) -> Option<Node> {
    let mut doc = loader.load_file(path)?;
    let extends = match &mut doc.kind {
        Kind::Object(map) => map.shift_remove("extends"),
        _ => None,
    };
    let mut acc = Node::object();
    chain.push(path.to_path_buf());
    if let Some(ext) = extends {
        let items = match ext.kind {
            Kind::Array(items) => items,
            Kind::Str(_) => vec![ext],
            Kind::Null => Vec::new(),
            _ => {
                loader.diagnostics.push(
                    Diagnostic::error(codes::SCHEMA, "`extends` must be a path or a list of paths")
                        .at(ext.span),
                );
                Vec::new()
            }
        };
        let base_dir = path.parent().unwrap_or(Path::new("/"));
        for item in items {
            let Some(raw) = item.as_str() else {
                loader.diagnostics.push(
                    Diagnostic::error(codes::SCHEMA, "`extends` entries must be strings")
                        .at(item.span.clone()),
                );
                continue;
            };
            if is_remote(raw) {
                loader.diagnostics.push(
                    Diagnostic::error(
                        codes::REMOTE_UNSUPPORTED,
                        format!("remote `extends` source `{raw}` is not supported yet"),
                    )
                    .at(item.span.clone()),
                );
                continue;
            }
            let target = match base_dir.join(raw).canonicalize() {
                Ok(p) => p,
                Err(e) => {
                    loader.diagnostics.push(
                        Diagnostic::error(
                            codes::NOT_FOUND,
                            format!("cannot read extended spec `{raw}`: {e}"),
                        )
                        .at(item.span.clone()),
                    );
                    continue;
                }
            };
            if chain.contains(&target) {
                let mut names: Vec<String> =
                    chain.iter().map(|p| p.display().to_string()).collect();
                names.push(target.display().to_string());
                loader.diagnostics.push(
                    Diagnostic::error(codes::CYCLE, format!("`extends` cycle through `{raw}`"))
                        .at(item.span.clone())
                        .note(format!("chain: {}", names.join(" → "))),
                );
                continue;
            }
            if let Some(layer) = load_layered(loader, &target, chain) {
                merge(&mut acc, layer, &[]);
            }
        }
    }
    chain.pop();
    merge(&mut acc, doc, &[]);
    Some(acc)
}
