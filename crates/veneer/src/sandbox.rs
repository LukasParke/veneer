//! Sandbox providers: `veneer up` creates sandboxes, stages the spec inside each, and
//! returns handles; `ls`, `enter`, and `rm` manage them (design spec §3, "both, as
//! separate layers": the in-environment stager stays the core).
//!
//! Providers: `docker` and `podman` (same CLI surface). Each sandbox is a container
//! labelled `dev.veneer.sandbox=1`, `dev.veneer.id=<id>`, `dev.veneer.spec=<spec>`.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use serde_json::json;

use crate::run::{Target, fail, load};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ProviderArg {
    Docker,
    Podman,
}

impl ProviderArg {
    fn cli(self) -> &'static str {
        match self {
            ProviderArg::Docker => "docker",
            ProviderArg::Podman => "podman",
        }
    }
}

#[derive(Debug, clap::Args)]
pub struct UpArgs {
    #[command(flatten)]
    pub target: Target,
    #[arg(long, value_enum, default_value_t = ProviderArg::Docker)]
    pub provider: ProviderArg,
    /// Image to start from (default: the spec's `base`; use a baked image for fast starts).
    #[arg(long)]
    pub image: Option<String>,
    /// Number of sandboxes to create in parallel.
    #[arg(long, default_value_t = 1)]
    pub count: usize,
    /// Sandbox id prefix (ids are `<prefix>-<n>`; default: the project name).
    #[arg(long)]
    pub name: Option<String>,
    /// Linux veneer binary to copy in when the image has none.
    #[arg(long, env = "VENEER_LINUX_BIN")]
    pub veneer_bin: Option<PathBuf>,
    /// Extra environment for the sandbox (repeatable): --env NAME=VALUE, or --env NAME to pass through.
    #[arg(long = "env", value_name = "NAME[=VALUE]")]
    pub envs: Vec<String>,
    /// Extra volume (repeatable): --volume SRC:DST[:ro].
    #[arg(long = "volume", short = 'v')]
    pub volumes: Vec<String>,
    /// Extra network to attach.
    #[arg(long)]
    pub network: Option<String>,
    /// NDJSON handles on stdout.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone)]
struct Handle {
    id: String,
    container: String,
    ok: bool,
    root: Option<String>,
    error: Option<String>,
}

fn cli_ok(cli: &str, args: &[&str]) -> bool {
    Command::new(cli)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn capture(cli: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cli)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

const PROJECT_IN_SANDBOX: &str = "/opt/veneer/project";

#[allow(clippy::too_many_arguments)]
fn create_one(
    cli: &str,
    id: &str,
    image: &str,
    spec_in_sandbox: &str,
    project: Option<&Path>,
    veneer_bin: Option<&Path>,
    args: &UpArgs,
) -> Handle {
    let container = format!("veneer-{id}");
    let mut h = Handle {
        id: id.to_string(),
        container: container.clone(),
        ok: false,
        root: None,
        error: None,
    };
    let _ = Command::new(cli).args(["rm", "-f", &container]).output();
    let mut run = Command::new(cli);
    run.args(["run", "-d", "--name", &container])
        .args(["--label", "dev.veneer.sandbox=1"])
        .args(["--label", &format!("dev.veneer.id={id}")])
        .args(["--label", &format!("dev.veneer.spec={spec_in_sandbox}")])
        .args(["-e", &format!("VENEER_WORKSPACE_ID={id}")]);
    for e in &args.envs {
        run.args(["-e", e]);
    }
    for v in &args.volumes {
        run.args(["-v", v]);
    }
    if let Some(n) = &args.network {
        run.args(["--network", n]);
    }
    run.args([
        "--entrypoint",
        "/bin/sh",
        image,
        "-c",
        "trap 'exit 0' TERM INT; while :; do sleep 3600 & wait $!; done",
    ]);
    match run.output() {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            h.error = Some(String::from_utf8_lossy(&o.stderr).trim().to_string());
            return h;
        }
        Err(e) => {
            h.error = Some(e.to_string());
            return h;
        }
    }
    if !cli_ok(cli, &["exec", &container, "sh", "-c", "command -v veneer"]) {
        let Some(bin) = veneer_bin else {
            h.error =
                Some("the image has no veneer binary: pass --veneer-bin <linux binary>".into());
            return h;
        };
        if !cli_ok(
            cli,
            &[
                "cp",
                &bin.display().to_string(),
                &format!("{container}:/usr/local/bin/veneer"),
            ],
        ) {
            h.error = Some("cannot copy the veneer binary into the sandbox".into());
            return h;
        }
    }
    if let Some(project) = project
        && !cli_ok(cli, &["exec", &container, "test", "-e", spec_in_sandbox])
    {
        let _ = Command::new(cli)
            .args(["exec", &container, "mkdir", "-p", "/opt/veneer"])
            .output();
        if !cli_ok(
            cli,
            &[
                "cp",
                &format!("{}/.", project.display()),
                &format!("{container}:{PROJECT_IN_SANDBOX}"),
            ],
        ) {
            h.error = Some("cannot copy the project into the sandbox".into());
            return h;
        }
    }
    let out = Command::new(cli)
        .args([
            "exec",
            &container,
            "veneer",
            "apply",
            spec_in_sandbox,
            "--id",
            id,
            "--json",
        ])
        .output();
    match out {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let done = stdout
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .find(|v| v["event"] == "run.done");
            match done {
                Some(d) if d["ok"] == true => {
                    h.ok = true;
                    h.root = d["root"].as_str().map(str::to_string);
                }
                Some(d) => {
                    h.error = Some(format!(
                        "apply failed at {}: {}",
                        d["step"].as_str().unwrap_or("?"),
                        d["error"].as_str().unwrap_or("?")
                    ))
                }
                None => h.error = Some(String::from_utf8_lossy(&o.stderr).trim().to_string()),
            }
        }
        Err(e) => h.error = Some(e.to_string()),
    }
    h
}

