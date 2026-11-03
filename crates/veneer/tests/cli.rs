use std::path::PathBuf;

use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;
use serde_json::{Value, json};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn veneer() -> Command {
    let mut cmd = Command::cargo_bin("veneer").unwrap();
    cmd.env("NO_COLOR", "1");
    cmd
}

#[test]
fn check_discovers_spec_from_a_subdirectory() {
    veneer()
        .current_dir(fixture("basic/app/src"))
        .arg("check")
        .assert()
        .success()
        .stdout(
            contains("ok ")
                .and(contains("veneer.yaml"))
                .and(contains("(3 documents, 0 warnings)")),
        );
}

#[test]
fn check_resolved_json_is_the_merged_spec() {
    let out = veneer()
        .current_dir(fixture("basic"))
        .args(["check", "--resolved", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        v,
        json!({
            "version": 1,
            "base": "ubuntu:24.04",
            "packages": ["git", "libpq-dev"],
            "env": {"LOG_LEVEL": "info", "DATABASE_URL": "postgres://localhost/{{ vars.db_name }}"},
            "vars": {"python": "3.12", "db_name": "bench_{{ workspace.id }}"},
            "tools": {"node": 22, "aqua:BurntSushi/ripgrep": "latest", "python": "{{ vars.python }}"},
            "services": {"db": {"image": "postgres:16", "env": {"POSTGRES_DB": "{{ vars.db_name }}"}}}
        })
    );
}

#[test]
fn check_applies_overlays() {
    let out = veneer()
        .current_dir(fixture("basic"))
        .args([
            "check",
            "--resolved",
            "--format",
            "json",
            "--overlay",
            ".veneer/overlays/bench.overlay.yaml",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["env"]["BENCH_MODE"], json!("1"));
    assert_eq!(v["services"], json!({}));
}

#[test]
fn check_resolved_yaml_snapshot() {
    let out = veneer()
        .current_dir(fixture("basic"))
        .args(["check", "--resolved"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    insta::assert_snapshot!("resolved_basic_yaml", String::from_utf8(out).unwrap());
}

#[test]
fn check_json_reports_cycle_and_exits_2() {
    let out = veneer()
        .current_dir(fixture("cycle"))
        .args(["check", "--json"])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["ok"], json!(false));
    assert_eq!(v["diagnostics"][0]["code"], json!("V1003"));
    assert_eq!(v["diagnostics"][0]["severity"], json!("error"));
}

#[test]
fn human_diagnostics_snapshot() {
    let out = veneer()
        .current_dir(fixture("unknown-key"))
        .arg("check")
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("V1009") && text.contains("pakages") && text.contains(".veneer/veneer.yaml"),
        "{text}"
    );
    insta::assert_snapshot!("unknown_key_human", text);
}

#[test]
fn missing_spec_is_v1012() {
    let dir = tempfile::tempdir().unwrap();
    veneer()
        .current_dir(dir.path())
        .arg("check")
        .assert()
        .code(2)
        .stderr(contains("V1012"));
}

#[test]
fn schema_prints_json_schema() {
    let out = veneer()
        .arg("schema")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["additionalProperties"], json!(false));
}

#[test]
fn init_scaffolds_from_existing_files_and_checks_clean() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join(".tool-versions"), "jq 1.7.1\n").unwrap();
    std::fs::write(root.join("package.json"), "{}").unwrap();
    std::fs::write(root.join("compose.yaml"), "services:\n  db:\n    image: postgres:16\n").unwrap();
    veneer().current_dir(root).arg("init").assert().success().stderr(contains(".tool-versions"));
    let spec = std::fs::read_to_string(root.join(".veneer/veneer.yaml")).unwrap();
    assert!(spec.contains("from: ../.tool-versions") && spec.contains("from: ../package.json") && spec.contains("from: ../compose.yaml"), "{spec}");
    veneer().current_dir(root).arg("check").assert().success();
    veneer().current_dir(root).arg("init").assert().code(2);
}
