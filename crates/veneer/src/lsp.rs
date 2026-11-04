//! `veneer lsp`: a Language Server Protocol server over stdio (spec §8.1 "later `veneer lsp`").
//!
//! - Diagnostics: every open/change/save re-resolves the project's entry spec (unsaved
//!   buffers included) and publishes source-located diagnostics for every document.
//! - Hover: the JSON Schema description of the key under the cursor.
//! - Completion: keys allowed at the cursor's mapping level, from the JSON Schema.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Value, json};
use veneer_resolve::{ResolveOptions, resolve};
use veneer_spec::{Diagnostic, Severity};

fn read_message(r: &mut impl BufRead) -> Option<Value> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            len = v.trim().parse::<usize>().ok();
        }
    }
    let mut buf = vec![0; len?];
    r.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

fn send(w: &mut impl Write, msg: &Value) {
    let body = msg.to_string();
    let _ = write!(w, "Content-Length: {}\r\n\r\n{body}", body.len());
    let _ = w.flush();
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    suspect_source::Uri::parse(uri).ok()?.as_path()
}

fn path_to_uri(p: &Path) -> String {
    suspect_source::Uri::from_path(p)
        .map(|u| u.to_string())
        .unwrap_or_default()
}

/// (line, character) of a byte offset, both 0-based (UTF-16 not needed for ASCII specs;
/// non-ASCII counts chars, which matches for the BMP).
fn position(text: &str, offset: usize) -> (u32, u32) {
    let offset = offset.min(text.len());
    let before = &text[..offset];
    let line = before.matches('\n').count() as u32;
    let col = before
        .rsplit('\n')
        .next()
        .unwrap_or_default()
        .chars()
        .count() as u32;
    (line, col)
}

struct Server {
    /// Unsaved buffers by path.
    open: HashMap<PathBuf, String>,
    /// Documents we published diagnostics for last time (to clear them).
    published: HashSet<String>,
    schema: Value,
}

impl Server {
    fn text(&self, path: &Path) -> String {
        self.open
            .get(path)
            .cloned()
            .unwrap_or_else(|| std::fs::read_to_string(path).unwrap_or_default())
    }

