//! veneer-resolve: load, layer, and resolve veneer specs.

use std::path::{Path, PathBuf};

use suspect_ref::WorkspaceBuilder;
use veneer_spec::{Diagnostic, Node, Spec, codes, from_node, has_errors, sort_diagnostics};

pub mod discover;
pub mod extends;
pub mod load;
pub mod merge;
pub mod overlay;

pub use discover::locate;

#[derive(Debug, Clone, Default)]
pub struct ResolveOptions {
    /// Overlay files, applied in order after layering.
    pub overlays: Vec<PathBuf>,
}

#[derive(Debug, Default)]
pub struct Resolved {
    /// The merged tree (present unless the entry itself failed to load).
    pub tree: Option<Node>,
    /// The typed spec (present only when resolution and typing succeeded).
    pub spec: Option<Spec>,
    /// URIs of every document read.
    pub documents: Vec<String>,
    /// Sorted diagnostics.
    pub diagnostics: Vec<Diagnostic>,
}

pub fn resolve(entry: &Path, opts: &ResolveOptions) -> Resolved {
    let mut out = Resolved::default();
    let ws = match WorkspaceBuilder::new().build() {
        Ok(ws) => ws,
        Err(e) => {
            out.diagnostics.push(Diagnostic::error(
                codes::NOT_FOUND,
                format!("cannot create workspace: {e}"),
            ));
            return out;
        }
    };
    let mut loader = load::Loader::new(&ws);
    let tree = extends::load_layered(&mut loader, entry, &mut Vec::new());
    out.documents = loader.documents.iter().cloned().collect();
    out.diagnostics.append(&mut loader.diagnostics);

    if let Some(mut tree) = tree {
        if !opts.overlays.is_empty() {
            tree = overlay::apply_overlays(tree, &opts.overlays, &mut out.diagnostics);
        }
        if !has_errors(&out.diagnostics) {
            out.diagnostics
                .extend(veneer_template::check_templates(&tree));
            match from_node::<Spec>(&tree) {
                Ok(spec) => {
                    for v in spec.validate() {
                        out.diagnostics.push(
                            Diagnostic::error(v.code, v.message).at(tree.nearest_span(&v.pointer)),
                        );
                    }
                    if !spec.modules.is_empty() {
                        out.diagnostics.push(
                            Diagnostic::warning(
                                codes::MODULES_UNSUPPORTED,
                                "`use:` modules are not supported yet and were ignored",
                            )
                            .at(tree.nearest_span("/use")),
                        );
                    }
                    out.spec = Some(spec);
                }
                Err(e) => {
                    let pointer = e.pointer.clone().unwrap_or_default();
                    out.diagnostics.push(
                        Diagnostic::error(codes::SCHEMA, e.message)
                            .at(tree.nearest_span(&pointer))
                            .note(format!(
                                "at `{}`",
                                if pointer.is_empty() { "/" } else { &pointer }
                            )),
                    );
                }
            }
        }
        out.tree = Some(tree);
    }
    sort_diagnostics(&mut out.diagnostics);
    out
}
