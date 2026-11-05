//! Runtime commands: apply, diff, exec, env, down, entrypoint.

use std::path::PathBuf;
use std::process::{Command, ExitCode};

use serde_json::json;
use veneer_exec::{
    Events, ExecError, Journal, Lock, Runner, build_journal_path, workspace_journal_path,
};
use veneer_plan::{Dirs, Phase, Plan, PlanOptions, Step, plan};
use veneer_resolve::{ResolveOptions, resolve};
use veneer_spec::{Spec, has_errors};

use crate::render::render;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PhaseArg {
    Build,
    Workspace,
    All,
}

#[derive(Debug, Clone, clap::Args)]
pub struct Target {
    /// Spec path, directory, or remote source (default: nearest .veneer/veneer.yaml).
    pub spec: Option<String>,
    /// Workspace id (separate root, journal, services, and ports per id).
    #[arg(long, default_value = "default", env = "VENEER_WORKSPACE_ID")]
    pub id: String,
    /// Override a template variable (repeatable): --set name=value.
    #[arg(long = "set", value_name = "NAME=VALUE", value_parser = parse_set)]
    pub sets: Vec<(String, String)>,
    /// Overlay file to apply (repeatable, in order).
    #[arg(long = "overlay", value_name = "FILE")]
    pub overlays: Vec<PathBuf>,
    /// Re-pin remote sources in veneer.lock instead of enforcing it.
    #[arg(long, hide = true)]
    pub update_lock: bool,
}

fn parse_set(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("expected NAME=VALUE, got `{s}`"))
}

#[derive(Debug, clap::Args)]
pub struct ApplyArgs {
    #[command(flatten)]
    pub target: Target,
    /// Which phase to run.
    #[arg(long, value_enum, default_value_t = PhaseArg::All)]
    pub phase: PhaseArg,
    /// Run only steps of this group or id (bake layers use this).
    #[arg(long)]
    pub only: Option<String>,
    /// Show what would run and why, without changing anything.
    #[arg(long)]
    pub dry_run: bool,
    /// NDJSON events on stdout.
    #[arg(long)]
    pub json: bool,
    /// Also write the environment manifest here (it is always kept in the state dir).
    #[arg(long, value_name = "FILE")]
    pub manifest: Option<PathBuf>,
}

pub struct Loaded {
    pub entry: PathBuf,
    /// Every document read while resolving (absolute paths).
    pub documents: Vec<PathBuf>,
    pub spec: Spec,
    pub tree: veneer_spec::Node,
    pub dirs: Dirs,
}

pub fn fail(code: u8, message: &str, json: bool) -> ExitCode {
    if json {
        println!(
            "{}",
            json!({"event": "run.done", "ok": false, "exit": code, "error": message})
        );
    } else {
        eprintln!("error: {message}");
    }
    ExitCode::from(code)
}

/// Locates, resolves, and validates the spec. Diagnostics are printed; errors exit 2.
pub fn load(target: &Target, json: bool) -> Result<Loaded, ExitCode> {
    let cwd = std::env::current_dir()
        .and_then(|p| p.canonicalize())
        .map_err(|_| ExitCode::from(1))?;
    let dirs = Dirs::from_env();
    let located = veneer_resolve::locate_any(
        target.spec.as_deref(),
        &cwd,
        &dirs.cache,
        target.update_lock,
    );
    let entry = match located {
        Ok(p) => p,
        Err(d) => {
            if json {
                println!(
                    "{}",
                    json!({"event": "run.done", "ok": false, "exit": 2, "diagnostics": [d]})
                );
            } else {
                eprint!("{}", render(&[d], &cwd));
            }
            return Err(ExitCode::from(2));
        }
    };
    let overlays = target.overlays.iter().map(|p| cwd.join(p)).collect();
    let resolved = resolve(
        &entry,
        &ResolveOptions {
            overlays,
            remote_cache: Some(dirs.cache.clone()),
            update_lock: target.update_lock,
        },
    );
    if !resolved.diagnostics.is_empty() {
        if json {
            for d in &resolved.diagnostics {
                println!("{}", json!({"event": "diagnostic", "diagnostic": d}));
            }
        } else {
            eprint!("{}", render(&resolved.diagnostics, &cwd));
        }
    }
    if has_errors(&resolved.diagnostics) {
        if json {
            println!("{}", json!({"event": "run.done", "ok": false, "exit": 2}));
        }
        return Err(ExitCode::from(2));
    }
    let documents = resolved
        .documents
        .iter()
        .filter_map(|u| suspect_source::Uri::parse(u).ok()?.as_path())
        .collect();
    let (Some(spec), Some(tree)) = (resolved.spec, resolved.tree) else {
        return Err(ExitCode::from(2));
    };
    Ok(Loaded {
        entry,
        documents,
        spec,
        tree,
        dirs,
    })
}

