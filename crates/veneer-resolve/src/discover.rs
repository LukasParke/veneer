//! Spec discovery and source classification.

/// True for sources M1 cannot fetch: `github:`, `git+…`, `oci:`, or any `scheme://` but `file://`.
pub(crate) fn is_remote(source: &str) -> bool {
    source.starts_with("github:")
        || source.starts_with("git+")
        || source.starts_with("oci:")
        || (source.contains("://") && !source.starts_with("file://"))
}

use std::path::{Path, PathBuf};

use veneer_spec::{Diagnostic, codes};

fn canonical(p: &Path) -> Result<PathBuf, Diagnostic> {
    p.canonicalize().map_err(|e| {
        Diagnostic::error(
            codes::SPEC_NOT_FOUND,
            format!("cannot open {}: {e}", p.display()),
        )
    })
}

/// Finds the entry spec for `arg` (or by walking up from `cwd` when `arg` is `None`).
pub fn locate(arg: Option<&str>, cwd: &Path) -> Result<PathBuf, Diagnostic> {
    let Some(arg) = arg else {
        for dir in cwd.ancestors() {
            let candidate = dir.join(".veneer/veneer.yaml");
            if candidate.is_file() {
                return canonical(&candidate);
            }
        }
        return Err(Diagnostic::error(
            codes::SPEC_NOT_FOUND,
            format!(
                "no .veneer/veneer.yaml found in {} or any parent directory",
                cwd.display()
            ),
        )
        .note("run `veneer check <path>` or create .veneer/veneer.yaml"));
    };
    if is_remote(arg) {
        return Err(Diagnostic::error(
            codes::REMOTE_UNSUPPORTED,
            format!("remote spec source `{arg}` is not supported yet"),
        ));
    }
    let path = cwd.join(arg);
    if path.is_file() {
        return canonical(&path);
    }
    if path.is_dir() {
        for candidate in [path.join(".veneer/veneer.yaml"), path.join("veneer.yaml")] {
            if candidate.is_file() {
                return canonical(&candidate);
            }
        }
    }
    Err(Diagnostic::error(
        codes::SPEC_NOT_FOUND,
        format!("no veneer spec at {}", path.display()),
    ))
}
