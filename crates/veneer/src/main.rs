//! veneer: define agent workspace environments once; stage them anywhere.

mod bake;
mod check;
mod feature;
mod init;
mod lsp;
mod render;
mod run;
mod sandbox;
mod vm;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "veneer",
    version,
    about = "Define agent workspace environments once; stage them anywhere."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Validate a spec and report diagnostics.
    Check(check::CheckArgs),
    /// Print the JSON Schema for veneer.yaml.
    Schema,
    /// Stage the environment: run the build and/or workspace phase.
    Apply(run::ApplyArgs),
    /// Re-observe a workspace and compare it with its manifest or an expected fingerprint.
    Verify(run::VerifyArgs),
    /// Show which steps would run and why.
    Diff(run::ApplyArgs),
    /// Run a command with the workspace environment and tools on PATH.
    Exec(run::ExecArgs),
    /// Print the resolved workspace environment.
    Env(run::EnvArgs),
    /// Container entrypoint: run the workspace phase, then exec the command.
    Entrypoint(run::EntrypointArgs),
    /// Stop workspace services (and optionally remove the workspace).
    Down(run::DownArgs),
    /// Bake the build phase into an OCI image or a microVM rootfs.
    Bake(bake::BakeArgs),
    /// Pin remote sources in .veneer/veneer.lock.
    Lock(run::LockArgs),
    /// Scaffold .veneer/veneer.yaml from existing project files.
    Init(init::InitArgs),
    /// Create sandboxes (containers), stage the spec in each, and print their handles.
    Up(sandbox::UpArgs),
    /// List veneer sandboxes.
    Ls(sandbox::LsArgs),
    /// Run a command (default: a shell) inside a sandbox with its workspace environment.
    Enter(sandbox::EnterArgs),
    /// Remove sandboxes.
    Rm(sandbox::RmArgs),
    /// Language server (stdio): diagnostics, hover, completion for veneer specs.
    Lsp,
    /// Boot Firecracker bundles and take/restore memory snapshots.
    #[command(subcommand)]
    Vm(vm::VmCmd),
    /// Internal helpers used by generated steps.
    #[command(hide = true, subcommand)]
    Internal(feature::Internal),
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Check(args) => check::run(args),
        Command::Apply(args) => run::apply(args),
        Command::Diff(mut args) => {
            args.dry_run = true;
            run::apply(args)
        }
        Command::Verify(args) => run::verify(args),
        Command::Exec(args) => run::exec(args),
        Command::Env(args) => run::env(args),
        Command::Entrypoint(args) => run::entrypoint(args),
        Command::Down(args) => run::down(args),
        Command::Bake(args) => bake::run(args),
        Command::Lock(args) => run::lock(args),
        Command::Init(args) => init::run(args),
        Command::Internal(cmd) => feature::run(cmd),
        Command::Up(args) => sandbox::up(args),
        Command::Lsp => lsp::run(),
        Command::Vm(cmd) => vm::run(cmd),
        Command::Ls(args) => sandbox::ls(args),
        Command::Enter(args) => sandbox::enter(args),
        Command::Rm(args) => sandbox::rm(args),
        Command::Schema => {
            print!("{}", veneer_spec::json_schema_string());
            ExitCode::SUCCESS
        }
    }
}