pub fn make_plan(
    l: &Loaded,
    target: &Target,
    build: bool,
    workspace: bool,
    only: Option<String>,
    json: bool,
) -> Result<Plan, ExitCode> {
    let opts = PlanOptions {
        workspace_id: target.id.clone(),
        build,
        workspace,
        only,
        sets: target.sets.clone(),
    };
    match plan(&l.spec, &l.tree, &l.entry, &l.dirs, &opts) {
        Ok(p) => {
            for w in &p.warnings {
                if json {
                    println!("{}", json!({"event": "warning", "message": w}));
                } else {
                    eprintln!("warning: {w}");
                }
            }
            Ok(p.plan)
        }
        Err(e) => Err(fail(e.exit, &e.message, json)),
    }
}

fn exec_error(e: ExecError, json: bool) -> ExitCode {
    if json {
        println!(
            "{}",
            json!({"event": "run.done", "ok": false, "exit": e.exit, "step": e.step, "error": e.message})
        );
    } else if e.exit == 130 {
        eprintln!("interrupted; rerun `veneer apply` to resume");
    } else if e.step.is_none() {
        eprintln!("error: {}", e.message);
    }
    ExitCode::from(e.exit)
}

fn phase_steps(plan: &Plan, phase: Phase) -> Vec<&Step> {
    plan.steps.iter().filter(|s| s.phase == phase).collect()
}

/// sha256 of the canonical resolved spec (templates unrendered, so it is workspace-independent).
pub fn spec_digest(l: &Loaded) -> String {
    format!(
        "sha256:{}",
        veneer_steps::manifest::sha256_hex(
            veneer_steps::manifest::jcs(&l.tree.to_json()).as_bytes()
        )
    )
}

pub fn workspace_manifest_path(dirs: &Dirs, id: &str) -> PathBuf {
    dirs.workspace_state(id).join("manifest.json")
}

pub fn build_manifest_path(dirs: &Dirs) -> PathBuf {
    dirs.state.join("build-manifest.json")
}

/// Observes and stores the manifest after a successful run. Workspace runs observe both
/// phases (the build half is re-planned without secrets when only the workspace ran).
fn record_manifest(
    l: &Loaded,
    target: &Target,
    plan: &Plan,
    build: bool,
    workspace: bool,
    json: bool,
) -> Option<serde_json::Value> {
    let full;
    let observed_plan = if workspace && !build {
        let mut b = make_plan(l, target, true, false, None, json).ok()?;
        b.steps.extend(plan.steps.iter().cloned());
        b.env = plan.env.clone();
        b.secret_values = plan.secret_values.clone();
        b.workspace_root = plan.workspace_root.clone();
        full = b;
        &full
    } else {
        plan
    };
    let m = veneer_steps::manifest::observe(observed_plan, &l.dirs, &spec_digest(l));
    let path = if workspace {
        workspace_manifest_path(&l.dirs, &target.id)
    } else {
        build_manifest_path(&l.dirs)
    };
    let bytes = serde_json::to_vec_pretty(&m).ok()?;
    veneer_exec::write_atomic(&path, &bytes).ok()?;
    Some(m)
}

/// Touches `$VENEER_READY_FILE` (readiness probes, cloud-init, microVM inits).
fn mark_ready() {
    if let Some(p) = std::env::var_os("VENEER_READY_FILE") {
        let p = PathBuf::from(p);
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(&p, b"ready\n");
    }
}

