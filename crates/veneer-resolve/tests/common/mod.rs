#![allow(dead_code)]
use std::path::PathBuf;

/// Writes `files` (relative path, contents) into a temp dir. Returns the dir guard and the
/// canonical path of the first file.
pub fn tree(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    for (rel, body) in files {
        let path = dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
    }
    let entry = dir.path().join(files[0].0).canonicalize().unwrap();
    (dir, entry)
}
