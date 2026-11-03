//! veneer-steps: concrete step implementations. Each delegates the real work to a
//! battle-tested tool and owns only check/apply/verify.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use veneer_exec::{Check, Cx, ExecError, Result, Steps, run_logged, write_atomic};
use veneer_plan::{Dirs, Plan, Step, StepKind, adapters};

pub mod mise;
mod repos;
mod services;
mod util;

pub use services::{down, service_status};
use util::{capture, digest, privileged, sh, succeeds, which};

/// The process environment for commands veneer runs: host env + mise tool env (+ workspace env).
pub fn command_env(plan: &Plan, dirs: &Dirs, workspace: bool) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = std::env::vars().collect();
    env.extend(mise::tool_env(dirs));
    if workspace {
        env.extend(plan.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    env
}

/// Shell-quoted `export K='v'` lines.
pub fn export_lines(env: &indexmap::IndexMap<String, String>) -> String {
    env.iter()
        .map(|(k, v)| format!("export {k}='{}'\n", v.replace('\'', "'\\''")))
        .collect()
}

fn dotenv(plan: &Plan) -> String {
    plan.env.iter().map(|(k, v)| format!("{k}={v}\n")).collect()
}

// ---------------------------------------------------------------- packages

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pm {
    Apt,
    Apk,
    Dnf,
    Brew,
}

fn package_manager() -> Result<Pm> {
    if which("apt-get").is_some() {
        Ok(Pm::Apt)
    } else if which("apk").is_some() {
        Ok(Pm::Apk)
    } else if which("dnf").is_some() {
        Ok(Pm::Dnf)
    } else if which("brew").is_some() {
        Ok(Pm::Brew)
    } else {
        Err(ExecError::failed(
            "no supported package manager found (apt-get, apk, dnf, brew)",
        ))
    }
}

fn installed(pm: Pm, name: &str) -> bool {
    match pm {
        Pm::Apt => capture(Command::new("dpkg-query").args(["-W", "-f=${Status}", name]))
            .is_some_and(|s| s.contains("install ok installed")),
        Pm::Apk => succeeds(Command::new("apk").args(["info", "-e", name])),
        Pm::Dnf => succeeds(Command::new("rpm").args(["-q", "--whatprovides", name])),
        Pm::Brew => succeeds(Command::new("brew").args(["list", "--versions", name])),
    }
}

fn missing_packages(names: &[String]) -> Result<Vec<String>> {
    let pm = package_manager()?;
    Ok(names
        .iter()
        .filter(|n| !installed(pm, n))
        .cloned()
        .collect())
}

fn apply_packages(names: &[String], cx: &mut Cx<'_>) -> Result<()> {
    let pm = package_manager()?;
    let missing = missing_packages(names)?;
    if missing.is_empty() {
        return Ok(());
    }
    match pm {
        Pm::Apt => {
            let noninteractive = |mut c: Command| {
                c.env("DEBIAN_FRONTEND", "noninteractive");
                c
            };
            // Recover from an interrupted dpkg run.
            let mut fix = noninteractive(privileged("dpkg")?);
            fix.args(["--configure", "-a"]);
            run_logged(&mut fix, cx.log)?;
            let mut update = noninteractive(privileged("apt-get")?);
            update.args(["-o", "DPkg::Lock::Timeout=300", "update"]);
            run_logged(&mut update, cx.log)?;
            let mut install = noninteractive(privileged("apt-get")?);
            install
                .args([
                    "-o",
                    "DPkg::Lock::Timeout=300",
                    "install",
                    "-y",
                    "--no-install-recommends",
                ])
                .args(&missing);
            run_logged(&mut install, cx.log)
        }
        Pm::Apk => {
            let mut c = privileged("apk")?;
            c.args(["add", "--no-cache"]).args(&missing);
            run_logged(&mut c, cx.log)
        }
        Pm::Dnf => {
            let mut c = privileged("dnf")?;
            c.args(["install", "-y", "--setopt=install_weak_deps=False"])
                .args(&missing);
            run_logged(&mut c, cx.log)
        }
        Pm::Brew => run_logged(Command::new("brew").arg("install").args(&missing), cx.log),
    }
}

// ---------------------------------------------------------------- images

fn docker_ready() -> bool {
    which("docker").is_some()
        && succeeds(Command::new("docker").args(["info", "--format", "{{.ServerVersion}}"]))
}

fn image_present(r: &str) -> bool {
    succeeds(Command::new("docker").args(["image", "inspect", r]))
}

fn image_tar(dirs: &Dirs, r: &str) -> PathBuf {
    let name: String = r
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    dirs.data.join("images").join(format!("{name}.tar"))
}

fn apply_images(refs: &[String], cx: &mut Cx<'_>) -> Result<()> {
    if docker_ready() {
        for r in refs.iter().filter(|r| !image_present(r)) {
            run_logged(Command::new("docker").args(["pull", r]), cx.log)?;
        }
        return Ok(());
    }
    // No daemon (e.g. inside an image build): save docker-loadable tarballs with crane.
    let bin = mise::ensure_mise(cx.dirs, cx.log)?;
    let platform = format!(
        "linux/{}",
        if std::env::consts::ARCH == "aarch64" {
            "arm64"
        } else {
            "amd64"
        }
    );
    for r in refs {
        let tar = image_tar(cx.dirs, r);
        if tar.is_file() {
            continue;
        }
        let tmp = tar.with_extension("tar.part");
        std::fs::create_dir_all(tar.parent().unwrap())
            .map_err(|e| ExecError::failed(e.to_string()))?;
        let mut c = mise::mise_cmd(&bin, cx.dirs);
        c.args([
            "exec",
            "aqua:google/go-containerregistry@latest",
            "--",
            "crane",
            "pull",
            "--platform",
            &platform,
            r,
        ])
        .arg(&tmp);
        run_logged(&mut c, cx.log)?;
        std::fs::rename(&tmp, &tar).map_err(|e| ExecError::failed(e.to_string()))?;
    }
    Ok(())
}

fn images_check(refs: &[String], dirs: &Dirs) -> Check {
    if docker_ready() {
        let missing: Vec<_> = refs.iter().filter(|r| !image_present(r)).cloned().collect();
        if missing.is_empty() {
            Check::Satisfied
        } else {
            Check::Needed(format!("missing images: {}", missing.join(", ")))
        }
    } else {
        let missing: Vec<_> = refs
            .iter()
            .filter(|r| !image_tar(dirs, r).is_file())
            .cloned()
            .collect();
        if missing.is_empty() {
            Check::Satisfied
        } else {
            Check::Needed(format!("missing image tarballs: {}", missing.join(", ")))
        }
    }
}

fn images_load_check(refs: &[String], dirs: &Dirs) -> Check {
    if !docker_ready() {
        return Check::Satisfied;
    }
    let loadable: Vec<_> = refs
        .iter()
        .filter(|r| !image_present(r) && image_tar(dirs, r).is_file())
        .cloned()
        .collect();
    if loadable.is_empty() {
        Check::Satisfied
    } else {
        Check::Needed(format!("images to load: {}", loadable.join(", ")))
    }
}

// ---------------------------------------------------------------- files

fn file_check(dest: &Path, content: &str, mode: Option<u32>) -> Check {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::read(dest) {
        Ok(bytes) if bytes == content.as_bytes() => match mode {
            Some(m)
                if std::fs::metadata(dest)
                    .map(|md| md.permissions().mode() & 0o7777)
                    .ok()
                    != Some(m) =>
            {
                Check::Needed("mode differs".into())
            }
            _ => Check::Satisfied,
        },
        Ok(_) => Check::Needed("content differs".into()),
        Err(_) => Check::Needed("missing".into()),
    }
}

fn write_file(dest: &Path, content: &str, mode: Option<u32>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    write_atomic(dest, content.as_bytes())
        .map_err(|e| ExecError::failed(format!("cannot write {}: {e}", dest.display())))?;
    if let Some(m) = mode {
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(m))
            .map_err(|e| ExecError::failed(format!("cannot chmod {}: {e}", dest.display())))?;
    }
    Ok(())
}

