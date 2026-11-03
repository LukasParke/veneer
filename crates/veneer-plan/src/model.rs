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
    },
    Compose {
        project: String,
        files: Vec<PathBuf>,
        generated: Option<String>,
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
