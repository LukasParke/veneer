//! Services: Compose engine when present, native supervised processes otherwise.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use veneer_exec::{Check, Cx, ExecError, Result, run_logged, write_atomic};
use veneer_plan::{Dirs, Healthcheck, Plan, Step, StepKind};

use crate::command_env;
use crate::util::{capture, pid_alive, succeeds};

fn compose_files(step: &Step, dirs: &Dirs, id: &str) -> Vec<PathBuf> {
    let StepKind::Compose {
        files, generated, ..
    } = &step.kind
    else {
        return Vec::new();
    };
    let mut all = files.clone();
    if generated.is_some() {
        all.push(dirs.workspace_state(id).join("compose.json"));
    }
    all
}

fn compose_cmd(project: &str, files: &[PathBuf]) -> Command {
    let mut c = Command::new("docker");
    c.args(["compose", "-p", project]);
    for f in files {
        c.arg("-f").arg(f);
    }
    c
}

pub fn compose_check(step: &Step, cx: &mut Cx<'_>) -> Check {
    let StepKind::Compose {
        project, generated, ..
    } = &step.kind
    else {
        return Check::Unknown;
    };
    if let Some(g) = generated {
        let path = cx
            .dirs
            .workspace_state(&cx.plan.workspace_id)
            .join("compose.json");
        if std::fs::read_to_string(&path).ok().as_deref() != Some(g.as_str()) {
            return Check::Needed("compose definition changed".into());
        }
    }
    let files = compose_files(step, cx.dirs, &cx.plan.workspace_id);
    let wanted = capture(compose_cmd(project, &files).args(["config", "--services"]));
    let running =
        capture(compose_cmd(project, &files).args(["ps", "--services", "--status", "running"]));
    match (wanted, running) {
        (Some(w), Some(r)) => {
            let mut w: Vec<&str> = w.lines().filter(|l| !l.is_empty()).collect();
            let mut r: Vec<&str> = r.lines().filter(|l| !l.is_empty()).collect();
            w.sort();
            r.sort();
            if w == r {
                Check::Satisfied
            } else {
                Check::Needed("services not running".into())
            }
        }
        _ => Check::Needed("compose services not running".into()),
    }
}

pub fn compose_apply(step: &Step, cx: &mut Cx<'_>) -> Result<()> {
    let StepKind::Compose {
        project, generated, ..
    } = &step.kind
    else {
        return Ok(());
    };
    if let Some(g) = generated {
        let path = cx
            .dirs
            .workspace_state(&cx.plan.workspace_id)
            .join("compose.json");
        write_atomic(&path, g.as_bytes()).map_err(|e| ExecError::failed(e.to_string()))?;
    }
    let files = compose_files(step, cx.dirs, &cx.plan.workspace_id);
    let mut c = compose_cmd(project, &files);
    c.args(["up", "-d", "--wait", "--remove-orphans"]);
    run_logged(&mut c, cx.log)
}

// ---------------------------------------------------------------- native

fn service_dir(dirs: &Dirs, id: &str) -> PathBuf {
    dirs.workspace_state(id).join("services")
}

fn read_pid(dirs: &Dirs, id: &str, name: &str) -> Option<i32> {
    std::fs::read_to_string(service_dir(dirs, id).join(format!("{name}.pid")))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Whether the native service `name` of this workspace has a live process.
pub fn native_running(dirs: &Dirs, plan: &veneer_plan::Plan, name: &str) -> bool {
    read_pid(dirs, &plan.workspace_id, name).is_some_and(pid_alive)
}

fn http_ok(url: &str) -> bool {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(2)))
        .http_status_as_error(false)
        .build()
        .into();
    agent
        .get(url)
        .call()
        .is_ok_and(|r| r.status().as_u16() < 500)
}

fn probe(
    hc: Option<&Healthcheck>,
    env: &std::collections::BTreeMap<String, String>,
    cwd: &PathBuf,
) -> bool {
    match hc {
        Some(Healthcheck { cmd: Some(c), .. }) => succeeds(
            Command::new("sh")
                .arg("-c")
                .arg(c)
                .current_dir(cwd)
                .envs(env),
        ),
        Some(Healthcheck { http: Some(u), .. }) => http_ok(u),
        _ => true,
    }
}

