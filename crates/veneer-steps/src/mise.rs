//! mise bootstrap and tool installation (design spec §8.3).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use indexmap::IndexMap;
use sha2::{Digest, Sha256};
use veneer_exec::{Check, ExecError, Log, Result, run_logged, write_atomic};
use veneer_plan::Dirs;

use crate::util::capture;

pub const MISE_VERSION: &str = "2026.11.0";

fn asset_name() -> Result<String> {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => {
            return Err(ExecError::failed(format!(
                "mise has no build for architecture {other}"
            )));
        }
    };
    let os = match std::env::consts::OS {
        "linux" => return Ok(format!("mise-v{MISE_VERSION}-linux-{arch}-musl")),
        "macos" => "macos",
        other => return Err(ExecError::failed(format!("mise has no build for {other}"))),
    };
    Ok(format!("mise-v{MISE_VERSION}-{os}-{arch}"))
}

fn download(url: &str) -> Result<Vec<u8>> {
    if crate::bundle::is_offline() {
        return Err(ExecError::failed(format!(
            "offline: cannot download {url} (restore a bundle with `--from`)"
        )));
    }
    let mut resp = ureq::get(url)
        .call()
        .map_err(|e| ExecError::failed(format!("download {url} failed: {e}")))?;
    resp.body_mut()
        .with_config()
        .limit(512 * 1024 * 1024)
        .read_to_vec()
        .map_err(|e| ExecError::failed(format!("download {url} failed: {e}")))
}

/// The mise binary veneer uses: `VENEER_MISE_BIN`, or a checksum-verified download.
pub fn ensure_mise(dirs: &Dirs, log: &mut Log) -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("VENEER_MISE_BIN") {
        return Ok(PathBuf::from(p));
    }
    let bin = dirs.mise_bin();
    if bin.is_file()
        && capture(Command::new(&bin).arg("--version")).is_some_and(|v| v.starts_with(MISE_VERSION))
    {
        return Ok(bin);
    }
    let asset = asset_name()?;
    let base = std::env::var("VENEER_MISE_MIRROR").unwrap_or_else(|_| {
        format!("https://github.com/jdx/mise/releases/download/v{MISE_VERSION}")
    });
    log.line(&format!("downloading {base}/{asset}"));
    let sums = String::from_utf8_lossy(&download(&format!("{base}/SHASUMS256.txt"))?).into_owned();
    let expected = sums
        .lines()
        .find_map(|l| {
            let (hash, name) = l.split_once(char::is_whitespace)?;
            (name.trim().trim_start_matches("./") == asset).then(|| hash.to_string())
        })
        .ok_or_else(|| ExecError::failed(format!("{asset} is not listed in SHASUMS256.txt")))?;
    let bytes = download(&format!("{base}/{asset}"))?;
    let actual = hex::encode(Sha256::digest(&bytes));
    if actual != expected {
        return Err(ExecError::failed(format!(
            "checksum mismatch for {asset}: expected {expected}, got {actual}"
        )));
    }
    write_atomic(&bin, &bytes)
        .map_err(|e| ExecError::failed(format!("cannot write {}: {e}", bin.display())))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| ExecError::failed(e.to_string()))?;
    Ok(bin)
}

fn mise_dir(dirs: &Dirs) -> PathBuf {
    dirs.state.join("mise")
}

pub fn config_path(dirs: &Dirs) -> PathBuf {
    mise_dir(dirs).join("config.toml")
}

/// mise's lockfile for veneer's config: exact versions plus per-platform checksums, which
/// mise records when the file exists and verifies on every install.
pub fn lock_path(dirs: &Dirs) -> PathBuf {
    mise_dir(dirs).join("mise.lock")
}

/// Puts the project's `mise.lock` (from `veneer lock`) in place, or starts an empty one.
fn place_lock(dirs: &Dirs, project_lock: Option<&std::path::Path>) -> Result<()> {
    let dest = lock_path(dirs);
    match project_lock {
        Some(src) => {
            let bytes = std::fs::read(src)
                .map_err(|e| ExecError::failed(format!("cannot read {}: {e}", src.display())))?;
            write_atomic(&dest, &bytes).map_err(|e| ExecError::failed(e.to_string()))
        }
        // No project lock: leave mise's lock behavior untouched.
        None => Ok(()),
    }
}

