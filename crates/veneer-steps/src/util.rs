//! Shared helpers for step implementations.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use veneer_exec::{ExecError, Log, Result, run_logged};

pub fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Finds an executable on `PATH`.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Runs a command quietly and reports whether it exited 0.
pub fn succeeds(cmd: &mut Command) -> bool {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Runs a command and captures stdout (stderr discarded). `None` on failure.
pub fn capture(cmd: &mut Command) -> Option<String> {
    let out = cmd
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `sh -c <script>` in `cwd` with extra environment, logged.
pub fn sh(script: &str, cwd: &Path, env: &BTreeMap<String, String>, log: &mut Log) -> Result<()> {
    std::fs::create_dir_all(cwd)
        .map_err(|e| ExecError::failed(format!("cannot create {}: {e}", cwd.display())))?;
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script).current_dir(cwd).envs(env);
    run_logged(&mut cmd, log).map_err(|mut e| {
        if e.exit == 3 {
            e.message = format!("`{script}` failed");
        }
        e
    })
}

/// A command prefixed with `sudo -n` when not root and sudo exists.
pub fn privileged(program: &str) -> Result<Command> {
    if veneer_plan::is_root() {
        return Ok(Command::new(program));
    }
    if which("sudo").is_some() {
        let mut c = Command::new("sudo");
        c.arg("-n").arg(program);
        return Ok(c);
    }
    Err(ExecError::failed(format!(
        "V4001: `{program}` needs root, and sudo is not available"
    )))
}

/// Moves `from` to `to`, replacing nothing (fails if `to` exists).
pub fn rename_into_place(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ExecError::failed(format!("cannot create {}: {e}", parent.display())))?;
    }
    std::fs::rename(from, to).map_err(|e| {
        ExecError::failed(format!(
            "cannot move {} to {}: {e}",
            from.display(),
            to.display()
        ))
    })
}

/// A sibling temp path for staging `dest` (`.veneer-tmp-<name>`), cleared first.
pub fn staging_path(dest: &Path) -> Result<PathBuf> {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "x".into());
    let tmp = dest.with_file_name(format!(".veneer-tmp-{name}"));
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp)
            .or_else(|_| std::fs::remove_file(&tmp))
            .map_err(|e| ExecError::failed(format!("cannot clear {}: {e}", tmp.display())))?;
    }
    if let Some(parent) = tmp.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ExecError::failed(format!("cannot create {}: {e}", parent.display())))?;
    }
    Ok(tmp)
}

pub fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks for existence/permission.
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0 && !is_zombie(pid)
}

fn is_zombie(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rsplit_once(") ").map(|(_, rest)| rest.starts_with('Z')))
        .unwrap_or(false)
}
