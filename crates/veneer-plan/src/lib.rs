//! veneer-plan: compiles a resolved spec into an ordered list of idempotent steps.
//!
//! Order (a linear topological order of the step DAG):
//! `packages → tools → deps:* → images → hooks.build → env → repos → files → images.load →
//! services → hooks.workspace`.

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use minijinja::Value;
use veneer_spec::{Hook, Node, Package, Scalar, SecretSource, ServiceEntry, Spec, ToolVersion};

pub mod adapters;
mod dirs;
pub mod harness;
mod model;
mod render;

pub use dirs::{Dirs, is_root};
pub use model::*;
pub use render::{Renderer, is_template};

pub const HARNESS_NAMES: &[&str] = veneer_spec::HARNESSES;

/// Exit status class for a planning failure (spec §5.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanError {
    /// `2` invalid spec, `4` missing secret.
    pub exit: u8,
    pub message: String,
}

impl PlanError {
    fn spec(message: impl Into<String>) -> Self {
        Self {
            exit: 2,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Debug, Clone)]
pub struct PlanOptions {
    pub workspace_id: String,
    pub build: bool,
    pub workspace: bool,
    /// Keep only steps whose group or id equals this.
    pub only: Option<String>,
    /// `--set k=v` overrides for `vars`.
    pub sets: Vec<(String, String)>,
    /// Pins from `veneer.lock`.
    pub pins: Pins,
    /// Fail when a tool, image, or repo has no pin (`--frozen`).
    pub frozen: bool,
    /// Render secrets as empty strings instead of resolving them (`veneer lock`, `fetch`).
    pub no_secrets: bool,
}

impl Default for PlanOptions {
    fn default() -> Self {
        Self {
            workspace_id: "default".into(),
            build: true,
            workspace: true,
            only: None,
            sets: Vec::new(),
            pins: Pins::default(),
            frozen: false,
            no_secrets: false,
        }
    }
}

/// Planning output: the plan plus non-fatal warnings.
#[derive(Debug)]
pub struct Planned {
    pub plan: Plan,
    pub warnings: Vec<String>,
}

/// The directory that holds the project: the parent of `.veneer/` or the spec's own directory.
pub fn project_root(entry: &Path) -> PathBuf {
    let dir = entry.parent().unwrap_or(Path::new("/"));
    if dir.file_name().is_some_and(|n| n == ".veneer") {
        dir.parent().unwrap_or(dir).to_path_buf()
    } else {
        dir.to_path_buf()
    }
}

/// Directory of the file that declared the value at `pointer` (falls back to `fallback`).
fn base_dir(tree: &Node, pointer: &str, fallback: &Path) -> PathBuf {
    tree.nearest_span(pointer)
        .and_then(|s| suspect_source::Uri::parse(&s.uri).ok())
        .and_then(|u| u.as_path())
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| fallback.to_path_buf())
}

fn normalize(path: PathBuf) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn hash(parts: &[&[u8]]) -> String {
    let mut h = blake3::Hasher::new();
    for p in parts {
        h.update(&(p.len() as u64).to_le_bytes());
        h.update(p);
    }
    h.finalize().to_hex().to_string()
}

fn step(
    id: impl Into<String>,
    group: impl Into<String>,
    phase: Phase,
    kind: StepKind,
    extra: &[&[u8]],
) -> Step {
    let id = id.into();
    let json = serde_json::to_vec(&kind).expect("step kinds serialize");
    let mut parts: Vec<&[u8]> = vec![id.as_bytes(), &json];
    parts.extend_from_slice(extra);
    let hash = hash(&parts);
    Step {
        id,
        group: group.into(),
        phase,
        kind,
        hash,
        retries: 0,
        retry_any: false,
        timeout_ms: None,
    }
}

fn scalar_string(s: &Scalar) -> String {
    match s {
        Scalar::Str(s) => s.clone(),
        Scalar::Int(i) => i.to_string(),
        Scalar::Float(f) => f.to_string(),
        Scalar::Bool(b) => b.to_string(),
    }
}