/// Runs (or diffs) the selected phases. Returns the number of applied steps and the manifest.
pub fn execute(
    l: &Loaded,
    target: &Target,
    phase: PhaseArg,
    only: Option<String>,
    dry_run: bool,
    json: bool,
) -> Result<(usize, Option<serde_json::Value>), ExitCode> {
    veneer_exec::install_signal_handlers();
    let build = phase != PhaseArg::Workspace;
    let workspace = phase != PhaseArg::Build;
    let dirs = &l.dirs;
    let _ws_lock = if workspace {
        Some(
            Lock::acquire(&dirs.workspace_state(&target.id).join("lock"))
                .map_err(|e| exec_error(e, json))?,
        )
    } else {
        None
    };
    let partial = only.is_some();
    let plan = make_plan(l, target, build, workspace, only, json)?;
    let events = Events::new(json, plan.secret_values.clone());
    let runner = Runner::new(&plan, dirs, &events);
    events.emit(
        "run.start",
        None,
        json!({"workspace": plan.workspace_id, "root": plan.workspace_root, "steps": plan.steps.len(), "dry_run": dry_run}),
        "",
    );
    let mut applied = 0;
    for (phase, enabled) in [(Phase::Build, build), (Phase::Workspace, workspace)] {
        if !enabled {
            continue;
        }
        let steps = phase_steps(&plan, phase);
        if steps.is_empty() {
            continue;
        }
        let (journal_path, lock_path) = match phase {
            Phase::Build => (build_journal_path(dirs), dirs.state.join("build.lock")),
            Phase::Workspace => (
                workspace_journal_path(dirs, &target.id),
                dirs.workspace_state(&target.id).join("journal.lock"),
            ),
        };
        let _lock = Lock::acquire(&lock_path).map_err(|e| exec_error(e, json))?;
        let mut journal = Journal::load(journal_path).map_err(|e| exec_error(e, json))?;
        if dry_run {
            let decisions = runner
                .diff(&steps, &mut journal, &veneer_steps::StepImpls)
                .map_err(|e| exec_error(e, json))?;
            for d in decisions {
                if json {
                    println!(
                        "{}",
                        json!({"event": "step.plan", "step": d.step, "phase": d.phase, "run": d.run, "reason": d.reason})
                    );
                } else {
                    let mark = if d.run { "+" } else { "=" };
                    println!("{mark} {:<32} {}", d.step, d.reason);
                }
            }
            continue;
        }
        applied += runner
            .run(&steps, &mut journal, &veneer_steps::StepImpls)
            .map_err(|e| exec_error(e, json))?;
        if phase == Phase::Workspace {
            for p in veneer_steps::prune_files(&plan, &mut journal.data.files) {
                events.emit(
                    "file.prune",
                    None,
                    json!({"path": p}),
                    &format!("- removed {}", p.display()),
                );
            }
            journal.save().map_err(|e| exec_error(e, json))?;
        }
    }
    let mut manifest = None;
    if !dry_run {
        if !partial {
            manifest = record_manifest(l, target, &plan, build, workspace, json);
        }
        if workspace {
            mark_ready();
        }
        let fp = manifest.as_ref().map(|m| m["fingerprint"].clone());
        events.emit(
            "run.done",
            None,
            json!({"ok": true, "exit": 0, "applied": applied, "root": plan.workspace_root, "fingerprint": fp}),
            &format!(
                "ready: {} ({} applied) at {}",
                plan.workspace_id,
                applied,
                plan.workspace_root.display()
            ),
        );
    }
    Ok((applied, manifest))
}

pub fn apply(args: ApplyArgs) -> ExitCode {
    let l = match load(&args.target, args.json) {
        Ok(l) => l,
        Err(code) => return code,
    };
    match execute(
        &l,
        &args.target,
        args.phase,
        args.only.clone(),
        args.dry_run,
        args.json,
    ) {
        Ok((_, manifest)) => {
            if let (Some(path), Some(m)) = (&args.manifest, manifest)
                && let Err(e) = std::fs::write(path, serde_json::to_vec_pretty(&m).unwrap())
            {
                return fail(
                    1,
                    &format!("cannot write {}: {e}", path.display()),
                    args.json,
                );
            }
            ExitCode::SUCCESS
        }
        Err(code) => code,
    }
}

// ---------------------------------------------------------------- verify

