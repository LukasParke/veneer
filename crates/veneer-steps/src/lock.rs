//! `veneer lock`: pins every tool version, image digest, and repo commit (standards
//! proposal §5.4.1). Resolution delegates to the tools that own each namespace: `mise latest`,
//! `crane digest`, and `git ls-remote`.

use std::process::Command;

use veneer_exec::{ExecError, Log, Result};
use veneer_plan::{Dirs, Pins, Plan, RepoMode, StepKind, repo_key, tool_key};

use crate::mise;

fn output(cmd: &mut Command, what: &str, log: &mut Log) -> Result<String> {
    log.line(&format!("$ {cmd:?}"));
    let out = cmd
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| ExecError::failed(format!("cannot run {what}: {e}")))?;
    log.line(&String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        return Err(ExecError::failed(format!(
            "{what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn is_commit(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// The commit `git_ref` (default `HEAD`) points at in `url`.
pub fn remote_commit(url: &str, git_ref: Option<&str>, log: &mut Log) -> Result<String> {
    let r = git_ref.unwrap_or("HEAD");
    if is_commit(r) {
        return Ok(r.to_string());
    }
    let text = output(
        Command::new("git").env("GIT_TERMINAL_PROMPT", "0").args([
            "ls-remote",
            url,
            r,
            &format!("{r}^{{}}"),
        ]),
        &format!("git ls-remote {url} {r}"),
        log,
    )?;
    // Prefer the peeled tag (`^{}`), then an exact match, then the first line.
    let lines: Vec<(&str, &str)> = text
        .lines()
        .filter_map(|l| l.split_once(char::is_whitespace))
        .collect();
    lines
        .iter()
        .find(|(_, name)| name.ends_with("^{}"))
        .or_else(|| lines.first())
        .map(|(sha, _)| sha.to_string())
        .ok_or_else(|| ExecError::failed(format!("`{r}` does not exist in {url}")))
}

/// Resolves pins for every tool, image, and repo in `plan` (planned without pins). Existing
/// pins are kept unless `update`; pins for things the spec no longer mentions are dropped.
pub fn resolve(
    plan: &Plan,
    dirs: &Dirs,
    existing: &Pins,
    update: bool,
    log: &mut Log,
) -> Result<Pins> {
    let mut out = Pins::default();
    let mut mise_bin = None;
    for step in &plan.steps {
        match &step.kind {
            StepKind::Tools { tools, .. } => {
                for (name, requested) in tools {
                    let key = tool_key(name, requested);
                    if let Some(v) = existing.tools.get(&key).filter(|_| !update) {
                        out.tools.insert(key, v.clone());
                        continue;
                    }
                    let bin = match &mise_bin {
                        Some(b) => b,
                        None => mise_bin.insert(mise::ensure_mise(dirs, log)?),
                    };
                    let arg = if requested == "latest" {
                        name.clone()
                    } else {
                        format!("{name}@{requested}")
                    };
                    let v = output(
                        mise::mise_cmd(bin, dirs).args(["latest", &arg]),
                        &format!("mise latest {arg}"),
                        log,
                    )?;
                    let v = v.lines().last().unwrap_or_default().trim().to_string();
                    if v.is_empty() {
                        return Err(ExecError::failed(format!(
                            "mise found no version for {arg}"
                        )));
                    }
                    out.tools.insert(key, v);
                }
            }
            StepKind::Images { refs, .. } => {
                for r in refs.iter().filter(|r| !r.contains('@')) {
                    if let Some(d) = existing.images.get(r).filter(|_| !update) {
                        out.images.insert(r.clone(), d.clone());
                        continue;
                    }
                    let bin = match &mise_bin {
                        Some(b) => b,
                        None => mise_bin.insert(mise::ensure_mise(dirs, log)?),
                    };
                    let d = output(
                        mise::mise_cmd(bin, dirs).args([
                            "exec",
                            "aqua:google/go-containerregistry@latest",
                            "--",
                            "crane",
                            "digest",
                            r,
                        ]),
                        &format!("crane digest {r}"),
                        log,
                    )?;
                    let d = d.lines().last().unwrap_or_default().trim().to_string();
                    if !d.starts_with("sha256:") {
                        return Err(ExecError::failed(format!(
                            "crane digest {r} returned `{d}`"
                        )));
                    }
                    out.images.insert(r.clone(), d);
                }
            }
            StepKind::Repo {
                url, git_ref, mode, ..
            } if *mode != RepoMode::Copy => {
                let key = repo_key(url, git_ref.as_deref());
                if let Some(c) = existing.repos.get(&key).filter(|_| !update) {
                    out.repos.insert(key, c.clone());
                    continue;
                }
                out.repos
                    .insert(key, remote_commit(url, git_ref.as_deref(), log)?);
            }
            _ => {}
        }
    }
    Ok(out)
}
