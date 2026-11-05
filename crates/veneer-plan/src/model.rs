//! Plan data model.

use std::path::PathBuf;

use indexmap::IndexMap;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Build,
    Workspace,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Build => "build",
            Phase::Workspace => "workspace",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Manager {
    Npm,
    Pnpm,
    Yarn,
    Bun,
    Uv,
    Poetry,
    Pip,
    Cargo,
    Go,
    Bundler,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RepoMode {
    Clone,
    Worktree,
    Copy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigFormat {
    Json,
    Toml,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Healthcheck {
    pub cmd: Option<String>,
    pub http: Option<String>,
    pub interval_ms: u64,
    pub retries: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepKind {
    Packages {
        names: Vec<String>,
    },
    Tools {
        tools: IndexMap<String, String>,
        /// mise tool options by tool (e.g. `allow_builds`).
        #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
        options: IndexMap<String, serde_json::Value>,
    },
    /// Flake references installed into veneer's own Nix profile.
    Nix {
        refs: Vec<String>,
    },
    Deps {
        dir: PathBuf,
        manifest: PathBuf,
        lockfile: Option<PathBuf>,
        manager: Manager,
        digest: String,
    },
    Images {
        refs: Vec<String>,
        /// Locked manifest digests by ref (`veneer.lock`).
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        digests: std::collections::BTreeMap<String, String>,
    },
    Hook {
        run: String,
        cwd: PathBuf,
        creates: Option<PathBuf>,
        unless: Option<String>,
    },
    Env {
        file: PathBuf,
    },
    Repo {
        url: String,
        git_ref: Option<String>,
        dest: PathBuf,
        mode: RepoMode,
    },
    File {
        dest: PathBuf,
        content: String,
        mode: Option<u32>,
    },
    ImagesLoad {
        refs: Vec<String>,
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        digests: std::collections::BTreeMap<String, String>,
    },
    Compose {
        project: String,
        files: Vec<PathBuf>,
        generated: Option<String>,
    },
    /// Sets one key of a JSON or TOML config file (other keys are kept).
    ConfigMerge {
        dest: PathBuf,
        format: ConfigFormat,
        path: Vec<String>,
        value: serde_json::Value,
        mode: Option<u32>,
    },
    /// `link → target` (relative), only when `target` exists next to it and `link` does not.
    /// Inside a git checkout the link is added to `.git/info/exclude`.
    Symlink {
        link: PathBuf,
        target: String,
    },
    Native {
        name: String,
        run: String,
        cwd: PathBuf,
        port: u16,
        env: IndexMap<String, String>,
        healthcheck: Option<Healthcheck>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Step {
    /// Stable identity within the plan (e.g. `deps:app/package.json`).
    pub id: String,
    /// Bake layer group (`packages`, `tools`, `deps:…`, `images`, `hooks`) or `workspace`.
    pub group: String,
    pub phase: Phase,
    #[serde(flatten)]
    pub kind: StepKind,
    /// blake3 of the step's inputs (hex).
    pub hash: String,
    /// Retries after a failed apply (see `retry_any`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub retries: u32,
    /// Retry on any failure (hooks with `retries:`), not only transient network errors.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retry_any: bool,
    /// Kill the step's commands after this many milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Versions and digests pinned in `veneer.lock` (standards proposal §5.4.1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Pins {
    /// `<tool>@<requested>` → exact version.
    #[serde(default)]
    pub tools: std::collections::BTreeMap<String, String>,
    /// image ref → manifest digest (`sha256:…`).
    #[serde(default)]
    pub images: std::collections::BTreeMap<String, String>,
    /// `<url>#<ref or HEAD>` → commit.
    #[serde(default)]
    pub repos: std::collections::BTreeMap<String, String>,
}

impl Pins {
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.images.is_empty() && self.repos.is_empty()
    }
}

pub fn tool_key(name: &str, requested: &str) -> String {
    format!("{name}@{requested}")
}

pub fn repo_key(url: &str, git_ref: Option<&str>) -> String {
    format!("{url}#{}", git_ref.unwrap_or("HEAD"))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Plan {
    pub workspace_id: String,
    pub workspace_root: PathBuf,
    pub project_root: PathBuf,
    /// Final environment for the workspace (rendered `env`, plus veneer variables).
    pub env: IndexMap<String, String>,
    /// Secret values, kept only for redaction. Never serialized.
    #[serde(skip)]
    pub secret_values: Vec<String>,
    pub steps: Vec<Step>,
}
