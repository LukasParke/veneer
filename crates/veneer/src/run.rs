//! Runtime commands: apply, diff, exec, env, down, entrypoint.

use std::path::PathBuf;
use std::process::{Command, ExitCode};

use serde_json::json;
use veneer_exec::{
    Events, ExecError, Journal, Lock, Runner, build_journal_path, workspace_journal_path,
};
use veneer_plan::{Dirs, Phase, Plan, PlanOptions, Step, plan};
use veneer_resolve::{ResolveOptions, locate, resolve};
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
}

pub struct Loaded {
    pub entry: PathBuf,
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
    let entry = match locate(target.spec.as_deref(), &cwd) {
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
    let resolved = resolve(&entry, &ResolveOptions { overlays });
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
    let (Some(spec), Some(tree)) = (resolved.spec, resolved.tree) else {
        return Err(ExitCode::from(2));
    };
    Ok(Loaded {
        entry,
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

/// Runs (or diffs) the selected phases. Returns the number of applied steps.
pub fn execute(
    l: &Loaded,
    target: &Target,
    phase: PhaseArg,
    only: Option<String>,
    dry_run: bool,
    json: bool,
) -> Result<usize, ExitCode> {
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
    if !dry_run {
        events.emit(
            "run.done",
            None,
            json!({"ok": true, "exit": 0, "applied": applied, "root": plan.workspace_root}),
            &format!(
                "ready: {} ({} applied) at {}",
                plan.workspace_id,
                applied,
                plan.workspace_root.display()
            ),
        );
    }
    Ok(applied)
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
        Ok(_) => ExitCode::SUCCESS,
        Err(code) => code,
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
    let mut env: indexmap::IndexMap<String, String> =
        veneer_steps::mise::tool_env(&l.dirs).into_iter().collect();
    env.extend(plan.env.clone());
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
