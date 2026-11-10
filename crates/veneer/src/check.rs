//! `veneer check`.

use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::json;
use veneer_resolve::{ResolveOptions, overlay::to_yaml, resolve};
use veneer_spec::{Severity, has_errors};

use crate::render::{display_path, render};

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Format {
    Yaml,
    Json,
}

#[derive(Debug, clap::Args)]
pub struct CheckArgs {
    /// Spec path or directory (default: nearest .veneer/veneer.yaml).
    pub spec: Option<String>,
    /// Print the fully resolved spec.
    #[arg(long)]
    pub resolved: bool,
    /// Output format for --resolved.
    #[arg(long, value_enum, default_value_t = Format::Yaml)]
    pub format: Format,
    /// Overlay file to apply (repeatable, in order).
    #[arg(long = "overlay", value_name = "FILE")]
    pub overlays: Vec<PathBuf>,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: CheckArgs) -> ExitCode {
    let cwd = std::env::current_dir()
        .and_then(|p| p.canonicalize())
        .expect("current directory is readable");
    let cache = veneer_plan::Dirs::from_env().cache;
    let entry = match veneer_resolve::locate_any(args.spec.as_deref(), &cwd, &cache, false) {
        Ok(p) => p,
        Err(diag) => {
            if args.json {
                println!(
                    "{}",
                    json!({"ok": false, "entry": null, "documents": [], "diagnostics": [diag], "resolved": null})
                );
            } else {
                eprint!("{}", render(&[diag], &cwd));
            }
            return ExitCode::from(2);
        }
    };
    let overlays = args.overlays.iter().map(|p| cwd.join(p)).collect();
    let resolved = resolve(
        &entry,
        &ResolveOptions {
            overlays,
            remote_cache: Some(veneer_plan::Dirs::from_env().cache),
            update_lock: false,
        },
    );
    let failed = has_errors(&resolved.diagnostics);
    let entry_shown = display_path(&entry, &cwd);

    if args.json {
        let tree = if args.resolved {
            resolved.tree.as_ref().map(|t| t.to_json())
        } else {
            None
        };
        println!(
            "{}",
            json!({
                "ok": !failed,
                "entry": entry_shown,
                "documents": resolved.documents,
                "diagnostics": resolved.diagnostics,
                "resolved": tree,
            })
        );
    } else {
        eprint!("{}", render(&resolved.diagnostics, &cwd));
        if !failed {
            if args.resolved {
                let tree = resolved
                    .tree
                    .as_ref()
                    .expect("a tree exists when there are no errors");
                match args.format {
                    Format::Yaml => print!("{}", to_yaml(tree)),
                    Format::Json => println!(
                        "{}",
                        serde_json::to_string_pretty(&tree.to_json()).expect("serializes")
                    ),
                }
            } else {
                let warnings = resolved
                    .diagnostics
                    .iter()
                    .filter(|d| d.severity == Severity::Warning)
                    .count();
                println!(
                    "ok {entry_shown} ({} documents, {warnings} warnings)",
                    resolved.documents.len()
                );
            }
        }
    }
    if failed {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}
