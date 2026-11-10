//! A minimal OCI Distribution client: pulls ORAS-style artifacts (spec sources) and
//! Dev Container Features. Anonymous or basic-auth bearer tokens; plain HTTP for
//! `localhost` registries or when `VENEER_OCI_INSECURE=1`.
//!
//! Layers are written by their `org.opencontainers.image.title` annotation. Tarball layers
//! (`…tar`, `…tar+gzip`, or ORAS `io.deis.oras.content.unpack=true`) are extracted.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub registry: String,
    pub repository: String,
    /// Tag or `sha256:` digest.
    pub reference: String,
}

impl Reference {
    /// Parses `registry/repo[:tag|@sha256:…]` (no scheme). Docker Hub short names are not
    /// supported: OCI artifacts always name their registry.
    pub fn parse(s: &str) -> Result<Self, String> {
        let (name, reference) = if let Some((n, d)) = s.split_once('@') {
            (n, d.to_string())
        } else {
            match s.rsplit_once(':') {
                Some((n, t)) if !t.contains('/') => (n, t.to_string()),
                _ => (s, "latest".to_string()),
            }
        };
        let (registry, repository) = name
            .split_once('/')
            .filter(|(r, _)| r.contains('.') || r.contains(':') || *r == "localhost")
            .ok_or_else(|| {
                format!("`{s}` must name a registry host (e.g. ghcr.io/org/name:tag)")
            })?;
        Ok(Self {
            registry: registry.to_string(),
            repository: repository.to_string(),
            reference,
        })
    }

    fn base(&self) -> String {
        let host = self.registry.split(':').next().unwrap_or_default();
        let insecure = std::env::var("VENEER_OCI_INSECURE").is_ok_and(|v| v == "1")
            || host == "localhost"
            || host == "127.0.0.1"
            || host.ends_with(".localhost");
        format!(
            "{}://{}/v2/{}",
            if insecure { "http" } else { "https" },
            self.registry,
            self.repository
        )
    }
}

#[derive(Debug, Deserialize)]
struct Descriptor {
    #[serde(rename = "mediaType", default)]
    media_type: String,
    digest: String,
    #[serde(default)]
    annotations: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(default)]
    layers: Vec<Descriptor>,
}

const ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into()
}

/// Parses `Bearer realm="…",service="…",scope="…"`.
fn challenge(header: &str) -> Option<(String, Vec<(String, String)>)> {
    let rest = header.strip_prefix("Bearer ")?;
    let mut realm = None;
    let mut params = Vec::new();
    for part in rest.split(',') {
        let (k, v) = part.trim().split_once('=')?;
        let v = v.trim_matches('"').to_string();
        if k == "realm" {
            realm = Some(v);
        } else {
            params.push((k.to_string(), v));
        }
    }
    Some((realm?, params))
}

struct Client {
    agent: ureq::Agent,
    token: Option<String>,
}

impl Client {
    fn get(&mut self, url: &str, accept: &str) -> Result<Vec<u8>, String> {
        for attempt in 0..2 {
            let mut req = self.agent.get(url).header("Accept", accept);
            if let Some(t) = &self.token {
                req = req.header("Authorization", &format!("Bearer {t}"));
            }
            let mut resp = req.call().map_err(|e| format!("GET {url}: {e}"))?;
            let status = resp.status().as_u16();
            if status == 401 && attempt == 0 {
                let header = resp
                    .headers()
                    .get("www-authenticate")
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                self.token = Some(self.fetch_token(&header)?);
                continue;
            }
            if !(200..300).contains(&status) {
                return Err(format!("GET {url}: HTTP {status}"));
            }
            return resp
                .body_mut()
                .with_config()
                .limit(1024 * 1024 * 1024)
                .read_to_vec()
                .map_err(|e| format!("GET {url}: {e}"));
        }
        Err(format!("GET {url}: unauthorized"))
    }

    fn fetch_token(&self, header: &str) -> Result<String, String> {
        let (realm, params) =
            challenge(header).ok_or_else(|| format!("unsupported auth challenge `{header}`"))?;
        let mut req = self.agent.get(&realm);
        for (k, v) in &params {
            req = req.query(k, v);
        }
        if let (Ok(u), Ok(p)) = (
            std::env::var("VENEER_OCI_USERNAME"),
            std::env::var("VENEER_OCI_PASSWORD"),
        ) {
            use base64_lite::encode;
            req = req.header(
                "Authorization",
                &format!("Basic {}", encode(format!("{u}:{p}").as_bytes())),
            );
        }
        let mut resp = req.call().map_err(|e| format!("token request: {e}"))?;
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("token response: {e}"))?;
        let body: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("token response: {e}"))?;
        body["token"]
            .as_str()
            .or_else(|| body["access_token"].as_str())
            .map(str::to_string)
            .ok_or_else(|| "token response has no token".to_string())
    }
}

