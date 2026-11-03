//! veneer: define agent workspace environments once; stage them anywhere.

mod check;
mod render;

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
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Check(args) => check::run(args),
        Command::Schema => {
            print!("{}", veneer_spec::json_schema_string());
            ExitCode::SUCCESS
        }
    }
}
