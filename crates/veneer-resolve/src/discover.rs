//! Spec discovery and source classification.

/// True for sources M1 cannot fetch: `github:`, `git+…`, `oci:`, or any `scheme://` but `file://`.
pub(crate) fn is_remote(source: &str) -> bool {
    source.starts_with("github:")
        || source.starts_with("git+")
        || source.starts_with("oci:")
        || (source.contains("://") && !source.starts_with("file://"))
}
