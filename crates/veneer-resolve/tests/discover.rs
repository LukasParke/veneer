mod common;

use veneer_resolve::discover::locate;
use veneer_spec::codes;

#[test]
fn walks_up_to_dot_veneer() {
    let (dir, entry) = common::tree(&[
        (".veneer/veneer.yaml", "version: 1\n"),
        ("app/src/.keep", ""),
    ]);
    let cwd = dir.path().join("app/src").canonicalize().unwrap();
    assert_eq!(locate(None, &cwd).unwrap(), entry);
}

#[test]
fn explicit_file_and_dir_arguments() {
    let (dir, entry) = common::tree(&[
        (".veneer/veneer.yaml", "version: 1\n"),
        ("other/veneer.yaml", "version: 1\n"),
    ]);
    let root = dir.path().canonicalize().unwrap();
    assert_eq!(locate(Some("."), &root).unwrap(), entry);
    assert_eq!(
        locate(Some("other"), &root).unwrap(),
        root.join("other/veneer.yaml")
    );
    assert_eq!(locate(Some(".veneer/veneer.yaml"), &root).unwrap(), entry);
}

#[test]
fn missing_and_remote_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    assert_eq!(locate(None, &root).unwrap_err().code, codes::SPEC_NOT_FOUND);
    assert_eq!(
        locate(Some("nope"), &root).unwrap_err().code,
        codes::SPEC_NOT_FOUND
    );
    assert_eq!(
        locate(Some("github:acme/app@main"), &root)
            .unwrap_err()
            .code,
        codes::REMOTE_UNSUPPORTED
    );
}