/// Deletes files veneer wrote earlier that the plan no longer declares, but only while their
/// content is still exactly what veneer wrote.
pub fn prune_files(plan: &Plan, files: &mut BTreeMap<String, String>) -> Vec<PathBuf> {
    let declared: std::collections::HashSet<String> = plan
        .steps
        .iter()
        .filter_map(|s| match &s.kind {
            StepKind::File { dest, .. } => Some(dest.display().to_string()),
            _ => None,
        })
        .collect();
    let mut removed = Vec::new();
    files.retain(|path, recorded| {
        if declared.contains(path) {
            return true;
        }
        let p = PathBuf::from(path);
        if std::fs::read(&p).ok().map(|b| digest(&b)).as_deref() == Some(recorded.as_str())
            && std::fs::remove_file(&p).is_ok()
        {
            removed.push(p);
        }
        false
    });
    removed
}

// ---------------------------------------------------------------- dispatcher

pub struct StepImpls;

impl Steps for StepImpls {
    fn check(&self, step: &Step, cx: &mut Cx<'_>) -> Result<Check> {
        Ok(match &step.kind {
            StepKind::Packages { names } => {
                let missing = missing_packages(names)?;
                if missing.is_empty() {
                    Check::Satisfied
                } else {
                    Check::Needed(format!("missing packages: {}", missing.join(", ")))
                }
            }
            StepKind::Tools { tools } => mise::check(cx.dirs, tools),
            StepKind::Deps { dir, manager, .. } => match adapters::install_marker(*manager) {
                Some(m) if !dir.join(m).exists() => Check::Needed(format!("{m} missing")),
                _ => Check::Unknown,
            },
            StepKind::Images { refs } => images_check(refs, cx.dirs),
            StepKind::ImagesLoad { refs } => images_load_check(refs, cx.dirs),
            StepKind::Hook {
                creates,
                unless,
                cwd,
                ..
            } => {
                if creates.as_ref().is_some_and(|c| c.exists()) {
                    Check::Satisfied
                } else if let Some(u) = unless {
                    let env = command_env(
                        cx.plan,
                        cx.dirs,
                        step.phase == veneer_plan::Phase::Workspace,
                    );
                    let ok = cwd.is_dir()
                        && succeeds(
                            Command::new("sh")
                                .arg("-c")
                                .arg(u)
                                .current_dir(cwd)
                                .envs(&env),
                        );
                    if ok { Check::Satisfied } else { Check::Unknown }
                } else {
                    Check::Unknown
                }
            }
            StepKind::Env { file } => file_check(file, &dotenv(cx.plan), None),
            StepKind::File {
                dest,
                content,
                mode,
            } => file_check(dest, content, *mode),
            StepKind::Repo { dest, mode, .. } => repos::check(dest, *mode),
            StepKind::Compose { .. } => services::compose_check(step, cx),
            StepKind::Native { .. } => services::native_check(step, cx),
        })
    }

