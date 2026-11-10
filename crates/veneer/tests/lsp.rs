use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

struct Lsp {
    child: std::process::Child,
    out: BufReader<std::process::ChildStdout>,
}

impl Lsp {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_veneer"))
            .arg("lsp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        Self { child, out }
    }

    fn send(&mut self, msg: Value) {
        let body = msg.to_string();
        let stdin = self.child.stdin.as_mut().unwrap();
        write!(stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        stdin.flush().unwrap();
    }

    fn recv(&mut self) -> Value {
        let mut len = 0;
        loop {
            let mut line = String::new();
            self.out.read_line(&mut line).unwrap();
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(v) = line.strip_prefix("Content-Length:") {
                len = v.trim().parse().unwrap();
            }
        }
        let mut buf = vec![0; len];
        self.out.read_exact(&mut buf).unwrap();
        serde_json::from_slice(&buf).unwrap()
    }

    /// Reads until a message satisfies `pred`.
    fn until(&mut self, pred: impl Fn(&Value) -> bool) -> Value {
        loop {
            let m = self.recv();
            if pred(&m) {
                return m;
            }
        }
    }
}

#[test]
fn diagnostics_hover_and_completion() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".veneer")).unwrap();
    let spec = root.join(".veneer/veneer.yaml");
    std::fs::write(&spec, "version: 1\n").unwrap();
    let uri = format!("file://{}", spec.display());

    let mut lsp = Lsp::start();
    lsp.send(
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"capabilities": {}}}),
    );
    let init = lsp.until(|m| m["id"] == 1);
    assert_eq!(init["result"]["capabilities"]["hoverProvider"], true);

    // Unsaved buffer with a typo -> V1009 at line 1 (0-based), not on disk.
    let bad = "version: 1\npakages: [git]\n";
    lsp.send(json!({"jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {"textDocument": {"uri": uri, "languageId": "yaml", "version": 1, "text": bad}}}));
    let diag = lsp.until(|m| {
        m["method"] == "textDocument/publishDiagnostics" && m["params"]["uri"] == uri.as_str()
    });
    let d = &diag["params"]["diagnostics"][0];
    assert_eq!(d["code"], "V1009", "{diag}");
    assert_eq!(d["range"]["start"]["line"], 1);
    assert_eq!(
        std::fs::read_to_string(&spec).unwrap(),
        "version: 1\n",
        "disk must be untouched"
    );

    // Fix it -> diagnostics cleared.
    lsp.send(json!({"jsonrpc": "2.0", "method": "textDocument/didChange", "params": {"textDocument": {"uri": uri, "version": 2}, "contentChanges": [{"text": "version: 1\npackages: [git]\nworkspace:\n  \n"}]}}));
    let diag = lsp.until(|m| {
        m["method"] == "textDocument/publishDiagnostics" && m["params"]["uri"] == uri.as_str()
    });
    assert_eq!(diag["params"]["diagnostics"], json!([]), "{diag}");

    // Hover on `packages`.
    lsp.send(json!({"jsonrpc": "2.0", "id": 2, "method": "textDocument/hover", "params": {"textDocument": {"uri": uri}, "position": {"line": 1, "character": 2}}}));
    let hover = lsp.until(|m| m["id"] == 2);
    assert!(
        hover["result"]["contents"]["value"]
            .as_str()
            .unwrap()
            .contains("OS packages"),
        "{hover}"
    );

    // Completion inside `workspace:` offers its keys.
    lsp.send(json!({"jsonrpc": "2.0", "id": 3, "method": "textDocument/completion", "params": {"textDocument": {"uri": uri}, "position": {"line": 3, "character": 2}}}));
    let comp = lsp.until(|m| m["id"] == 3);
    let labels: Vec<&str> = comp["result"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    assert!(
        labels.contains(&"repos") && labels.contains(&"files") && labels.contains(&"root"),
        "{labels:?}"
    );

    lsp.send(json!({"jsonrpc": "2.0", "id": 4, "method": "shutdown"}));
    lsp.until(|m| m["id"] == 4);
    lsp.send(json!({"jsonrpc": "2.0", "method": "exit"}));
    assert!(lsp.child.wait().unwrap().success());
}
