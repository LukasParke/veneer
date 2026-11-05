//! veneer-resolve: load, layer, and resolve veneer specs.

use std::path::{Path, PathBuf};

use suspect_ref::WorkspaceBuilder;
use veneer_spec::{Diagnostic, Node, Spec, codes, from_node, has_errors, sort_diagnostics};

pub mod devcontainer;
pub mod devfile;
pub mod discover;
pub mod extends;
pub mod load;
pub mod merge;
pub mod oci;
pub mod overlay;
pub mod remote;

pub use discover::locate;

/// Like [`locate`], but fetches remote sources (`github:`, `git+`, `https:`, `oci://`) into
/// `cache` first.
pub fn locate_any(
    arg: Option<&str>,
    cwd: &Path,
    cache: &Path,
    update_lock: bool,
) -> Result<PathBuf, Diagnostic> {
    match arg {
        Some(a) if discover::is_remote(a) => {
            let mut r = remote::Remote::new(cache.to_path_buf(), None, update_lock);
            remote::fetch(&mut r, a)
        }
        other => locate(other, cwd),
    }
}

#[derive(Debug, Clone, Default)]
pub struct ResolveOptions {
    /// Overlay files, applied in order after layering.
    pub overlays: Vec<PathBuf>,
    /// Enables remote `extends`/`use` sources, cached here and pinned in `veneer.lock`.
    pub remote_cache: Option<PathBuf>,
    /// Re-pin remote sources instead of enforcing the lock.
    pub update_lock: bool,
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
    if let Some(cache) = &opts.remote_cache {
        // Only local projects get a lock file; remote entries are pinned by their source.
        let lock = (!entry.starts_with(cache)).then(|| entry.with_file_name("veneer.lock"));
        loader.remote = Some(remote::Remote::new(cache.clone(), lock, opts.update_lock));
    }
    let tree = extends::load_layered(&mut loader, entry, &mut Vec::new());
    if let Some(r) = loader.remote.as_mut()
        && !has_errors(&loader.diagnostics)
        && let Err(d) = r.save()
    {
        loader.diagnostics.push(d);
    }
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
