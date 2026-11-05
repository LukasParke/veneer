//! Offline bundles (standards proposal §5.4.4): `veneer fetch` packs everything the
//! network would provide into an **OCI Image Layout** directory; `veneer apply --offline
//! --from <dir>` restores it and forbids network access.
//!
//! One artifact manifest (`artifactType: application/vnd.veneer.bundle.v1+json`) lists
//! the layers, each annotated with `dev.veneer.kind`:
//! - `tools`: veneer's mise installs, the mise binary, and its config (platform-specific)
//! - `image`: one `docker save`-format tarball per image (`dev.veneer.ref`)
//! - `repo`: one `git bundle` per repository (`dev.veneer.ref` = the URL)
//! - `sources`: cached remote spec sources and built-in modules
//!
//! OS packages, Nix profiles, and dependency installs are not bundled: bake them into the
//! image (`veneer bake`) and run only the workspace phase offline.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use veneer_exec::{ExecError, Log, Result, run_logged};
use veneer_plan::{Dirs, Plan, RepoMode, StepKind};

pub const ARTIFACT_TYPE: &str = "application/vnd.veneer.bundle.v1+json";
const EMPTY_CONFIG: &str = "{}";
const EMPTY_MEDIA: &str = "application/vnd.oci.empty.v1+json";

fn io(e: impl std::fmt::Display) -> ExecError {
    ExecError::failed(e.to_string())
}

pub fn platform() -> String {
    format!(
        "linux/{}",
        if std::env::consts::ARCH == "aarch64" {
            "arm64"
        } else {
            "amd64"
        }
    )
}

struct Layout {
    dir: PathBuf,
    layers: Vec<Value>,
}

impl Layout {
    fn new(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir.join("blobs/sha256")).map_err(io)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            layers: Vec::new(),
        })
    }

    /// Moves `file` into `blobs/sha256/<digest>` and returns (digest, size).
    fn put_file(&self, file: &Path) -> Result<(String, u64)> {
        let mut h = Sha256::new();
        let mut f = std::fs::File::open(file).map_err(io)?;
        let size = std::io::copy(&mut f, &mut h).map_err(io)?;
        let hex = hex::encode(h.finalize());
        let dest = self.dir.join("blobs/sha256").join(&hex);
        if std::fs::rename(file, &dest).is_err() {
            std::fs::copy(file, &dest).map_err(io)?;
        }
        Ok((format!("sha256:{hex}"), size))
    }

    fn put_bytes(&self, bytes: &[u8]) -> Result<(String, u64)> {
        let hex = hex::encode(Sha256::digest(bytes));
        std::fs::write(self.dir.join("blobs/sha256").join(&hex), bytes).map_err(io)?;
        Ok((format!("sha256:{hex}"), bytes.len() as u64))
    }

    fn layer(
        &mut self,
        file: &Path,
        media: &str,
        title: &str,
        kind: &str,
        r: Option<&str>,
    ) -> Result<()> {
        let (digest, size) = self.put_file(file)?;
        let mut ann = serde_json::Map::new();
        ann.insert("org.opencontainers.image.title".into(), title.into());
        ann.insert("dev.veneer.kind".into(), kind.into());
        if let Some(r) = r {
            ann.insert("dev.veneer.ref".into(), r.into());
        }
        self.layers
            .push(json!({"mediaType": media, "digest": digest, "size": size, "annotations": ann}));
        Ok(())
    }

    fn finish(self, annotations: Value) -> Result<String> {
        let (cfg_digest, cfg_size) = self.put_bytes(EMPTY_CONFIG.as_bytes())?;
        let manifest = json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "artifactType": ARTIFACT_TYPE,
            "config": {"mediaType": EMPTY_MEDIA, "digest": cfg_digest, "size": cfg_size},
            "layers": self.layers,
            "annotations": annotations,
        });
        let bytes = serde_json::to_vec_pretty(&manifest).expect("manifest serializes");
        let (digest, size) = self.put_bytes(&bytes)?;
        let index = json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [{
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "artifactType": ARTIFACT_TYPE,
                "digest": digest,
                "size": size,
                "annotations": {"org.opencontainers.image.ref.name": "bundle"},
            }],
        });
        std::fs::write(
            self.dir.join("index.json"),
            serde_json::to_vec_pretty(&index).unwrap(),
        )
        .map_err(io)?;
        std::fs::write(
            self.dir.join("oci-layout"),
            br#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .map_err(io)?;
        Ok(digest)
    }
}