/// Writes a mise.lock covering every platform for `tools` (used by `veneer lock`).
pub fn lock_all(
    dirs: &Dirs,
    tools: &IndexMap<String, String>,
    options: &IndexMap<String, serde_json::Value>,
    log: &mut Log,
) -> Result<Vec<u8>> {
    let bin = ensure_mise(dirs, log)?;
    write_atomic(&config_path(dirs), render_config(tools, options).as_bytes())
        .map_err(|e| ExecError::failed(format!("cannot write mise config: {e}")))?;
    if !lock_path(dirs).exists() {
        write_atomic(&lock_path(dirs), b"").map_err(|e| ExecError::failed(e.to_string()))?;
    }
    run_logged(mise_cmd(&bin, dirs).args(["lock", "--global"]), log)?;
    std::fs::read(lock_path(dirs)).map_err(|e| ExecError::failed(e.to_string()))
}

/// Environment that isolates mise from the user's own installation.
pub fn mise_env(dirs: &Dirs) -> BTreeMap<String, String> {
    let d = mise_dir(dirs);
    let s = |p: PathBuf| p.display().to_string();
    BTreeMap::from([
        ("MISE_DATA_DIR".into(), s(dirs.data.join("mise"))),
        ("MISE_CACHE_DIR".into(), s(dirs.cache.join("mise"))),
        ("MISE_STATE_DIR".into(), s(d.join("state"))),
        ("MISE_CONFIG_DIR".into(), s(d.join("conf.d"))),
        ("MISE_GLOBAL_CONFIG_FILE".into(), s(config_path(dirs))),
        ("MISE_TRUSTED_CONFIG_PATHS".into(), s(d.clone())),
        ("MISE_YES".into(), "1".into()),
        ("MISE_QUIET".into(), "1".into()),
    ])
}

pub fn mise_cmd(bin: &PathBuf, dirs: &Dirs) -> Command {
    let mut c = Command::new(bin);
    let cwd = mise_dir(dirs);
    let _ = std::fs::create_dir_all(&cwd);
    c.current_dir(cwd).envs(mise_env(dirs));
    c
}

pub fn render_config(
    tools: &IndexMap<String, String>,
    options: &IndexMap<String, serde_json::Value>,
) -> String {
    let mut table = toml::Table::new();
    let mut t = toml::Table::new();
    for (k, v) in tools {
        let value = match options.get(k).and_then(|o| toml::Table::try_from(o).ok()) {
            Some(mut opts) => {
                opts.insert("version".into(), toml::Value::String(v.clone()));
                toml::Value::Table(opts)
            }
            None => toml::Value::String(v.clone()),
        };
        t.insert(k.clone(), value);
    }
    table.insert("tools".into(), toml::Value::Table(t));
    toml::to_string(&table).expect("toml serializes")
}

/// Tools whose `mise ls --json` entries are all installed; `None` when mise can't answer.
fn missing_tools(
    bin: &PathBuf,
    dirs: &Dirs,
    tools: &IndexMap<String, String>,
) -> Option<Vec<String>> {
    let out = capture(mise_cmd(bin, dirs).args(["ls", "--json"]))?;
    let v: serde_json::Value = serde_json::from_str(&out).ok()?;
    let obj = v.as_object()?;
    Some(
        tools
            .keys()
            .filter(|name| {
                let entries = obj.get(name.as_str()).and_then(|e| e.as_array());
                !entries.is_some_and(|es| {
                    !es.is_empty() && es.iter().all(|e| e["installed"].as_bool() == Some(true))
                })
            })
            .cloned()
            .collect(),
    )
}

