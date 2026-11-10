//! `veneer bake`.

use std::path::PathBuf;
use std::process::ExitCode;

use veneer_bake::{BakeJob, BakeOptions, Target, build, prepare};
use veneer_plan::bake_inputs;

use crate::run::{Target as SpecTarget, fail, load, make_plan};

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum TargetArg {
    Oci,
    Rootfs,
    Qcow2,
    Firecracker,
    /// Kernel + ext4 rootfs + launchers for Firecracker, Cloud Hypervisor, and QEMU.
    Microvm,
    Disk,
    /// `#cloud-config` user-data that stages the workspace on a stock cloud image.
    CloudInit,
}

#[derive(Debug, clap::Args)]
pub struct BakeArgs {
    #[command(flatten)]
    pub target: SpecTarget,
    #[arg(long = "target", value_enum, default_value_t = TargetArg::Oci)]
    pub kind: TargetArg,
    /// Image tag (default: veneer/<project>:latest).
    #[arg(long)]
    pub tag: Option<String>,
    /// Push instead of loading into the local image store.
    #[arg(long)]
    pub push: bool,
    /// Target platform, e.g. linux/amd64.
    #[arg(long)]
    pub platform: Option<String>,
    /// Linux veneer binary to put in the image (default: this binary, on Linux).
    #[arg(long, env = "VENEER_LINUX_BIN")]
    pub veneer_bin: Option<PathBuf>,
    /// Output file for --target rootfs|qcow2, or directory for firecracker.
    #[arg(long, short)]
    pub output: Option<PathBuf>,
    /// Keep the generated build context in this directory.
    #[arg(long)]
    pub context: Option<PathBuf>,
    /// Guest kernel (uncompressed vmlinux) for --target firecracker.
    #[arg(long)]
    pub kernel: Option<PathBuf>,
    /// --target disk -o x.vhd: fixed-size, 1 MiB-aligned VHD (Hyper-V/Azure).
    #[arg(long)]
    pub vhd_fixed: bool,
    /// --target cloud-init: also write a NoCloud seed ISO (label `cidata`).
    #[arg(long, value_name = "ISO")]
    pub seed: Option<PathBuf>,
    /// --target cloud-init: fetch the project from this remote source at boot instead of
    /// embedding it (github:, git+…, https:, oci://).
    #[arg(long, value_name = "SOURCE")]
    pub project: Option<String>,
    /// --target cloud-init: embed the Linux veneer binary instead of downloading a release.
    #[arg(long)]
    pub embed_veneer: bool,
    /// --target cloud-init: release base URL holding veneer-<arch>-unknown-linux-musl.tar.xz{,.sha256}.
    #[arg(long, value_name = "URL")]
    pub veneer_url: Option<String>,
    /// --target cloud-init: the image already has the build phase (from --target disk).
    #[arg(long)]
    pub prebaked: bool,
    #[command(flatten)]
    pub supply: crate::supply::SupplyArgs,
}

