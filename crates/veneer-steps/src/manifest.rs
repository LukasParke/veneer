//! Environment manifest and fingerprint (standards proposal §5.1).
//!
//! The manifest records what a workspace *actually* contains, observed live: tool versions,
//! OS package versions, image config digests, repo commits, managed-file digests, and so
//! on. Its `stable` section excludes everything that differs between two otherwise-identical
//! workspaces: the workspace id, root, allocated ports, and secret values are replaced by
//! placeholders before hashing. The fingerprint is `sha256:` over the RFC 8785 (JCS)
//! canonical JSON of `stable`.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use veneer_plan::{Dirs, Plan, StepKind};

use crate::util::capture;

pub const SCHEMA: &str = "veneer.manifest/v1";

// ---------------------------------------------------------------- RFC 8785

/// RFC 8785 JSON Canonicalization Scheme. Object keys sort by UTF-16 code units; numbers
/// use their shortest round-trip form (integers print as integers).
pub fn jcs(v: &Value) -> String {
    let mut out = String::new();
    write_jcs(v, &mut out);
    out
}

fn write_jcs(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                out.push_str(&i.to_string());
            } else if let Some(u) = n.as_u64() {
                out.push_str(&u.to_string());
            } else {
                let f = n.as_f64().unwrap_or(0.0);
                if f.fract() == 0.0 && f.abs() < 1e21 {
                    out.push_str(&format!("{}", f as i64));
                } else {
                    out.push_str(&format!("{f}"));
                }
            }
        }
        Value::String(s) => write_str(s, out),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_jcs(x, out);
            }
            out.push(']');
        }
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_str(k, out);
                out.push(':');
                write_jcs(&m[*k], out);
            }
            out.push('}');
        }
    }
}

fn write_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// `sha256:<hex>` of the canonical form of `stable`.
pub fn fingerprint(stable: &Value) -> String {
    format!("sha256:{}", sha256_hex(jcs(stable).as_bytes()))
}

// ---------------------------------------------------------------- normalization

/// Replaces per-workspace values with placeholders so identical environments hash equal.
struct Normalizer {
    /// (needle, placeholder, needs word boundaries)
    rules: Vec<(String, String, bool)>,
}

impl Normalizer {
    fn new(plan: &Plan) -> Self {
        let mut rules = Vec::new();
        for (i, s) in plan.secret_values.iter().enumerate() {
            if !s.is_empty() {
                rules.push((s.clone(), format!("${{secret:{i}}}"), false));
            }
        }
        let root = plan.workspace_root.display().to_string();
        if root.len() > 1 {
            rules.push((root, "${workspace.root}".into(), false));
        }
        for s in &plan.steps {
            if let StepKind::Native { name, port, .. } = &s.kind
                && *port > 0
            {
                rules.push((port.to_string(), format!("${{port:{name}}}"), true));
            }
        }
        if !plan.workspace_id.is_empty() {
            rules.push((plan.workspace_id.clone(), "${workspace.id}".into(), true));
        }
        // Longest needles first so a root containing the id is replaced whole.
        rules.sort_by_key(|r| std::cmp::Reverse(r.0.len()));
        Self { rules }
    }

    fn apply(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (needle, placeholder, bounded) in &self.rules {
            out = if *bounded {
                replace_bounded(&out, needle, placeholder)
            } else {
                out.replace(needle.as_str(), placeholder)
            };
        }
        out
    }
}

/// Replaces `needle` only where it is not adjacent to an ASCII alphanumeric character.
fn replace_bounded(hay: &str, needle: &str, with: &str) -> String {
    let mut out = String::with_capacity(hay.len());
    let bytes = hay.as_bytes();
    let mut i = 0;
    while let Some(pos) = hay[i..].find(needle) {
        let start = i + pos;
        let end = start + needle.len();
        let before_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let after_ok = end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
        out.push_str(&hay[i..start]);
        if before_ok && after_ok {
            out.push_str(with);
        } else {
            out.push_str(needle);
        }
        i = end;
    }
    out.push_str(&hay[i..]);
    out
}

// ---------------------------------------------------------------- observations

fn platform() -> String {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    };
    format!("{}/{arch}", std::env::consts::OS)
}

fn os_release() -> Value {
    let text = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let field = |k: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{k}=")))
            .map(|v| v.trim_matches('"').to_string())
    };
    json!({"id": field("ID"), "version_id": field("VERSION_ID")})
}

