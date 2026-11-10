//! `veneer render k8s` (standards proposal §5.6): plain Kubernetes YAML, with no CRDs.
//!
//! - `--count 1`: one Pod. `--count N`: an Indexed Job (`completionMode: Indexed`) whose
//!   pods get the workspace id `<name>-<JOB_COMPLETION_INDEX>`.
//! - Image services become native sidecars (init containers with `restartPolicy: Always`,
//!   Kubernetes ≥ 1.29); the workspace skips its own compose step (`VENEER_EXTERNAL_SERVICES`).
//! - Secrets become `secretKeyRef`s / Secret volumes on one named Secret; values are never
//!   rendered.
//! - Readiness: the workspace phase touches `$VENEER_READY_FILE`.
//! - `--stage-at-start`: a stock image; an init container copies veneer in from the published
//!   image, and the main container runs both phases at start.

use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::{Value, json};
use veneer_spec::{SecretSource, ServiceEntry};

use crate::run::{Target, fail, load};

pub const READY_FILE: &str = "/tmp/veneer-ready";

#[derive(Debug, clap::Subcommand)]
pub enum RenderCmd {
    /// Kubernetes manifests (a Pod, or an Indexed Job with --count > 1).
    K8s(K8sArgs),
}

#[derive(Debug, clap::Args)]
pub struct K8sArgs {
    #[command(flatten)]
    pub target: Target,
    /// Workspace image (a `veneer bake` image, or any image with --stage-at-start).
    #[arg(long)]
    pub image: String,
    /// Number of identical workspaces (> 1 renders an Indexed Job).
    #[arg(long, default_value_t = 1)]
    pub count: u32,
    /// Object name (default: the project name).
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub namespace: Option<String>,
    /// Secret holding the spec's secrets (keys: the secret names).
    #[arg(long, default_value = "veneer-secrets")]
    pub secret_name: String,
    /// The image has no veneer: copy it in and run the build phase at start.
    #[arg(long)]
    pub stage_at_start: bool,
    /// Image carrying the veneer binary for --stage-at-start.
    #[arg(long, default_value_t = format!("ghcr.io/lukasparke/veneer:{}", env!("CARGO_PKG_VERSION")))]
    pub veneer_image: String,
    /// Spec location inside the pod for --stage-at-start (a remote source, or a path the image has).
    #[arg(long, value_name = "SOURCE")]
    pub project: Option<String>,
    /// Output file (default: stdout).
    #[arg(long, short)]
    pub output: Option<PathBuf>,
    /// The workload command, after the workspace is ready (default: sleep forever).
    #[arg(last = true)]
    pub command: Vec<String>,
}

fn dns_name(s: &str) -> String {
    let mut out: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    out = out.trim_matches('-').to_string();
    out.truncate(52);
    if out.is_empty() {
        "workspace".into()
    } else {
        out
    }
}

fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Port from a compose-style mapping (`8080:80` → 80, `80` → 80, `127.0.0.1:8080:80/tcp` → 80).
fn container_port(p: &str) -> Option<u16> {
    p.split('/').next()?.rsplit(':').next()?.parse().ok()
}

fn probe(h: &veneer_spec::Healthcheck) -> Option<Value> {
    let period = h
        .interval
        .as_deref()
        .and_then(veneer_plan::parse_duration_ms)
        .map(|ms| (ms / 1000).max(1))
        .unwrap_or(2);
    let failures = h.retries.unwrap_or(60);
    if let Some(cmd) = &h.cmd {
        return Some(
            json!({"exec": {"command": ["sh", "-c", cmd]}, "periodSeconds": period, "failureThreshold": failures}),
        );
    }
    let url = h.http.as_deref()?;
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let (hostport, path) = rest
        .split_once('/')
        .map(|(h, p)| (h, format!("/{p}")))
        .unwrap_or((rest, "/".into()));
    let port: u16 = hostport
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
        .unwrap_or(80);
    Some(
        json!({"httpGet": {"path": path, "port": port}, "periodSeconds": period, "failureThreshold": failures}),
    )
}