pub fn run(args: BakeArgs) -> ExitCode {
    let l = match load(&args.target, false) {
        Ok(l) => l,
        Err(code) => return code,
    };
    if matches!(args.kind, TargetArg::CloudInit) {
        return cloud_init(&args, &l);
    }
    let Some(base) = l.spec.base.clone() else {
        return fail(
            2,
            "bake needs `base:` (e.g. `base: ubuntu:24.04`) in the spec",
            false,
        );
    };
    let veneer_bin = match args.veneer_bin.clone() {
        Some(p) => p,
        None if cfg!(target_os = "linux") => std::env::current_exe().expect("current exe"),
        None => {
            return fail(
                2,
                "this host is not Linux: pass --veneer-bin <linux veneer binary> (or set VENEER_LINUX_BIN)",
                false,
            );
        }
    };
    let plan = match make_plan(&l, &args.target, true, false, None, false) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let inputs = match bake_inputs(&l.spec, &l.tree, &l.entry) {
        Ok(i) => i,
        Err(e) => return fail(e.exit, &e.message, false),
    };
    let tmp = tempfile::tempdir().expect("temp dir");
    let ctx = args
        .context
        .clone()
        .unwrap_or_else(|| tmp.path().to_path_buf());
    let cwd = std::env::current_dir().expect("cwd");
    let overlays: Vec<PathBuf> = args
        .target
        .overlays
        .iter()
        .map(|o| cwd.join(o).canonicalize().unwrap_or_else(|_| cwd.join(o)))
        .collect();
    let name = plan
        .project_root
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| "env".into());
    let tag = args
        .tag
        .clone()
        .unwrap_or_else(|| format!("veneer/{name}:latest"));
    let labels = crate::supply::labels(&l, &plan.project_root, &tag);
    let job = BakeJob {
        base: &base,
        plan: &plan,
        entry: &l.entry,
        documents: &l.documents,
        inputs: &inputs,
        veneer_bin: &veneer_bin,
        overlays: &overlays,
        sets: &args.target.sets,
        labels: &labels,
    };
    if let Err(e) = prepare(&job, &ctx) {
        return fail(e.exit, &e.message, false);
    }
    let opts = BakeOptions {
        target: match args.kind {
            TargetArg::Oci => Target::Oci,
            TargetArg::Rootfs => Target::Rootfs,
            TargetArg::Qcow2 => Target::Qcow2,
            TargetArg::Firecracker | TargetArg::Microvm => Target::Firecracker,
            TargetArg::Disk => Target::Disk,
            TargetArg::CloudInit => unreachable!("handled above"),
        },
        tag: tag.clone(),
        // OCI images are pushed after labelling and fingerprinting (see supply::finish).
        push: args.push && !(matches!(args.kind, TargetArg::Oci) && docker_available()),
        platform: args.platform.clone(),
        veneer_bin,
        output: args.output.clone(),
        context_dir: args.context.clone(),
        kernel: args.kernel.clone(),
        vhd_fixed: args.vhd_fixed,
        spec_in_image: format!(
            "{}/{}",
            veneer_bake::PROJECT_DIR,
            l.entry
                .strip_prefix(&plan.project_root)
                .unwrap_or(&l.entry)
                .display()
        ),
    };
    let log_path = l
        .dirs
        .state
        .join("logs")
        .join(format!("bake-{}.log", std::process::id()));
    let mut log = match veneer_exec::Log::create(log_path.clone()) {
        Ok(log) => log,
        Err(e) => return fail(e.exit, &e.message, false),
    };
    eprintln!(
        "▸ baking {} from {base} (log: {})",
        opts.tag,
        log_path.display()
    );
    let started = crate::supply::build_time().min(veneer_steps::sbom::now_secs());
    match build(&ctx, &opts, &mut log) {
        Ok(baked) => {
            eprintln!(
                "✓ {}{}",
                baked.image,
                baked.id.map(|i| format!(" ({i})")).unwrap_or_default()
            );
            if let Some(r) = baked.rootfs {
                eprintln!("✓ {}", r.display());
            }
            if matches!(args.kind, TargetArg::Oci) && docker_available() {
                let overlays: Vec<String> = args
                    .target
                    .overlays
                    .iter()
                    .map(|o| o.display().to_string())
                    .collect();
                match crate::supply::finish(
                    &args.supply,
                    &l,
                    &tag,
                    &base,
                    args.push,
                    started,
                    &overlays,
                    &mut log,
                ) {
                    Ok(out) => {
                        if let Some(d) = &out.digest {
                            eprintln!("✓ pushed {d}");
                            println!("{d}");
                        }
                        for f in &out.files {
                            eprintln!("✓ {}", f.display());
                        }
                    }
                    Err(e) => {
                        eprintln!("{}", log.tail(40));
                        return fail(e.exit, &format!("bake failed: {}", e.message), false);
                    }
                }
            } else if args.supply.wanted() {
                eprintln!("warning: --sbom/--provenance/--sign apply to --target oci with Docker");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{}", log.tail(60));
            fail(e.exit, &format!("bake failed: {}", e.message), false)
        }
    }
}

