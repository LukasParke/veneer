//! SBOM and provenance documents derived from the environment manifest (standards
//! proposal §5.3). The manifest is what veneer *observed* it installed, so these are
//! authoritative rather than scanned.
//!
//! - CycloneDX 1.6 JSON (default) and SPDX 2.3 JSON
//! - in-toto Statement v1 with a SLSA Provenance v1 predicate

use serde_json::{Value, json};

/// RFC 3339 UTC timestamp for unix seconds (civil-from-days, no time zone database).
pub fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Howard Hinnant's days-from-civil inverse.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A UUID (version 8, RFC 9562) derived from the fingerprint, so one environment always
/// gets the same serial number.
fn uuid_from(fp: &str) -> String {
    let h = blake3::hash(fp.as_bytes());
    let mut b = [0u8; 16];
    b.copy_from_slice(&h.as_bytes()[..16]);
    b[6] = (b[6] & 0x0f) | 0x80;
    b[8] = (b[8] & 0x3f) | 0x80;
    let x = hex::encode(b);
    format!(
        "{}-{}-{}-{}-{}",
        &x[..8],
        &x[8..12],
        &x[12..16],
        &x[16..20],
        &x[20..]
    )
}

fn pct(s: &str) -> String {
    s.replace('%', "%25").replace('@', "%40")
}

/// Package URL (purl) for an OS package.
fn os_purl(os: &Value, name: &str, version: &str) -> String {
    let id = os["id"].as_str().unwrap_or("linux");
    let ty = match id {
        "ubuntu" | "debian" => "deb",
        "alpine" => "apk",
        "fedora" | "rhel" | "centos" | "rocky" | "almalinux" | "amzn" => "rpm",
        _ => "generic",
    };
    format!("pkg:{ty}/{id}/{}@{version}", pct(name))
}

/// purl for a mise tool (`npm:`, `pipx:`/`pypi:`, `cargo:`, `go:`, `aqua:`/`github:`, core).
fn tool_purl(name: &str, version: &str) -> String {
    let (backend, rest) = name.split_once(':').unwrap_or(("", name));
    match backend {
        "npm" => format!("pkg:npm/{}@{version}", pct(rest)),
        "pipx" | "pypi" => format!("pkg:pypi/{}@{version}", rest.to_lowercase()),
        "cargo" => format!("pkg:cargo/{rest}@{version}"),
        "go" => format!("pkg:golang/{rest}@{version}"),
        "aqua" | "github" | "ubi" => format!("pkg:github/{rest}@{version}"),
        _ => format!("pkg:generic/{}@{version}", pct(rest)),
    }
}

/// (name, version, purl, kind) for every component in the manifest.
fn components(m: &Value) -> Vec<(String, String, String, &'static str)> {
    let st = &m["stable"];
    let mut out = Vec::new();
    for p in st["packages"].as_array().into_iter().flatten() {
        let name = p["name"].as_str().unwrap_or_default();
        let version = p["version"].as_str().unwrap_or("unknown");
        out.push((
            name.into(),
            version.into(),
            os_purl(&st["os"], name, version),
            "library",
        ));
    }
    for t in st["tools"].as_array().into_iter().flatten() {
        let name = t["name"].as_str().unwrap_or_default();
        for v in t["versions"].as_array().into_iter().flatten() {
            let v = v.as_str().unwrap_or_default();
            out.push((name.into(), v.into(), tool_purl(name, v), "application"));
        }
    }
    for n in st["nix"].as_array().into_iter().flatten() {
        let r = n.as_str().unwrap_or_default();
        out.push((
            r.into(),
            "unknown".into(),
            format!("pkg:nix/{}", pct(r)),
            "application",
        ));
    }
    for i in st["images"].as_array().into_iter().flatten() {
        let r = i["ref"].as_str().unwrap_or_default();
        let id = i["id"].as_str().unwrap_or("unknown");
        let (repo, tag) = r
            .rsplit_once(':')
            .filter(|(_, t)| !t.contains('/'))
            .unwrap_or((r, "latest"));
        out.push((
            r.into(),
            id.into(),
            format!(
                "pkg:oci/{}@{}?repository_url={}&tag={tag}",
                repo.rsplit('/').next().unwrap_or(repo),
                pct(id),
                repo
            ),
            "container",
        ));
    }
    for r in st["repos"].as_array().into_iter().flatten() {
        let url = r["url"].as_str().unwrap_or_default();
        let commit = r["commit"].as_str().unwrap_or("unknown");
        out.push((
            url.into(),
            commit.into(),
            format!(
                "pkg:generic/{}@{commit}?vcs_url={}",
                pct(r["path"].as_str().unwrap_or("repo")),
                pct(url)
            ),
            "application",
        ));
    }
    out
}

/// CycloneDX 1.6 JSON BOM for an environment named `name` at `version`.
pub fn cyclonedx(m: &Value, name: &str, version: &str) -> Value {
    let fp = m["fingerprint"].as_str().unwrap_or_default();
    let comps: Vec<Value> = components(m)
        .into_iter()
        .enumerate()
        .map(|(i, (n, v, purl, kind))| {
            json!({"type": kind, "bom-ref": format!("c{i}"), "name": n, "version": v, "purl": purl})
        })
        .collect();
    json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.6",
        "serialNumber": format!("urn:uuid:{}", uuid_from(fp)),
        "version": 1,
        "metadata": {
            "timestamp": rfc3339(now_secs()),
            "tools": {"components": [{"type": "application", "name": "veneer", "version": env!("CARGO_PKG_VERSION")}]},
            "component": {
                "type": "container",
                "bom-ref": "root",
                "name": name,
                "version": version,
                "properties": [{"name": "dev.veneer.fingerprint", "value": fp}],
            },
        },
        "components": comps,
    })
}

