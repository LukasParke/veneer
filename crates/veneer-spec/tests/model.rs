use serde_json::json;
use veneer_spec::{
    Hook, Node, Package, RepoMode, ServiceEntry, Spec, ToolVersion, codes, from_node,
};

fn spec(v: serde_json::Value) -> Result<Spec, veneer_spec::DeError> {
    from_node(&Node::from_json(&v))
}

#[test]
fn parses_the_full_design_example() {
    let s = spec(json!({
        "version": 1,
        "base": "ubuntu:24.04",
        "use": ["veneer/harness/claude-code@1", {"module": "x/y@2", "with": {"a": 1}}],
        "vars": {"python": "3.12"},
        "secrets": {
            "A": {"from": "env"},
            "B": {"from": "file", "path": "/run/secrets/b"},
            "C": {"from": "command", "run": "op read x"}
        },
        "env": {"DATABASE_URL": "postgres://x", "PORT": 5432, "DEBUG": true},
        "packages": ["git", {"from": "../Brewfile"}],
        "tools": {"node": 22, "python": "{{ vars.python }}", "from": "../mise.toml"},
        "deps": [{"from": "../package.json"}],
        "images": ["postgres:16"],
        "workspace": {
            "root": "/w",
            "repos": [{"url": "https://github.com/acme/app", "ref": "main", "path": "app", "mode": "worktree"}],
            "files": [{"src": "templates/.env.j2", "dest": "app/.env", "mode": "0600"}]
        },
        "services": {
            "db": {"image": "postgres:16", "env": {"POSTGRES_DB": "x"}, "healthcheck": {"cmd": "pg_isready", "interval": "1s", "retries": 30}},
            "worker": {"run": "python -m worker", "healthcheck": {"http": "http://localhost:8001/health"}},
            "from": "../compose.yaml"
        },
        "hooks": {
            "build": ["pip install -r r.txt", {"run": "make", "creates": "out"}],
            "workspace": [{"run": "make migrate", "unless": "make migrate-status"}]
        }
    }))
    .unwrap();
    assert_eq!(s.version, 1);
    assert_eq!(s.modules.len(), 2);
    assert!(matches!(s.packages[1], Package::Import(_)));
    assert_eq!(s.tools["node"], ToolVersion::Int(22));
    assert_eq!(
        s.workspace.as_ref().unwrap().repos[0].mode,
        RepoMode::Worktree
    );
    assert!(matches!(s.services["from"], ServiceEntry::Import(_)));
    assert!(matches!(s.hooks.build[0], Hook::Cmd(_)));
    assert!(s.validate().is_empty());
}

#[test]
fn minimal_spec_only_needs_version() {
    let s = spec(json!({"version": 1})).unwrap();
    assert!(s.packages.is_empty() && s.services.is_empty() && s.workspace.is_none());
}

#[test]
fn unknown_top_level_key_is_rejected() {
    let err = spec(json!({"version": 1, "pakages": ["git"]})).unwrap_err();
    assert_eq!(err.pointer.as_deref(), Some("/pakages"));
}

#[test]
fn validate_reports_version_secret_and_service_problems() {
    let s = spec(json!({
        "version": 2,
        "secrets": {"F": {"from": "file"}, "C": {"from": "command"}},
        "services": {"both": {"image": "x", "run": "y"}, "neither": {}}
    }))
    .unwrap();
    let got: Vec<(&str, String)> = s
        .validate()
        .into_iter()
        .map(|v| (v.code, v.pointer))
        .collect();
    assert_eq!(
        got,
        vec![
            (codes::VERSION, "/version".to_string()),
            (codes::SECRET_SOURCE, "/secrets/F".to_string()),
            (codes::SECRET_SOURCE, "/secrets/C".to_string()),
            (codes::SERVICE_KIND, "/services/both".to_string()),
            (codes::SERVICE_KIND, "/services/neither".to_string()),
        ]
    );
}
