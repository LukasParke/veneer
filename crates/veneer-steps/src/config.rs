//! `ConfigMerge` (one key of a JSON/TOML config file) and `Symlink` steps.

use std::path::Path;
use std::process::Command;

use serde_json::Value;
use veneer_exec::{Check, ExecError, Result, write_atomic};
use veneer_plan::ConfigFormat;

use crate::util::capture;

fn read_doc(dest: &Path, format: ConfigFormat) -> Result<Value> {
    let text = match std::fs::read_to_string(dest) {
        Ok(t) if !t.trim().is_empty() => t,
        Ok(_) => return Ok(Value::Object(Default::default())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Value::Object(Default::default()));
        }
        Err(e) => {
            return Err(ExecError::failed(format!(
                "cannot read {}: {e}",
                dest.display()
            )));
        }
    };
    match format {
        ConfigFormat::Json => serde_json::from_str(&text),
        ConfigFormat::Toml => toml::from_str::<toml::Table>(&text)
            .map_err(|e| ExecError::failed(format!("{} is not valid TOML: {e}", dest.display())))
            .and_then(|t| serde_json::to_value(t).map_err(|e| ExecError::failed(e.to_string())))
            .map(Ok)?,
    }
    .map_err(|e| ExecError::failed(format!("{} is not valid JSON: {e}", dest.display())))
}

fn get<'a>(doc: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(doc, |v, k| v.get(k))
}

fn mode_ok(dest: &Path, mode: Option<u32>) -> bool {
    use std::os::unix::fs::PermissionsExt;
    mode.is_none_or(|m| {
        std::fs::metadata(dest)
            .map(|md| md.permissions().mode() & 0o7777 == m)
            .unwrap_or(false)
    })
}

pub fn merge_check(
    dest: &Path,
    format: ConfigFormat,
    path: &[String],
    value: &Value,
    mode: Option<u32>,
) -> Check {
    match read_doc(dest, format) {
        Ok(doc) if get(&doc, path) == Some(value) && mode_ok(dest, mode) => Check::Satisfied,
        Ok(_) => Check::Needed(format!("{} differs at {}", dest.display(), path.join("."))),
        Err(e) => Check::Needed(e.message),
    }
}

pub fn merge_apply(
    dest: &Path,
    format: ConfigFormat,
    path: &[String],
    value: &Value,
    mode: Option<u32>,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut doc = read_doc(dest, format)?;
    let mut cur = &mut doc;
    for (i, k) in path.iter().enumerate() {
        if !cur.is_object() {
            *cur = Value::Object(Default::default());
        }
        let obj = cur.as_object_mut().expect("object");
        if i + 1 == path.len() {
            obj.insert(k.clone(), value.clone());
            break;
        }
        cur = obj
            .entry(k.clone())
            .or_insert_with(|| Value::Object(Default::default()));
    }
    let text = match format {
        ConfigFormat::Json => serde_json::to_string_pretty(&doc).expect("json") + "\n",
        ConfigFormat::Toml => {
            let t: toml::Table = toml::Table::try_from(&doc).map_err(|e| {
                ExecError::failed(format!("cannot express {} as TOML: {e}", dest.display()))
            })?;
            toml::to_string(&t).map_err(|e| ExecError::failed(e.to_string()))?
        }
    };
    // Create private files private from the start (they may hold credentials).
    if let Some(m) = mode
        && !dest.exists()
    {
        if let Some(d) = dest.parent() {
            std::fs::create_dir_all(d).map_err(|e| ExecError::failed(e.to_string()))?;
        }
        std::fs::write(dest, b"")
            .and_then(|_| std::fs::set_permissions(dest, std::fs::Permissions::from_mode(m)))
            .map_err(|e| ExecError::failed(e.to_string()))?;
    }
    write_atomic(dest, text.as_bytes())
        .map_err(|e| ExecError::failed(format!("cannot write {}: {e}", dest.display())))?;
    if let Some(m) = mode {
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(m))
            .map_err(|e| ExecError::failed(e.to_string()))?;
    }
    Ok(())
}

pub fn link_check(link: &Path, target: &str) -> Check {
    if std::fs::symlink_metadata(link).is_ok() {
        return Check::Satisfied; // never clobber an existing file or link
    }
    match link.parent() {
        Some(d) if d.join(target).exists() => {
            Check::Needed(format!("{} → {target} missing", link.display()))
        }
        _ => Check::Satisfied, // nothing to point at (yet)
    }
}

pub fn link_apply(link: &Path, target: &str) -> Result<()> {
    if link_check(link, target) == Check::Satisfied {
        return Ok(());
    }
    std::os::unix::fs::symlink(target, link)
        .map_err(|e| ExecError::failed(format!("cannot link {}: {e}", link.display())))?;
    // Keep `git status` clean inside checkouts.
    let dir = link.parent().unwrap_or(Path::new("/"));
    let top = capture(
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", "--show-toplevel"]),
    );
    let exclude = capture(Command::new("git").arg("-C").arg(dir).args([
        "rev-parse",
        "--git-path",
        "info/exclude",
    ]));
    if let (Some(top), Some(ex)) = (top, exclude) {
        let top = Path::new(top.trim());
        let ex = Path::new(ex.trim());
        let ex = if ex.is_absolute() {
            ex.to_path_buf()
        } else {
            dir.join(ex)
        };
        if let Ok(rel) = link.strip_prefix(top) {
            let line = format!("/{}", rel.display());
            let cur = std::fs::read_to_string(&ex).unwrap_or_default();
            if !cur.lines().any(|l| l == line) {
                let _ = std::fs::create_dir_all(ex.parent().unwrap_or(Path::new("/")));
                let mut text = cur;
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&line);
                text.push('\n');
                let _ = std::fs::write(&ex, text);
            }
        }
    }
    Ok(())
}
