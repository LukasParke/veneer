//! `extends: [../.devcontainer/devcontainer.json]` (design spec §4.4).
//!
//! Mapping:
//! - `image` → `base`
//! - `containerEnv`, `remoteEnv` → `env` (`${localEnv:X[:default]}`, `${containerEnv:X}`,
//!   `${containerWorkspaceFolder}`, `${localWorkspaceFolder}` become templates)
//! - `features` → `hooks.build` entries running `veneer internal feature <id>` (OCI or local)
//! - `onCreateCommand`, `updateContentCommand` → `hooks.build` (they run at prebuild time)
//! - `postCreateCommand`, `postStartCommand`, `postAttachCommand` → `hooks.workspace`
//! - `build`/`dockerFile`/`dockerComposeFile` → warning (W3002); other keys are ignored.

use std::path::Path;

use indexmap::IndexMap;
use veneer_spec::{Diagnostic, Kind, Node, Span};

pub fn is_devcontainer(path: &str) -> bool {
    path.ends_with("devcontainer.json")
}

/// Strips `//` and `/* */` comments and trailing commas (JSONC → JSON).
pub fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut in_str = false;
    while i < chars.len() {
        let c = chars[i];
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match (c, chars.get(i + 1)) {
            ('"', _) => {
                in_str = true;
                out.push(c);
                i += 1;
            }
            ('/', Some('/')) => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            ('/', Some('*')) => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
            }
            (',', _) => {
                let mut j = i + 1;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                if !matches!(chars.get(j), Some('}') | Some(']')) {
                    out.push(c);
                }
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Rewrites devcontainer `${…}` variables into veneer templates.
pub fn rewrite_vars(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let inner = &rest[start + 2..start + end];
        let replacement = if let Some(v) = inner
            .strip_prefix("localEnv:")
            .or_else(|| inner.strip_prefix("containerEnv:"))
        {
            match v.split_once(':') {
                Some((name, default)) => format!("{{{{ env.{name} | default(\"{default}\") }}}}"),
                None => format!("{{{{ env.{v} }}}}"),
            }
        } else if inner == "containerWorkspaceFolder" || inner == "localWorkspaceFolder" {
            "{{ workspace.root }}".to_string()
        } else if inner == "containerWorkspaceFolderBasename"
            || inner == "localWorkspaceFolderBasename"
        {
            "{{ workspace.id }}".to_string()
        } else {
            rest[start..start + end + 1].to_string()
        };
        out.push_str(&replacement);
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// devcontainer lifecycle command (string | array | object of either) → shell commands.
fn commands(v: &serde_json::Value) -> Vec<String> {
    match v {
        serde_json::Value::String(s) => vec![rewrite_vars(s)],
        serde_json::Value::Array(a) => {
            vec![
                a.iter()
                    .filter_map(|x| x.as_str())
                    .map(|x| shell_quote(&rewrite_vars(x)))
                    .collect::<Vec<_>>()
                    .join(" "),
            ]
        }
        serde_json::Value::Object(m) => m.values().flat_map(commands).collect(),
        _ => Vec::new(),
    }
}

fn str_node(s: String, span: &Option<Span>) -> Node {
    Node::with_span(Kind::Str(s), span.clone())
}

fn object(entries: Vec<(String, Node)>, span: &Option<Span>) -> Node {
    Node::with_span(
        Kind::Object(entries.into_iter().collect::<IndexMap<_, _>>()),
        span.clone(),
    )
}

fn scalar_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Converts a devcontainer.json into a veneer spec layer. Returns (layer, warnings).
pub fn to_node(path: &Path, uri: &str) -> Result<(Node, Vec<Diagnostic>), Diagnostic> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        Diagnostic::error(
            veneer_spec::codes::NOT_FOUND,
            format!("cannot read {}: {e}", path.display()),
        )
    })?;
    let json: serde_json::Value = serde_json::from_str(&strip_jsonc(&text)).map_err(|e| {
        Diagnostic::error(
            veneer_spec::codes::SYNTAX,
            format!("invalid devcontainer.json {}: {e}", path.display()),
        )
    })?;
    let span = Some(Span {
        uri: uri.to_string(),
        start: 0,
        end: 0,
        line: 1,
        col: 1,
    });
    let mut warnings = Vec::new();
    let mut top: Vec<(String, Node)> = vec![(
        "version".into(),
        Node::with_span(Kind::Int(1), span.clone()),
    )];

    if let Some(image) = json.get("image").and_then(|v| v.as_str()) {
        top.push(("base".into(), str_node(image.to_string(), &span)));
    }
    for key in ["build", "dockerFile", "dockerfile", "dockerComposeFile"] {
        if json.get(key).is_some() {
            warnings.push(
                Diagnostic::warning(
                    "W3002",
                    format!("devcontainer `{key}` is not supported; veneer uses `image` as `base`"),
                )
                .at(span.clone()),
            );
        }
    }
    let mut env = Vec::new();
    for key in ["containerEnv", "remoteEnv"] {
        if let Some(m) = json.get(key).and_then(|v| v.as_object()) {
            for (k, v) in m {
                env.push((k.clone(), str_node(rewrite_vars(&scalar_string(v)), &span)));
            }
        }
    }
    if !env.is_empty() {
        top.push(("env".into(), object(env, &span)));
    }

    let base_dir = path.parent().unwrap_or(Path::new("/"));
    let mut build_hooks = Vec::new();
    if let Some(features) = json.get("features").and_then(|v| v.as_object()) {
        for (id, opts) in features {
            let mut cmd = format!(
                "veneer internal feature {} --base {}",
                shell_quote(id),
                shell_quote(&base_dir.display().to_string())
            );
            match opts {
                serde_json::Value::Object(m) => {
                    for (k, v) in m {
                        cmd.push_str(&format!(
                            " --option {}",
                            shell_quote(&format!("{k}={}", scalar_string(v)))
                        ));
                    }
                }
                serde_json::Value::String(v) => cmd.push_str(&format!(
                    " --option {}",
                    shell_quote(&format!("version={v}"))
                )),
                _ => {}
            }
            build_hooks.push(str_node(cmd, &span));
        }
    }
    let cwd_prefix = |c: String| {
        format!(
            "cd {} && {c}",
            shell_quote(&base_dir.parent().unwrap_or(base_dir).display().to_string())
        )
    };
    for key in ["onCreateCommand", "updateContentCommand"] {
        if let Some(v) = json.get(key) {
            build_hooks.extend(
                commands(v)
                    .into_iter()
                    .map(|c| str_node(cwd_prefix(c), &span)),
            );
        }
    }
    let mut ws_hooks = Vec::new();
    for key in ["postCreateCommand", "postStartCommand", "postAttachCommand"] {
        if let Some(v) = json.get(key) {
            ws_hooks.extend(commands(v).into_iter().map(|c| str_node(c, &span)));
        }
    }
    let mut hooks = Vec::new();
    if !build_hooks.is_empty() {
        hooks.push((
            "build".into(),
            Node::with_span(Kind::Array(build_hooks), span.clone()),
        ));
    }
    if !ws_hooks.is_empty() {
        hooks.push((
            "workspace".into(),
            Node::with_span(Kind::Array(ws_hooks), span.clone()),
        ));
    }
    if !hooks.is_empty() {
        top.push(("hooks".into(), object(hooks, &span)));
    }
    Ok((object(top, &span), warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonc_and_vars() {
        let j = strip_jsonc("{\n // c\n \"a\": \"x // not a comment\", /* b */ \"b\": [1, 2,],\n}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&j).unwrap(),
            serde_json::json!({"a": "x // not a comment", "b": [1, 2]})
        );
        assert_eq!(rewrite_vars("${localEnv:HOME}/x"), "{{ env.HOME }}/x");
        assert_eq!(
            rewrite_vars("${localEnv:T:dflt}"),
            "{{ env.T | default(\"dflt\") }}"
        );
        assert_eq!(
            rewrite_vars("${containerWorkspaceFolder}/a ${other}"),
            "{{ workspace.root }}/a ${other}"
        );
    }

    #[test]
    fn maps_a_devcontainer() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("devcontainer.json");
        std::fs::write(
            &p,
            r#"{
  // a comment
  "image": "mcr.microsoft.com/devcontainers/base:ubuntu",
  "containerEnv": { "A": "1", "H": "${localEnv:HOME}" },
  "features": { "ghcr.io/devcontainers/features/node:1": { "version": "22" }, "./local": {} },
  "onCreateCommand": ["make", "setup x"],
  "postCreateCommand": { "a": "echo a", "b": "echo b" },
  "forwardPorts": [3000],
}"#,
        )
        .unwrap();
        let (node, warnings) = to_node(&p, "file:///x").unwrap();
        assert!(warnings.is_empty());
        let v = node.to_json();
        assert_eq!(v["base"], "mcr.microsoft.com/devcontainers/base:ubuntu");
        assert_eq!(v["env"]["H"], "{{ env.HOME }}");
        let build = v["hooks"]["build"].as_array().unwrap();
        assert!(
            build[0].as_str().unwrap().starts_with(
                "veneer internal feature ghcr.io/devcontainers/features/node:1 --base "
            )
        );
        assert!(build[0].as_str().unwrap().ends_with("--option version=22"));
        assert!(build[2].as_str().unwrap().ends_with("make 'setup x'"));
        assert_eq!(
            v["hooks"]["workspace"],
            serde_json::json!(["echo a", "echo b"])
        );
    }
}
