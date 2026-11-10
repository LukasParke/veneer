//! Harness configuration (standards proposal §5.8.2): one `mcp:` block rendered into each
//! enabled harness's own config file, and `CLAUDE.md → AGENTS.md` links.
//!
//! | harness     | file                                   | key                    | secrets            |
//! |-------------|----------------------------------------|------------------------|--------------------|
//! | claude-code | `~/.claude.json`                       | `mcpServers.<name>`    | literal, mode 0600 |
//! | codex       | `$CODEX_HOME/config.toml` (`~/.codex`) | `mcp_servers.<name>`   | literal, mode 0600 |
//! | pi          | `~/.pi/agent/mcp.json`                 | `mcpServers.<name>`    | `${VAR}`           |
//! | omp         | `~/.omp/agent/mcp.json`                | `mcpServers.<name>`    | `${VAR}`           |
//! | opencode    | `~/.config/opencode/opencode.json`     | `mcp.<name>`           | `{env:VAR}`        |
//!
//! Files are written at user level so repositories stay clean. Secret-bearing values for
//! env-reference harnesses go into the workspace environment as `VENEER_MCP_<SERVER>_<KEY>`
//! (exported by `veneer exec`), and the file holds only the reference.

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use crate::{ConfigFormat, StepKind};

/// A rendered MCP server.
#[derive(Debug, Clone, Default)]
pub struct Server {
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: IndexMap<String, String>,
    pub cwd: Option<String>,
    pub url: Option<String>,
    pub headers: IndexMap<String, String>,
}

fn env_name(server: &str, key: &str) -> String {
    let clean = |s: &str| {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    format!("VENEER_MCP_{}_{}", clean(server), clean(key))
}

enum Refs {
    Literal,
    Dollar,
    OpenCode,
}

/// Values carrying a secret become env references (harness-specific syntax).
fn value(
    v: &str,
    server: &str,
    key: &str,
    refs: &Refs,
    secrets: &[String],
    exports: &mut IndexMap<String, String>,
) -> String {
    let secret = secrets
        .iter()
        .any(|s| s.len() >= 3 && v.contains(s.as_str()));
    if !secret || matches!(refs, Refs::Literal) {
        return v.to_string();
    }
    let var = env_name(server, key);
    exports.insert(var.clone(), v.to_string());
    match refs {
        Refs::OpenCode => format!("{{env:{var}}}"),
        _ => format!("${{{var}}}"),
    }
}

fn map(
    m: &IndexMap<String, String>,
    server: &str,
    prefix: &str,
    refs: &Refs,
    secrets: &[String],
    exports: &mut IndexMap<String, String>,
) -> Value {
    Value::Object(
        m.iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    Value::String(value(
                        v,
                        server,
                        &format!("{prefix}_{k}"),
                        refs,
                        secrets,
                        exports,
                    )),
                )
            })
            .collect::<Map<_, _>>(),
    )
}

fn entry(
    h: &str,
    name: &str,
    s: &Server,
    secrets: &[String],
    exports: &mut IndexMap<String, String>,
) -> Value {
    let refs = match h {
        "claude-code" | "codex" => Refs::Literal,
        "opencode" => Refs::OpenCode,
        _ => Refs::Dollar,
    };
    let env = map(&s.env, name, "env", &refs, secrets, exports);
    let headers = map(&s.headers, name, "header", &refs, secrets, exports);
    let has = |v: &Value| v.as_object().is_some_and(|m| !m.is_empty());
    let mut o = Map::new();
    match (h, &s.command, &s.url) {
        ("opencode", Some(cmd), _) => {
            o.insert("type".into(), "local".into());
            let mut command = vec![Value::String(cmd.clone())];
            command.extend(s.args.iter().cloned().map(Value::String));
            o.insert("command".into(), Value::Array(command));
            if has(&env) {
                o.insert("environment".into(), env);
            }
            o.insert("enabled".into(), true.into());
        }
        ("opencode", None, Some(url)) => {
            o.insert("type".into(), "remote".into());
            o.insert("url".into(), url.clone().into());
            if has(&headers) {
                o.insert("headers".into(), headers);
            }
            o.insert("enabled".into(), true.into());
        }
        (_, Some(cmd), _) => {
            if h == "claude-code" {
                o.insert("type".into(), "stdio".into());
            }
            o.insert("command".into(), cmd.clone().into());
            o.insert("args".into(), json!(s.args));
            if has(&env) {
                o.insert("env".into(), env);
            }
            if let Some(c) = &s.cwd {
                o.insert("cwd".into(), c.clone().into());
            }
        }
        (_, None, Some(url)) => {
            if h != "codex" && h != "pi" {
                o.insert("type".into(), "http".into());
            }
            o.insert("url".into(), url.clone().into());
            if has(&headers) {
                o.insert(
                    if h == "codex" {
                        "http_headers"
                    } else {
                        "headers"
                    }
                    .into(),
                    headers,
                );
            }
        }
        _ => {}
    }
    Value::Object(o)
}