pub fn up(args: UpArgs) -> ExitCode {
    let cli = args.provider.cli();
    if !cli_ok(cli, &["version"]) {
        return fail(1, &format!("provider `{cli}` is not available"), args.json);
    }
    let remote = args
        .target
        .spec
        .as_deref()
        .is_some_and(veneer_resolve::discover::is_remote);
    let (image, spec_in_sandbox, project, name) = if remote {
        let Some(image) = args.image.clone() else {
            return fail(2, "a remote spec needs --image", args.json);
        };
        let spec = args.target.spec.clone().unwrap();
        (
            image,
            spec,
            None,
            args.name.clone().unwrap_or_else(|| "sandbox".into()),
        )
    } else {
        let l = match load(&args.target, args.json) {
            Ok(l) => l,
            Err(code) => return code,
        };
        let project = veneer_plan::project_root(&l.entry);
        let rel = l
            .entry
            .strip_prefix(&project)
            .unwrap_or(&l.entry)
            .display()
            .to_string();
        let Some(image) = args.image.clone().or(l.spec.base.clone()) else {
            return fail(2, "pass --image or set `base:` in the spec", args.json);
        };
        let name = args.name.clone().unwrap_or_else(|| {
            project
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_else(|| "sandbox".into())
        });
        (
            image,
            format!("{PROJECT_IN_SANDBOX}/{rel}"),
            Some(project),
            name,
        )
    };
    let veneer_bin = args.veneer_bin.clone().or_else(|| {
        cfg!(target_os = "linux")
            .then(|| std::env::current_exe().ok())
            .flatten()
    });
    let ids: Vec<String> = (1..=args.count.max(1))
        .map(|i| {
            if args.count <= 1 {
                name.clone()
            } else {
                format!("{name}-{i}")
            }
        })
        .collect();
    if !args.json {
        eprintln!(
            "▸ creating {} sandbox(es) from {image} with {cli}",
            ids.len()
        );
    }
    let handles: Vec<Handle> = std::thread::scope(|s| {
        let jobs: Vec<_> = ids
            .iter()
            .map(|id| {
                let (image, spec, project, bin, args) = (
                    &image,
                    &spec_in_sandbox,
                    project.as_deref(),
                    veneer_bin.as_deref(),
                    &args,
                );
                s.spawn(move || create_one(cli, id, image, spec, project, bin, args))
            })
            .collect();
        jobs.into_iter()
            .map(|j| j.join().expect("sandbox thread"))
            .collect()
    });
    let mut failed = 0;
    for h in &handles {
        if !h.ok {
            failed += 1;
        }
        if args.json {
            println!(
                "{}",
                json!({"event": "sandbox", "id": h.id, "provider": cli, "container": h.container, "ok": h.ok, "root": h.root, "error": h.error})
            );
        } else if h.ok {
            eprintln!(
                "✓ {} ({}) at {}",
                h.id,
                h.container,
                h.root.as_deref().unwrap_or("?")
            );
        } else {
            eprintln!("✗ {}: {}", h.id, h.error.as_deref().unwrap_or("failed"));
        }
    }
    if failed > 0 {
        ExitCode::from(3)
    } else {
        ExitCode::SUCCESS
    }
}