/// `{tool: [installed versions]}` from `mise ls --json`.
fn mise_versions(dirs: &Dirs) -> BTreeMap<String, Vec<String>> {
    let bin = std::env::var_os("VENEER_MISE_BIN")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| dirs.mise_bin());
    if !bin.is_file() {
        return BTreeMap::new();
    }
    let Some(out) = capture(crate::mise::mise_cmd(&bin, dirs).args(["ls", "--json"])) else {
        return BTreeMap::new();
    };
    let v: Value = serde_json::from_str(&out).unwrap_or_default();
    v.as_object()
        .into_iter()
        .flatten()
        .map(|(k, entries)| {
            let mut versions: Vec<String> = entries
                .as_array()
                .into_iter()
                .flatten()
                .filter(|e| e["installed"].as_bool() == Some(true))
                .filter_map(|e| e["version"].as_str().map(str::to_string))
                .collect();
            versions.sort();
            (k.clone(), versions)
        })
        .collect()
}

/// The image's config digest (= Docker image id): from the daemon, or from the saved tarball.
fn image_id(dirs: &Dirs, r: &str) -> Option<String> {
    if let Some(id) =
        capture(Command::new("docker").args(["image", "inspect", "--format", "{{.Id}}", r]))
    {
        let id = id.trim().to_string();
        if !id.is_empty() {
            return Some(id);
        }
    }
    let tar = crate::image_tar(dirs, r);
    tar_config_digest(&tar)
}

/// Reads `manifest.json` from a `docker save`-format tarball and returns `sha256:<config>`.
pub fn tar_config_digest(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut archive = tar::Archive::new(file);
    for entry in archive.entries().ok()? {
        let mut entry = entry.ok()?;
        let name = entry.path().ok()?.display().to_string();
        if name.trim_start_matches("./") == "manifest.json" {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut entry, &mut text).ok()?;
            let v: Value = serde_json::from_str(&text).ok()?;
            let cfg = v[0]["Config"].as_str()?;
            let hex = cfg
                .trim_start_matches("blobs/sha256/")
                .trim_start_matches("sha256:")
                .trim_end_matches(".json");
            return Some(format!("sha256:{hex}"));
        }
    }
    None
}

fn git_head(dest: &Path) -> Option<String> {
    capture(
        Command::new("git")
            .arg("-C")
            .arg(dest)
            .args(["rev-parse", "HEAD"]),
    )
    .map(|s| s.trim().to_string())
}

fn file_mode(p: &Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .ok()
        .map(|m| format!("{:o}", m.permissions().mode() & 0o7777))
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .map(|r| r.display().to_string())
        .unwrap_or_else(|_| p.display().to_string())
}