/// Builds the manifest objects. Returns (objects, warnings).
pub fn render(l: &crate::run::Loaded, a: &K8sArgs) -> (Vec<Value>, Vec<String>) {
    let mut warnings = Vec::new();
    let project = veneer_plan::project_root(&l.entry);
    let name = dns_name(&a.name.clone().unwrap_or_else(|| {
        project
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }));
    let spec_path = match (&a.project, a.stage_at_start) {
        (Some(p), _) => p.clone(),
        (None, _) => format!(
            "{}/{}",
            veneer_bake::PROJECT_DIR,
            l.entry.strip_prefix(&project).unwrap_or(&l.entry).display()
        ),
    };
    let labels = json!({"app.kubernetes.io/name": name, "app.kubernetes.io/managed-by": "veneer"});

    // Secrets → env refs and Secret volumes.
    let mut env = vec![
        json!({"name": "VENEER_READY_FILE", "value": READY_FILE}),
        json!({"name": "VENEER_STATE_DIR", "value": veneer_bake::STATE_DIR}),
    ];
    let mut volumes = Vec::new();
    let mut mounts = Vec::new();
    for (key, s) in &l.spec.secrets {
        match s.from {
            SecretSource::Env => env.push(json!({
                "name": s.var.clone().unwrap_or_else(|| key.clone()),
                "valueFrom": {"secretKeyRef": {"name": a.secret_name, "key": key}},
            })),
            SecretSource::File => {
                let path = s.path.clone().unwrap_or_default();
                if !path.starts_with('/') || path.contains("{{") {
                    warnings.push(format!(
                        "W4101: secret `{key}` reads `{path}`; only absolute, untemplated paths can be mounted, so mount it yourself"
                    ));
                    continue;
                }
                let p = std::path::Path::new(&path);
                let vol = dns_name(&format!("secret-{key}"));
                volumes.push(json!({"name": vol, "secret": {"secretName": a.secret_name, "items": [{"key": key, "path": p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()}]}}));
                mounts.push(json!({"name": vol, "mountPath": p.parent().map(|d| d.display().to_string()).unwrap_or_else(|| "/".into()), "readOnly": true}));
            }
            SecretSource::Command => {}
        }
    }

    // Image services → native sidecars.
    let mut init = Vec::new();
    if a.stage_at_start {
        volumes.push(json!({"name": "veneer-bin", "emptyDir": {}}));
        mounts.push(json!({"name": "veneer-bin", "mountPath": "/opt/veneer/bin"}));
        init.push(json!({
            "name": "veneer",
            "image": a.veneer_image,
            "command": ["/veneer", "internal", "copy-self", "/opt/veneer/bin/veneer"],
            "volumeMounts": [{"name": "veneer-bin", "mountPath": "/opt/veneer/bin"}],
        }));
        env.push(json!({"name": "PATH", "value": "/opt/veneer/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"}));
    }
    let mut sidecars = 0;
    for (svc_name, entry) in &l.spec.services {
        match entry {
            ServiceEntry::Def(svc) => {
                let Some(image) = &svc.image else { continue };
                if image.contains("{{") {
                    warnings.push(format!("W4102: service `{svc_name}` has a templated image; rendered as written"));
                }
                let mut c = json!({
                    "name": dns_name(svc_name),
                    "image": image,
                    "restartPolicy": "Always",
                });
                let senv: Vec<Value> = svc
                    .env
                    .iter()
                    .map(|(k, v)| json!({"name": k, "value": match v {
                        veneer_spec::Scalar::Str(s) => s.clone(),
                        veneer_spec::Scalar::Int(i) => i.to_string(),
                        veneer_spec::Scalar::Float(f) => f.to_string(),
                        veneer_spec::Scalar::Bool(b) => b.to_string(),
                    }}))
                    .collect();
                if !senv.is_empty() {
                    c["env"] = json!(senv);
                }
                let ports: Vec<Value> = svc
                    .ports
                    .iter()
                    .filter_map(|p| container_port(p))
                    .map(|p| json!({"containerPort": p}))
                    .collect();
                if !ports.is_empty() {
                    c["ports"] = json!(ports);
                }
                if let Some(p) = svc.healthcheck.as_ref().and_then(probe) {
                    // A sidecar's startupProbe gates the workspace container's start.
                    c["startupProbe"] = p.clone();
                    c["readinessProbe"] = p;
                }
                init.push(c);
                sidecars += 1;
            }
            ServiceEntry::Import(path) => warnings.push(format!(
                "W4103: compose file `{path}` is not rendered; declare its services in the spec, or run Docker in the pod"
            )),
        }
    }
    if sidecars > 0 {
        env.push(json!({"name": "VENEER_EXTERNAL_SERVICES", "value": "1"}));
    }

    let id = if a.count > 1 {
        format!("{name}-${{JOB_COMPLETION_INDEX}}")
    } else {
        name.clone()
    };
    let workload = if a.command.is_empty() {
        "sleep infinity".to_string()
    } else {
        a.command
            .iter()
            .map(|c| sh_quote(c))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut extra = String::new();
    for (k, v) in &a.target.sets {
        extra.push_str(&format!(" --set {}", sh_quote(&format!("{k}={v}"))));
    }
    let spec_q = sh_quote(&spec_path);
    let script = if a.stage_at_start {
        format!(
            "export VENEER_WORKSPACE_ID=\"{id}\"; veneer apply {spec_q}{extra} --phase build && exec veneer entrypoint {spec_q}{extra} -- {workload}"
        )
    } else {
        format!(
            "export VENEER_WORKSPACE_ID=\"{id}\"; exec veneer entrypoint {spec_q}{extra} -- {workload}"
        )
    };
    let mut main = json!({
        "name": "workspace",
        "image": a.image,
        "command": ["sh", "-c", script],
        "env": env,
        "readinessProbe": {"exec": {"command": ["test", "-f", READY_FILE]}, "periodSeconds": 2, "failureThreshold": 900},
    });
    if !mounts.is_empty() {
        main["volumeMounts"] = json!(mounts);
    }
    let mut pod_spec = json!({
        "containers": [main],
        "restartPolicy": if a.count > 1 { "Never" } else { "Always" },
    });
    if !init.is_empty() {
        pod_spec["initContainers"] = json!(init);
    }
    if !volumes.is_empty() {
        pod_spec["volumes"] = json!(volumes);
    }
    let mut meta = json!({"name": name, "labels": labels});
    if let Some(ns) = &a.namespace {
        meta["namespace"] = json!(ns);
    }
    let obj = if a.count > 1 {
        json!({
            "apiVersion": "batch/v1",
            "kind": "Job",
            "metadata": meta,
            "spec": {
                "completionMode": "Indexed",
                "completions": a.count,
                "parallelism": a.count,
                "backoffLimit": 0,
                "template": {"metadata": {"labels": labels}, "spec": pod_spec},
            },
        })
    } else {
        json!({"apiVersion": "v1", "kind": "Pod", "metadata": meta, "spec": pod_spec})
    };
    (vec![obj], warnings)
}

// ---------------------------------------------------------------- YAML

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => serde_json::to_string(s).unwrap(),
        other => other.to_string(),
    }
}