fn tool_version(v: &ToolVersion) -> String {
    match v {
        ToolVersion::Str(s) => s.clone(),
        ToolVersion::Int(i) => i.to_string(),
        ToolVersion::Float(f) => f.to_string(),
        ToolVersion::Full(o) => scalar_string(&o.version),
    }
}

/// Parses `500ms`, `2s`, `1m`, or a bare number of seconds.
pub fn parse_duration_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix("ms") {
        return n.trim().parse().ok();
    }
    if let Some(n) = s.strip_suffix('s') {
        return n.trim().parse::<f64>().ok().map(|v| (v * 1000.0) as u64);
    }
    if let Some(n) = s.strip_suffix('m') {
        return n.trim().parse::<f64>().ok().map(|v| (v * 60_000.0) as u64);
    }
    s.parse::<f64>().ok().map(|v| (v * 1000.0) as u64)
}

fn os_value() -> Value {
    let family = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "other"
    };
    let distro = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|t| {
            t.lines().find_map(|l| {
                l.strip_prefix("ID=")
                    .map(|v| v.trim_matches('"').to_string())
            })
        })
        .unwrap_or_else(|| family.to_string());
    Value::from_iter([
        ("family".to_string(), Value::from(family)),
        ("arch".to_string(), Value::from(std::env::consts::ARCH)),
        ("distro".to_string(), Value::from(distro)),
    ])
}

fn resolve_secret(name: &str, s: &veneer_spec::Secret, r: &Renderer) -> Result<String, PlanError> {
    let missing = |why: String| PlanError {
        exit: 4,
        message: format!("secret `{name}`: {why}"),
    };
    match s.from {
        SecretSource::Env => {
            let var = s.var.clone().unwrap_or_else(|| name.to_string());
            std::env::var(&var)
                .map_err(|_| missing(format!("environment variable `{var}` is not set")))
        }
        SecretSource::File => {
            let path = r
                .render(s.path.as_deref().unwrap_or_default())
                .map_err(PlanError::spec)?;
            std::fs::read_to_string(&path)
                .map(|t| t.trim_end_matches(['\n', '\r']).to_string())
                .map_err(|e| missing(format!("cannot read {path}: {e}")))
        }
        SecretSource::Command => {
            let run = s.run.clone().unwrap_or_default();
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(&run)
                .stderr(std::process::Stdio::inherit())
                .output()
                .map_err(|e| missing(format!("cannot run `{run}`: {e}")))?;
            if !out.status.success() {
                return Err(missing(format!("`{run}` exited with {}", out.status)));
            }
            Ok(String::from_utf8_lossy(&out.stdout)
                .trim_end_matches(['\n', '\r'])
                .to_string())
        }
    }
}

/// Stable native-service ports, persisted at `<workspace state>/ports.json`.
fn allocate_ports(
    dirs: &Dirs,
    id: &str,
    names: &[String],
) -> Result<BTreeMap<String, u16>, PlanError> {
    let path = dirs.workspace_state(id).join("ports.json");
    let mut ports: BTreeMap<String, u16> = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let mut changed = false;
    for name in names {
        if !ports.contains_key(name) {
            let port = TcpListener::bind("127.0.0.1:0")
                .and_then(|l| l.local_addr())
                .map(|a| a.port())
                .map_err(|e| {
                    PlanError::spec(format!("cannot allocate a port for service `{name}`: {e}"))
                })?;
            ports.insert(name.clone(), port);
            changed = true;
        }
    }
    ports.retain(|k, _| names.contains(k));
    if changed {
        std::fs::create_dir_all(path.parent().unwrap())
            .and_then(|_| std::fs::write(&path, serde_json::to_vec_pretty(&ports).unwrap()))
            .map_err(|e| PlanError::spec(format!("cannot write {}: {e}", path.display())))?;
    }
    Ok(ports)
}

fn compose_project(id: &str) -> String {
    let clean: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("veneer-{clean}")
}