/// `tar -cf out -C root <paths that exist>`.
fn tar_paths(root: &Path, paths: &[&str], out: &Path, log: &mut Log) -> Result<bool> {
    let present: Vec<&str> = paths
        .iter()
        .copied()
        .filter(|p| root.join(p).exists())
        .collect();
    if present.is_empty() {
        return Ok(false);
    }
    run_logged(
        Command::new("tar")
            .arg("-cf")
            .arg(out)
            .arg("-C")
            .arg(root)
            .args(&present),
        log,
    )?;
    Ok(true)
}

#[derive(Debug, Default)]
pub struct Summary {
    pub digest: String,
    pub tools: bool,
    pub images: usize,
    pub repos: usize,
    pub sources: bool,
}

/// Fetches everything `plan` downloads into an OCI layout at `out`.
pub fn fetch(plan: &Plan, dirs: &Dirs, out: &Path, log: &mut Log) -> Result<Summary> {
    let mut layout = Layout::new(out)?;
    let work = tempfile::tempdir_in(out).map_err(io)?;
    let mut sum = Summary::default();
    for step in &plan.steps {
        match &step.kind {
            StepKind::Tools { tools } => {
                if crate::mise::check(dirs, tools) != veneer_exec::Check::Satisfied {
                    crate::mise::apply(dirs, tools, log)?;
                }
                let t = work.path().join("tools.tar");
                if tar_paths(
                    &dirs.state,
                    &["data/mise", "data/bin/mise", "mise"],
                    &t,
                    log,
                )? {
                    layout.layer(
                        &t,
                        "application/vnd.oci.image.layer.v1.tar",
                        "tools.tar",
                        "tools",
                        None,
                    )?;
                    sum.tools = true;
                }
            }
            StepKind::Images { refs, digests } => {
                crate::crane_save(refs, digests, dirs, log)?;
                for r in refs {
                    let t = work.path().join("image.tar");
                    std::fs::copy(crate::image_tar(dirs, r), &t).map_err(io)?;
                    layout.layer(
                        &t,
                        "application/vnd.docker.image.tar",
                        &format!("{}.tar", sanitize(r)),
                        "image",
                        Some(r),
                    )?;
                    sum.images += 1;
                }
            }
            StepKind::Repo { url, mode, .. } if *mode != RepoMode::Copy => {
                let mirror = work.path().join("mirror.git");
                let _ = std::fs::remove_dir_all(&mirror);
                run_logged(
                    Command::new("git")
                        .args(["clone", "--mirror", url])
                        .arg(&mirror),
                    log,
                )?;
                let b = work.path().join("repo.bundle");
                run_logged(
                    Command::new("git")
                        .arg("-C")
                        .arg(&mirror)
                        .args(["bundle", "create"])
                        .arg(&b)
                        .arg("--all"),
                    log,
                )?;
                layout.layer(
                    &b,
                    "application/x-git-bundle",
                    &format!("{}.bundle", sanitize(url)),
                    "repo",
                    Some(url),
                )?;
                sum.repos += 1;
            }
            _ => {}
        }
    }
    let t = work.path().join("sources.tar");
    if tar_paths(
        &dirs.state,
        &["cache/sources", "cache/oci", "cache/modules"],
        &t,
        log,
    )? {
        layout.layer(
            &t,
            "application/vnd.oci.image.layer.v1.tar",
            "sources.tar",
            "sources",
            None,
        )?;
        sum.sources = true;
    }
    drop(work);
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    sum.digest = layout.finish(json!({
        "dev.veneer.platform": platform(),
        "dev.veneer.version": env!("CARGO_PKG_VERSION"),
        "org.opencontainers.image.created": created.to_string(),
    }))?;
    Ok(sum)
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn blob(dir: &Path, digest: &str) -> PathBuf {
    dir.join("blobs/sha256")
        .join(digest.trim_start_matches("sha256:"))
}

fn verified(dir: &Path, desc: &Value) -> Result<PathBuf> {
    let digest = desc["digest"].as_str().unwrap_or_default();
    let path = blob(dir, digest);
    let mut f = std::fs::File::open(&path)
        .map_err(|e| ExecError::failed(format!("bundle blob {digest} is missing: {e}")))?;
    let mut h = Sha256::new();
    let size = std::io::copy(&mut f, &mut h).map_err(io)?;
    let actual = format!("sha256:{}", hex::encode(h.finalize()));
    if actual != digest || desc["size"].as_u64().is_some_and(|s| s != size) {
        return Err(ExecError::failed(format!(
            "bundle blob {digest} is corrupt (got {actual}, {size} bytes)"
        )));
    }
    Ok(path)
}

/// Restores a bundle into `dirs`, verifying every blob against its digest.
pub fn restore(dir: &Path, dirs: &Dirs, log: &mut Log) -> Result<Summary> {
    let index: Value = std::fs::read(dir.join("index.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| {
            ExecError::failed(format!(
                "{} is not an OCI layout (no index.json)",
                dir.display()
            ))
        })?;
    let desc = index["manifests"]
        .as_array()
        .and_then(|m| m.iter().find(|d| d["artifactType"] == ARTIFACT_TYPE))
        .ok_or_else(|| ExecError::failed(format!("{} holds no veneer bundle", dir.display())))?;
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(verified(dir, desc)?).map_err(io)?).map_err(io)?;
    let mut sum = Summary {
        digest: desc["digest"].as_str().unwrap_or_default().to_string(),
        ..Default::default()
    };
    let bundle_platform = manifest["annotations"]["dev.veneer.platform"]
        .as_str()
        .unwrap_or_default();
    std::fs::create_dir_all(&dirs.state).map_err(io)?;
    for layer in manifest["layers"].as_array().into_iter().flatten() {
        let path = verified(dir, layer)?;
        let kind = layer["annotations"]["dev.veneer.kind"]
            .as_str()
            .unwrap_or_default();
        let r = layer["annotations"]["dev.veneer.ref"]
            .as_str()
            .unwrap_or_default();
        match kind {
            "tools" | "sources" => {
                if kind == "tools" && bundle_platform != platform() {
                    return Err(ExecError::failed(format!(
                        "the bundle's tools are for {bundle_platform}, but this host is {}; run `veneer fetch` on {}",
                        platform(),
                        platform()
                    )));
                }
                run_logged(
                    Command::new("tar")
                        .arg("-xf")
                        .arg(&path)
                        .arg("-C")
                        .arg(&dirs.state),
                    log,
                )?;
                if kind == "tools" {
                    sum.tools = true;
                } else {
                    sum.sources = true;
                }
            }
            "image" => {
                let dest = crate::image_tar(dirs, r);
                std::fs::create_dir_all(dest.parent().unwrap()).map_err(io)?;
                std::fs::copy(&path, &dest).map_err(io)?;
                sum.images += 1;
            }
            "repo" => {
                let mirror = crate::repos::mirror_for(dirs, r);
                let _ = std::fs::remove_dir_all(&mirror);
                std::fs::create_dir_all(mirror.parent().unwrap()).map_err(io)?;
                run_logged(
                    Command::new("git")
                        .arg("clone")
                        .arg("--mirror")
                        .arg(&path)
                        .arg(&mirror),
                    log,
                )?;
                sum.repos += 1;
            }
            other => log.line(&format!("skipping unknown bundle layer kind `{other}`")),
        }
    }
    Ok(sum)
}

/// Environment that makes any network access by veneer or its children fail fast.
pub fn offline_env() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        ("VENEER_OFFLINE", "1"),
        ("MISE_OFFLINE", "1"),
        ("HTTP_PROXY", "http://127.0.0.1:9"),
        ("HTTPS_PROXY", "http://127.0.0.1:9"),
        ("http_proxy", "http://127.0.0.1:9"),
        ("https_proxy", "http://127.0.0.1:9"),
        ("ALL_PROXY", "http://127.0.0.1:9"),
        ("NO_PROXY", ""),
        ("no_proxy", ""),
        ("GIT_ALLOW_PROTOCOL", "file"),
    ])
}

pub fn is_offline() -> bool {
    std::env::var_os("VENEER_OFFLINE").is_some()
}