#[derive(Debug, clap::Args)]
pub struct LsArgs {
    #[arg(long, value_enum, default_value_t = ProviderArg::Docker)]
    pub provider: ProviderArg,
    #[arg(long)]
    pub json: bool,
}

fn sandboxes(cli: &str) -> Vec<(String, String, String)> {
    capture(
        cli,
        &[
            "ps",
            "-a",
            "--filter",
            "label=dev.veneer.sandbox=1",
            "--format",
            "{{.Label \"dev.veneer.id\"}}\t{{.Names}}\t{{.Status}}",
        ],
    )
    .unwrap_or_default()
    .lines()
    .filter_map(|l| {
        let mut p = l.splitn(3, '\t');
        Some((
            p.next()?.to_string(),
            p.next()?.to_string(),
            p.next().unwrap_or_default().to_string(),
        ))
    })
    .collect()
}

pub fn ls(args: LsArgs) -> ExitCode {
    for (id, container, status) in sandboxes(args.provider.cli()) {
        if args.json {
            println!(
                "{}",
                json!({"id": id, "container": container, "status": status})
            );
        } else {
            println!("{id:<24} {container:<32} {status}");
        }
    }
    ExitCode::SUCCESS
}

#[derive(Debug, clap::Args)]
pub struct EnterArgs {
    /// Sandbox id.
    pub id: String,
    #[arg(long, value_enum, default_value_t = ProviderArg::Docker)]
    pub provider: ProviderArg,
    /// Command (default: a shell) run with the workspace environment.
    #[arg(last = true)]
    pub command: Vec<String>,
}

pub fn enter(args: EnterArgs) -> ExitCode {
    let cli = args.provider.cli();
    let container = format!("veneer-{}", args.id);
    let Some(spec) = capture(
        cli,
        &[
            "inspect",
            "--format",
            "{{index .Config.Labels \"dev.veneer.spec\"}}",
            &container,
        ],
    ) else {
        return fail(2, &format!("no sandbox `{}`", args.id), false);
    };
    let argv = if args.command.is_empty() {
        vec!["sh".to_string()]
    } else {
        args.command.clone()
    };
    let tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let mut c = Command::new(cli);
    c.arg("exec");
    if tty {
        c.arg("-it");
    }
    c.args([&container, "veneer", "exec", &spec, "--id", &args.id, "--"])
        .args(&argv);
    match c.status() {
        Ok(s) => ExitCode::from(s.code().unwrap_or(1) as u8),
        Err(e) => fail(1, &e.to_string(), false),
    }
}

#[derive(Debug, clap::Args)]
pub struct RmArgs {
    /// Sandbox ids to remove.
    pub ids: Vec<String>,
    /// Remove every veneer sandbox.
    #[arg(long)]
    pub all: bool,
    #[arg(long, value_enum, default_value_t = ProviderArg::Docker)]
    pub provider: ProviderArg,
}

pub fn rm(args: RmArgs) -> ExitCode {
    let cli = args.provider.cli();
    let targets: Vec<String> = if args.all {
        sandboxes(cli).into_iter().map(|(_, c, _)| c).collect()
    } else {
        args.ids.iter().map(|i| format!("veneer-{i}")).collect()
    };
    for t in &targets {
        if cli_ok(cli, &["rm", "-f", t]) {
            eprintln!("removed {t}");
        } else {
            eprintln!("error: cannot remove {t}");
        }
    }
    ExitCode::SUCCESS
}
