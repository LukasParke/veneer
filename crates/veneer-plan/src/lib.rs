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
mod model;
mod render;

pub use dirs::{Dirs, is_root};
pub use model::*;
pub use render::{Renderer, is_template};

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
}

impl Default for PlanOptions {
    fn default() -> Self {
        Self {
            workspace_id: "default".into(),
            build: true,
            workspace: true,
            only: None,
            sets: Vec::new(),
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
            let value = resolve_secret(name, s, &r)?;
            secret_values.push(value.clone());
            map.push((name.clone(), Value::from(value)));
        }
        r.set("secrets", Value::from_iter(map));
    }

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

    if opts.build {
        // packages
        let mut names = Vec::new();
        for (i, p) in spec.packages.iter().enumerate() {
            match p {
                Package::Name(n) => names.push(rs(n)?),
                Package::Import(f) => {
                    let path = normalize(
                        base_dir(tree, &format!("/packages/{i}/from"), &project).join(rs(&f.from)?),
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
        for (name, v) in &spec.tools {
            if name == "from" {
                let path =
                    normalize(base_dir(tree, "/tools/from", &project).join(rs(&tool_version(v))?));
                tools.extend(adapters::tools_from(&path).map_err(PlanError::spec)?);
            }
        }
        for (name, v) in &spec.tools {
            if name != "from" {
                tools.insert(name.clone(), rs(&tool_version(v))?);
            }
        }
        if !tools.is_empty() {
            steps.push(step(
                "tools",
                "tools",
                Phase::Build,
                StepKind::Tools { tools },
                &[],
            ));
        }

        // deps
        for (i, d) in spec.deps.iter().enumerate() {
            let manifest =
                normalize(base_dir(tree, &format!("/deps/{i}/from"), &project).join(rs(&d.from)?));
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
            let rel = manifest
                .strip_prefix(&project)
                .unwrap_or(&manifest)
                .display()
                .to_string();
            let id = format!("deps:{rel}");
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
        if !spec.images.is_empty() {
            let refs = spec
                .images
                .iter()
                .map(|i| rs(i))
                .collect::<Result<Vec<_>, _>>()?;
            steps.push(step(
                "images",
                "images",
                Phase::Build,
                StepKind::Images { refs },
                &[],
            ));
        }

        // build hooks
        for (i, h) in spec.hooks.build.iter().enumerate() {
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
                        base_dir(tree, &format!("/workspace/repos/{i}/url"), &project).join(&url),
                    )
                    .display()
                    .to_string();
                }
                let git_ref = repo.git_ref.as_deref().map(&rs).transpose()?;
                let path = rs(&repo.path)?;
                let mode = match repo.mode {
                    veneer_spec::RepoMode::Clone => RepoMode::Clone,
                    veneer_spec::RepoMode::Worktree => RepoMode::Worktree,
                    veneer_spec::RepoMode::Copy => RepoMode::Copy,
                };
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
                    base_dir(tree, &format!("/workspace/files/{i}/src"), &project)
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

        if !spec.images.is_empty() {
            let refs = spec
                .images
                .iter()
                .map(|i| rs(i))
                .collect::<Result<Vec<_>, _>>()?;
            steps.push(step(
                "images.load",
                "workspace",
                Phase::Workspace,
                StepKind::ImagesLoad { refs },
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
                        base_dir(tree, "/services/from", &project).join(rs(path)?),
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
                                cmd: h.cmd.as_deref().map(&rs).transpose()?,
                                http: h.http.as_deref().map(&rs).transpose()?,
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
