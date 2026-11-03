//! Git repositories: clone, worktree, or copy, all staged through a local mirror cache.
//! Existing checkouts are never clobbered: an agent may have work in progress there.

use std::path::{Path, PathBuf};
use std::process::Command;

use veneer_exec::{Check, Cx, ExecError, Result, run_logged};
use veneer_plan::RepoMode;

use crate::util::{capture, digest, rename_into_place, staging_path};

fn is_checkout(dest: &Path) -> bool {
    capture(
        Command::new("git")
            .arg("-C")
            .arg(dest)
            .args(["rev-parse", "--show-toplevel"]),
    )
    .is_some_and(|top| Path::new(top.trim()).canonicalize().ok() == dest.canonicalize().ok())
}

pub fn check(dest: &Path, mode: RepoMode) -> Check {
    match mode {
        RepoMode::Copy if dest.is_dir() => Check::Satisfied,
        RepoMode::Clone | RepoMode::Worktree if dest.is_dir() && is_checkout(dest) => {
            Check::Satisfied
        }
        _ if dest.exists() => Check::Needed(format!(
            "{} exists but is not a valid checkout",
            dest.display()
        )),
        _ => Check::Needed("not checked out".into()),
    }
}

pub fn fingerprint(dest: &Path, mode: RepoMode) -> String {
    match mode {
        RepoMode::Copy => "copy".into(),
        _ => capture(
            Command::new("git")
                .arg("-C")
                .arg(dest)
                .args(["rev-parse", "HEAD"]),
        )
        .map(|s| s.trim().to_string())
        .unwrap_or_default(),
    }
}

fn mirror_path(cx: &Cx<'_>, url: &str) -> PathBuf {
    cx.dirs
        .cache
        .join("git")
        .join(format!("{}.git", &digest(url.as_bytes())[..16]))
}

/// Creates or refreshes a bare mirror of `url`. Staged, so an interrupted clone leaves nothing.
fn update_mirror(cx: &mut Cx<'_>, url: &str) -> Result<PathBuf> {
    let mirror = mirror_path(cx, url);
    if mirror.join("HEAD").is_file() {
        run_logged(
            Command::new("git")
                .arg("-C")
                .arg(&mirror)
                .args(["fetch", "--prune", "--tags", "origin"]),
            cx.log,
        )?;
        let _ = run_logged(
            Command::new("git")
                .arg("-C")
                .arg(&mirror)
                .args(["worktree", "prune"]),
            cx.log,
        );
    } else {
        let tmp = staging_path(&mirror)?;
        run_logged(
            Command::new("git")
                .args(["clone", "--mirror", url])
                .arg(&tmp),
            cx.log,
        )?;
        rename_into_place(&tmp, &mirror)?;
    }
    Ok(mirror)
}

fn ensure_parent(dest: &Path) -> Result<()> {
    if let Some(p) = dest.parent() {
        std::fs::create_dir_all(p)
            .map_err(|e| ExecError::failed(format!("cannot create {}: {e}", p.display())))?;
    }
    Ok(())
}

pub fn apply(
    url: &str,
    git_ref: Option<&str>,
    dest: &Path,
    mode: RepoMode,
    cx: &mut Cx<'_>,
) -> Result<()> {
    if dest.exists() {
        // A stale, half-created staging leftover is safe to clear; anything else is the user's.
        let empty = std::fs::read_dir(dest)
            .map(|mut d| d.next().is_none())
            .unwrap_or(false);
        if empty {
            std::fs::remove_dir(dest).map_err(|e| ExecError::failed(e.to_string()))?;
        } else {
            return Err(ExecError::failed(format!(
                "{} exists and is not a {mode:?} checkout; refusing to overwrite it",
                dest.display()
            )));
        }
    }
    ensure_parent(dest)?;
    match mode {
        RepoMode::Copy => {
            let tmp = staging_path(dest)?;
            std::fs::create_dir_all(&tmp).map_err(|e| ExecError::failed(e.to_string()))?;
            run_logged(
                Command::new("cp")
                    .arg("-R")
                    .arg(format!("{url}/."))
                    .arg(&tmp),
                cx.log,
            )?;
            rename_into_place(&tmp, dest)
        }
        RepoMode::Clone => {
            let mirror = update_mirror(cx, url)?;
            let tmp = staging_path(dest)?;
            run_logged(
                Command::new("git").arg("clone").arg(&mirror).arg(&tmp),
                cx.log,
            )?;
            run_logged(
                Command::new("git")
                    .arg("-C")
                    .arg(&tmp)
                    .args(["remote", "set-url", "origin", url]),
                cx.log,
            )?;
            if let Some(r) = git_ref {
                run_logged(
                    Command::new("git")
                        .arg("-C")
                        .arg(&tmp)
                        .args(["checkout", "--quiet", r]),
                    cx.log,
                )?;
            }
            rename_into_place(&tmp, dest)
        }
        RepoMode::Worktree => {
            let mirror = update_mirror(cx, url)?;
            let tmp = staging_path(dest)?;
            run_logged(
                Command::new("git")
                    .arg("-C")
                    .arg(&mirror)
                    .args(["worktree", "add", "--detach"])
                    .arg(&tmp)
                    .arg(git_ref.unwrap_or("HEAD")),
                cx.log,
            )?;
            run_logged(
                Command::new("git")
                    .arg("-C")
                    .arg(&mirror)
                    .args(["worktree", "move"])
                    .arg(&tmp)
                    .arg(dest),
                cx.log,
            )
        }
    }
}
