//! veneer-exec: the idempotent, interrupt-safe runner (design spec §6).
//!
//! Per step: live `check` → (`apply` → `verify`) with a write-ahead journal. A step is
//! stamped `done` only after `verify` passes; a step found `running` or `interrupted` is
//! re-checked and redone.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use veneer_plan::{Dirs, Phase, Plan, Step};

// ---------------------------------------------------------------- errors

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecError {
    /// Process exit code class: 1 internal, 3 step failed, 130 interrupted.
    pub exit: u8,
    pub step: Option<String>,
    pub message: String,
    pub log: Option<PathBuf>,
}

impl ExecError {
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            exit: 1,
            step: None,
            message: message.into(),
            log: None,
        }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            exit: 3,
            step: None,
            message: message.into(),
            log: None,
        }
    }

    pub fn interrupted() -> Self {
        Self {
            exit: 130,
            step: None,
            message: "interrupted".into(),
            log: None,
        }
    }
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub type Result<T> = std::result::Result<T, ExecError>;

// ---------------------------------------------------------------- signals

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static CHILD_PGID: AtomicI32 = AtomicI32::new(0);

/// Installs SIGINT/SIGTERM handlers that set a flag and forward SIGTERM to the running
/// child's process group. Idempotent.
pub fn install_signal_handlers() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
            // SAFETY: the handler only touches atomics and calls kill(2), which is
            // async-signal-safe.
            let _ = unsafe {
                signal_hook::low_level::register(sig, || {
                    INTERRUPTED.store(true, Ordering::SeqCst);
                    let pg = CHILD_PGID.load(Ordering::SeqCst);
                    if pg > 0 {
                        libc::kill(-pg, libc::SIGTERM);
                    }
                })
            };
        }
    });
}

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------- atomic files

/// Writes `bytes` to `path` atomically: temp file in the same dir → fsync → rename → fsync dir.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

// ---------------------------------------------------------------- locks

/// An exclusive `flock`, released when dropped (or by the kernel when the process dies).
pub struct Lock {
    _file: File,
}

impl Lock {
    pub fn acquire(path: &Path) -> Result<Self> {
        use fs4::fs_std::FileExt;
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))
            .map_err(|e| ExecError::internal(format!("cannot create {}: {e}", path.display())))?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(|e| {
                ExecError::internal(format!("cannot open lock {}: {e}", path.display()))
            })?;
        file.lock_exclusive()
            .map_err(|e| ExecError::internal(format!("cannot lock {}: {e}", path.display())))?;
        Ok(Self { _file: file })
    }
}

// ---------------------------------------------------------------- journal

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Running,
    Done,
    Interrupted,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub status: Status,
    pub hash: String,
    #[serde(default)]
    pub fingerprint: String,
    pub updated: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalData {
    #[serde(default)]
    pub steps: BTreeMap<String, Entry>,
    /// Ownership manifest: files veneer wrote (path → blake3 of content).
    #[serde(default)]
    pub files: BTreeMap<String, String>,
}

pub struct Journal {
    path: PathBuf,
    pub data: JournalData,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Journal {
    pub fn load(path: PathBuf) -> Result<Self> {
        let data = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                ExecError::internal(format!("corrupt journal {}: {e}", path.display()))
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => JournalData::default(),
            Err(e) => {
                return Err(ExecError::internal(format!(
                    "cannot read {}: {e}",
                    path.display()
                )));
            }
        };
        Ok(Self { path, data })
    }

    pub fn save(&self) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&self.data).expect("journal serializes");
        write_atomic(&self.path, &bytes)
            .map_err(|e| ExecError::internal(format!("cannot write {}: {e}", self.path.display())))
    }

    pub fn set(&mut self, id: &str, status: Status, hash: &str, fingerprint: &str) -> Result<()> {
        self.data.steps.insert(
            id.to_string(),
            Entry {
                status,
                hash: hash.to_string(),
                fingerprint: fingerprint.to_string(),
                updated: now(),
            },
        );
        self.save()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub fn build_journal_path(dirs: &Dirs) -> PathBuf {
    dirs.state.join("build.json")
}

pub fn workspace_journal_path(dirs: &Dirs, id: &str) -> PathBuf {
    dirs.workspace_state(id).join("journal.json")
}

// ---------------------------------------------------------------- events

/// Progress output: human lines on stderr, or NDJSON on stdout. Secret values are masked.
pub struct Events {
    json: bool,
    secrets: Vec<String>,
}

impl Events {
    pub fn new(json: bool, secrets: Vec<String>) -> Self {
        let secrets = secrets.into_iter().filter(|s| s.len() >= 3).collect();
        Self { json, secrets }
    }

    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for s in &self.secrets {
            out = out.replace(s.as_str(), "********");
        }
        out
    }

    pub fn emit(&self, event: &str, step: Option<&Step>, fields: serde_json::Value, human: &str) {
        if self.json {
            let mut obj = serde_json::Map::new();
            obj.insert("event".into(), event.into());
            if let Some(s) = step {
                obj.insert("step".into(), s.id.clone().into());
                obj.insert("phase".into(), s.phase.as_str().into());
            }
            if let serde_json::Value::Object(extra) = fields {
                obj.extend(extra);
            }
            let line = self.redact(&serde_json::Value::Object(obj).to_string());
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        } else if !human.is_empty() {
            eprintln!("{}", self.redact(human));
        }
    }
}