/// Observes the environment described by `plan` and returns the full manifest.
pub fn observe(plan: &Plan, dirs: &Dirs, spec_digest: &str) -> Value {
    let n = Normalizer::new(plan);
    let root = &plan.workspace_root;
    let mut tools = Vec::new();
    let mut nix = Vec::new();
    let mut packages = Vec::new();
    let mut deps = Vec::new();
    let mut images = BTreeMap::new();
    let mut hooks = Vec::new();
    let mut repos = Vec::new();
    let mut files = Vec::new();
    let mut services = Vec::new();
    let mut observed_services = BTreeMap::new();
    let mut env = BTreeMap::new();
    let installed = mise_versions(dirs);

    for step in &plan.steps {
        match &step.kind {
            StepKind::Packages { names } => {
                for (name, version) in crate::package_versions(names) {
                    packages.push(json!({"name": name, "version": version}));
                }
            }
            StepKind::Tools { tools: t } => {
                for (name, requested) in t {
                    tools.push(json!({
                        "name": name,
                        "requested": requested,
                        "versions": installed.get(name).cloned().unwrap_or_default(),
                    }));
                }
            }
            StepKind::Nix { refs } => nix.extend(refs.iter().cloned()),
            StepKind::Deps {
                manager,
                digest,
                manifest,
                ..
            } => deps.push(json!({
                "manifest": rel(&plan.project_root, manifest),
                "manager": manager,
                "inputs": digest,
            })),
            StepKind::Images { refs, .. } | StepKind::ImagesLoad { refs, .. } => {
                for r in refs {
                    images.insert(r.clone(), image_id(dirs, r));
                }
            }
            StepKind::Hook { run, .. } => hooks.push(json!({
                "id": step.id,
                "run": sha256_hex(n.apply(run).as_bytes()),
            })),
            StepKind::Env { .. } => {
                for (k, v) in &plan.env {
                    env.insert(k.clone(), sha256_hex(n.apply(v).as_bytes()));
                }
            }
            StepKind::Repo {
                url,
                dest,
                mode,
                git_ref,
            } => repos.push(json!({
                "path": rel(root, dest),
                "url": n.apply(url),
                "ref": git_ref,
                "mode": mode,
                "commit": match mode {
                    veneer_plan::RepoMode::Copy => None,
                    _ => git_head(dest),
                },
            })),
            StepKind::File { dest, .. } => {
                let content = std::fs::read(dest).ok();
                files.push(json!({
                    "path": n.apply(&rel(root, dest)),
                    "mode": file_mode(dest),
                    "sha256": content.map(|c| sha256_hex(n.apply(&String::from_utf8_lossy(&c)).as_bytes())),
                }));
            }
            StepKind::Compose {
                files: compose_files,
                generated,
                ..
            } => {
                let mut digests: Vec<String> = compose_files
                    .iter()
                    .map(|f| sha256_hex(&std::fs::read(f).unwrap_or_default()))
                    .collect();
                if let Some(g) = generated {
                    digests.push(sha256_hex(n.apply(g).as_bytes()));
                }
                services.push(json!({"name": "compose", "kind": "compose", "inputs": digests}));
            }
            StepKind::Native { name, run, .. } => {
                services.push(json!({
                    "name": name,
                    "kind": "native",
                    "run": sha256_hex(n.apply(run).as_bytes()),
                }));
                observed_services.insert(
                    name.clone(),
                    crate::services::native_running(dirs, plan, name),
                );
            }
        }
    }
    let images: Vec<Value> = images
        .into_iter()
        .map(|(r, id)| json!({"ref": r, "id": id}))
        .collect();
    let stable = json!({
        "spec": spec_digest,
        "platform": platform(),
        "os": os_release(),
        "packages": packages,
        "tools": tools,
        "nix": nix,
        "deps": deps,
        "images": images,
        "hooks": hooks,
        "env": env,
        "repos": repos,
        "files": files,
        "services": services,
    });
    let ports: BTreeMap<String, u16> = plan
        .steps
        .iter()
        .filter_map(|s| match &s.kind {
            StepKind::Native { name, port, .. } => Some((name.clone(), *port)),
            _ => None,
        })
        .collect();
    json!({
        "schema": SCHEMA,
        "veneer": env!("CARGO_PKG_VERSION"),
        "fingerprint": fingerprint(&stable),
        "stable": stable,
        "observed": {
            "workspace_id": plan.workspace_id,
            "workspace_root": plan.workspace_root,
            "created": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            "ports": ports,
            "services": observed_services,
        },
    })
}

/// Leaf-level differences between two JSON values, as `pointer: a → b` lines.
pub fn diff(a: &Value, b: &Value) -> Vec<String> {
    let mut out = Vec::new();
    diff_at("", a, b, &mut out);
    out
}

fn diff_at(ptr: &str, a: &Value, b: &Value, out: &mut Vec<String>) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                let p = format!("{ptr}/{k}");
                match (x.get(k), y.get(k)) {
                    (Some(l), Some(r)) => diff_at(&p, l, r, out),
                    (Some(l), None) => out.push(format!("{p}: {l} → (absent)")),
                    (None, Some(r)) => out.push(format!("{p}: (absent) → {r}")),
                    (None, None) => {}
                }
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            for i in 0..x.len().max(y.len()) {
                let p = format!("{ptr}/{i}");
                match (x.get(i), y.get(i)) {
                    (Some(l), Some(r)) => diff_at(&p, l, r, out),
                    (Some(l), None) => out.push(format!("{p}: {l} → (absent)")),
                    (None, Some(r)) => out.push(format!("{p}: (absent) → {r}")),
                    (None, None) => {}
                }
            }
        }
        _ if a != b => out.push(format!("{ptr}: {a} → {b}")),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jcs_sorts_keys_and_escapes() {
        let v = json!({"b": 1, "a": [true, null, "x\ny"], "\u{e000}": 1, "\u{1f600}": 2});
        // U+1F600 encodes as a surrogate pair (0xD83D…), which sorts before U+E000.
        assert_eq!(
            jcs(&v),
            "{\"a\":[true,null,\"x\\ny\"],\"b\":1,\"\u{1f600}\":2,\"\u{e000}\":1}"
        );
        assert_eq!(jcs(&json!(1.0)), "1");
    }

    #[test]
    fn bounded_replacement() {
        assert_eq!(
            replace_bounded("t1 http1 /x/t1/y", "t1", "ID"),
            "ID http1 /x/ID/y"
        );
        assert_eq!(
            replace_bounded("port=8080 18080", "8080", "P"),
            "port=P 18080"
        );
    }

    #[test]
    fn diff_lists_leaves() {
        let d = diff(&json!({"a": 1, "b": [1, 2]}), &json!({"a": 2, "b": [1]}));
        assert_eq!(d, vec!["/a: 1 → 2", "/b/1: 2 → (absent)"]);
    }
}
