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
    Disk,
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
}

pub fn run(args: BakeArgs) -> ExitCode {
    let l = match load(&args.target, false) {
        Ok(l) => l,
        Err(code) => return code,
    };
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
    let job = BakeJob {
        base: &base,
        plan: &plan,
        entry: &l.entry,
        documents: &l.documents,
        inputs: &inputs,
        veneer_bin: &veneer_bin,
        overlays: &overlays,
        sets: &args.target.sets,
    };
    if let Err(e) = prepare(&job, &ctx) {
        return fail(e.exit, &e.message, false);
    }
    let name = plan
        .project_root
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| "env".into());
    let tag = args
        .tag
        .clone()
        .unwrap_or_else(|| format!("veneer/{name}:latest"));
    let opts = BakeOptions {
        target: match args.kind {
            TargetArg::Oci => Target::Oci,
            TargetArg::Rootfs => Target::Rootfs,
            TargetArg::Qcow2 => Target::Qcow2,
            TargetArg::Firecracker => Target::Firecracker,
            TargetArg::Disk => Target::Disk,
        },
        tag,
        push: args.push,
        platform: args.platform.clone(),
        veneer_bin,
        output: args.output.clone(),
        context_dir: args.context.clone(),
        kernel: args.kernel.clone(),
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
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{}", log.tail(60));
            fail(e.exit, &format!("bake failed: {}", e.message), false)
        }
    }
}
