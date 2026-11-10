//! Supply-chain metadata for `veneer bake --target oci` (standards proposal §5.3): OCI labels,
//! the environment fingerprint label, CycloneDX/SPDX SBOMs, SLSA provenance, and Sigstore
//! signatures. SBOMs and provenance are pushed as OCI 1.1 referrers with `oras attach`, or
//! as signed attestations with `cosign attest` when signing. veneer implements no crypto.

use std::path::{Path, PathBuf};
use std::process::Command;

use veneer_exec::{ExecError, Log, Result, run_logged};

use crate::run::Loaded;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SbomFormat {
    Cyclonedx,
    Spdx,
}

#[derive(Debug, Clone, clap::Args)]
pub struct SupplyArgs {
    /// Generate SBOMs (comma-separated: cyclonedx,spdx); attached to pushed images.
    #[arg(long, value_enum, value_delimiter = ',', num_args = 0..)]
    pub sbom: Vec<SbomFormat>,
    /// Generate SLSA v1 provenance (in-toto); attached to pushed images.
    #[arg(long)]
    pub provenance: bool,
    /// Sign the pushed image (and attest the SBOM/provenance) with cosign.
    #[arg(long)]
    pub sign: bool,
    /// cosign private key (default: keyless OIDC). Also reads $COSIGN_KEY.
    #[arg(long, env = "COSIGN_KEY", value_name = "KEY")]
    pub sign_key: Option<String>,
    /// Don't upload signatures to the Rekor transparency log (private/air-gapped registries).
    #[arg(long)]
    pub no_tlog: bool,
    /// The registry speaks plain HTTP (local registries).
    #[arg(long)]
    pub insecure_registry: bool,
    /// Where SBOM/provenance files are written (default: ./veneer-attestations).
    #[arg(long, value_name = "DIR")]
    pub attest_dir: Option<PathBuf>,
}

