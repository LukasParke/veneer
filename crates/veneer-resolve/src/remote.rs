//! Remote spec sources and the `.veneer/veneer.lock` pin file (design spec §4.1, §4.3).
//!
//! Sources: `github:owner/repo[//path][@ref]`, `git+<url>[//path][@ref]`, and
//! `https://…` (a single file). Repo sources without `//path` resolve to
//! `.veneer/veneer.yaml`. Every fetched entry is pinned in the lock by commit and
//! sha256 digest; later resolutions use the pinned commit and fail on digest mismatch.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use veneer_spec::{Diagnostic, codes};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub digest: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockFile {
    pub version: u32,
    #[serde(default)]
    pub sources: BTreeMap<String, LockEntry>,
    /// `<tool>@<requested>` → exact version (`veneer lock`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, String>,
    /// image ref → manifest digest.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub images: BTreeMap<String, String>,
    /// `<url>#<ref>` → commit.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub repos: BTreeMap<String, String>,
}

/// Fetch context shared across one resolution.
#[derive(Debug, Clone)]
pub struct Remote {
    pub cache: PathBuf,
    pub lock_path: Option<PathBuf>,
    pub lock: LockFile,
    /// Ignore pins and re-pin to what the refs point at now.
    pub update: bool,
    pub dirty: bool,
}

impl Remote {
    pub fn new(cache: PathBuf, lock_path: Option<PathBuf>, update: bool) -> Self {
        let lock = lock_path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(LockFile {
                version: 1,
                ..Default::default()
            });
        Self {
            cache,
            lock_path,
            lock,
            update,
            dirty: false,
        }
    }

    /// Writes the lock when it changed.
    pub fn save(&mut self) -> Result<(), Diagnostic> {
        let Some(path) = &self.lock_path else {
            return Ok(());
        };
        if !self.dirty {
            return Ok(());
        }
        let mut text = serde_json::to_string_pretty(&self.lock).expect("lock serializes");
        text.push('\n');
        std::fs::write(path, text).map_err(|e| {
            Diagnostic::error(
                codes::NOT_FOUND,
                format!("cannot write {}: {e}", path.display()),
            )
        })?;
        self.dirty = false;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    Git {
        url: String,
        path: Option<String>,
        git_ref: Option<String>,
    },
    Https {
        url: String,
    },
    Oci {
        reference: String,
        path: Option<String>,
    },
}

fn parse(source: &str) -> Option<Source> {
    let split_ref_path = |rest: &str| -> (String, Option<String>, Option<String>) {
        let (rest, git_ref) = match rest.rsplit_once('@') {
            Some((r, g)) if !g.contains('/') || rest.contains("//") => {
                (r.to_string(), Some(g.to_string()))
            }
            _ => (rest.to_string(), None),
        };
        match rest.split_once("//") {
            Some((repo, path)) if !repo.ends_with(':') => {
                (repo.to_string(), Some(path.to_string()), git_ref)
            }
            _ => (rest, None, git_ref),
        }
    };
    if let Some(rest) = source.strip_prefix("github:") {
        let (repo, path, git_ref) = split_ref_path(rest);
        return Some(Source::Git {
            url: format!("https://github.com/{repo}.git"),
            path,
            git_ref,
        });
    }
    if let Some(rest) = source.strip_prefix("git+") {
        // git+https://host/x.git//sub/file.yaml@ref — the scheme's `//` is not a path split.
        let (scheme, tail) = rest.split_once("://")?;
        let (tail_repo, path, git_ref) = split_ref_path(tail);
        return Some(Source::Git {
            url: format!("{scheme}://{tail_repo}"),
            path,
            git_ref,
        });
    }
    if let Some(rest) = source.strip_prefix("oci://") {
        let (reference, path) = match rest.split_once("//") {
            Some((r, p)) => (r.to_string(), Some(p.to_string())),
            None => (rest.to_string(), None),
        };
        return Some(Source::Oci { reference, path });
    }
    if source.starts_with("https://") || source.starts_with("http://") {
        return Some(Source::Https {
            url: source.to_string(),
        });
    }
    None
}

fn sha256_file(path: &Path) -> Option<String> {
    std::fs::read(path)
        .ok()
        .map(|b| format!("sha256-{}", hex::encode(Sha256::digest(b))))
}

fn short(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))[..16].to_string()
}

