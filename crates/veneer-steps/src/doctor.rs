//! Preflight (`veneer doctor`, standards proposal §5.4.3): the host programs a plan needs,
//! checked before any step runs, so a run fails up front instead of half-way.

use std::path::Path;
use std::process::Command;

use veneer_plan::{Plan, RepoMode, StepKind};

use crate::util::{succeeds, which};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    pub code: &'static str,
    /// What needs it (step id).
    pub step: String,
    pub need: String,
    pub ok: bool,
    /// A missing hard requirement fails the run before any step starts; a soft one warns
    /// (the plan itself might provide it, e.g. a hook that installs Docker).
    pub hard: bool,
    pub hint: String,
}

const CA_BUNDLES: &[&str] = &[
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/cert.pem",
    "/etc/ssl/certs/ca-bundle.crt",
];

fn docker_daemon() -> bool {
    which("docker").is_some()
        && succeeds(Command::new("docker").args(["info", "--format", "{{.ServerVersion}}"]))
}

/// Requirements of `plan` on this host.
pub fn check(plan: &Plan) -> Vec<Requirement> {
    let mut out = Vec::new();
    let packages: Vec<&String> = plan
        .steps
        .iter()
        .filter_map(|s| match &s.kind {
            StepKind::Packages { names } => Some(names.iter()),
            _ => None,
        })
        .flatten()
        .collect();
    let provides = |p: &str| packages.iter().any(|n| n.as_str() == p);
    let has_build_hooks = plan
        .steps
        .iter()
        .any(|s| matches!(s.kind, StepKind::Hook { .. }) && s.phase == veneer_plan::Phase::Build);
    let mut push = |code, step: &str, need: String, ok: bool, hard: bool, hint: &str| {
        if !out
            .iter()
            .any(|r: &Requirement| r.code == code && r.need == need)
        {
            out.push(Requirement {
                code,
                step: step.to_string(),
                need,
                ok,
                hard,
                hint: hint.to_string(),
            });
        }
    };
    for s in &plan.steps {
        match &s.kind {
            StepKind::Packages { names } => match crate::missing_packages(names) {
                Err(_) => push(
                    "V5001",
                    &s.id,
                    "a package manager (apt-get, apk, dnf, brew)".into(),
                    false,
                    true,
                    "install packages another way, or drop `packages:` for this host",
                ),
                Ok(missing)
                    if !missing.is_empty()
                        && !veneer_plan::is_root()
                        && which("brew").is_none() =>
                {
                    let sudo = which("sudo").is_some()
                        && succeeds(Command::new("sudo").args(["-n", "true"]));
                    push(
                        "V5002",
                        &s.id,
                        format!(
                            "root or passwordless sudo to install {}",
                            missing.join(", ")
                        ),
                        sudo,
                        true,
                        "run as root, or allow `sudo -n` for the package manager",
                    );
                }
                Ok(_) => {}
            },
            StepKind::Tools { .. } => {
                let supported = matches!(std::env::consts::OS, "linux" | "macos")
                    && matches!(std::env::consts::ARCH, "x86_64" | "aarch64");
                push(
                    "V5007",
                    &s.id,
                    format!(
                        "a mise build for {}/{}",
                        std::env::consts::OS,
                        std::env::consts::ARCH
                    ),
                    supported,
                    true,
                    "mise ships linux and macOS builds for x86_64 and arm64",
                );
            }
            StepKind::Nix { .. } => {
                let ok = which("nix").is_some()
                    || Path::new("/nix/var/nix/profiles/default/bin/nix").is_file();
                push(
                    "V5004",
                    &s.id,
                    "nix".into(),
                    ok,
                    !has_build_hooks,
                    "install Nix (https://nixos.org/download)",
                );
            }
            StepKind::Repo { mode, dest, .. }
                if *mode != RepoMode::Copy
                    && crate::repos::check(dest, *mode) != veneer_exec::Check::Satisfied =>
            {
                push(
                    "V5003",
                    &s.id,
                    "git".into(),
                    which("git").is_some() || provides("git"),
                    true,
                    "add `git` to `packages`",
                );
            }
            StepKind::Images { .. } => {
                let ok = docker_daemon()
                    || !cfg!(target_os = "linux")
                    || std::env::var_os("SSL_CERT_FILE").is_some()
                    || CA_BUNDLES.iter().any(|p| Path::new(p).is_file())
                    || provides("ca-certificates");
                push(
                    "V5005",
                    &s.id,
                    "a Docker daemon, or a CA bundle for daemonless pulls".into(),
                    ok,
                    true,
                    "add `ca-certificates` to `packages`",
                );
            }
            StepKind::Compose { .. } => {
                let ok = which("docker").is_some()
                    && succeeds(Command::new("docker").args(["compose", "version"]));
                push(
                    "V5006",
                    &s.id,
                    "docker with the compose plugin".into(),
                    ok,
                    !has_build_hooks,
                    "install Docker Engine + compose, or use native `run:` services",
                );
            }
            StepKind::Deps { manager, .. } => {
                let bin = format!("{manager:?}").to_lowercase();
                let tools_provide = plan
                    .steps
                    .iter()
                    .any(|t| matches!(&t.kind, StepKind::Tools { .. }));
                push(
                    "W5010",
                    &s.id,
                    bin.clone(),
                    which(&bin).is_some() || tools_provide,
                    false,
                    "declare the toolchain under `tools:`",
                );
            }
            _ => {}
        }
    }
    out
}

/// Missing hard requirements as one error message, or `None` when the plan can run.
pub fn blocking(reqs: &[Requirement]) -> Option<String> {
    let missing: Vec<String> = reqs
        .iter()
        .filter(|r| r.hard && !r.ok)
        .map(|r| format!("  {}: {} needs {} ({})", r.code, r.step, r.need, r.hint))
        .collect();
    (!missing.is_empty()).then(|| {
        format!(
            "this host is missing what the plan needs (nothing was changed):\n{}",
            missing.join("\n")
        )
    })
}
