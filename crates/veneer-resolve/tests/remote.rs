mod common;

use std::process::Command;

use veneer_resolve::{ResolveOptions, resolve};

fn git(dir: &std::path::Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

#[test]
fn git_extends_are_pinned_and_digest_checked() {
    let (dir, _) = common::tree(&[("upstream/envs/base.yaml", "version: 1\npackages: [git]\n")]);
    let up = dir.path().join("upstream");
    git(&up, &["init", "-q", "-b", "main"]);
    git(&up, &["add", "."]);
    git(&up, &["commit", "-qm", "v1"]);
    let src = format!(
        "git+file://{}//envs/base.yaml@main",
        up.canonicalize().unwrap().display()
    );
    let (_d2, entry) = common::tree(&[(
        ".veneer/veneer.yaml",
        &format!("version: 1\nextends: [\"{src}\"]\nimages: [redis:7]\n"),
    )]);
    let cache = dir.path().join("cache");
    let opts = ResolveOptions {
        remote_cache: Some(cache.clone()),
        ..Default::default()
    };

    let r = resolve(&entry, &opts);
    assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics);
    assert_eq!(r.spec.as_ref().unwrap().packages.len(), 1);
    let lock_path = entry.with_file_name("veneer.lock");
    let lock = std::fs::read_to_string(&lock_path).unwrap();
    assert!(
        lock.contains("sha256-") && lock.contains("\"commit\""),
        "{lock}"
    );

    // Upstream moves on: the pinned commit keeps resolving to the old content.
    std::fs::write(up.join("envs/base.yaml"), "version: 1\npackages: [curl]\n").unwrap();
    git(&up, &["commit", "-qam", "v2"]);
    let r = resolve(&entry, &opts);
    assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics);
    assert_eq!(format!("{:?}", r.spec.unwrap().packages), "[Name(\"git\")]");

    // --update re-pins to the new commit.
    let r = resolve(
        &entry,
        &ResolveOptions {
            update_lock: true,
            ..opts.clone()
        },
    );
    assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics);
    assert_eq!(
        format!("{:?}", r.spec.unwrap().packages),
        "[Name(\"curl\")]"
    );

    // Tampered lock digest -> V1005.
    let tampered = std::fs::read_to_string(&lock_path)
        .unwrap()
        .replace("sha256-", "sha256-00");
    std::fs::write(&lock_path, tampered).unwrap();
    let r = resolve(&entry, &opts);
    assert_eq!(r.diagnostics[0].code, "V1005", "{:?}", r.diagnostics);
}

#[test]
fn builtin_modules_merge_with_inputs() {
    let (_d, entry) = common::tree(&[(
        ".veneer/veneer.yaml",
        "version: 1\nuse:\n  - module: veneer/harness/claude-code@1\n    with: { claude_code_version: \"1.2.3\" }\n",
    )]);
    let r = resolve(&entry, &ResolveOptions::default());
    assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics);
    let tree = r.tree.unwrap().to_json();
    assert_eq!(tree["vars"]["claude_code_version"], "1.2.3");
    assert!(tree["tools"]["npm:@anthropic-ai/claude-code"].is_string());
    let r = resolve(
        &common::tree(&[("v.yaml", "version: 1\nuse: [veneer/nope@1]\n")]).1,
        &ResolveOptions::default(),
    );
    assert_eq!(r.diagnostics[0].code, "V1001");
}