fn git(args: &[&str], cwd: Option<&Path>) -> Result<String, String> {
    let mut c = Command::new("git");
    if let Some(d) = cwd {
        c.arg("-C").arg(d);
    }
    let out = c
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Checks out `url` at `want` (a pinned commit) or `git_ref` into the cache; returns (dir, commit).
fn fetch_git(
    cache: &Path,
    url: &str,
    git_ref: Option<&str>,
    want: Option<&str>,
) -> Result<(PathBuf, String), String> {
    let dir = cache
        .join("sources")
        .join(short(&format!("{url}@{}", git_ref.unwrap_or("HEAD"))));
    if let Some(commit) = want
        && dir.join(".git").exists()
        && git(&["rev-parse", "HEAD"], Some(&dir)).ok().as_deref() == Some(commit)
    {
        return Ok((dir, commit.to_string()));
    }
    let tmp = dir.with_extension("tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.parent().unwrap()).map_err(|e| e.to_string())?;
    git(&["init", "-q", tmp.to_str().unwrap()], None)?;
    git(&["remote", "add", "origin", url], Some(&tmp))?;
    let target = want.or(git_ref).unwrap_or("HEAD");
    if git(
        &["fetch", "-q", "--depth", "1", "origin", target],
        Some(&tmp),
    )
    .is_err()
    {
        git(&["fetch", "-q", "--tags", "origin"], Some(&tmp))?;
    }
    let checkout = if want.is_some()
        || git(&["rev-parse", "--verify", "-q", "FETCH_HEAD"], Some(&tmp)).is_err()
    {
        target.to_string()
    } else {
        "FETCH_HEAD".to_string()
    };
    git(&["checkout", "-q", "--detach", &checkout], Some(&tmp))?;
    let commit = git(&["rev-parse", "HEAD"], Some(&tmp))?;
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::rename(&tmp, &dir).map_err(|e| e.to_string())?;
    Ok((dir, commit))
}

fn fetch_https(cache: &Path, url: &str) -> Result<PathBuf, String> {
    let name = url
        .rsplit('/')
        .next()
        .filter(|n| !n.is_empty())
        .unwrap_or("veneer.yaml");
    let dir = cache.join("sources").join(short(url));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(name);
    if std::env::var_os("VENEER_OFFLINE").is_some() {
        return if path.is_file() {
            Ok(path)
        } else {
            Err(format!("offline and {url} is not cached"))
        };
    }
    let mut resp = ureq::get(url)
        .call()
        .map_err(|e| format!("GET {url}: {e}"))?;
    let body = resp
        .body_mut()
        .read_to_vec()
        .map_err(|e| format!("GET {url}: {e}"))?;
    std::fs::write(&path, body).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Fetches `source`, pins it in the lock, and returns the local path of its entry file.
pub fn fetch(remote: &mut Remote, source: &str) -> Result<PathBuf, Diagnostic> {
    let err = |msg: String| {
        Diagnostic::error(codes::NOT_FOUND, format!("cannot fetch `{source}`: {msg}"))
    };
    let parsed = parse(source).ok_or_else(|| {
        Diagnostic::error(
            codes::REMOTE_UNSUPPORTED,
            format!("unsupported remote source `{source}` (use github:, git+…, https:, or oci://)"),
        )
    })?;
    let pinned = if remote.update {
        None
    } else {
        remote.lock.sources.get(source).cloned()
    };
    let (path, commit) = match &parsed {
        Source::Git { url, path, git_ref } => {
            let (dir, commit) = fetch_git(
                &remote.cache,
                url,
                git_ref.as_deref(),
                pinned.as_ref().and_then(|p| p.commit.as_deref()),
            )
            .map_err(err)?;
            let rel = path.clone().unwrap_or_else(|| ".veneer/veneer.yaml".into());
            (dir.join(rel), Some(commit))
        }
        Source::Https { url } => (fetch_https(&remote.cache, url).map_err(err)?, None),
        Source::Oci { reference, path } => {
            let mut r = crate::oci::Reference::parse(reference).map_err(err)?;
            if let Some(d) = pinned.as_ref().and_then(|p| p.commit.clone()) {
                r.reference = d; // pull exactly the pinned manifest
            }
            let dir = crate::oci::cache_dir(&remote.cache, &r);
            let offline = std::env::var_os("VENEER_OFFLINE").is_some();
            let digest = match pinned.as_ref().and_then(|p| p.commit.clone()) {
                Some(d) if offline && dir.is_dir() => d,
                _ if offline => return Err(err("offline and not cached".into())),
                _ => crate::oci::pull(&r, &dir).map_err(err)?,
            };
            let rel = path.clone().unwrap_or_else(|| {
                if dir.join(".veneer/veneer.yaml").is_file() {
                    ".veneer/veneer.yaml".into()
                } else {
                    "veneer.yaml".into()
                }
            });
            (dir.join(rel), Some(digest))
        }
    };
    if !path.is_file() {
        return Err(err(format!(
            "{} does not exist in the source",
            path.display()
        )));
    }
    let digest = sha256_file(&path).unwrap_or_default();
    match pinned {
        Some(p) if p.digest != digest => {
            return Err(Diagnostic::error(
                "V1005",
                format!(
                    "`{source}` does not match veneer.lock (expected {}, got {digest})",
                    p.digest
                ),
            )
            .note("run `veneer lock --update` to re-pin remote sources"));
        }
        Some(_) => {}
        None => {
            remote
                .lock
                .sources
                .insert(source.to_string(), LockEntry { commit, digest });
            remote.dirty = true;
        }
    }
    path.canonicalize().map_err(|e| err(e.to_string()))
}

/// Built-in modules shipped inside the binary (`use: [veneer/harness/claude-code@1]`).
pub fn builtin_module(name: &str) -> Option<&'static str> {
    match name {
        "veneer/harness/claude-code@1" => Some(include_str!("../modules/harness-claude-code.yaml")),
        "veneer/harness/codex@1" => Some(include_str!("../modules/harness-codex.yaml")),
        "veneer/harness/pi@1" => Some(include_str!("../modules/harness-pi.yaml")),
        "veneer/harness/omp@1" => Some(include_str!("../modules/harness-omp.yaml")),
        "veneer/harness/opencode@1" => Some(include_str!("../modules/harness-opencode.yaml")),
        _ => None,
    }
}

/// Materializes a built-in module into the cache so it loads like any other document.
pub fn builtin_path(cache: &Path, name: &str) -> Option<PathBuf> {
    let body = builtin_module(name)?;
    let path = cache
        .join("modules")
        .join(format!("{}.yaml", name.replace(['/', '@'], "_")));
    std::fs::create_dir_all(path.parent()?).ok()?;
    if std::fs::read_to_string(&path).ok().as_deref() != Some(body) {
        std::fs::write(&path, body).ok()?;
    }
    path.canonicalize().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sources() {
        assert_eq!(
            parse("github:acme/envs//base.yaml@v3"),
            Some(Source::Git {
                url: "https://github.com/acme/envs.git".into(),
                path: Some("base.yaml".into()),
                git_ref: Some("v3".into())
            })
        );
        assert_eq!(
            parse("github:acme/app@main"),
            Some(Source::Git {
                url: "https://github.com/acme/app.git".into(),
                path: None,
                git_ref: Some("main".into())
            })
        );
        assert_eq!(
            parse("git+https://example.com/x/y.git//envs/a.yaml@abc"),
            Some(Source::Git {
                url: "https://example.com/x/y.git".into(),
                path: Some("envs/a.yaml".into()),
                git_ref: Some("abc".into())
            })
        );
        assert_eq!(
            parse("git+file:///tmp/repo"),
            Some(Source::Git {
                url: "file:///tmp/repo".into(),
                path: None,
                git_ref: None
            })
        );
        assert_eq!(
            parse("https://x.dev/e.yaml"),
            Some(Source::Https {
                url: "https://x.dev/e.yaml".into()
            })
        );
        assert_eq!(
            parse("oci://ghcr.io/acme/envs:v1//base.yaml"),
            Some(Source::Oci {
                reference: "ghcr.io/acme/envs:v1".into(),
                path: Some("base.yaml".into())
            })
        );
        assert_eq!(parse("ftp://x"), None);
    }
}