/// Compiles a resolved spec into a plan.
pub fn plan(
    spec: &Spec,
    tree: &Node,
    entry: &Path,
    dirs: &Dirs,
    opts: &PlanOptions,
) -> Result<Planned, PlanError> {
    let project = project_root(entry);
    // Relative paths resolve against the declaring file; values without a source span
    // (e.g. added by an overlay) resolve against the entry spec's directory.
    let spec_dir = entry.parent().unwrap_or(&project).to_path_buf();
    let mut warnings = Vec::new();
    let mut r = Renderer::new();
    let id = opts.workspace_id.clone();

    r.set("env", Value::from_iter(std::env::vars()));
    r.set("os", os_value());
    r.set(
        "veneer",
        Value::from_iter([(
            "version".to_string(),
            Value::from(env!("CARGO_PKG_VERSION")),
        )]),
    );
    r.set("secrets", Value::from_iter(Vec::<(String, Value)>::new()));

    // Secrets: workspace phase only.
    let mut secret_values = Vec::new();
    if opts.workspace && !spec.secrets.is_empty() {
        let mut map = Vec::new();
        for (name, s) in &spec.secrets {
            let value = if opts.no_secrets {
                String::new()
            } else {
                let v = resolve_secret(name, s, &r)?;
                secret_values.push(v.clone());
                v
            };
            map.push((name.clone(), Value::from(value)));
        }
        r.set("secrets", Value::from_iter(map));
    }
    let unpinned = |what: &str, key: &str| PlanError {
        exit: 2,
        message: format!(
            "V1006: {what} `{key}` is not pinned in veneer.lock (--frozen); run `veneer lock`"
        ),
    };

    // Native service names (for ports) are known before rendering.
    let native_names: Vec<String> = spec
        .services
        .iter()
        .filter_map(|(n, e)| match e {
            ServiceEntry::Def(s) if s.run.is_some() => Some(n.clone()),
            _ => None,
        })
        .collect();
    let ports = if opts.workspace {
        allocate_ports(dirs, &id, &native_names)?
    } else {
        BTreeMap::new()
    };
    let ports_value = Value::from_iter(ports.iter().map(|(k, v)| (k.clone(), Value::from(*v))));

    let set_workspace = |r: &mut Renderer, root: &Path| {
        r.set(
            "workspace",
            Value::from_iter([
                ("id".to_string(), Value::from(id.as_str())),
                ("root".to_string(), Value::from(root.display().to_string())),
                ("ports".to_string(), ports_value.clone()),
            ]),
        );
    };

    // vars: apply --set, then render to a fixpoint (3 passes covers chains of depth 3).
    let mut vars_node = tree.get("vars").cloned().unwrap_or_else(Node::object);
    if let veneer_spec::Kind::Object(map) = &mut vars_node.kind {
        for (k, v) in &opts.sets {
            map.insert(k.clone(), Node::new(veneer_spec::Kind::Str(v.clone())));
        }
    }
    let default_root = dirs.default_workspace_root(&id);
    set_workspace(&mut r, &default_root);
    let render_vars = |r: &mut Renderer| -> Result<(), PlanError> {
        r.set("vars", render::node_value(&vars_node));
        for _ in 0..3 {
            let v = r.render_value(&vars_node).map_err(PlanError::spec)?;
            r.set("vars", v);
        }
        Ok(())
    };
    render_vars(&mut r)?;

    let root = match spec.workspace.as_ref().and_then(|w| w.root.as_deref()) {
        Some(t) => PathBuf::from(r.render(t).map_err(PlanError::spec)?),
        None => default_root.clone(),
    };
    let root = if root.is_absolute() {
        root
    } else {
        project.join(root)
    };
    set_workspace(&mut r, &root);
    render_vars(&mut r)?;

    let rs = |s: &str| r.render(s).map_err(PlanError::spec);
    let mut steps = Vec::new();
    // `--only` skips other groups *before* reading their files: bake layers copy in only
    // the files their own group needs.
    let want = |group: &str, id: &str| opts.only.as_deref().is_none_or(|o| o == group || o == id);

    if opts.build {
        // packages
        let mut names = Vec::new();
        for (i, p) in spec
            .packages
            .iter()
            .enumerate()
            .filter(|_| want("packages", "packages"))
        {
            match p {
                Package::Name(n) => names.push(rs(n)?),
                Package::Import(f) => {
                    let path = normalize(
                        base_dir(tree, &format!("/packages/{i}/from"), &spec_dir)
                            .join(rs(&f.from)?),
                    );
                    names.extend(adapters::packages_from(&path).map_err(PlanError::spec)?);
                }
            }
        }
        names.dedup();
        if !names.is_empty() {
            steps.push(step(
                "packages",
                "packages",
                Phase::Build,
                StepKind::Packages { names },
                &[],
            ));
        }

        // tools
        let mut tools = IndexMap::new();
        let mut tool_options: IndexMap<String, serde_json::Value> = IndexMap::new();
        let tools_wanted = want("tools", "tools") || want("nix", "nix");
        for (name, v) in spec.tools.iter().filter(|_| tools_wanted) {
            if name == "from" {
                let path =
                    normalize(base_dir(tree, "/tools/from", &spec_dir).join(rs(&tool_version(v))?));
                tools.extend(adapters::tools_from(&path).map_err(PlanError::spec)?);
            }
        }
        for (name, v) in spec.tools.iter().filter(|_| tools_wanted) {
            if name != "from" {
                tools.insert(name.clone(), rs(&tool_version(v))?);
                if let ToolVersion::Full(o) = v
                    && !o.options.is_empty()
                {
                    tool_options.insert(name.clone(), serde_json::to_value(&o.options).unwrap());
                }
            }
        }
        // `nix:<attr>` tools install through Nix instead of mise. The value is `latest`
        // (the registry's nixpkgs) or a nixpkgs revision/branch (e.g. `nixos-24.05`).
        let mut nix_refs = Vec::new();
        tools.retain(|name, version| match name.strip_prefix("nix:") {
            Some(attr) if attr.contains('#') => {
                nix_refs.push(attr.to_string());
                false
            }
            Some(attr) => {
                nix_refs.push(if version == "latest" {
                    format!("nixpkgs#{attr}")
                } else {
                    format!("github:NixOS/nixpkgs/{version}#{attr}")
                });
                false
            }
            None => true,
        });
        if !nix_refs.is_empty() && want("nix", "nix") {
            steps.push(step(
                "nix",
                "nix",
                Phase::Build,
                StepKind::Nix { refs: nix_refs },
                &[],
            ));
        }
        for (name, version) in tools.iter_mut() {
            let key = tool_key(name, version);
            match opts.pins.tools.get(&key) {
                Some(pinned) => *version = pinned.clone(),
                None if opts.frozen => return Err(unpinned("tool", &key)),
                None => {}
            }
        }
        if !tools.is_empty() && want("tools", "tools") {
            let lockfile = Some(spec_dir.join("mise.lock")).filter(|p| p.is_file());
            let lock_bytes = lockfile
                .as_ref()
                .and_then(|p| std::fs::read(p).ok())
                .unwrap_or_default();
            steps.push(step(
                "tools",
                "tools",
                Phase::Build,
                StepKind::Tools {
                    tools,
                    options: tool_options,
                    lockfile,
                },
                &[&lock_bytes],
            ));
        }

        // deps
        for (i, d) in spec.deps.iter().enumerate() {
            let manifest =
                normalize(base_dir(tree, &format!("/deps/{i}/from"), &spec_dir).join(rs(&d.from)?));
            let rel = manifest
                .strip_prefix(&project)
                .unwrap_or(&manifest)
                .display()
                .to_string();
            let id = format!("deps:{rel}");
            if !want(&id, &id) {
                continue;
            }
            if !manifest.is_file() {
                return Err(PlanError::spec(format!(
                    "dependency manifest {} does not exist",
                    manifest.display()
                )));
            }
            let (manager, lockfile) =
                adapters::detect_manager(&manifest).map_err(PlanError::spec)?;
            if lockfile.is_none() {
                warnings.push(format!(
                    "W3001: {} has no lockfile; installing unfrozen",
                    manifest.display()
                ));
            }
            let mut digest_parts = vec![std::fs::read(&manifest).unwrap_or_default()];
            if let Some(l) = &lockfile {
                digest_parts.push(std::fs::read(l).unwrap_or_default());
            }
            let refs: Vec<&[u8]> = digest_parts.iter().map(Vec::as_slice).collect();
            let digest = hash(&refs);
            let dir = manifest.parent().unwrap().to_path_buf();
            steps.push(step(
                id.clone(),
                id,
                Phase::Build,
                StepKind::Deps {
                    dir,
                    manifest,
                    lockfile,
                    manager,
                    digest,
                },
                &[],
            ));
        }

        // images
        if !spec.images.is_empty() && want("images", "images") {
            let refs = spec
                .images
                .iter()
                .map(|i| rs(i))
                .collect::<Result<Vec<_>, _>>()?;
            let digests = image_pins(&refs, opts, &unpinned)?;
            steps.push(step(
                "images",
                "images",
                Phase::Build,
                StepKind::Images { refs, digests },
                &[],
            ));
        }

        // build hooks
        for (i, h) in spec.hooks.build.iter().enumerate() {
            if !want("hooks", &format!("hook.build:{i}")) {
                continue;
            }
            let (run, creates, unless) = hook_parts(h, &rs)?;
            let creates = creates.map(|c| project.join(c));
            steps.push(step(
                format!("hook.build:{i}"),
                "hooks",
                Phase::Build,
                StepKind::Hook {
                    run,
                    cwd: project.clone(),
                    creates,
                    unless,
                },
                &[],
            ));
        }
    }

    let mut env = IndexMap::new();
    if opts.workspace {
        for (k, v) in &spec.env {
            env.insert(k.clone(), rs(&scalar_string(v))?);
        }
        env.insert("VENEER_WORKSPACE_ID".into(), id.clone());
        env.insert("VENEER_WORKSPACE_ROOT".into(), root.display().to_string());

        // Harness MCP config: rendered now, so secret-bearing values can join the env.
        let mut harness_homes = IndexMap::new();
        for (h, ho) in &spec.harnesses {
            let home = match &ho.home {
                Some(t) => PathBuf::from(rs(t)?),
                None => PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into())),
            };
            harness_homes.insert(h.clone(), home);
        }
        let mut servers = IndexMap::new();
        for (name, m) in &spec.mcp {
            let rmap =
                |m: &IndexMap<String, String>| -> Result<IndexMap<String, String>, PlanError> {
                    m.iter().map(|(k, v)| Ok((k.clone(), rs(v)?))).collect()
                };
            servers.insert(
                name.clone(),
                harness::Server {
                    command: m.command.as_deref().map(rs).transpose()?,
                    args: m.args.iter().map(|a| rs(a)).collect::<Result<_, _>>()?,
                    env: rmap(&m.env)?,
                    cwd: m.cwd.as_deref().map(rs).transpose()?,
                    url: m.url.as_deref().map(rs).transpose()?,
                    headers: rmap(&m.headers)?,
                },
            );
        }
        let codex_home = std::env::var("CODEX_HOME").ok();
        let (harness_steps, mcp_exports) = harness::steps(
            &harness_homes,
            &servers,
            &secret_values,
            codex_home.as_deref(),
        );
        env.extend(mcp_exports);

        let env_text: String = env.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
        let env_file = dirs.workspace_state(&id).join("env");
        steps.push(step(
            "env",
            "workspace",
            Phase::Workspace,
            StepKind::Env { file: env_file },
            &[env_text.as_bytes()],
        ));

        if let Some(ws) = &spec.workspace {
            for (i, repo) in ws.repos.iter().enumerate() {
                let mut url = rs(&repo.url)?;
                let looks_remote = url.contains("://") || url.starts_with("git@");
                if !looks_remote {
                    url = normalize(
                        base_dir(tree, &format!("/workspace/repos/{i}/url"), &spec_dir).join(&url),
                    )
                    .display()
                    .to_string();
                }
                let mut git_ref = repo.git_ref.as_deref().map(rs).transpose()?;
                let path = rs(&repo.path)?;
                let mode = match repo.mode {
                    veneer_spec::RepoMode::Clone => RepoMode::Clone,
                    veneer_spec::RepoMode::Worktree => RepoMode::Worktree,
                    veneer_spec::RepoMode::Copy => RepoMode::Copy,
                };
                if mode != RepoMode::Copy {
                    let key = repo_key(&url, git_ref.as_deref());
                    match opts.pins.repos.get(&key) {
                        Some(commit) => git_ref = Some(commit.clone()),
                        None if opts.frozen => return Err(unpinned("repo", &key)),
                        None => {}
                    }
                }
                steps.push(step(
                    format!("repo:{path}"),
                    "workspace",
                    Phase::Workspace,
                    StepKind::Repo {
                        url,
                        git_ref,
                        dest: root.join(&path),
                        mode,
                    },
                    &[],
                ));
            }
            for (i, f) in ws.files.iter().enumerate() {
                let src = normalize(
                    base_dir(tree, &format!("/workspace/files/{i}/src"), &spec_dir)
                        .join(rs(&f.src)?),
                );
                let raw = std::fs::read_to_string(&src).map_err(|e| {
                    PlanError::spec(format!("cannot read file source {}: {e}", src.display()))
                })?;
                let templated = src.extension().is_some_and(|e| e == "j2" || e == "jinja");
                let content = if templated {
                    r.render_always(&raw).map_err(PlanError::spec)?
                } else {
                    raw
                };
                let dest_rel = rs(&f.dest)?;
                let mode = match &f.mode {
                    Some(m) => Some(
                        u32::from_str_radix(m.trim_start_matches("0o"), 8).map_err(|_| {
                            PlanError::spec(format!("file mode `{m}` is not octal"))
                        })?,
                    ),
                    None => None,
                };
                steps.push(step(
                    format!("file:{dest_rel}"),
                    "workspace",
                    Phase::Workspace,
                    StepKind::File {
                        dest: root.join(&dest_rel),
                        content,
                        mode,
                    },
                    &[],
                ));
            }
        }

        for (sid, kind) in harness_steps {
            steps.push(step(sid, "workspace", Phase::Workspace, kind, &[]));
        }
        if spec.harnesses.contains_key("claude-code") {
            // One instruction file for every harness: Claude Code reads CLAUDE.md.
            let mut dirs_with_agents: Vec<PathBuf> = vec![root.clone()];
            for s in &steps {
                match &s.kind {
                    StepKind::Repo { dest, .. } => dirs_with_agents.push(dest.clone()),
                    StepKind::File { dest, .. }
                        if dest.file_name().is_some_and(|n| n == "AGENTS.md") =>
                    {
                        if let Some(d) = dest.parent() {
                            dirs_with_agents.push(d.to_path_buf());
                        }
                    }
                    _ => {}
                }
            }
            dirs_with_agents.dedup();
            for d in dirs_with_agents {
                let link = d.join("CLAUDE.md");
                steps.push(step(
                    format!("agents:{}", link.display()),
                    "workspace",
                    Phase::Workspace,
                    StepKind::Symlink {
                        link,
                        target: "AGENTS.md".into(),
                    },
                    &[],
                ));
            }
        }

        if !spec.images.is_empty() {
            let refs = spec
                .images
                .iter()
                .map(|i| rs(i))
                .collect::<Result<Vec<_>, _>>()?;
            let digests = image_pins(&refs, opts, &unpinned)?;
            steps.push(step(
                "images.load",
                "workspace",
                Phase::Workspace,
                StepKind::ImagesLoad { refs, digests },
                &[],
            ));
        }

        // services
        let mut compose_files = Vec::new();
        let mut generated = serde_json::Map::new();
        for (name, entry) in &spec.services {
            match entry {
                ServiceEntry::Import(path) if name == "from" => {
                    compose_files.push(normalize(
                        base_dir(tree, "/services/from", &spec_dir).join(rs(path)?),
                    ));
                }
                ServiceEntry::Import(path) => {
                    return Err(PlanError::spec(format!(
                        "service `{name}` is a string (`{path}`); only the key `from` may name a compose file"
                    )));
                }
                ServiceEntry::Def(svc) => {
                    let mut svc_env = IndexMap::new();
                    for (k, v) in &svc.env {
                        svc_env.insert(k.clone(), rs(&scalar_string(v))?);
                    }
                    let hc = svc
                        .healthcheck
                        .as_ref()
                        .map(|h| -> Result<Healthcheck, PlanError> {
                            Ok(Healthcheck {
                                cmd: h.cmd.as_deref().map(rs).transpose()?,
                                http: h.http.as_deref().map(rs).transpose()?,
                                interval_ms: h
                                    .interval
                                    .as_deref()
                                    .and_then(parse_duration_ms)
                                    .unwrap_or(1000),
                                retries: h.retries.unwrap_or(30),
                            })
                        });
                    let hc = hc.transpose()?;
                    if let Some(image) = &svc.image {
                        let mut s = serde_json::Map::new();
                        s.insert("image".into(), rs(image)?.into());
                        if !svc_env.is_empty() {
                            s.insert(
                                "environment".into(),
                                serde_json::to_value(&svc_env).unwrap(),
                            );
                        }
                        if !svc.ports.is_empty() {
                            let ports = svc
                                .ports
                                .iter()
                                .map(|p| rs(p))
                                .collect::<Result<Vec<_>, _>>()?;
                            s.insert("ports".into(), ports.into());
                        }
                        if !svc.depends_on.is_empty() {
                            s.insert("depends_on".into(), svc.depends_on.clone().into());
                        }
                        if let Some(h) = &hc {
                            let test = match (&h.cmd, &h.http) {
                                (Some(c), _) => serde_json::json!(["CMD-SHELL", c]),
                                (None, Some(u)) => serde_json::json!([
                                    "CMD-SHELL",
                                    format!("wget -qO- {u} >/dev/null || curl -fsS {u} >/dev/null")
                                ]),
                                _ => serde_json::Value::Null,
                            };
                            if !test.is_null() {
                                s.insert(
                                    "healthcheck".into(),
                                    serde_json::json!({"test": test, "interval": format!("{}ms", h.interval_ms), "retries": h.retries}),
                                );
                            }
                        }
                        generated.insert(name.clone(), serde_json::Value::Object(s));
                    } else if let Some(run) = &svc.run {
                        let port = ports.get(name).copied().unwrap_or(0);
                        svc_env
                            .entry("PORT".into())
                            .or_insert_with(|| port.to_string());
                        steps.push(step(
                            format!("service:{name}"),
                            "workspace",
                            Phase::Workspace,
                            StepKind::Native {
                                name: name.clone(),
                                run: rs(run)?,
                                cwd: root.clone(),
                                port,
                                env: svc_env,
                                healthcheck: hc,
                            },
                            &[],
                        ));
                    }
                }
            }
        }
        // Image services run as sidecars elsewhere (e.g. `veneer render k8s`).
        if std::env::var_os("VENEER_EXTERNAL_SERVICES").is_some() {
            compose_files.clear();
            generated.clear();
        }
        if !compose_files.is_empty() || !generated.is_empty() {
            let generated = (!generated.is_empty()).then(|| {
                serde_json::to_string_pretty(&serde_json::json!({ "services": generated })).unwrap()
            });
            steps.push(step(
                "services.compose",
                "workspace",
                Phase::Workspace,
                StepKind::Compose {
                    project: compose_project(&id),
                    files: compose_files,
                    generated,
                },
                &[],
            ));
        }

        for (i, h) in spec.hooks.workspace.iter().enumerate() {
            let (run, creates, unless) = hook_parts(h, &rs)?;
            let creates = creates.map(|c| root.join(c));
            steps.push(step(
                format!("hook.workspace:{i}"),
                "workspace",
                Phase::Workspace,
                StepKind::Hook {
                    run,
                    cwd: root.clone(),
                    creates,
                    unless,
                },
                &[],
            ));
        }
    }

    if let Some(only) = &opts.only {
        steps.retain(|s| &s.group == only || &s.id == only);
    }
    apply_retry_policy(spec, &mut steps)?;

    Ok(Planned {
        plan: Plan {
            workspace_id: id,
            workspace_root: root,
            project_root: project,
            env,
            secret_values,
            steps,
        },
        warnings,
    })
}