pub fn check(
    dirs: &Dirs,
    tools: &IndexMap<String, String>,
    options: &IndexMap<String, serde_json::Value>,
    project_lock: Option<&std::path::Path>,
) -> Check {
    if let Some(l) = project_lock
        && std::fs::read(l).ok() != std::fs::read(lock_path(dirs)).ok()
    {
        return Check::Needed("mise.lock changed".into());
    }
    let bin = std::env::var_os("VENEER_MISE_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs.mise_bin());
    if !bin.is_file() {
        return Check::Needed("mise is not installed".into());
    }
    let desired = render_config(tools, options);
    if std::fs::read_to_string(config_path(dirs)).ok().as_deref() != Some(desired.as_str()) {
        return Check::Needed("tool versions changed".into());
    }
    match missing_tools(&bin, dirs, tools) {
        Some(m) if m.is_empty() => Check::Satisfied,
        Some(m) => Check::Needed(format!("missing tools: {}", m.join(", "))),
        None => Check::Needed("cannot query mise".into()),
    }
}

pub fn apply(
    dirs: &Dirs,
    tools: &IndexMap<String, String>,
    options: &IndexMap<String, serde_json::Value>,
    project_lock: Option<&std::path::Path>,
    log: &mut Log,
) -> Result<()> {
    let bin = ensure_mise(dirs, log)?;
    place_lock(dirs, project_lock)?;
    write_atomic(&config_path(dirs), render_config(tools, options).as_bytes())
        .map_err(|e| ExecError::failed(format!("cannot write mise config: {e}")))?;
    run_logged(mise_cmd(&bin, dirs).arg("install"), log)
}

/// The ELF program interpreter (`PT_INTERP`) of a file, if it is a dynamically linked ELF.
pub fn elf_interpreter(path: &std::path::Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() < 64 || &bytes[..4] != b"\x7fELF" || bytes[4] != 2 || bytes[5] != 1 {
        return None; // only 64-bit little-endian ELF
    }
    let u16_at =
        |o: usize| Some(u16::from_le_bytes(bytes.get(o..o + 2)?.try_into().ok()?) as usize);
    let u32_at = |o: usize| Some(u32::from_le_bytes(bytes.get(o..o + 4)?.try_into().ok()?));
    let u64_at =
        |o: usize| Some(u64::from_le_bytes(bytes.get(o..o + 8)?.try_into().ok()?) as usize);
    let (phoff, phentsize, phnum) = (u64_at(32)?, u16_at(54)?, u16_at(56)?);
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        if u32_at(ph)? == 3 {
            let (off, size) = (u64_at(ph + 8)?, u64_at(ph + 32)?);
            let raw = bytes.get(off..off + size)?;
            return Some(
                String::from_utf8_lossy(raw)
                    .trim_end_matches('\0')
                    .to_string(),
            );
        }
    }
    None
}

/// Installed tool executables that need a dynamic loader this system doesn't have
/// (e.g. a glibc-only release on a musl distro): (tool, binary, loader).
fn unrunnable(bin: &PathBuf, dirs: &Dirs) -> Vec<(String, PathBuf, String)> {
    use std::os::unix::fs::PermissionsExt;
    let Some(out) = capture(mise_cmd(bin, dirs).args(["ls", "--json"])) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&out) else {
        return Vec::new();
    };
    let mut bad = Vec::new();
    for (tool, entries) in v.as_object().into_iter().flatten() {
        for e in entries.as_array().into_iter().flatten() {
            let Some(root) = e["install_path"].as_str() else {
                continue;
            };
            let mut stack = vec![(PathBuf::from(root), 0)];
            while let Some((dir, depth)) = stack.pop() {
                for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                    let p = entry.path();
                    let Ok(ft) = entry.file_type() else { continue };
                    if ft.is_dir() && depth < 3 {
                        stack.push((p, depth + 1));
                    } else if ft.is_file()
                        && entry
                            .metadata()
                            .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
                        && let Some(interp) = elf_interpreter(&p)
                        && !std::path::Path::new(&interp).exists()
                    {
                        bad.push((tool.clone(), p, interp));
                    }
                }
            }
        }
    }
    bad
}

pub fn verify(dirs: &Dirs, tools: &IndexMap<String, String>) -> Result<String> {
    let bin = std::env::var_os("VENEER_MISE_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs.mise_bin());
    if let Some((tool, exe, interp)) = unrunnable(&bin, dirs).into_iter().next() {
        return Err(ExecError::failed(format!(
            "V4002: `{tool}` installed {} which needs the loader {interp}, but this system doesn't \
             have it (e.g. a glibc-only release on a musl distro). Pin a version or backend that \
             publishes a build for this platform, or install it through `packages` or `nix:`",
            exe.display()
        )));
    }
    match missing_tools(&bin, dirs, tools) {
        Some(m) if m.is_empty() => Ok(tools
            .iter()
            .map(|(k, v)| format!("{k}@{v}"))
            .collect::<Vec<_>>()
            .join(",")),
        Some(m) => Err(ExecError::failed(format!(
            "tools still missing after install: {}",
            m.join(", ")
        ))),
        None => Err(ExecError::failed("cannot query mise after install")),
    }
}

/// Environment additions (mainly `PATH`) that activate the installed tools.
pub fn tool_env(dirs: &Dirs) -> BTreeMap<String, String> {
    let bin = std::env::var_os("VENEER_MISE_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs.mise_bin());
    if !bin.is_file() || !config_path(dirs).is_file() {
        return BTreeMap::new();
    }
    capture(mise_cmd(&bin, dirs).args(["env", "--json"]))
        .and_then(|out| serde_json::from_str::<BTreeMap<String, serde_json::Value>>(&out).ok())
        .map(|m| {
            m.into_iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}