// ---------------------------------------------------------------- logs + processes

pub struct Log {
    pub path: PathBuf,
    file: File,
}

impl Log {
    pub fn create(path: PathBuf) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| {
                ExecError::internal(format!("cannot create {}: {e}", dir.display()))
            })?;
        }
        let file = File::create(&path)
            .map_err(|e| ExecError::internal(format!("cannot create {}: {e}", path.display())))?;
        Ok(Self { path, file })
    }

    pub fn line(&mut self, text: &str) {
        let _ = writeln!(self.file, "{text}");
    }

    /// The last `n` lines of the log.
    pub fn tail(&self, n: usize) -> String {
        let text = std::fs::read_to_string(&self.path).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(n)..].join("\n")
    }
}

/// Runs `cmd` in its own process group with output appended to `log`. On SIGINT/SIGTERM the
/// group gets SIGTERM, then SIGKILL after 10 s.
pub fn run_logged(cmd: &mut Command, log: &mut Log) -> Result<()> {
    use std::os::unix::process::CommandExt;
    if interrupted() {
        return Err(ExecError::interrupted());
    }
    log.line(&format!("$ {:?}", cmd));
    let out = log
        .file
        .try_clone()
        .map_err(|e| ExecError::internal(e.to_string()))?;
    let err = log
        .file
        .try_clone()
        .map_err(|e| ExecError::internal(e.to_string()))?;
    cmd.stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|e| ExecError::failed(format!("cannot start {:?}: {e}", cmd.get_program())))?;
    let pgid = child.id() as i32;
    CHILD_PGID.store(pgid, Ordering::SeqCst);
    let mut interrupted_at: Option<Instant> = None;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                CHILD_PGID.store(0, Ordering::SeqCst);
                return Err(ExecError::internal(e.to_string()));
            }
        }
        if interrupted() {
            let at = *interrupted_at.get_or_insert_with(Instant::now);
            if at.elapsed() > Duration::from_secs(10) {
                // SAFETY: kill(2) on our own child's process group.
                unsafe { libc::kill(-pgid, libc::SIGKILL) };
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    CHILD_PGID.store(0, Ordering::SeqCst);
    if interrupted() {
        return Err(ExecError::interrupted());
    }
    if status.success() {
        Ok(())
    } else {
        log.line(&format!("exited with {status}"));
        Err(ExecError::failed(format!(
            "{:?} exited with {status}",
            cmd.get_program()
        )))
    }
}

// ---------------------------------------------------------------- runner

/// Live-state verdict from a step's `check`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// The desired state is present; skip regardless of the journal.
    Satisfied,
    /// Must apply, with a reason.
    Needed(String),
    /// No cheap live check; trust the journal.
    Unknown,
}

pub struct Cx<'a> {
    pub plan: &'a Plan,
    pub dirs: &'a Dirs,
    pub log: &'a mut Log,
    /// Ownership manifest of the journal this step belongs to.
    pub files: &'a mut BTreeMap<String, String>,
}