fn image_pins(
    refs: &[String],
    opts: &PlanOptions,
    unpinned: &dyn Fn(&str, &str) -> PlanError,
) -> Result<BTreeMap<String, String>, PlanError> {
    let mut out = BTreeMap::new();
    for r in refs {
        match opts.pins.images.get(r) {
            Some(d) => {
                out.insert(r.clone(), d.clone());
            }
            None if opts.frozen && !r.contains('@') => return Err(unpinned("image", r)),
            None => {}
        }
    }
    Ok(out)
}

/// Default network retries (standards proposal §5.4.2).
pub const DEFAULT_RETRIES: u32 = 4;

/// Sets retries and timeouts: network steps retry transient failures; hooks retry only when
/// they ask to (and then on any failure).
fn apply_retry_policy(spec: &Spec, steps: &mut [Step]) -> Result<(), PlanError> {
    let dur = |s: &str| {
        parse_duration_ms(s)
            .ok_or_else(|| PlanError::spec(format!("`{s}` is not a duration (e.g. 90s, 10m)")))
    };
    let default_timeout = spec.defaults.timeout.as_deref().map(dur).transpose()?;
    let retries = spec.defaults.retries.unwrap_or(DEFAULT_RETRIES);
    for st in steps.iter_mut() {
        st.timeout_ms = default_timeout;
        match &st.kind {
            StepKind::Packages { .. }
            | StepKind::Tools { .. }
            | StepKind::Nix { .. }
            | StepKind::Deps { .. }
            | StepKind::Images { .. }
            | StepKind::Repo { .. }
            | StepKind::Compose { .. } => st.retries = retries,
            StepKind::Hook { .. } => {
                let (list, idx) = match st.id.split_once(':') {
                    Some(("hook.build", i)) => (&spec.hooks.build, i.parse::<usize>().ok()),
                    Some(("hook.workspace", i)) => (&spec.hooks.workspace, i.parse::<usize>().ok()),
                    _ => continue,
                };
                if let Some(Hook::Full(h)) = idx.and_then(|i| list.get(i)) {
                    if let Some(n) = h.retries {
                        st.retries = n;
                        st.retry_any = true;
                    }
                    if let Some(t) = &h.timeout {
                        st.timeout_ms = Some(dur(t)?);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

type Rs<'a> = dyn Fn(&str) -> Result<String, PlanError> + 'a;

fn hook_parts(
    h: &Hook,
    rs: &Rs<'_>,
) -> Result<(String, Option<String>, Option<String>), PlanError> {
    Ok(match h {
        Hook::Cmd(c) => (rs(c)?, None, None),
        Hook::Full(f) => (
            rs(&f.run)?,
            f.creates.as_deref().map(rs).transpose()?,
            f.unless.as_deref().map(rs).transpose()?,
        ),
    })
}

/// Files each bake layer needs, relative to the project root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BakeInputs {
    /// Files read by the `packages` and `tools` groups (`from:` imports).
    pub early: Vec<PathBuf>,
    /// Per dependency group: (group id, manifest, lockfile).
    pub deps: Vec<(String, PathBuf, Option<PathBuf>)>,
}

/// Lists the project files bake must copy before each layer (absolute paths).
pub fn bake_inputs(spec: &Spec, tree: &Node, entry: &Path) -> Result<BakeInputs, PlanError> {
    let project = project_root(entry);
    let spec_dir = entry.parent().unwrap_or(&project).to_path_buf();
    let mut out = BakeInputs::default();
    for (i, p) in spec.packages.iter().enumerate() {
        if let Package::Import(f) = p {
            out.early.push(normalize(
                base_dir(tree, &format!("/packages/{i}/from"), &spec_dir).join(&f.from),
            ));
        }
    }
    if let Some(v) = spec.tools.get("from") {
        out.early.push(normalize(
            base_dir(tree, "/tools/from", &spec_dir).join(tool_version(v)),
        ));
    }
    if spec_dir.join("mise.lock").is_file() {
        out.early.push(spec_dir.join("mise.lock"));
    }
    for (i, d) in spec.deps.iter().enumerate() {
        let manifest =
            normalize(base_dir(tree, &format!("/deps/{i}/from"), &spec_dir).join(&d.from));
        let rel = manifest
            .strip_prefix(&project)
            .unwrap_or(&manifest)
            .display()
            .to_string();
        let lock = adapters::detect_manager(&manifest)
            .ok()
            .and_then(|(_, l)| l);
        out.deps.push((format!("deps:{rel}"), manifest, lock));
    }
    Ok(out)
}