    fn apply(&self, step: &Step, cx: &mut Cx<'_>) -> Result<()> {
        match &step.kind {
            StepKind::Packages { names } => apply_packages(names, cx),
            StepKind::Tools { tools } => mise::apply(cx.dirs, tools, cx.log),
            StepKind::Deps {
                dir,
                manager,
                lockfile,
                ..
            } => {
                let env = command_env(cx.plan, cx.dirs, false);
                sh(
                    &adapters::install_command(*manager, lockfile.is_some(), dir),
                    dir,
                    &env,
                    cx.log,
                )
            }
            StepKind::Images { refs } => apply_images(refs, cx),
            StepKind::ImagesLoad { refs } => {
                for r in refs.iter().filter(|r| !image_present(r)) {
                    let tar = image_tar(cx.dirs, r);
                    if tar.is_file() {
                        run_logged(
                            Command::new("docker").arg("load").arg("-i").arg(&tar),
                            cx.log,
                        )?;
                    }
                }
                Ok(())
            }
            StepKind::Hook { run, cwd, .. } => {
                let env = command_env(
                    cx.plan,
                    cx.dirs,
                    step.phase == veneer_plan::Phase::Workspace,
                );
                sh(run, cwd, &env, cx.log)
            }
            StepKind::Env { file } => {
                write_file(file, &dotenv(cx.plan), Some(0o600))?;
                write_file(
                    &file.with_extension("sh"),
                    &export_lines(&cx.plan.env),
                    Some(0o600),
                )
            }
            StepKind::File {
                dest,
                content,
                mode,
            } => {
                write_file(dest, content, *mode)?;
                cx.files
                    .insert(dest.display().to_string(), digest(content.as_bytes()));
                Ok(())
            }
            StepKind::Repo {
                url,
                git_ref,
                dest,
                mode,
            } => repos::apply(url, git_ref.as_deref(), dest, *mode, cx),
            StepKind::Compose { .. } => services::compose_apply(step, cx),
            StepKind::Native { .. } => services::native_apply(step, cx),
        }
    }

