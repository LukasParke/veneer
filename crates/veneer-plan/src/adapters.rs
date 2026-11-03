//! `from:` adapters: read other tools' native files (design spec §4.4).

use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use crate::model::Manager;

fn file_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
}

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

/// Tools from `mise.toml`/`.mise.toml`, `.tool-versions`, `.nvmrc`, `.node-version`, `.python-version`.
pub fn tools_from(path: &Path) -> Result<IndexMap<String, String>, String> {
    let text = read(path)?;
    let name = file_name(path);
    let mut out = IndexMap::new();
    match name {
        ".nvmrc" | ".node-version" => {
            let v = text.trim().trim_start_matches('v');
            out.insert("node".into(), v.to_string());
        }
        ".python-version" => {
            let v = text.lines().next().unwrap_or_default().trim();
            out.insert("python".into(), v.to_string());
        }
        ".tool-versions" => {
            for line in text.lines() {
                let line = line.split('#').next().unwrap_or_default().trim();
                let mut parts = line.split_whitespace();
                if let (Some(tool), Some(version)) = (parts.next(), parts.next()) {
                    out.insert(tool.to_string(), version.to_string());
                }
            }
        }
        n if n.ends_with(".toml") => {
            let doc: toml::Table = text
                .parse()
                .map_err(|e| format!("invalid TOML in {}: {e}", path.display()))?;
            if let Some(tools) = doc.get("tools").and_then(toml::Value::as_table) {
                for (tool, value) in tools {
                    let version = match value {
                        toml::Value::String(s) => s.clone(),
                        toml::Value::Integer(i) => i.to_string(),
                        toml::Value::Float(f) => f.to_string(),
                        toml::Value::Array(a) => a
                            .first()
                            .and_then(toml::Value::as_str)
                            .unwrap_or("latest")
                            .to_string(),
                        toml::Value::Table(t) => t
                            .get("version")
                            .and_then(toml::Value::as_str)
                            .unwrap_or("latest")
                            .to_string(),
                        _ => "latest".to_string(),
                    };
                    out.insert(tool.clone(), version);
                }
            }
        }
        _ => {
            return Err(format!(
                "unsupported tools file {} (expected mise.toml, .tool-versions, .nvmrc, .node-version, .python-version)",
                path.display()
            ));
        }
    }
    Ok(out)
}

/// Package names from `Brewfile` (`brew "x"`) or a newline list (`apt.txt`, `packages.txt`).
pub fn packages_from(path: &Path) -> Result<Vec<String>, String> {
    let text = read(path)?;
    let brewfile = file_name(path) == "Brewfile";
    Ok(text
        .lines()
        .map(|l| l.split('#').next().unwrap_or_default().trim())
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            if brewfile {
                let rest = l.strip_prefix("brew ")?;
                Some(
                    rest.trim()
                        .trim_matches(|c| c == '"' || c == '\'')
                        .split(['"', '\'', ','])
                        .next()?
                        .to_string(),
                )
            } else {
                Some(l.to_string())
            }
        })
        .collect())
}

/// Detects the package manager for a dependency manifest. Returns (manager, lockfile).
pub fn detect_manager(manifest: &Path) -> Result<(Manager, Option<PathBuf>), String> {
    let dir = manifest.parent().unwrap_or(Path::new("."));
    let first = |candidates: &[(&str, Manager)]| {
        candidates.iter().find_map(|(lock, m)| {
            let p = dir.join(lock);
            p.is_file().then_some((*m, Some(p)))
        })
    };
    let detected = match file_name(manifest) {
        "package.json" => first(&[
            ("pnpm-lock.yaml", Manager::Pnpm),
            ("yarn.lock", Manager::Yarn),
            ("bun.lock", Manager::Bun),
            ("bun.lockb", Manager::Bun),
            ("package-lock.json", Manager::Npm),
        ])
        .unwrap_or((Manager::Npm, None)),
        "pyproject.toml" => first(&[("uv.lock", Manager::Uv), ("poetry.lock", Manager::Poetry)])
            .unwrap_or((Manager::Uv, None)),
        "requirements.txt" => (Manager::Pip, Some(manifest.to_path_buf())),
        "Cargo.toml" => first(&[("Cargo.lock", Manager::Cargo)]).unwrap_or((Manager::Cargo, None)),
        "go.mod" => first(&[("go.sum", Manager::Go)]).unwrap_or((Manager::Go, None)),
        "Gemfile" => {
            first(&[("Gemfile.lock", Manager::Bundler)]).unwrap_or((Manager::Bundler, None))
        }
        other => return Err(format!("unsupported dependency manifest `{other}`")),
    };
    Ok(detected)
}

/// The install command for a manager, frozen when a lockfile exists.
pub fn install_command(manager: Manager, locked: bool, dir: &Path) -> String {
    match manager {
        Manager::Npm => if locked { "npm ci" } else { "npm install" }.into(),
        Manager::Pnpm => if locked {
            "pnpm install --frozen-lockfile"
        } else {
            "pnpm install"
        }
        .into(),
        Manager::Yarn => {
            if dir.join(".yarnrc.yml").is_file() {
                if locked {
                    "yarn install --immutable"
                } else {
                    "yarn install"
                }
                .into()
            } else if locked {
                "yarn install --frozen-lockfile".into()
            } else {
                "yarn install".into()
            }
        }
        Manager::Bun => if locked {
            "bun install --frozen-lockfile"
        } else {
            "bun install"
        }
        .into(),
        Manager::Uv => if locked {
            "uv sync --frozen"
        } else {
            "uv sync"
        }
        .into(),
        Manager::Poetry => "poetry install --no-interaction".into(),
        Manager::Pip => "python3 -m venv .venv && .venv/bin/pip install -r requirements.txt".into(),
        Manager::Cargo => if locked {
            "cargo fetch --locked"
        } else {
            "cargo fetch"
        }
        .into(),
        Manager::Go => "go mod download".into(),
        Manager::Bundler => if locked {
            "BUNDLE_FROZEN=true bundle install"
        } else {
            "bundle install"
        }
        .into(),
    }
}

/// Directory whose existence means the install already ran (None = trust the journal).
pub fn install_marker(manager: Manager) -> Option<&'static str> {
    match manager {
        Manager::Npm | Manager::Pnpm | Manager::Yarn | Manager::Bun => Some("node_modules"),
        Manager::Uv | Manager::Poetry | Manager::Pip => Some(".venv"),
        Manager::Cargo | Manager::Go | Manager::Bundler => None,
    }
}
