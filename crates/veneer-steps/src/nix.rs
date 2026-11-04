//! Nix backend for `nix:` tools: one profile owned by veneer at `<data>/nix/profile`.
//! The installed set is recorded next to it, so the profile is rebuilt (atomically, as a
//! new Nix generation) only when the requested references change.

use std::path::PathBuf;
use std::process::Command;

use veneer_exec::{Check, ExecError, Log, Result, run_logged, write_atomic};
use veneer_plan::Dirs;

use crate::util::which;

pub fn profile(dirs: &Dirs) -> PathBuf {
    dirs.data.join("nix").join("profile")
}

fn manifest(dirs: &Dirs) -> PathBuf {
    dirs.data.join("nix").join("refs.json")
}

fn nix() -> Result<PathBuf> {
    which("nix")
        .or_else(|| {
            [
                "/nix/var/nix/profiles/default/bin/nix",
                "/root/.nix-profile/bin/nix",
            ]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
        })
        .ok_or_else(|| {
            ExecError::failed("`nix:` tools need Nix installed (https://nixos.org/download)")
        })
}

fn recorded(dirs: &Dirs) -> Option<Vec<String>> {
    serde_json::from_slice(&std::fs::read(manifest(dirs)).ok()?).ok()
}

pub fn check(dirs: &Dirs, refs: &[String]) -> Check {
    if recorded(dirs).as_deref() == Some(refs) && profile(dirs).join("bin").is_dir() {
        Check::Satisfied
    } else {
        Check::Needed("nix profile out of date".into())
    }
}

pub fn apply(dirs: &Dirs, refs: &[String], log: &mut Log) -> Result<()> {
    let nix = nix()?;
    let p = profile(dirs);
    std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| ExecError::failed(e.to_string()))?;
    // Remove the old profile link (generations stay until GC), then install everything in
    // one transaction so the profile flips atomically.
    let _ = std::fs::remove_file(&p);
    let mut c = Command::new(nix);
    c.args([
        "--extra-experimental-features",
        "nix-command flakes",
        "profile",
        "install",
        "--profile",
    ])
    .arg(&p)
    .args(refs);
    run_logged(&mut c, log)?;
    let bytes = serde_json::to_vec(refs).expect("refs serialize");
    write_atomic(&manifest(dirs), &bytes).map_err(|e| ExecError::failed(e.to_string()))
}

pub fn verify(dirs: &Dirs, refs: &[String]) -> Result<String> {
    match check(dirs, refs) {
        Check::Satisfied => Ok(refs.join(",")),
        _ => Err(ExecError::failed("nix profile was not created")),
    }
}