/// (file, format, key path prefix, file mode) for a harness under `home`.
fn target(
    h: &str,
    home: &Path,
    codex_home: Option<&str>,
) -> (PathBuf, ConfigFormat, &'static str, Option<u32>) {
    match h {
        "claude-code" => (
            home.join(".claude.json"),
            ConfigFormat::Json,
            "mcpServers",
            Some(0o600),
        ),
        "codex" => (
            codex_home
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex"))
                .join("config.toml"),
            ConfigFormat::Toml,
            "mcp_servers",
            Some(0o600),
        ),
        "pi" => (
            home.join(".pi/agent/mcp.json"),
            ConfigFormat::Json,
            "mcpServers",
            None,
        ),
        "omp" => (
            home.join(".omp/agent/mcp.json"),
            ConfigFormat::Json,
            "mcpServers",
            None,
        ),
        _ => (
            home.join(".config/opencode/opencode.json"),
            ConfigFormat::Json,
            "mcp",
            None,
        ),
    }
}

/// Steps (id, kind) for every enabled harness × MCP server, plus env exports.
pub fn steps(
    harnesses: &IndexMap<String, PathBuf>,
    servers: &IndexMap<String, Server>,
    secrets: &[String],
    codex_home: Option<&str>,
) -> (Vec<(String, StepKind)>, IndexMap<String, String>) {
    let mut out = Vec::new();
    let mut exports = IndexMap::new();
    for (h, home) in harnesses {
        let (dest, format, key, mode) = target(h, home, codex_home);
        for (name, s) in servers {
            out.push((
                format!("mcp:{h}:{name}"),
                StepKind::ConfigMerge {
                    dest: dest.clone(),
                    format,
                    path: vec![key.to_string(), name.clone()],
                    value: entry(h, name, s, secrets, &mut exports),
                    mode,
                },
            ));
        }
    }
    (out, exports)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_each_harness_format() {
        let servers = IndexMap::from([
            (
                "gh".to_string(),
                Server {
                    command: Some("github-mcp".into()),
                    args: vec!["stdio".into()],
                    env: IndexMap::from([("GITHUB_TOKEN".to_string(), "s3cr3t".to_string())]),
                    ..Default::default()
                },
            ),
            (
                "docs".to_string(),
                Server {
                    url: Some("https://x/mcp".into()),
                    headers: IndexMap::from([(
                        "Authorization".to_string(),
                        "Bearer s3cr3t".to_string(),
                    )]),
                    ..Default::default()
                },
            ),
        ]);
        let hs: IndexMap<String, PathBuf> = crate::HARNESS_NAMES
            .iter()
            .map(|h| (h.to_string(), PathBuf::from("/h")))
            .collect();
        let (steps, exports) = steps(&hs, &servers, &["s3cr3t".into()], None);
        let get = |id: &str| {
            steps
                .iter()
                .find(|(i, _)| i == id)
                .map(|(_, k)| k.clone())
                .unwrap()
        };
        let v = |k: StepKind| match k {
            StepKind::ConfigMerge { value, dest, .. } => (value, dest),
            _ => unreachable!(),
        };
        let (c, dest) = v(get("mcp:claude-code:gh"));
        assert_eq!(dest, PathBuf::from("/h/.claude.json"));
        assert_eq!(
            c,
            json!({"type": "stdio", "command": "github-mcp", "args": ["stdio"], "env": {"GITHUB_TOKEN": "s3cr3t"}})
        );
        let (o, dest) = v(get("mcp:opencode:gh"));
        assert_eq!(dest, PathBuf::from("/h/.config/opencode/opencode.json"));
        assert_eq!(
            o,
            json!({"type": "local", "command": ["github-mcp", "stdio"], "environment": {"GITHUB_TOKEN": "{env:VENEER_MCP_GH_ENV_GITHUB_TOKEN}"}, "enabled": true})
        );
        let (p, _) = v(get("mcp:pi:docs"));
        assert_eq!(
            p,
            json!({"url": "https://x/mcp", "headers": {"Authorization": "${VENEER_MCP_DOCS_HEADER_AUTHORIZATION}"}})
        );
        let (x, dest) = v(get("mcp:codex:docs"));
        assert_eq!(dest, PathBuf::from("/h/.codex/config.toml"));
        assert_eq!(x["http_headers"]["Authorization"], "Bearer s3cr3t");
        let (m, _) = v(get("mcp:omp:docs"));
        assert_eq!(m["type"], "http");
        assert_eq!(
            exports["VENEER_MCP_DOCS_HEADER_AUTHORIZATION"],
            "Bearer s3cr3t"
        );
    }
}