    fn verify(&self, step: &Step, cx: &mut Cx<'_>) -> Result<String> {
        let expect = |c: Check, what: &str| match c {
            Check::Needed(why) => Err(ExecError::failed(format!(
                "{what} not in desired state after apply: {why}"
            ))),
            _ => Ok(()),
        };
        Ok(match &step.kind {
            StepKind::Packages { names } => {
                let missing = missing_packages(names)?;
                if !missing.is_empty() {
                    return Err(ExecError::failed(format!(
                        "packages still missing: {}",
                        missing.join(", ")
                    )));
                }
                names.join(",")
            }
            StepKind::Tools { tools } => mise::verify(cx.dirs, tools)?,
            StepKind::Deps {
                dir,
                manager,
                digest,
                ..
            } => {
                if let Some(m) = adapters::install_marker(*manager)
                    && !dir.join(m).exists()
                {
                    return Err(ExecError::failed(format!("{m} was not created")));
                }
                digest.clone()
            }
            StepKind::Images { refs } => {
                expect(images_check(refs, cx.dirs), "images")?;
                refs.join(",")
            }
            StepKind::ImagesLoad { refs } => {
                expect(images_load_check(refs, cx.dirs), "images")?;
                refs.join(",")
            }
            StepKind::Hook { run, creates, .. } => {
                if let Some(c) = creates
                    && !c.exists()
                {
                    return Err(ExecError::failed(format!(
                        "hook did not create {}",
                        c.display()
                    )));
                }
                digest(run.as_bytes())
            }
            StepKind::Env { file } => {
                expect(file_check(file, &dotenv(cx.plan), None), "env file")?;
                digest(dotenv(cx.plan).as_bytes())
            }
            StepKind::File {
                dest,
                content,
                mode,
            } => {
                expect(file_check(dest, content, *mode), "file")?;
                cx.files
                    .insert(dest.display().to_string(), digest(content.as_bytes()));
                digest(content.as_bytes())
            }
            StepKind::Repo { dest, mode, .. } => {
                expect(repos::check(dest, *mode), "repository")?;
                repos::fingerprint(dest, *mode)
            }
            StepKind::Compose { .. } => {
                expect(services::compose_check(step, cx), "compose services")?;
                "running".into()
            }
            StepKind::Native { .. } => {
                expect(services::native_check(step, cx), "service")?;
                "running".into()
            }
        })
    }
}