/// SPDX 2.3 JSON document.
pub fn spdx(m: &Value, name: &str, version: &str) -> Value {
    let fp = m["fingerprint"].as_str().unwrap_or_default();
    let mut packages = vec![json!({
        "name": name,
        "SPDXID": "SPDXRef-Package-root",
        "versionInfo": version,
        "downloadLocation": "NOASSERTION",
        "filesAnalyzed": false,
        "primaryPackagePurpose": "CONTAINER",
        "comment": format!("veneer environment fingerprint {fp}"),
    })];
    let mut relationships = vec![json!({
        "spdxElementId": "SPDXRef-DOCUMENT",
        "relationshipType": "DESCRIBES",
        "relatedSpdxElement": "SPDXRef-Package-root",
    })];
    for (i, (n, v, purl, _)) in components(m).into_iter().enumerate() {
        let id = format!("SPDXRef-Package-{i}");
        packages.push(json!({
            "name": n,
            "SPDXID": id,
            "versionInfo": v,
            "downloadLocation": "NOASSERTION",
            "filesAnalyzed": false,
            "externalRefs": [{"referenceCategory": "PACKAGE-MANAGER", "referenceType": "purl", "referenceLocator": purl}],
        }));
        relationships.push(json!({"spdxElementId": "SPDXRef-Package-root", "relationshipType": "CONTAINS", "relatedSpdxElement": id}));
    }
    json!({
        "spdxVersion": "SPDX-2.3",
        "dataLicense": "CC0-1.0",
        "SPDXID": "SPDXRef-DOCUMENT",
        "name": name,
        "documentNamespace": format!("https://veneer.dev/spdx/{}", fp.trim_start_matches("sha256:")),
        "creationInfo": {
            "created": rfc3339(now_secs()),
            "creators": [format!("Tool: veneer-{}", env!("CARGO_PKG_VERSION"))],
        },
        "packages": packages,
        "relationships": relationships,
    })
}

/// Inputs recorded in the SLSA provenance.
pub struct BuildFacts<'a> {
    pub subject: &'a str,
    /// `sha256:<hex>` of the pushed image manifest.
    pub digest: &'a str,
    pub base: &'a str,
    pub spec: &'a str,
    pub spec_digest: &'a str,
    pub lock_digest: Option<&'a str>,
    pub target: &'a str,
    pub sets: &'a [(String, String)],
    pub overlays: &'a [String],
    pub started: u64,
    pub finished: u64,
    pub fingerprint: &'a str,
}

/// The SLSA Provenance v1 predicate alone (what `cosign attest --predicate` takes).
pub fn slsa_predicate(f: &BuildFacts<'_>) -> Value {
    let mut deps = vec![
        json!({"uri": format!("pkg:oci/{}", f.base), "name": "base"}),
        json!({"uri": "veneer:spec", "name": f.spec, "digest": {"sha256": f.spec_digest.trim_start_matches("sha256:")}}),
    ];
    if let Some(l) = f.lock_digest {
        deps.push(json!({"uri": "veneer:lock", "name": "veneer.lock", "digest": {"sha256": l.trim_start_matches("sha256:")}}));
    }
    json!({
        "buildDefinition": {
            "buildType": "https://veneer.dev/bake/v1",
            "externalParameters": {
                "spec": f.spec,
                "target": f.target,
                "sets": f.sets.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>(),
                "overlays": f.overlays,
            },
            "internalParameters": {"veneer": env!("CARGO_PKG_VERSION"), "fingerprint": f.fingerprint},
            "resolvedDependencies": deps,
        },
        "runDetails": {
            "builder": {"id": std::env::var("VENEER_BUILDER_ID").unwrap_or_else(|_| "https://veneer.dev/builder/local".into())},
            "metadata": {
                "invocationId": format!("{}-{}", f.started, std::process::id()),
                "startedOn": rfc3339(f.started),
                "finishedOn": rfc3339(f.finished),
            },
        },
    })
}

/// in-toto Statement v1 wrapping the SLSA predicate.
pub fn provenance(f: &BuildFacts<'_>) -> Value {
    json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [{"name": f.subject, "digest": {"sha256": f.digest.trim_start_matches("sha256:")}}],
        "predicateType": "https://slsa.dev/provenance/v1",
        "predicate": slsa_predicate(f),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known_instants() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_793_836_800), "2026-11-05T00:00:00Z");
    }

    #[test]
    fn documents_cover_every_component() {
        let m = json!({
            "fingerprint": "sha256:abc",
            "stable": {
                "os": {"id": "ubuntu"},
                "packages": [{"name": "git", "version": "1:2.43.0-1"}],
                "tools": [{"name": "npm:@openai/codex", "versions": ["0.1.0"]}, {"name": "jq", "versions": ["1.7.1"]}],
                "images": [{"ref": "busybox:1.36", "id": "sha256:ff"}],
                "repos": [], "nix": [],
            }
        });
        let c = cyclonedx(&m, "acme/env", "v1");
        let purls: Vec<&str> = c["components"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["purl"].as_str().unwrap())
            .collect();
        assert_eq!(purls[0], "pkg:deb/ubuntu/git@1:2.43.0-1");
        assert_eq!(purls[1], "pkg:npm/%40openai/codex@0.1.0");
        assert_eq!(purls[2], "pkg:generic/jq@1.7.1");
        assert!(purls[3].starts_with("pkg:oci/busybox@sha256:ff?repository_url=busybox&tag=1.36"));
        assert!(c["serialNumber"].as_str().unwrap().starts_with("urn:uuid:"));
        let s = spdx(&m, "acme/env", "v1");
        assert_eq!(s["packages"].as_array().unwrap().len(), 5);
        assert_eq!(s["relationships"].as_array().unwrap().len(), 5);
    }
}
