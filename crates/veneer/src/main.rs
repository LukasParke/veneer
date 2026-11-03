//! veneer: define agent workspace environments once; stage them anywhere.

mod check;
mod render;
mod run;

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
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Check(args) => check::run(args),
        Command::Apply(args) => run::apply(args),
        Command::Diff(mut args) => {
            args.dry_run = true;
            run::apply(args)
        }
        Command::Exec(args) => run::exec(args),
        Command::Env(args) => run::env(args),
        Command::Entrypoint(args) => run::entrypoint(args),
        Command::Down(args) => run::down(args),
        Command::Schema => {
            print!("{}", veneer_spec::json_schema_string());
            ExitCode::SUCCESS
        }
    }
}