mod base64_lite {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    pub fn encode(b: &[u8]) -> String {
        let mut out = String::new();
        for chunk in b.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |acc, (i, &v)| acc | (u32::from(v) << (16 - 8 * i)));
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }
}

/// Whether a layer is an archive to unpack. ORAS marks directories with the unpack
/// annotation and labels plain files `…layer.v1.tar`, so a titled layer without the
/// annotation is a file, unless it is a Dev Container Feature layer.
fn is_archive(d: &Descriptor) -> bool {
    if d.annotations
        .get("io.deis.oras.content.unpack")
        .is_some_and(|v| v == "true")
    {
        return true;
    }
    if d.media_type.contains("devcontainers.layer") {
        return true;
    }
    !d.annotations.contains_key("org.opencontainers.image.title")
        && (d.media_type.ends_with("tar") || d.media_type.ends_with("tar+gzip"))
}

fn extract(bytes: &[u8], gz: bool, dest: &Path) -> Result<(), String> {
    let reader: Box<dyn Read> = if gz {
        Box::new(flate2::read::GzDecoder::new(bytes))
    } else {
        Box::new(bytes)
    };
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive
        .unpack(dest)
        .map_err(|e| format!("cannot extract layer: {e}"))
}

/// Pulls `reference` into `dest` (replaced atomically). Returns the manifest digest.
pub fn pull(reference: &Reference, dest: &Path) -> Result<String, String> {
    let mut client = Client {
        agent: agent(),
        token: None,
    };
    let base = reference.base();
    let manifest_bytes =
        client.get(&format!("{base}/manifests/{}", reference.reference), ACCEPT)?;
    let digest = format!("sha256:{}", hex::encode(Sha256::digest(&manifest_bytes)));
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|e| format!("invalid manifest for {}: {e}", reference.repository))?;
    let tmp = dest.with_extension("tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    for layer in &manifest.layers {
        let blob = client.get(&format!("{base}/blobs/{}", layer.digest), "*/*")?;
        let actual = format!("sha256:{}", hex::encode(Sha256::digest(&blob)));
        if actual != layer.digest {
            return Err(format!(
                "blob digest mismatch: expected {}, got {actual}",
                layer.digest
            ));
        }
        let gz = layer.media_type.contains("gzip") || blob.starts_with(&[0x1f, 0x8b]);
        let title = layer
            .annotations
            .get("org.opencontainers.image.title")
            .cloned();
        if is_archive(layer) {
            extract(&blob, gz, &tmp)?;
        } else if let Some(t) = title {
            let rel = Path::new(&t);
            if rel.is_absolute()
                || rel
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(format!("refusing layer title `{t}` outside the artifact"));
            }
            let p = tmp.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
            std::fs::write(&p, &blob).map_err(|e| e.to_string())?;
        }
    }
    let _ = std::fs::remove_dir_all(dest);
    std::fs::create_dir_all(dest.parent().unwrap()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dest).map_err(|e| e.to_string())?;
    Ok(digest)
}

/// Cache directory for a reference.
pub fn cache_dir(cache: &Path, reference: &Reference) -> PathBuf {
    let key = format!(
        "{}/{}@{}",
        reference.registry, reference.repository, reference.reference
    );
    cache
        .join("oci")
        .join(&hex::encode(Sha256::digest(key.as_bytes()))[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_references() {
        let r = Reference::parse("ghcr.io/acme/envs:v3").unwrap();
        assert_eq!(
            (
                r.registry.as_str(),
                r.repository.as_str(),
                r.reference.as_str()
            ),
            ("ghcr.io", "acme/envs", "v3")
        );
        let r = Reference::parse("localhost:5000/x/y").unwrap();
        assert_eq!(
            (r.registry.as_str(), r.reference.as_str()),
            ("localhost:5000", "latest")
        );
        let r = Reference::parse("ghcr.io/a/b@sha256:abc").unwrap();
        assert_eq!(r.reference, "sha256:abc");
        assert!(Reference::parse("ubuntu:24.04").is_err());
        assert!(r.base().starts_with("https://ghcr.io/v2/a/b"));
        assert!(
            Reference::parse("localhost:5000/x")
                .unwrap()
                .base()
                .starts_with("http://")
        );
    }

    #[test]
    fn auth_challenge_and_base64() {
        let (realm, params) = challenge(
            r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:a/b:pull""#,
        )
        .unwrap();
        assert_eq!(realm, "https://ghcr.io/token");
        assert_eq!(params.len(), 2);
        assert_eq!(base64_lite::encode(b"user:pass"), "dXNlcjpwYXNz");
        assert_eq!(base64_lite::encode(b"ab"), "YWI=");
    }
}
