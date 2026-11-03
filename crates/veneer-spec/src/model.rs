//! The typed veneer spec (version 1).

use indexmap::IndexMap;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{codes, escape_pointer_token};

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// Spec format version. Must be `1`.
    pub version: u32,
    /// Base image used by `veneer bake`.
    #[serde(default)]
    pub base: Option<String>,
    /// Specs to layer underneath this one (consumed during resolution).
    #[serde(default)]
    pub extends: Vec<String>,
    /// Reusable spec fragments.
    #[serde(default, rename = "use")]
    pub modules: Vec<ModuleUse>,
    /// Template variables.
    #[serde(default)]
    pub vars: IndexMap<String, serde_json::Value>,
    /// Secrets, resolved at workspace time only.
    #[serde(default)]
    pub secrets: IndexMap<String, Secret>,
    /// Environment variables.
    #[serde(default)]
    pub env: IndexMap<String, Scalar>,
    /// OS packages.
    #[serde(default)]
    pub packages: Vec<Package>,
    /// Tools installed through mise. The key `from` names a mise.toml/.tool-versions file.
    #[serde(default)]
    pub tools: IndexMap<String, ToolVersion>,
    /// Project dependency installs, detected from lockfiles.
    #[serde(default)]
    pub deps: Vec<Dep>,
    /// Container images pre-pulled at build time.
    #[serde(default)]
    pub images: Vec<String>,
    /// Workspace folder layout.
    #[serde(default)]
    pub workspace: Option<Workspace>,
    /// Services. The key `from` names a compose file.
    #[serde(default)]
    pub services: IndexMap<String, ServiceEntry>,
    /// Lifecycle hooks.
    #[serde(default)]
    pub hooks: Hooks,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ModuleUse {
    Ref(String),
    Full(ModuleRef),
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModuleRef {
    pub module: String,
    #[serde(default)]
    pub with: IndexMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Secret {
    pub from: SecretSource,
    /// Path for `from: file`.
    #[serde(default)]
    pub path: Option<String>,
    /// Command for `from: command`.
    #[serde(default)]
    pub run: Option<String>,
    /// Host variable name for `from: env` (defaults to the secret's name).
    #[serde(default)]
    pub var: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SecretSource {
    Env,
    File,
    Command,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Scalar {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Package {
    Name(String),
    Import(FromRef),
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FromRef {
    pub from: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ToolVersion {
    Str(String),
    Int(i64),
    Float(f64),
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Dep {
    pub from: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default)]
    pub repos: Vec<Repo>,
    #[serde(default)]
    pub files: Vec<FileSpec>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Repo {
    pub url: String,
    #[serde(default, rename = "ref")]
    pub git_ref: Option<String>,
    pub path: String,
    #[serde(default)]
    pub mode: RepoMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RepoMode {
    #[default]
    Clone,
    Worktree,
    Copy,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileSpec {
    pub src: String,
    pub dest: String,
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ServiceEntry {
    Def(Box<Service>),
    Import(String),
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Service {
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub env: IndexMap<String, Scalar>,
    #[serde(default)]
    pub ports: Vec<String>,
    #[serde(default)]
    pub healthcheck: Option<Healthcheck>,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Healthcheck {
    #[serde(default)]
    pub cmd: Option<String>,
    #[serde(default)]
    pub http: Option<String>,
    #[serde(default)]
    pub interval: Option<String>,
    #[serde(default)]
    pub retries: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    #[serde(default)]
    pub build: Vec<Hook>,
    #[serde(default)]
    pub workspace: Vec<Hook>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Hook {
    Cmd(String),
    Full(HookSpec),
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HookSpec {
    pub run: String,
    #[serde(default)]
    pub creates: Option<String>,
    #[serde(default)]
    pub unless: Option<String>,
}

/// A semantic rule violation found after successful deserialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub code: &'static str,
    pub pointer: String,
    pub message: String,
}

impl Spec {
    /// Rules the type system cannot express. Order: version, secrets, services (declaration order).
    pub fn validate(&self) -> Vec<Violation> {
        let mut out = Vec::new();
        if self.version != 1 {
            out.push(Violation {
                code: codes::VERSION,
                pointer: "/version".into(),
                message: format!(
                    "unsupported spec version {}; this veneer supports version 1",
                    self.version
                ),
            });
        }
        for (name, secret) in &self.secrets {
            let missing = match secret.from {
                SecretSource::File if secret.path.is_none() => Some("path"),
                SecretSource::Command if secret.run.is_none() => Some("run"),
                _ => None,
            };
            if let Some(field) = missing {
                out.push(Violation {
                    code: codes::SECRET_SOURCE,
                    pointer: format!("/secrets/{}", escape_pointer_token(name)),
                    message: format!("secret `{name}` needs `{field}` for this `from` source"),
                });
            }
        }
        for (name, entry) in &self.services {
            if let ServiceEntry::Def(svc) = entry
                && svc.image.is_some() == svc.run.is_some()
            {
                out.push(Violation {
                    code: codes::SERVICE_KIND,
                    pointer: format!("/services/{}", escape_pointer_token(name)),
                    message: format!("service `{name}` must set exactly one of `image` or `run`"),
                });
            }
        }
        out
    }
}