fn emit(v: &Value, indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    match v {
        Value::Object(m) => {
            for (k, val) in m {
                match val {
                    Value::Object(x) if !x.is_empty() => {
                        out.push_str(&format!("{pad}{k}:\n"));
                        emit(val, indent + 2, out);
                    }
                    Value::Array(x) if !x.is_empty() => {
                        out.push_str(&format!("{pad}{k}:\n"));
                        emit(val, indent, out);
                    }
                    Value::Object(_) => out.push_str(&format!("{pad}{k}: {{}}\n")),
                    Value::Array(_) => out.push_str(&format!("{pad}{k}: []\n")),
                    _ => out.push_str(&format!("{pad}{k}: {}\n", scalar(val))),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                match item {
                    Value::Object(m) if !m.is_empty() => {
                        let mut inner = String::new();
                        emit(item, indent + 2, &mut inner);
                        let first = inner.trim_start_matches(' ');
                        out.push_str(&format!("{pad}- {first}"));
                    }
                    Value::Array(_) => {
                        out.push_str(&format!("{pad}-\n"));
                        emit(item, indent + 2, out);
                    }
                    _ => out.push_str(&format!("{pad}- {}\n", scalar(item))),
                }
            }
        }
        other => out.push_str(&format!("{pad}{}\n", scalar(other))),
    }
}

/// Block-style YAML (strings JSON-quoted, which YAML accepts verbatim).
pub fn to_yaml(objects: &[Value]) -> String {
    let mut out = String::from("# Generated by `veneer render k8s`.\n");
    for (i, o) in objects.iter().enumerate() {
        if i > 0 {
            out.push_str("---\n");
        }
        emit(o, 0, &mut out);
    }
    out
}

pub fn run(cmd: RenderCmd) -> ExitCode {
    let RenderCmd::K8s(a) = cmd;
    let l = match load(&a.target, false) {
        Ok(l) => l,
        Err(code) => return code,
    };
    let (objects, warnings) = render(&l, &a);
    for w in warnings {
        eprintln!("warning: {w}");
    }
    let yaml = to_yaml(&objects);
    match &a.output {
        Some(p) => match std::fs::write(p, yaml) {
            Ok(()) => {
                eprintln!("✓ {}", p.display());
                ExitCode::SUCCESS
            }
            Err(e) => fail(1, &format!("cannot write {}: {e}", p.display()), false),
        },
        None => {
            print!("{yaml}");
            ExitCode::SUCCESS
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_round_trips_structure() {
        let y = to_yaml(&[json!({"a": {"b": [1, {"c": "x: y", "d": []}], "e": {}}, "f": true})]);
        assert_eq!(
            y,
            "# Generated by `veneer render k8s`.\na:\n  b:\n  - 1\n  - c: \"x: y\"\n    d: []\n  e: {}\nf: true\n"
        );
    }

    #[test]
    fn ports_and_names() {
        assert_eq!(container_port("8080:80"), Some(80));
        assert_eq!(container_port("127.0.0.1:5432:5432/tcp"), Some(5432));
        assert_eq!(dns_name("My_Project!"), "my-project");
    }
}
