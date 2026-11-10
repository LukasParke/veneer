use std::path::Path;
use std::process::Command;

use veneer_exec::{Events, Journal, Runner};
use veneer_plan::{Dirs, PlanOptions, Step, plan};
use veneer_resolve::{ResolveOptions, resolve};
use veneer_steps::{StepImpls, down, prune_files};

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn git(dir: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap()
            .success(),
        "git {args:?}"
    );
}

fn source_repo(root: &Path) -> std::path::PathBuf {
    let src = root.join("src-repo");
    std::fs::create_dir_all(&src).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    write(&src, "README.md", "hello\n");
    git(
        &src,
        &["-c", "user.email=t@t", "-c", "user.name=t", "add", "."],
    );
    git(
        &src,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "init",
        ],
    );
    src
}

fn run(root: &Path, spec: &str) -> (veneer_plan::Plan, Dirs, Journal, usize) {
    write(root, ".veneer/veneer.yaml", spec);
    let entry = root.join(".veneer/veneer.yaml");
    let r = resolve(&entry, &ResolveOptions::default());
    assert!(
        !veneer_spec::has_errors(&r.diagnostics),
        "{:?}",
        r.diagnostics
    );
    let dirs = Dirs::under(root.join("state"));
    let p = plan(
        r.spec.as_ref().unwrap(),
        r.tree.as_ref().unwrap(),
        &entry,
        &dirs,
        &PlanOptions {
            workspace_id: "t".into(),
            ..Default::default()
        },
    )
    .unwrap()
    .plan;
    let events = Events::new(true, p.secret_values.clone());
    let mut j = Journal::load(root.join("state/j.json")).unwrap();
    let steps: Vec<&Step> = p.steps.iter().collect();
    let n = Runner::new(&p, &dirs, &events)
        .run(&steps, &mut j, &StepImpls)
        .unwrap();
    prune_files(&p, &mut j.data.files);
    j.save().unwrap();
    (p, dirs, j, n)
}

#[test]
fn files_env_hooks_repos_and_services_converge() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let src = source_repo(&root);
    write(
        &root,
        ".veneer/t/greet.txt.j2",
        "hello {{ workspace.id }}\n",
    );
    write(&root, ".veneer/t/raw.txt", "{{ untouched }}\n");
    let spec = format!(
        r#"version: 1
env:
  GREETING: "hi {{{{ workspace.id }}}}"
workspace:
  root: "{root}/ws"
  repos:
    - {{ url: "{src}", path: clone, mode: clone }}
    - {{ url: "{src}", path: wt, mode: worktree, ref: main }}
    - {{ url: "{src}", path: copy, mode: copy }}
  files:
    - {{ src: t/greet.txt.j2, dest: greet.txt, mode: "0640" }}
    - {{ src: t/raw.txt, dest: sub/raw.txt }}
services:
  sleeper:
    run: "sleep 300"
    healthcheck: {{ cmd: "true", interval: 50ms, retries: 5 }}
hooks:
  workspace:
    - {{ run: "echo $GREETING > hook.out", creates: hook.out }}
    - {{ run: "echo again >> count.txt", unless: "test -f count.txt" }}
"#,
        root = root.display(),
        src = src.display()
    );
    let (p, dirs, _j, applied) = run(&root, &spec);
    assert_eq!(applied, p.steps.len());
    let ws = root.join("ws");
    assert_eq!(
        std::fs::read_to_string(ws.join("greet.txt")).unwrap(),
        "hello t\n"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("sub/raw.txt")).unwrap(),
        "{{ untouched }}\n"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("hook.out")).unwrap().trim(),
        "hi t"
    );
    assert!(
        ws.join("clone/README.md").is_file()
            && ws.join("wt/README.md").is_file()
            && ws.join("copy/README.md").is_file()
    );
    assert!(
        std::fs::read_to_string(dirs.workspace_state("t").join("env"))
            .unwrap()
            .contains("GREETING=hi t")
    );
    assert!(
        veneer_steps::service_status(&p, &dirs)
            .iter()
            .all(|(_, up)| *up)
    );

    // idempotent: second run applies nothing
    let (_, _, _, applied) = run(&root, &spec);
    assert_eq!(applied, 0);
    assert_eq!(
        std::fs::read_to_string(ws.join("count.txt")).unwrap(),
        "again\n"
    );

    // drift: edit a managed file -> rewritten; drop a file from the spec -> pruned
    std::fs::write(ws.join("greet.txt"), "tampered").unwrap();
    let pruned_spec = spec.replace("    - { src: t/raw.txt, dest: sub/raw.txt }\n", "");
    let (p2, dirs2, _, applied) = run(&root, &pruned_spec);
    assert_eq!(applied, 1);
    assert_eq!(
        std::fs::read_to_string(ws.join("greet.txt")).unwrap(),
        "hello t\n"
    );
    assert!(!ws.join("sub/raw.txt").exists());

    let mut log = veneer_exec::Log::create(root.join("down.log")).unwrap();
    down(&p2, &dirs2, &mut log).unwrap();
    assert!(
        veneer_steps::service_status(&p2, &dirs2)
            .iter()
            .all(|(_, up)| !*up)
    );
}
