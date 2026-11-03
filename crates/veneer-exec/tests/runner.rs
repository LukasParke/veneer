use std::cell::RefCell;
use std::collections::HashSet;

use veneer_exec::{
    Check, Cx, Events, Journal, Lock, Runner, Status, Steps, run_logged, write_atomic,
};
use veneer_plan::{Dirs, Phase, Plan, Step, StepKind};

fn plan(ids: &[&str]) -> Plan {
    Plan {
        workspace_id: "t".into(),
        workspace_root: "/tmp".into(),
        project_root: "/tmp".into(),
        env: Default::default(),
        secret_values: vec![],
        steps: ids
            .iter()
            .map(|id| Step {
                id: id.to_string(),
                group: "g".into(),
                phase: Phase::Workspace,
                kind: StepKind::Images { refs: vec![] },
                hash: format!("h-{id}"),
            })
            .collect(),
    }
}

#[derive(Default)]
struct Fake {
    satisfied: HashSet<String>,
    unknown: HashSet<String>,
    fail: HashSet<String>,
    applied: RefCell<Vec<String>>,
}

impl Steps for Fake {
    fn check(&self, s: &Step, _: &mut Cx<'_>) -> veneer_exec::Result<Check> {
        Ok(if self.satisfied.contains(&s.id) {
            Check::Satisfied
        } else if self.unknown.contains(&s.id) {
            Check::Unknown
        } else {
            Check::Needed("missing".into())
        })
    }
    fn apply(&self, s: &Step, cx: &mut Cx<'_>) -> veneer_exec::Result<()> {
        if self.fail.contains(&s.id) {
            cx.log.line("boom");
            return Err(veneer_exec::ExecError::failed("boom"));
        }
        self.applied.borrow_mut().push(s.id.clone());
        Ok(())
    }
    fn verify(&self, s: &Step, _: &mut Cx<'_>) -> veneer_exec::Result<String> {
        Ok(format!("fp-{}", s.id))
    }
}

#[test]
fn journal_and_checks_drive_skips() {
    let dir = tempfile::tempdir().unwrap();
    let dirs = Dirs::under(dir.path().to_path_buf());
    let p = plan(&["a", "b", "c"]);
    let events = Events::new(true, vec![]);
    let runner = Runner::new(&p, &dirs, &events);
    let steps: Vec<&Step> = p.steps.iter().collect();
    let jpath = dir.path().join("j.json");

    let fake = Fake {
        satisfied: ["a".into()].into(),
        unknown: ["b".into()].into(),
        ..Default::default()
    };
    let mut j = Journal::load(jpath.clone()).unwrap();
    assert_eq!(runner.run(&steps, &mut j, &fake).unwrap(), 2);
    assert_eq!(*fake.applied.borrow(), vec!["b", "c"]);
    assert_eq!(j.data.steps["a"].fingerprint, "fp-a");

    // second run: b trusts the journal, c's live check still says needed
    let fake2 = Fake {
        satisfied: ["a".into(), "c".into()].into(),
        unknown: ["b".into()].into(),
        ..Default::default()
    };
    let mut j = Journal::load(jpath.clone()).unwrap();
    assert_eq!(runner.run(&steps, &mut j, &fake2).unwrap(), 0);
    let d = runner.diff(&steps, &mut j, &fake2).unwrap();
    assert!(d.iter().all(|d| !d.run));
}

#[test]
fn failures_and_interruptions_are_redone() {
    let dir = tempfile::tempdir().unwrap();
    let dirs = Dirs::under(dir.path().to_path_buf());
    let p = plan(&["a", "b"]);
    let events = Events::new(true, vec![]);
    let runner = Runner::new(&p, &dirs, &events);
    let steps: Vec<&Step> = p.steps.iter().collect();
    let jpath = dir.path().join("j.json");

    let failing = Fake {
        unknown: ["a".into(), "b".into()].into(),
        fail: ["b".into()].into(),
        ..Default::default()
    };
    let mut j = Journal::load(jpath.clone()).unwrap();
    let err = runner.run(&steps, &mut j, &failing).unwrap_err();
    assert_eq!(err.exit, 3);
    assert_eq!(err.step.as_deref(), Some("b"));
    assert!(
        std::fs::read_to_string(err.log.unwrap())
            .unwrap()
            .contains("boom")
    );
    assert_eq!(
        Journal::load(jpath.clone()).unwrap().data.steps["b"].status,
        Status::Failed
    );

    // simulate a crash mid-step: journal says running
    let mut j = Journal::load(jpath.clone()).unwrap();
    j.set("a", Status::Running, "h-a", "").unwrap();
    let ok = Fake {
        unknown: ["a".into(), "b".into()].into(),
        ..Default::default()
    };
    let mut j = Journal::load(jpath).unwrap();
    runner.run(&steps, &mut j, &ok).unwrap();
    assert_eq!(*ok.applied.borrow(), vec!["a", "b"]);
}

#[test]
fn atomic_write_lock_and_logged_process() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("x/y.txt");
    write_atomic(&f, b"hello").unwrap();
    assert_eq!(std::fs::read(&f).unwrap(), b"hello");
    let _l = Lock::acquire(&dir.path().join("l.lock")).unwrap();
    let mut log = veneer_exec::Log::create(dir.path().join("log.txt")).unwrap();
    run_logged(
        std::process::Command::new("sh").args(["-c", "echo out; echo err >&2"]),
        &mut log,
    )
    .unwrap();
    let t = log.tail(10);
    assert!(t.contains("out") && t.contains("err"), "{t}");
    let e = run_logged(
        std::process::Command::new("sh").args(["-c", "exit 7"]),
        &mut log,
    )
    .unwrap_err();
    assert_eq!(e.exit, 3);
}

#[test]
fn events_redact_secrets() {
    let e = Events::new(false, vec!["s3cr3t".into(), "x".into()]);
    assert_eq!(e.redact("token=s3cr3t x"), "token=******** x");
}
