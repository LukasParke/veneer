//! `veneer vm`: run Firecracker bundles from `veneer bake --target firecracker`, and take or
//! restore full memory snapshots so a microVM resumes with its workspace already staged.
//!
//! - `boot`: boot the bundle; serial console on stdio.
//! - `snapshot`: boot, wait for the init shim's ready marker, pause, and write
//!   `snapshot/vmstate` + `snapshot/mem` into the bundle.
//! - `restore`: clone the bundle's rootfs into a fresh run dir and resume from the snapshot;
//!   serial console on stdio. Many restores can share one snapshot.
//!
//! Firecracker is driven through its API socket (HTTP over a Unix socket).

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use crate::run::fail;

#[derive(Debug, clap::Subcommand)]
pub enum VmCmd {
    /// Boot a bundle with the serial console on stdio.
    Boot(BootArgs),
    /// Boot, wait for the workspace, and save a full memory snapshot into the bundle.
    Snapshot(SnapshotArgs),
    /// Resume a copy of the bundle from its snapshot (console on stdio).
    Restore(RestoreArgs),
}

#[derive(Debug, Clone, clap::Args)]
pub struct Machine {
    /// Firecracker bundle directory.
    pub bundle: PathBuf,
    /// firecracker binary (default: $VENEER_FIRECRACKER, then PATH).
    #[arg(long, env = "VENEER_FIRECRACKER")]
    pub firecracker: Option<PathBuf>,
    #[arg(long, default_value_t = 2)]
    pub vcpus: u32,
    #[arg(long, default_value_t = 512)]
    pub mem_mib: u32,
}

#[derive(Debug, clap::Args)]
pub struct BootArgs {
    #[command(flatten)]
    pub machine: Machine,
    /// Command to run after the workspace phase (default: a shell on the console).
    #[arg(long)]
    pub cmd: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct SnapshotArgs {
    #[command(flatten)]
    pub machine: Machine,
    /// Seconds to wait for the workspace to become ready.
    #[arg(long, default_value_t = 300)]
    pub timeout: u64,
}

#[derive(Debug, clap::Args)]
pub struct RestoreArgs {
    #[command(flatten)]
    pub machine: Machine,
    /// Run directory for this instance (default: <bundle>/runs/<pid>).
    #[arg(long)]
    pub run_dir: Option<PathBuf>,
}

fn firecracker_bin(m: &Machine) -> Result<PathBuf, String> {
    if let Some(p) = &m.firecracker {
        return Ok(p.clone());
    }
    std::env::var_os("PATH")
        .and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join("firecracker"))
                .find(|p| p.is_file())
        })
        .ok_or_else(|| "firecracker not found: pass --firecracker or put it on PATH".into())
}

/// Machine config with this run's sizing and optional `veneer.cmd=`.
fn config(m: &Machine, cmd: Option<&str>) -> String {
    let mut boot = "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/veneer-init".to_string();
    if let Some(c) = cmd {
        boot.push_str(&format!(" veneer.cmd={}", c.replace(' ', "%20")));
    }
    serde_json::json!({
        "boot-source": {"kernel_image_path": "vmlinux", "boot_args": boot},
        "drives": [{"drive_id": "rootfs", "path_on_host": "rootfs.ext4", "is_root_device": true, "is_read_only": false}],
        "machine-config": {"vcpu_count": m.vcpus, "mem_size_mib": m.mem_mib},
    })
    .to_string()
}

/// One HTTP/1.1 request over Firecracker's API socket. Returns the status code.
fn api(sock: &Path, method: &str, path: &str, body: &serde_json::Value) -> Result<u16, String> {
    let mut s =
        UnixStream::connect(sock).map_err(|e| format!("connect {}: {e}", sock.display()))?;
    s.set_read_timeout(Some(Duration::from_secs(120))).ok();
    let body = body.to_string();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(s);
    let mut status = String::new();
    reader.read_line(&mut status).map_err(|e| e.to_string())?;
    let code: u16 = status
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    if !(200..300).contains(&code) {
        let mut rest = String::new();
        let _ = reader.take(4096).read_to_string(&mut rest);
        return Err(format!(
            "{method} {path}: {} {}",
            status.trim(),
            rest.rsplit("\r\n\r\n").next().unwrap_or("")
        ));
    }
    Ok(code)
}