#[derive(Debug, clap::Args)]
pub struct VerifyArgs {
    #[command(flatten)]
    pub target: Target,
    /// Expected fingerprint (`sha256:…`); exit 1 if the workspace differs.
    #[arg(long)]
    pub expect: Option<String>,
    /// Compare against this manifest instead of the one recorded by the last apply.
    #[arg(long, value_name = "FILE")]
    pub against: Option<PathBuf>,
    /// Print the observed manifest as JSON on stdout.
    #[arg(long)]
    pub json: bool,
}

pub fn verify(args: VerifyArgs) -> ExitCode {
    let l = match load(&args.target, false) {
        Ok(l) => l,
        Err(code) => return code,
    };
    let plan = match make_plan(&l, &args.target, true, true, None, false) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let now = veneer_steps::manifest::observe(&plan, &l.dirs, &spec_digest(&l));
    let fp = now["fingerprint"].as_str().unwrap_or_default().to_string();
    if args.json {
        println!("{}", serde_json::to_string_pretty(&now).unwrap());
    }
    let reference_path = args
        .against
        .clone()
        .unwrap_or_else(|| workspace_manifest_path(&l.dirs, &args.target.id));
    let reference: Option<serde_json::Value> = std::fs::read(&reference_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let mut ok = true;
    if let Some(expect) = &args.expect {
        if expect != &fp {
            eprintln!("✗ fingerprint {fp} does not match the expected {expect}");
            ok = false;
        }
    } else if reference.is_none() {
        return fail(
            2,
            &format!(
                "no manifest at {} (run `veneer apply` first, or pass --expect/--against)",
                reference_path.display()
            ),
            false,
        );
    }
    if let Some(r) = &reference {
        let diffs = veneer_steps::manifest::diff(&r["stable"], &now["stable"]);
        if !diffs.is_empty() {
            ok = false;
            eprintln!("✗ the workspace differs from {}:", reference_path.display());
            for d in diffs.iter().take(50) {
                eprintln!("  {d}");
            }
            if diffs.len() > 50 {
                eprintln!("  … {} more", diffs.len() - 50);
            }
        }
    }
    if ok {
        eprintln!("✓ {fp}");
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

// ---------------------------------------------------------------- exec / env / entrypoint

#[derive(Debug, clap::Args)]
pub struct ExecArgs {
    #[command(flatten)]
    pub target: Target,
    /// Command to run with the workspace environment.
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

#[cfg(unix)]
fn exec_into(cmd: &mut Command) -> ExitCode {
    use std::os::unix::process::CommandExt;
    let err = cmd.exec();
    eprintln!("error: cannot run {:?}: {err}", cmd.get_program());
    ExitCode::from(127)
}

fn workspace_command(plan: &Plan, dirs: &Dirs, argv: &[String]) -> Command {
    let env = veneer_steps::command_env(plan, dirs, true);
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).env_clear().envs(&env);
    // Resolve the program against the workspace PATH, not veneer's own.
    if !argv[0].contains('/')
        && let Some(path) = env.get("PATH")
        && let Some(found) = std::env::split_paths(path)
            .map(|d| d.join(&argv[0]))
            .find(|p| p.is_file())
    {
        let mut c = Command::new(found);
        c.args(&argv[1..]).env_clear().envs(&env);
        return c;
    }
    cmd
}

pub fn exec(args: ExecArgs) -> ExitCode {
    let l = match load(&args.target, false) {
        Ok(l) => l,
        Err(code) => return code,
    };
    let plan = match make_plan(&l, &args.target, false, true, None, false) {
        Ok(p) => p,
        Err(code) => return code,
    };
    exec_into(&mut workspace_command(&plan, &l.dirs, &args.command))
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum EnvFormat {
    Sh,
    Json,
    Dotenv,
}

#[derive(Debug, clap::Args)]
pub struct EnvArgs {
    #[command(flatten)]
    pub target: Target,
    #[arg(long, value_enum, default_value_t = EnvFormat::Sh)]
    pub format: EnvFormat,
    /// Print secret values instead of masking them.
    #[arg(long)]
    pub reveal: bool,
}

pub fn env(args: EnvArgs) -> ExitCode {
    let l = match load(&args.target, false) {
        Ok(l) => l,
        Err(code) => return code,
    };
    let plan = match make_plan(&l, &args.target, false, true, None, false) {
        Ok(p) => p,
        Err(code) => return code,
    };
    // Everything veneer adds on top of the host environment: tools, features, workspace env.
    let host: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    let mut env: indexmap::IndexMap<String, String> =
        veneer_steps::command_env(&plan, &l.dirs, true)
            .into_iter()
            .filter(|(k, v)| host.get(k) != Some(v))
            .collect();
    env.sort_keys();
    let events = Events::new(
        false,
        if args.reveal {
            vec![]
        } else {
            plan.secret_values.clone()
        },
    );
    let text = match args.format {
        EnvFormat::Sh => veneer_steps::export_lines(&env),
        EnvFormat::Dotenv => env.iter().map(|(k, v)| format!("{k}={v}\n")).collect(),
        EnvFormat::Json => format!("{}\n", serde_json::to_string_pretty(&env).unwrap()),
    };
    print!("{}", events.redact(&text));
    ExitCode::SUCCESS
}

#[derive(Debug, clap::Args)]
pub struct EntrypointArgs {
    #[command(flatten)]
    pub target: Target,
    /// Command to exec once the workspace is ready (default: $SHELL or sh).
    #[arg(last = true)]
    pub command: Vec<String>,
}

pub fn entrypoint(args: EntrypointArgs) -> ExitCode {
    let l = match load(&args.target, false) {
        Ok(l) => l,
        Err(code) => return code,
    };
    if let Err(code) = execute(&l, &args.target, PhaseArg::Workspace, None, false, false) {
        return code;
    }
    let plan = match make_plan(&l, &args.target, false, true, None, false) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let argv = if args.command.is_empty() {
        vec![std::env::var("SHELL").unwrap_or_else(|_| "sh".into())]
    } else {
        args.command.clone()
    };
    let mut cmd = workspace_command(&plan, &l.dirs, &argv);
    if plan.workspace_root.is_dir() {
        cmd.current_dir(&plan.workspace_root);
    }
    exec_into(&mut cmd)
}

#[derive(Debug, clap::Args)]
pub struct DownArgs {
    #[command(flatten)]
    pub target: Target,
    /// Also delete the workspace root and its state.
    #[arg(long)]
    pub rm: bool,
}

pub fn down(args: DownArgs) -> ExitCode {
    let l = match load(&args.target, false) {
        Ok(l) => l,
        Err(code) => return code,
    };
    let _lock = match Lock::acquire(&l.dirs.workspace_state(&args.target.id).join("lock")) {
        Ok(lock) => lock,
        Err(e) => return exec_error(e, false),
    };
    let plan = match make_plan(&l, &args.target, false, true, None, false) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let mut log =
        match veneer_exec::Log::create(l.dirs.workspace_state(&args.target.id).join("down.log")) {
            Ok(log) => log,
            Err(e) => return exec_error(e, false),
        };
    if let Err(e) = veneer_steps::down(&plan, &l.dirs, &mut log) {
        return exec_error(e, false);
    }
    if args.rm {
        let _ = std::fs::remove_dir_all(&plan.workspace_root);
        let _ = std::fs::remove_file(workspace_journal_path(&l.dirs, &args.target.id));
        let _ = std::fs::remove_file(l.dirs.workspace_state(&args.target.id).join("ports.json"));
        eprintln!(
            "removed workspace {} ({})",
            plan.workspace_id,
            plan.workspace_root.display()
        );
    } else {
        eprintln!("stopped services for {}", plan.workspace_id);
    }
    ExitCode::SUCCESS
}

#[derive(Debug, clap::Args)]
pub struct LockArgs {
    #[command(flatten)]
    pub target: Target,
    /// Re-pin every remote source to what its ref points at now.
    #[arg(long)]
    pub update: bool,
}

pub fn lock(mut args: LockArgs) -> ExitCode {
    args.target.update_lock = args.update;
    match load(&args.target, false) {
        Ok(l) => {
            let lock = l.entry.with_file_name("veneer.lock");
            if lock.is_file() {
                eprintln!("✓ {}", lock.display());
            } else {
                eprintln!("no remote sources; nothing to lock");
            }
            ExitCode::SUCCESS
        }
        Err(code) => code,
    }
}