/// Implemented by `veneer-steps`.
pub trait Steps {
    fn check(&self, step: &Step, cx: &mut Cx<'_>) -> Result<Check>;
    fn apply(&self, step: &Step, cx: &mut Cx<'_>) -> Result<()>;
    /// Returns a fingerprint of the resulting state.
    fn verify(&self, step: &Step, cx: &mut Cx<'_>) -> Result<String>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Decision {
    pub step: String,
    pub phase: Phase,
    pub run: bool,
    pub reason: String,
}

fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn journal_reason(entry: Option<&Entry>, step: &Step) -> Option<String> {
    match entry {
        None => Some("never applied".into()),
        Some(e) if e.hash != step.hash => Some("inputs changed".into()),
        Some(e) if e.status == Status::Running || e.status == Status::Interrupted => {
            Some("previous run was interrupted".into())
        }
        Some(e) if e.status == Status::Failed => Some("previous run failed".into()),
        Some(_) => None,
    }
}

pub struct Runner<'a> {
    pub plan: &'a Plan,
    pub dirs: &'a Dirs,
    pub events: &'a Events,
    pub run_id: String,
}

impl<'a> Runner<'a> {
    pub fn new(plan: &'a Plan, dirs: &'a Dirs, events: &'a Events) -> Self {
        let run_id = format!("{}-{}", now(), std::process::id());
        Self {
            plan,
            dirs,
            events,
            run_id,
        }
    }

    fn log_for(&self, step: &Step) -> Result<Log> {
        Log::create(
            self.dirs
                .state
                .join("logs")
                .join(&self.run_id)
                .join(format!("{}.log", sanitize(&step.id))),
        )
    }

    /// What would run and why, without applying anything.
    pub fn diff(
        &self,
        steps: &[&Step],
        journal: &mut Journal,
        impls: &dyn Steps,
    ) -> Result<Vec<Decision>> {
        let mut out = Vec::new();
        for step in steps {
            let mut log = self.log_for(step)?;
            let mut cx = Cx {
                plan: self.plan,
                dirs: self.dirs,
                log: &mut log,
                files: &mut journal.data.files,
            };
            let (run, reason) = match impls.check(step, &mut cx)? {
                Check::Satisfied => (false, "up to date".to_string()),
                Check::Needed(why) => (true, why),
                Check::Unknown => match journal_reason(journal.data.steps.get(&step.id), step) {
                    Some(why) => (true, why),
                    None => (false, "up to date".to_string()),
                },
            };
            out.push(Decision {
                step: step.id.clone(),
                phase: step.phase,
                run,
                reason,
            });
        }
        Ok(out)
    }

    /// Runs `steps` against `journal`. Stops at the first failure.
    pub fn run(&self, steps: &[&Step], journal: &mut Journal, impls: &dyn Steps) -> Result<usize> {
        let mut applied = 0;
        for step in steps {
            if interrupted() {
                return Err(ExecError::interrupted());
            }
            let mut log = self.log_for(step)?;
            let started = Instant::now();
            let verdict = {
                let mut cx = Cx {
                    plan: self.plan,
                    dirs: self.dirs,
                    log: &mut log,
                    files: &mut journal.data.files,
                };
                impls
                    .check(step, &mut cx)
                    .map_err(|e| with_step(e, step, &log_path(&log)))?
            };
            let reason = match verdict {
                Check::Satisfied => None,
                Check::Needed(why) => Some(why),
                Check::Unknown => journal_reason(journal.data.steps.get(&step.id), step),
            };
            let Some(reason) = reason else {
                let fp = journal
                    .data
                    .steps
                    .get(&step.id)
                    .map(|e| e.fingerprint.clone())
                    .unwrap_or_default();
                let fp = if fp.is_empty()
                    || journal
                        .data
                        .steps
                        .get(&step.id)
                        .is_none_or(|e| e.hash != step.hash)
                {
                    let mut cx = Cx {
                        plan: self.plan,
                        dirs: self.dirs,
                        log: &mut log,
                        files: &mut journal.data.files,
                    };
                    impls.verify(step, &mut cx).unwrap_or_default()
                } else {
                    fp
                };
                journal.set(&step.id, Status::Done, &step.hash, &fp)?;
                self.events.emit(
                    "step.skip",
                    Some(step),
                    serde_json::json!({"reason": "up to date"}),
                    &format!("· {} (up to date)", step.id),
                );
                continue;
            };
            self.events.emit(
                "step.start",
                Some(step),
                serde_json::json!({"reason": reason}),
                &format!("▸ {} ({reason})", step.id),
            );
            journal.set(&step.id, Status::Running, &step.hash, "")?;
            let result = {
                let mut cx = Cx {
                    plan: self.plan,
                    dirs: self.dirs,
                    log: &mut log,
                    files: &mut journal.data.files,
                };
                impls
                    .apply(step, &mut cx)
                    .and_then(|_| impls.verify(step, &mut cx))
            };
            match result {
                Ok(fp) => {
                    journal.set(&step.id, Status::Done, &step.hash, &fp)?;
                    applied += 1;
                    let ms = started.elapsed().as_millis() as u64;
                    self.events.emit(
                        "step.done",
                        Some(step),
                        serde_json::json!({"duration_ms": ms}),
                        &format!("✓ {} ({:.1}s)", step.id, ms as f64 / 1000.0),
                    );
                }
                Err(e) => {
                    let status = if e.exit == 130 {
                        Status::Interrupted
                    } else {
                        Status::Failed
                    };
                    journal.set(&step.id, status, &step.hash, "")?;
                    let e = with_step(e, step, &log_path(&log));
                    let tail = self.events.redact(&log.tail(50));
                    self.events.emit(
                        "step.fail",
                        Some(step),
                        serde_json::json!({"error": e.message, "log": e.log, "exit": e.exit}),
                        &format!(
                            "✗ {} failed: {}\n{}\n  log: {}",
                            step.id,
                            e.message,
                            indent(&tail),
                            log.path.display()
                        ),
                    );
                    return Err(e);
                }
            }
        }
        Ok(applied)
    }
}

fn log_path(log: &Log) -> PathBuf {
    log.path.clone()
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn with_step(mut e: ExecError, step: &Step, log: &Path) -> ExecError {
    e.step.get_or_insert_with(|| step.id.clone());
    e.log.get_or_insert_with(|| log.to_path_buf());
    e
}