fn wait_for_socket(sock: &Path, child: &mut Child) -> Result<(), String> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if UnixStream::connect(sock).is_ok() {
            return Ok(());
        }
        if let Ok(Some(s)) = child.try_wait() {
            return Err(format!("firecracker exited early with {s}"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err("firecracker API socket did not appear".into())
}

fn spawn(
    fc: &Path,
    cwd: &Path,
    sock: &Path,
    config_file: Option<&Path>,
    console: Stdio,
    stdin: Stdio,
) -> Result<Child, String> {
    let _ = std::fs::remove_file(sock);
    let mut c = Command::new(fc);
    c.current_dir(cwd).arg("--api-sock").arg(sock);
    if let Some(cfg) = config_file {
        c.arg("--config-file").arg(cfg);
    }
    c.stdin(stdin).stdout(console).stderr(Stdio::inherit());
    c.spawn()
        .map_err(|e| format!("cannot start {}: {e}", fc.display()))
}

fn check_bundle(bundle: &Path) -> Result<(), String> {
    for f in ["vmlinux", "rootfs.ext4"] {
        if !bundle.join(f).is_file() {
            return Err(format!(
                "{} is not a firecracker bundle (missing {f})",
                bundle.display()
            ));
        }
    }
    Ok(())
}

pub fn run(cmd: VmCmd) -> ExitCode {
    let result = match cmd {
        VmCmd::Boot(a) => boot(a),
        VmCmd::Snapshot(a) => snapshot(a),
        VmCmd::Restore(a) => restore(a),
    };
    match result {
        Ok(code) => code,
        Err(e) => fail(3, &e, false),
    }
}

fn boot(a: BootArgs) -> Result<ExitCode, String> {
    let m = &a.machine;
    check_bundle(&m.bundle)?;
    let fc = firecracker_bin(m)?;
    let cfg = m.bundle.join("vm-config.run.json");
    std::fs::write(&cfg, config(m, a.cmd.as_deref())).map_err(|e| e.to_string())?;
    let sock = m.bundle.join("firecracker.sock");
    let mut child = spawn(
        &fc,
        &m.bundle,
        &sock,
        Some(&cfg),
        Stdio::inherit(),
        Stdio::inherit(),
    )?;
    let status = child.wait().map_err(|e| e.to_string())?;
    Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
}

fn snapshot(a: SnapshotArgs) -> Result<ExitCode, String> {
    let m = &a.machine;
    check_bundle(&m.bundle)?;
    let fc = firecracker_bin(m)?;
    let bundle = m.bundle.canonicalize().map_err(|e| e.to_string())?;
    let snap = bundle.join("snapshot");
    std::fs::create_dir_all(&snap).map_err(|e| e.to_string())?;
    let cfg = bundle.join("vm-config.run.json");
    std::fs::write(&cfg, config(m, None)).map_err(|e| e.to_string())?;
    let sock = bundle.join("firecracker.sock");
    let console_log = snap.join("boot.log");
    let log = std::fs::File::create(&console_log).map_err(|e| e.to_string())?;
    let mut child = spawn(
        &fc,
        &bundle,
        &sock,
        Some(&cfg),
        Stdio::from(log),
        Stdio::piped(),
    )?;
    wait_for_socket(&sock, &mut child)?;
    eprintln!(
        "▸ booting {} (console: {})",
        bundle.display(),
        console_log.display()
    );
    let start = Instant::now();
    loop {
        let text = std::fs::read_to_string(&console_log).unwrap_or_default();
        if text.contains(veneer_bake::READY_MARKER) {
            break;
        }
        if let Ok(Some(s)) = child.try_wait() {
            return Err(format!(
                "the VM exited before the workspace was ready ({s}); see {}",
                console_log.display()
            ));
        }
        if start.elapsed() > Duration::from_secs(a.timeout) {
            let _ = child.kill();
            return Err(format!(
                "workspace not ready after {}s; see {}",
                a.timeout,
                console_log.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let booted = start.elapsed();
    std::thread::sleep(Duration::from_millis(500)); // let the console shell start
    api(
        &sock,
        "PATCH",
        "/vm",
        &serde_json::json!({"state": "Paused"}),
    )?;
    api(
        &sock,
        "PUT",
        "/snapshot/create",
        &serde_json::json!({"snapshot_type": "Full", "snapshot_path": "snapshot/vmstate", "mem_file_path": "snapshot/mem"}),
    )?;
    let _ = child.kill();
    let _ = child.wait();
    // The disk must match the memory image: freeze a copy taken at the same instant.
    std::fs::copy(bundle.join("rootfs.ext4"), snap.join("rootfs.ext4"))
        .map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&sock);
    eprintln!(
        "✓ snapshot in {} (workspace ready after {:.1}s)",
        snap.display(),
        booted.as_secs_f64()
    );
    Ok(ExitCode::SUCCESS)
}

fn restore(a: RestoreArgs) -> Result<ExitCode, String> {
    let m = &a.machine;
    let fc = firecracker_bin(m)?;
    let bundle = m.bundle.canonicalize().map_err(|e| e.to_string())?;
    let snap = bundle.join("snapshot");
    for f in ["vmstate", "mem", "rootfs.ext4"] {
        if !snap.join(f).is_file() {
            return Err(format!(
                "no snapshot in {} (run `veneer vm snapshot` first)",
                bundle.display()
            ));
        }
    }
    let run = a
        .run_dir
        .clone()
        .unwrap_or_else(|| bundle.join("runs").join(std::process::id().to_string()));
    std::fs::create_dir_all(run.join("snapshot")).map_err(|e| e.to_string())?;
    // Each instance gets its own disk (the snapshot references `rootfs.ext4` relative to cwd);
    // memory is mapped privately, so instances share the one mem file.
    let disk = run.join("rootfs.ext4");
    let reflinked = Command::new("cp")
        .args(["--reflink=auto", "--sparse=always"])
        .arg(snap.join("rootfs.ext4"))
        .arg(&disk)
        .status()
        .is_ok_and(|s| s.success());
    if !reflinked {
        std::fs::copy(snap.join("rootfs.ext4"), &disk).map_err(|e| e.to_string())?;
    }
    for f in ["vmstate", "mem"] {
        let link = run.join("snapshot").join(f);
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(snap.join(f), &link).map_err(|e| e.to_string())?;
    }
    let sock = run.join("firecracker.sock");
    let start = Instant::now();
    let mut child = spawn(&fc, &run, &sock, None, Stdio::inherit(), Stdio::inherit())?;
    wait_for_socket(&sock, &mut child)?;
    api(
        &sock,
        "PUT",
        "/snapshot/load",
        &serde_json::json!({
            "snapshot_path": "snapshot/vmstate",
            "mem_backend": {"backend_type": "File", "backend_path": "snapshot/mem"},
            "resume_vm": true
        }),
    )?;
    eprintln!(
        "✓ resumed from snapshot in {}ms ({})",
        start.elapsed().as_millis(),
        run.display()
    );
    let status = child.wait().map_err(|e| e.to_string())?;
    Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
}
