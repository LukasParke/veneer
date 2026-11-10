use std::path::{Path, PathBuf};

use veneer_plan::{Dirs, Phase, PlanOptions, StepKind, plan};
use veneer_resolve::{ResolveOptions, resolve};

fn write(root: &Path, files: &[(&str, &str)]) {
    for (rel, body) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
}

fn planned(root: &Path, opts: PlanOptions) -> Result<veneer_plan::Planned, veneer_plan::PlanError> {
    let entry = root.join(".veneer/veneer.yaml").canonicalize().unwrap();
    let r = resolve(&entry, &ResolveOptions::default());
    assert!(
        r.diagnostics
            .iter()
            .all(|d| d.severity != veneer_spec::Severity::Error),
        "{:?}",
        r.diagnostics
    );
    let dirs = Dirs::under(root.join("state"));
    plan(
        r.spec.as_ref().unwrap(),
        r.tree.as_ref().unwrap(),
        &entry,
        &dirs,
        &opts,
    )
}

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write(
        &root,
        &[
            (
                ".veneer/veneer.yaml",
                r#"version: 1
vars:
  greeting: "hi {{ workspace.id }}"
  tool_ver: "14.1.1"
env:
  GREETING: "{{ vars.greeting }}"
packages:
  - git
  - from: ./apt.txt
tools:
  from: ../.nvmrc
  "aqua:BurntSushi/ripgrep": "{{ vars.tool_ver }}"
deps:
  - from: ../package.json
workspace:
  root: "{{ vars.base }}/ws-{{ workspace.id }}"
  files:
    - { src: templates/hello.txt.j2, dest: hello.txt, mode: "0600" }
services:
  web:
    run: "python3 -m http.server $PORT"
    healthcheck: { http: "http://127.0.0.1:{{ workspace.ports.web }}/", interval: 200ms }
hooks:
  build: ["echo build"]
  workspace: [{ run: "touch done", creates: done }]
"#,
            ),
            (".veneer/apt.txt", "curl\n# comment\njq\n"),
            (
                ".veneer/templates/hello.txt.j2",
                "{{ vars.greeting }} from {{ os.family }}\n",
            ),
            (".nvmrc", "v22.11.0\n"),
            ("package.json", "{}"),
            ("package-lock.json", "{}"),
        ],
    );
    (dir, root)
}

#[test]
fn builds_ordered_steps_with_rendered_values() {
    let (_d, root) = fixture();
    let base = ("base".to_string(), root.join("roots").display().to_string());
    let p = planned(
        &root,
        PlanOptions {
            workspace_id: "w1".into(),
            sets: vec![base.clone()],
            ..Default::default()
        },
    )
    .unwrap()
    .plan;
    let ids: Vec<&str> = p.steps.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "packages",
            "tools",
            "deps:package.json",
            "hook.build:0",
            "env",
            "file:hello.txt",
            "service:web",
            "hook.workspace:0"
        ]
    );
    match &p.steps[0].kind {
        StepKind::Packages { names } => assert_eq!(names, &["git", "curl", "jq"]),
        k => panic!("{k:?}"),
    }
    match &p.steps[1].kind {
        StepKind::Tools { tools, .. } => {
            assert_eq!(tools["node"], "22.11.0");
            assert_eq!(tools["aqua:BurntSushi/ripgrep"], "14.1.1");
        }
        k => panic!("{k:?}"),
    }
    match &p.steps[2].kind {
        StepKind::Deps {
            manager, lockfile, ..
        } => {
            assert_eq!(*manager, veneer_plan::Manager::Npm);
            assert!(lockfile.is_some());
        }
        k => panic!("{k:?}"),
    }
    assert_eq!(p.workspace_root, root.join("roots/ws-w1"));
    assert_eq!(p.env["GREETING"], "hi w1");
    match &p.steps[5].kind {
        StepKind::File {
            content,
            mode,
            dest,
        } => {
            assert!(content.starts_with("hi w1 from "), "{content}");
            assert_eq!(*mode, Some(0o600));
            assert_eq!(dest, &root.join("roots/ws-w1/hello.txt"));
        }
        k => panic!("{k:?}"),
    }
    match &p.steps[6].kind {
        StepKind::Native {
            port,
            env,
            healthcheck,
            ..
        } => {
            assert!(*port > 0);
            assert_eq!(env["PORT"], port.to_string());
            assert_eq!(
                healthcheck.as_ref().unwrap().http.as_deref(),
                Some(format!("http://127.0.0.1:{port}/").as_str())
            );
            assert_eq!(healthcheck.as_ref().unwrap().interval_ms, 200);
        }
        k => panic!("{k:?}"),
    }
    // ports are stable across plans
    let again = planned(
        &root,
        PlanOptions {
            workspace_id: "w1".into(),
            sets: vec![base.clone()],
            ..Default::default()
        },
    )
    .unwrap()
    .plan;
    assert_eq!(again.steps[6].hash, p.steps[6].hash);
    assert!(p.steps.iter().take(4).all(|s| s.phase == Phase::Build));
}

#[test]
fn phase_filter_only_and_sets() {
    let (_d, root) = fixture();
    let base = ("base".to_string(), root.join("roots").display().to_string());
    let build = planned(
        &root,
        PlanOptions {
            workspace: false,
            sets: vec![base.clone()],
            ..Default::default()
        },
    )
    .unwrap()
    .plan;
    assert!(build.steps.iter().all(|s| s.phase == Phase::Build));
    let only = planned(
        &root,
        PlanOptions {
            workspace: false,
            only: Some("tools".into()),
            ..Default::default()
        },
    )
    .unwrap()
    .plan;
    assert_eq!(only.steps.len(), 1);
    let set = planned(
        &root,
        PlanOptions {
            sets: vec![base.clone(), ("tool_ver".into(), "15.0.0".into())],
            only: Some("tools".into()),
            ..Default::default()
        },
    )
    .unwrap()
    .plan;
    match &set.steps[0].kind {
        StepKind::Tools { tools, .. } => assert_eq!(tools["aqua:BurntSushi/ripgrep"], "15.0.0"),
        k => panic!("{k:?}"),
    }
}

#[test]
fn missing_secret_is_exit_4_only_for_workspace_phase() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write(
        &root,
        &[(
            ".veneer/veneer.yaml",
            "version: 1\nsecrets:\n  VENEER_NOPE_SECRET: { from: env }\nenv:\n  T: \"{{ secrets.VENEER_NOPE_SECRET }}\"\n",
        )],
    );
    let err = planned(&root, PlanOptions::default()).unwrap_err();
    assert_eq!(err.exit, 4);
    assert!(
        planned(
            &root,
            PlanOptions {
                workspace: false,
                ..Default::default()
            }
        )
        .is_ok()
    );
}