impl SupplyArgs {
    pub fn wanted(&self) -> bool {
        !self.sbom.is_empty() || self.provenance || self.sign
    }
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `SOURCE_DATE_EPOCH` (reproducible builds) or now.
pub fn build_time() -> u64 {
    std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(veneer_steps::sbom::now_secs)
}

/// Standard OCI annotation labels for the image.
pub fn labels(l: &Loaded, project: &Path, tag: &str) -> Vec<(String, String)> {
    let mut out = vec![
        (
            "org.opencontainers.image.title".to_string(),
            tag.rsplit_once(':')
                .map(|(r, _)| r)
                .unwrap_or(tag)
                .to_string(),
        ),
        (
            "org.opencontainers.image.created".into(),
            veneer_steps::sbom::rfc3339(build_time()),
        ),
        ("dev.veneer.spec".into(), crate::run::spec_digest(l)),
        (
            "dev.veneer.version".into(),
            env!("CARGO_PKG_VERSION").into(),
        ),
    ];
    if let Some((_, v)) = tag.rsplit_once(':').filter(|(_, t)| !t.contains('/')) {
        out.push(("org.opencontainers.image.version".into(), v.into()));
    }
    if let Some(src) = git(project, &["remote", "get-url", "origin"]) {
        out.push(("org.opencontainers.image.source".into(), src));
    }
    if let Some(rev) = git(project, &["rev-parse", "HEAD"]) {
        out.push(("org.opencontainers.image.revision".into(), rev));
    }
    out
}

fn capture(cmd: &mut Command) -> Option<String> {
    let out = cmd.output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The build-phase manifest the final `veneer apply --phase build` wrote inside the image.
pub fn image_manifest(tag: &str) -> Option<serde_json::Value> {
    let text = capture(Command::new("docker").args([
        "run",
        "--rm",
        "--entrypoint",
        "cat",
        tag,
        "/var/lib/veneer/build-manifest.json",
    ]))?;
    serde_json::from_str(&text).ok()
}

/// Rebuilds `tag` with one more label (metadata-only layer).
pub fn add_label(tag: &str, key: &str, value: &str, log: &mut Log) -> Result<()> {
    let dir = tempfile::tempdir().map_err(|e| ExecError::failed(e.to_string()))?;
    std::fs::write(
        dir.path().join("Containerfile"),
        format!(
            "FROM {tag}\nLABEL {key}={}\n",
            serde_json::to_string(value).unwrap()
        ),
    )
    .map_err(|e| ExecError::failed(e.to_string()))?;
    run_logged(
        Command::new("docker")
            .args(["build", "-q", "-t", tag, "-f"])
            .arg(dir.path().join("Containerfile"))
            .arg(dir.path()),
        log,
    )
}

/// Pushes `tag` and returns `repo@sha256:…`.
pub fn push(tag: &str, log: &mut Log) -> Result<String> {
    run_logged(Command::new("docker").args(["push", tag]), log)?;
    let repo = tag
        .rsplit_once(':')
        .filter(|(_, t)| !t.contains('/'))
        .map(|(r, _)| r)
        .unwrap_or(tag);
    let digests = capture(Command::new("docker").args([
        "image",
        "inspect",
        "--format",
        "{{range .RepoDigests}}{{println .}}{{end}}",
        tag,
    ]))
    .unwrap_or_default();
    digests
        .lines()
        .find(|d| d.starts_with(&format!("{repo}@")))
        .map(str::to_string)
        .ok_or_else(|| ExecError::failed(format!("cannot find the pushed digest of {tag}")))
}

/// A tool run through veneer's mise (`aqua:` backend), pinned for stable flags.
pub fn tool(dirs: &veneer_plan::Dirs, log: &mut Log, spec: &str, bin: &str) -> Result<Command> {
    let mise = veneer_steps::mise::ensure_mise(dirs, log)?;
    let mut c = veneer_steps::mise::mise_cmd(&mise, dirs);
    c.args(["exec", spec, "--", bin]);
    if let Ok(cwd) = std::env::current_dir() {
        c.current_dir(cwd);
    }
    Ok(c)
}

pub const ORAS: &str = "aqua:oras-project/oras@1.2.3";
const COSIGN: &str = "aqua:sigstore/cosign@2.5.3";

pub struct Outcome {
    pub files: Vec<PathBuf>,
    pub digest: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub fn finish(
    s: &SupplyArgs,
    l: &Loaded,
    tag: &str,
    base: &str,
    push_image: bool,
    started: u64,
    overlays: &[String],
    log: &mut Log,
) -> Result<Outcome> {
    let manifest = image_manifest(tag);
    if let Some(fp) = manifest.as_ref().and_then(|m| m["fingerprint"].as_str()) {
        add_label(tag, "dev.veneer.fingerprint", fp, log)?;
    }
    let digest = if push_image {
        Some(push(tag, log)?)
    } else {
        None
    };
    let mut out = Outcome {
        files: Vec::new(),
        digest: digest.clone(),
    };
    if !s.wanted() {
        return Ok(out);
    }
    let manifest = manifest.ok_or_else(|| {
        ExecError::failed("the image has no /var/lib/veneer/build-manifest.json to describe")
    })?;
    let dir = s
        .attest_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("veneer-attestations"));
    std::fs::create_dir_all(&dir).map_err(|e| ExecError::failed(e.to_string()))?;
    let (name, version) = tag
        .rsplit_once(':')
        .filter(|(_, t)| !t.contains('/'))
        .unwrap_or((tag, "latest"));
    let mut docs: Vec<(PathBuf, &str, &str)> = Vec::new(); // (file, media type, cosign --type)
    for f in &s.sbom {
        let (file, doc, media, ty) = match f {
            SbomFormat::Cyclonedx => (
                dir.join("sbom.cdx.json"),
                veneer_steps::sbom::cyclonedx(&manifest, name, version),
                "application/vnd.cyclonedx+json",
                "cyclonedx",
            ),
            SbomFormat::Spdx => (
                dir.join("sbom.spdx.json"),
                veneer_steps::sbom::spdx(&manifest, name, version),
                "application/spdx+json",
                "spdxjson",
            ),
        };
        std::fs::write(&file, serde_json::to_vec_pretty(&doc).unwrap())
            .map_err(|e| ExecError::failed(e.to_string()))?;
        docs.push((file, media, ty));
    }
    if s.provenance {
        let lock = crate::run::lock_path(&l.entry);
        let lock_digest = std::fs::read(&lock)
            .ok()
            .map(|b| format!("sha256:{}", veneer_steps::manifest::sha256_hex(&b)));
        let spec_digest = crate::run::spec_digest(l);
        let fp = manifest["fingerprint"].as_str().unwrap_or_default();
        let entry = l.entry.display().to_string();
        let subject = digest
            .as_deref()
            .map(|d| d.split('@').next().unwrap_or(d))
            .unwrap_or(name);
        let dig = digest
            .as_deref()
            .and_then(|d| d.split_once('@').map(|(_, x)| x.to_string()))
            .or_else(|| {
                capture(
                    Command::new("docker").args(["image", "inspect", "--format", "{{.Id}}", tag]),
                )
            })
            .unwrap_or_default();
        let facts = veneer_steps::sbom::BuildFacts {
            subject,
            digest: &dig,
            base,
            spec: &entry,
            spec_digest: &spec_digest,
            lock_digest: lock_digest.as_deref(),
            target: "oci",
            sets: &[],
            overlays,
            started,
            finished: veneer_steps::sbom::now_secs(),
            fingerprint: fp,
        };
        let statement = dir.join("provenance.intoto.json");
        std::fs::write(
            &statement,
            serde_json::to_vec_pretty(&veneer_steps::sbom::provenance(&facts)).unwrap(),
        )
        .map_err(|e| ExecError::failed(e.to_string()))?;
        let predicate = dir.join("provenance.slsa.json");
        std::fs::write(
            &predicate,
            serde_json::to_vec_pretty(&veneer_steps::sbom::slsa_predicate(&facts)).unwrap(),
        )
        .map_err(|e| ExecError::failed(e.to_string()))?;
        out.files.push(statement.clone());
        if s.sign {
            docs.push((predicate, "application/vnd.in-toto+json", "slsaprovenance1"));
        } else {
            docs.push((statement, "application/vnd.in-toto+json", ""));
        }
    }
    out.files.extend(docs.iter().map(|(f, _, _)| f.clone()));
    let Some(image) = digest else {
        if s.sign {
            return Err(ExecError::failed(
                "--sign needs --push (signatures live in the registry)",
            ));
        }
        return Ok(out);
    };
    if s.sign {
        let key_args = |c: &mut Command| {
            if let Some(k) = &s.sign_key {
                c.args(["--key", k]);
            }
            if s.no_tlog {
                c.arg("--tlog-upload=false");
            }
            if s.insecure_registry {
                c.arg("--allow-insecure-registry")
                    .arg("--allow-http-registry");
            }
            c.arg("--yes");
        };
        let mut c = tool(&l.dirs, log, COSIGN, "cosign")?;
        c.arg("sign");
        key_args(&mut c);
        c.arg(&image);
        run_logged(&mut c, log)?;
        for (file, _, ty) in &docs {
            let mut c = tool(&l.dirs, log, COSIGN, "cosign")?;
            c.arg("attest");
            key_args(&mut c);
            c.args(["--type", ty, "--predicate"]).arg(file).arg(&image);
            run_logged(&mut c, log)?;
        }
    } else {
        for (file, media, _) in &docs {
            let mut c = tool(&l.dirs, log, ORAS, "oras")?;
            c.args(["attach", "--artifact-type", media]);
            if s.insecure_registry {
                c.arg("--plain-http");
            }
            let parent = file.parent().unwrap_or(Path::new("."));
            let fname = file.file_name().unwrap().to_string_lossy().to_string();
            c.current_dir(parent)
                .arg(&image)
                .arg(format!("{fname}:{media}"));
            run_logged(&mut c, log)?;
        }
    }
    Ok(out)
}