    /// Resolves the entry for `changed`, with unsaved buffers swapped in via shadow files.
    fn diagnose(&mut self, changed: &Path, out: &mut impl Write) {
        let dir = changed.parent().unwrap_or(Path::new("/"));
        let entry = veneer_resolve::locate(None, dir)
            .ok()
            .filter(|e| changed.starts_with(e.parent().unwrap_or(Path::new("/"))) || e == changed)
            .unwrap_or_else(|| changed.to_path_buf());
        // Shadow every dirty buffer under the entry's directory tree: write the buffer over the
        // file, resolve, then restore the on-disk bytes. Diagnostics keep the real paths.
        let mut restore: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::new();
        for (p, text) in &self.open {
            let disk = std::fs::read(p).ok();
            if disk.as_deref() != Some(text.as_bytes()) {
                restore.push((p.clone(), disk));
                let _ = std::fs::write(p, text);
            }
        }
        let cache = veneer_plan::Dirs::from_env().cache;
        let resolved = resolve(
            &entry,
            &ResolveOptions {
                remote_cache: Some(cache),
                ..Default::default()
            },
        );
        for (p, disk) in restore {
            match disk {
                Some(bytes) => {
                    let _ = std::fs::write(&p, bytes);
                }
                None => {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        let mut by_uri: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for d in &resolved.documents {
            by_uri.entry(d.clone()).or_default();
        }
        by_uri.entry(path_to_uri(changed)).or_default();
        for d in &resolved.diagnostics {
            let uri = d
                .span
                .as_ref()
                .map(|s| s.uri.clone())
                .unwrap_or_else(|| path_to_uri(&entry));
            let text = uri_to_path(&uri).map(|p| self.text(&p)).unwrap_or_default();
            by_uri
                .entry(uri)
                .or_default()
                .push(lsp_diagnostic(d, &text));
        }
        let now: HashSet<String> = by_uri.keys().cloned().collect();
        for stale in self.published.difference(&now) {
            by_uri.insert(stale.clone(), Vec::new());
        }
        for (uri, diags) in by_uri {
            send(
                out,
                &json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": uri, "diagnostics": diags}}),
            );
        }
        self.published = now;
    }

    /// Path of mapping keys from the root to the line containing `offset` (by indentation).
    fn key_path(text: &str, line: u32) -> (Vec<String>, usize) {
        let lines: Vec<&str> = text.lines().collect();
        let indent = |l: &str| l.len() - l.trim_start().len();
        let cur = lines.get(line as usize).copied().unwrap_or_default();
        let mut level = indent(cur);
        let mut path = Vec::new();
        for l in lines[..(line as usize).min(lines.len())].iter().rev() {
            let t = l.trim_start();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            let i = indent(l);
            if i < level {
                if let Some((k, _)) = t.trim_start_matches("- ").split_once(':') {
                    path.push(k.trim().trim_matches('"').to_string());
                }
                level = i;
            }
        }
        path.reverse();
        (path, indent(cur))
    }

    /// The JSON Schema node for a key path (follows `$ref`, `anyOf` objects, array `items`,
    /// and map `additionalProperties`).
    fn schema_at(&self, path: &[String]) -> Option<Value> {
        let defs = &self.schema["$defs"];
        let deref = |v: &Value| -> Value {
            v.get("$ref")
                .and_then(Value::as_str)
                .and_then(|r| r.rsplit('/').next())
                .map(|n| defs[n].clone())
                .unwrap_or_else(|| v.clone())
        };
        let mut node = self.schema.clone();
        for key in path {
            let props = node.get("properties").cloned().or_else(|| {
                node["anyOf"].as_array().and_then(|a| {
                    a.iter()
                        .map(&deref)
                        .find_map(|x| x.get("properties").cloned())
                })
            });
            let next = props
                .as_ref()
                .and_then(|p| p.get(key))
                .cloned()
                .or_else(|| {
                    node.get("additionalProperties")
                        .filter(|v| v.is_object())
                        .cloned()
                })?;
            let mut owned = deref(&next);
            if let Some(items) = owned.get("items").cloned() {
                owned = deref(&items);
            }
            if let Some(any) = owned.get("anyOf").and_then(Value::as_array).cloned()
                && let Some(obj) = any
                    .iter()
                    .map(&deref)
                    .find(|a| a.get("properties").is_some())
            {
                owned = obj;
            }
            node = owned;
        }
        Some(node)
    }

    fn hover(&self, path: &Path, line: u32, character: u32) -> Value {
        let text = self.text(path);
        let l = text.lines().nth(line as usize).unwrap_or_default();
        let Some((key, _)) = l.trim_start().trim_start_matches("- ").split_once(':') else {
            return Value::Null;
        };
        let key = key.trim().trim_matches('"').to_string();
        let start = l.find(&key).unwrap_or(0) as u32;
        if character < start || character > start + key.len() as u32 {
            return Value::Null;
        }
        let (parents, _) = Self::key_path(&text, line);
        // Read the property's own description (before descending into array items).
        let Some(desc) = self.schema_at(&parents).and_then(|parent| {
            let props = parent.get("properties").cloned().or_else(|| {
                parent["anyOf"]
                    .as_array()
                    .and_then(|a| a.iter().find_map(|x| x.get("properties").cloned()))
            })?;
            let prop = props.get(&key)?;
            let defs = &self.schema["$defs"];
            prop.get("description")
                .or_else(|| {
                    let name = prop.get("$ref")?.as_str()?.rsplit('/').next()?;
                    defs[name].get("description")
                })
                .and_then(Value::as_str)
                .map(str::to_string)
        }) else {
            return Value::Null;
        };
        json!({"contents": {"kind": "markdown", "value": format!("**{key}**\n\n{desc}")}})
    }

    fn completion(&self, path: &Path, line: u32) -> Value {
        let text = self.text(path);
        let (parents, _) = Self::key_path(&text, line);
        let Some(node) = self.schema_at(&parents) else {
            return json!([]);
        };
        let items: Vec<Value> = node
            .get("properties")
            .and_then(Value::as_object)
            .map(|props| {
                props
                    .iter()
                    .map(|(k, v)| {
                        json!({"label": k, "kind": 10, "insertText": format!("{k}: "), "documentation": v.get("description").cloned().unwrap_or(Value::Null)})
                    })
                    .collect()
            })
            .unwrap_or_default();
        json!(items)
    }
}

fn lsp_diagnostic(d: &Diagnostic, text: &str) -> Value {
    let (start, end) = match &d.span {
        Some(s) => (position(text, s.start), position(text, s.end.max(s.start))),
        None => ((0, 0), (0, 0)),
    };
    let message = if d.notes.is_empty() {
        d.message.clone()
    } else {
        format!("{}\n{}", d.message, d.notes.join("\n"))
    };
    json!({
        "range": {"start": {"line": start.0, "character": start.1}, "end": {"line": end.0, "character": end.1}},
        "severity": if d.severity == Severity::Error { 1 } else { 2 },
        "code": d.code,
        "source": "veneer",
        "message": message,
    })
}

pub fn run() -> ExitCode {
    let stdin = std::io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut server = Server {
        open: HashMap::new(),
        published: HashSet::new(),
        schema: serde_json::from_str(&veneer_spec::json_schema_string()).unwrap_or_default(),
    };
    let mut shutdown = false;
    while let Some(msg) = read_message(&mut input) {
        let method = msg["method"].as_str().unwrap_or_default().to_string();
        let id = msg.get("id").cloned();
        let params = &msg["params"];
        let doc_path = || params["textDocument"]["uri"].as_str().and_then(uri_to_path);
        let result = match method.as_str() {
            "initialize" => Some(json!({
                "capabilities": {
                    "textDocumentSync": {"openClose": true, "change": 1, "save": {"includeText": false}},
                    "hoverProvider": true,
                    "completionProvider": {"triggerCharacters": []},
                },
                "serverInfo": {"name": "veneer", "version": env!("CARGO_PKG_VERSION")},
            })),
            "shutdown" => {
                shutdown = true;
                Some(Value::Null)
            }
            "exit" => {
                return if shutdown {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(1)
                };
            }
            "textDocument/didOpen" => {
                if let Some(p) = doc_path() {
                    server.open.insert(
                        p.clone(),
                        params["textDocument"]["text"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                    );
                    server.diagnose(&p, &mut out);
                }
                None
            }
            "textDocument/didChange" => {
                if let (Some(p), Some(text)) = (
                    doc_path(),
                    params["contentChanges"]
                        .as_array()
                        .and_then(|c| c.last())
                        .and_then(|c| c["text"].as_str()),
                ) {
                    server.open.insert(p.clone(), text.to_string());
                    server.diagnose(&p, &mut out);
                }
                None
            }
            "textDocument/didSave" => {
                if let Some(p) = doc_path() {
                    server.diagnose(&p, &mut out);
                }
                None
            }
            "textDocument/didClose" => {
                if let Some(p) = doc_path() {
                    server.open.remove(&p);
                }
                None
            }
            "textDocument/hover" => doc_path().map(|p| {
                server.hover(
                    &p,
                    params["position"]["line"].as_u64().unwrap_or(0) as u32,
                    params["position"]["character"].as_u64().unwrap_or(0) as u32,
                )
            }),
            "textDocument/completion" => doc_path().map(|p| {
                server.completion(&p, params["position"]["line"].as_u64().unwrap_or(0) as u32)
            }),
            _ => id.as_ref().map(|_| Value::Null),
        };
        if let (Some(id), Some(result)) = (id, result) {
            send(
                &mut out,
                &json!({"jsonrpc": "2.0", "id": id, "result": result}),
            );
        }
    }
    ExitCode::SUCCESS
}