fn cloud_init(args: &BakeArgs, l: &crate::run::Loaded) -> ExitCode {
    use veneer_bake::cloud_init::{CloudInit, Install, Project, USER_DATA_LIMIT, release_sums};
    for (name, secret) in &l.spec.secrets {
        if secret.from == veneer_spec::SecretSource::Env {
            eprintln!(
                "warning: W3101: secret `{name}` reads an environment variable, which a cloud-init boot does not have; use `from: file` or `from: command` (instance metadata, a secret manager)"
            );
        }
    }
    let project_root = veneer_plan::project_root(&l.entry);
    let cwd = std::env::current_dir().expect("cwd");
    let mut extra_args = Vec::new();
    for o in &args.target.overlays {
        let abs = cwd.join(o).canonicalize().unwrap_or_else(|_| cwd.join(o));
        match (abs.strip_prefix(&project_root), &args.project) {
            (Ok(rel), None) => {
                extra_args.push("--overlay".into());
                extra_args.push(format!("{}/{}", veneer_bake::PROJECT_DIR, rel.display()));
            }
            _ => {
                return fail(
                    2,
                    &format!(
                        "overlay {} must be inside the embedded project",
                        abs.display()
                    ),
                    false,
                );
            }
        }
    }
    for (k, v) in &args.target.sets {
        extra_args.push("--set".into());
        extra_args.push(format!("{k}={v}"));
    }
    let project = match &args.project {
        Some(src) => Project::Remote(src.clone()),
        None => Project::Embed {
            root: project_root.clone(),
            entry: l
                .entry
                .strip_prefix(&project_root)
                .unwrap_or(&l.entry)
                .display()
                .to_string(),
        },
    };
    let install = if args.embed_veneer {
        let bin = match args.veneer_bin.clone() {
            Some(p) => p,
            None if cfg!(target_os = "linux") => std::env::current_exe().expect("current exe"),
            None => {
                return fail(
                    2,
                    "--embed-veneer on a non-Linux host needs --veneer-bin <linux veneer binary>",
                    false,
                );
            }
        };
        Install::Embed(bin)
    } else {
        let base = args.veneer_url.clone().unwrap_or_else(|| {
            format!(
                "https://github.com/LukasParke/veneer/releases/download/v{}",
                env!("CARGO_PKG_VERSION")
            )
        });
        match release_sums(&base) {
            Ok(sums) => Install::Release {
                base_url: base,
                sums,
            },
            Err(e) => {
                return fail(
                    3,
                    &format!(
                        "cannot pin the veneer release: {}; pass --embed-veneer or --veneer-url",
                        e.message
                    ),
                    false,
                );
            }
        }
    };
    let ci = CloudInit {
        install,
        project,
        workspace_id: args.target.id.clone(),
        prebaked: args.prebaked,
        extra_args,
    };
    let ud = match veneer_bake::cloud_init::user_data(&ci) {
        Ok(u) => u,
        Err(e) => return fail(e.exit, &e.message, false),
    };
    if ud.len() > USER_DATA_LIMIT {
        eprintln!(
            "warning: W3102: user-data is {} KiB; EC2 caps user-data at 16 KiB (use --project <remote source>, and a release instead of --embed-veneer)",
            ud.len().div_ceil(1024)
        );
    }
    let out = args
        .output
        .clone()
        .unwrap_or_else(|| PathBuf::from("user-data"));
    if let Err(e) = std::fs::write(&out, &ud) {
        return fail(1, &format!("cannot write {}: {e}", out.display()), false);
    }
    eprintln!("✓ {} ({} KiB)", out.display(), ud.len().div_ceil(1024));
    if let Some(iso) = &args.seed {
        let log_path = l
            .dirs
            .state
            .join("logs")
            .join(format!("seed-{}.log", std::process::id()));
        let mut log = match veneer_exec::Log::create(log_path) {
            Ok(log) => log,
            Err(e) => return fail(e.exit, &e.message, false),
        };
        if let Err(e) = veneer_bake::cloud_init::seed_iso(&ud, iso, &mut log) {
            eprintln!("{}", log.tail(30));
            return fail(e.exit, &format!("seed ISO failed: {}", e.message), false);
        }
        eprintln!("✓ {} (NoCloud seed, label cidata)", iso.display());
    }
    ExitCode::SUCCESS
}

fn docker_available() -> bool {
    std::process::Command::new("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}