pub fn native_check(step: &Step, cx: &mut Cx<'_>) -> Check {
    let StepKind::Native {
        name,
        healthcheck,
        cwd,
        ..
    } = &step.kind
    else {
        return Check::Unknown;
    };
    match read_pid(cx.dirs, &cx.plan.workspace_id, name) {
        Some(pid) if pid_alive(pid) => {
            let env = command_env(cx.plan, cx.dirs, true);
            if probe(healthcheck.as_ref(), &env, cwd) {
                Check::Satisfied
            } else {
                Check::Needed("unhealthy".into())
            }
        }
        _ => Check::Needed("not running".into()),
    }
}

fn stop_pid(pid: i32) {
    // SAFETY: signalling the service's own process group.
    unsafe { libc::kill(-pid, libc::SIGTERM) };
    for _ in 0..100 {
        if !pid_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    unsafe { libc::kill(-pid, libc::SIGKILL) };
}

pub fn native_apply(step: &Step, cx: &mut Cx<'_>) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let StepKind::Native {
        name,
        run,
        cwd,
        env: svc_env,
        healthcheck,
        ..
    } = &step.kind
    else {
        return Ok(());
    };
    let id = cx.plan.workspace_id.clone();
    if let Some(pid) = read_pid(cx.dirs, &id, name)
        && pid_alive(pid)
    {
        stop_pid(pid);
    }
    let dir = service_dir(cx.dirs, &id);
    std::fs::create_dir_all(&dir).map_err(|e| ExecError::failed(e.to_string()))?;
    std::fs::create_dir_all(cwd).map_err(|e| ExecError::failed(e.to_string()))?;
    let log_path = dir.join(format!("{name}.log"));
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| ExecError::failed(e.to_string()))?;
    let mut env = command_env(cx.plan, cx.dirs, true);
    env.extend(svc_env.iter().map(|(k, v)| (k.clone(), v.clone())));
    let child = Command::new("sh")
        .arg("-c")
        .arg(format!("exec {run}"))
        .current_dir(cwd)
        .envs(&env)
        .stdin(Stdio::null())
        .stdout(
            log.try_clone()
                .map_err(|e| ExecError::failed(e.to_string()))?,
        )
        .stderr(log)
        .process_group(0)
        .spawn()
        .map_err(|e| ExecError::failed(format!("cannot start service `{name}`: {e}")))?;
    let pid = child.id() as i32;
    std::mem::forget(child); // detached: the service outlives this process
    write_atomic(&dir.join(format!("{name}.pid")), pid.to_string().as_bytes())
        .map_err(|e| ExecError::failed(e.to_string()))?;
    cx.log.line(&format!(
        "started `{name}` as pid {pid}; log: {}",
        log_path.display()
    ));

    let (interval, retries) = healthcheck
        .as_ref()
        .map_or((200, 10), |h| (h.interval_ms, h.retries));
    for _ in 0..=retries {
        if !pid_alive(pid) {
            let tail = std::fs::read_to_string(&log_path).unwrap_or_default();
            cx.log.line(&tail);
            return Err(ExecError::failed(format!(
                "service `{name}` exited during startup"
            )));
        }
        if probe(healthcheck.as_ref(), &env, cwd) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(interval));
    }
    Err(ExecError::failed(format!(
        "service `{name}` did not become healthy"
    )))
}

/// Human-readable status lines for `veneer status`-style output.
pub fn service_status(plan: &Plan, dirs: &Dirs) -> Vec<(String, bool)> {
    plan.steps
        .iter()
        .filter_map(|s| match &s.kind {
            StepKind::Native { name, .. } => Some((
                name.clone(),
                read_pid(dirs, &plan.workspace_id, name).is_some_and(pid_alive),
            )),
            _ => None,
        })
        .collect()
}

/// Stops every service of the plan's workspace.
pub fn down(plan: &Plan, dirs: &Dirs, log: &mut veneer_exec::Log) -> Result<()> {
    for step in &plan.steps {
        match &step.kind {
            StepKind::Native { name, .. } => {
                if let Some(pid) = read_pid(dirs, &plan.workspace_id, name) {
                    if pid_alive(pid) {
                        stop_pid(pid);
                    }
                    let _ = std::fs::remove_file(
                        service_dir(dirs, &plan.workspace_id).join(format!("{name}.pid")),
                    );
                }
            }
            StepKind::Compose { project, .. } => {
                let files = compose_files(step, dirs, &plan.workspace_id);
                let present = files.iter().all(|f| f.is_file());
                if present {
                    let mut c = compose_cmd(project, &files);
                    c.args(["down", "--remove-orphans"]);
                    run_logged(&mut c, log)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}
