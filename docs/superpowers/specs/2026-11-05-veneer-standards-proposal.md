# veneer: standards, portability, and resilience proposal

- **Status:** Accepted (decisions in §8 resolved 2026-11-05)
- **Date:** 2026-11-05
- **Builds on:** `2026-11-03-veneer-design.md`

## 1. Summary

veneer's job is narrow: **make an agent workspace identical and fast to create in any runtime.**
This proposal keeps that job narrow and makes veneer more portable and resilient in four ways:

1. **Standards out:** veneer emits a small set of universal artifacts (OCI, cloud-init, disk
   images, kernel+rootfs, Kubernetes manifests) instead of per-cloud targets.
2. **Standards in:** veneer ingests the formats people already have (devcontainer.json,
   Compose, Devfile, `.tool-versions`) and keeps `veneer.yaml` as thin glue.
3. **Provable environments:** every run produces a canonical manifest and fingerprint, with
   an SBOM, provenance, and signatures derived from it.
4. **Resilience:** everything is locked by digest, network steps retry, there's an offline
   bundle, and host dependencies are checked before any work starts.

**Already done with this proposal:** the `ami` bake target is removed. Publishing to clouds is
out of scope (§3). The supported answer for VMs is a standard disk image or cloud-init (§5.2).

## 2. Scope test

A feature belongs in veneer only if it answers yes to:

> Does this make an agent workspace **identical** and **fast** to create in **another runtime**,
> using a format that runtime already understands?

**Architecture rule:** there's one core and many thin adapters.

- **Core:** a static musl binary that runs `veneer apply` and converges idempotently. It's
  journaled, safe to resume after `kill -9`, and its events are NDJSON. Every runtime ends up
  running this.
- **Input adapters:** standard formats are translated into the spec tree. They hold no runtime
  logic.
- **Output adapters:** these turn a plan into a standard artifact. They never call cloud APIs.

## 3. Non-goals (explicit)

| Out of scope | Use instead |
|---|---|
| Publishing to cloud image registries (AMI, GCE images, Azure images) | `disk` + the cloud's own import tool, or cloud-init on a stock image |
| Native providers for Modal, E2B, Daytona, Fly, or Cloud Run | They all accept OCI images: `bake --target oci --push` |
| Provisioning infrastructure (networks, clusters, buckets) | Terraform / OpenTofu / Pulumi |
| Being a container runtime, VMM, or orchestrator | Docker/Podman, Firecracker/Cloud Hypervisor, Kubernetes |
| Workstation dotfiles sync | chezmoi, home-manager |
| Implementing crypto or signing | Sigstore cosign (shelled out) |

## 4. Standards inventory

### Already supported

| Area | Standard | Governance |
|---|---|---|
| Container image | OCI Image + Distribution Spec 1.1, OCI artifacts | OCI / Linux Foundation |
| Spec structure | JSON Schema 2020-12, JSON Reference, JSONPath (RFC 9535) | IETF / json-schema.org |
| Spec patching | Overlay Specification 1.0 | OpenAPI Initiative |
| Dev environments | Dev Container spec + Features | containers.dev |
| Services | Compose Specification | compose-spec.io |
| Editor | Language Server Protocol 3.17 | Open spec |
| Disks | ext4, raw, qcow2, VMDK, VHD, VDI (via d2vm) | Linux / QEMU / vendor-published specs |
| Tool versions | `.tool-versions`, idiomatic version files (through mise) | De facto |

### Added by this proposal

| Area | Standard | Section |
|---|---|---|
| VM first boot | cloud-init (`#cloud-config`, NoCloud datasource) | §5.2 |
| Canonical hashing | JSON Canonicalization Scheme (RFC 8785) | §5.1 |
| SBOM | CycloneDX 1.6, SPDX 2.3 | §5.3 |
| Provenance | in-toto Statement v1 + SLSA Provenance v1 | §5.3 |
| Signing | Sigstore (cosign), attached as OCI 1.1 referrers | §5.3 |
| Offline bundles | OCI Image Layout, `git bundle` | §5.4 |
| VM appliance | DMTF OVF 2.x (OVA) | §5.5 |
| Orchestration | Kubernetes Pod / Job / Indexed Job | §5.6 |
| Telemetry | OpenTelemetry OTLP, W3C Trace Context | §5.7 |
| Cloud dev envs | Devfile 2.x (CNCF) | §5.8 |
| Agent config | MCP server config, `AGENTS.md` | §5.8 |

## 5. Proposals

Each one lists the user-facing surface, the design, how it's tested, and the effort for an
agent working with the existing test harness.

---

### 5.1 Environment manifest and fingerprint (foundation)

**Why:** benchmarks need proof that two runs on two runtimes got the same environment. It's
also the single source for the SBOM (§5.3) and the lock (§5.4).

**Surface**

```sh
veneer apply ... --manifest out/manifest.json   # also written to the state dir on every run
veneer verify [spec] --id <ws>                  # re-observes the workspace, compares, exits 0/1
veneer verify --expect sha256:<fingerprint>     # assert a known-good environment
```

**Design**

- The manifest is the observed result, not the requested one. It records:
  - the resolved spec digest
  - each tool's name, version, platform, and artifact sha256
  - OS package names and versions (dpkg/apk/rpm query)
  - image digests
  - repo commits and worktree heads
  - the sha256 of each managed file. Secret-bearing files record only the fact that a secret
    was present, never a hash of the content.
  - service names and their health
- Volatile fields (timestamps, PIDs, paths containing the workspace id) go in a separate
  `observed` section and are left out of the fingerprint.
- **Fingerprint:** sha256 over the RFC 8785 canonical JSON of the stable section.
- The chaos test already compares fingerprints internally. This turns that into a public
  contract.

**Tests**

- A clean run and a run after `kill -9` produce equal fingerprints. The existing chaos suite
  switches to `verify`.
- The same spec on Ubuntu and on Debian produces different fingerprints, with a readable diff
  of what changed.

**Effort:** ~1 day.

---

### 5.2 cloud-init target (largest portability gain)

**Why:** every major cloud and hypervisor runs cloud-init on first boot. One target covers EC2,
GCE, Azure, Hetzner, DigitalOcean, OpenStack, Proxmox, Lima, and Multipass, on their **stock**
images, with no publishing step. It replaces what `ami` was for.

**Surface**

```sh
veneer bake --target cloud-init -o user-data              # #cloud-config file
veneer bake --target cloud-init --seed seed.iso           # NoCloud seed ISO (label "cidata")
veneer bake --target cloud-init --project oci://ghcr.io/acme/agent-env:v3   # large projects
```

**Design**

- The generated `#cloud-config` does three things:
  1. installs a pinned veneer release, with its sha256 inlined into the user-data
  2. delivers the project
  3. runs `veneer apply` for both phases, then writes the ready marker to the console and to
     `/run/veneer/ready`
- **Project delivery:**
  - Small projects are embedded as a base64 `write_files` tarball. A size guard warns above
    16 KiB, the smallest common user-data limit.
  - Otherwise veneer pushes the project as an OCI artifact (reusing the existing OCI client),
    and user-data pulls it by digest.
- **Secrets are never embedded.** The generated config reads them from the instance at boot
  (env, file, or a command), as the spec's `secrets:` already declares. Plan-time diagnostics
  reject a secret with an inline value for this target.
- Optional `--prebaked`: when the image is already from `bake --target disk`, user-data runs
  only the workspace phase.

**Tests**

- Boot a stock Ubuntu cloud image and a stock Debian cloud image under QEMU/KVM (Lima) with
  the NoCloud seed ISO. Wait for the ready marker, then `veneer verify` the fingerprint against
  a container run of the same spec.
- `cloud-init schema --config-file user-data` validates the output (run in a container).

**Effort:** ~1 day.

---

### 5.3 Supply-chain metadata: labels, SBOM, provenance, signatures

**Why:** a benchmark result should name the exact environment that produced it, and anyone
should be able to check that claim.

**Surface**

```sh
veneer bake --target oci --push --sbom cyclonedx,spdx --provenance --sign
veneer sbom [spec] --id <ws> --format cyclonedx|spdx     # for non-OCI targets
```

**Design**

- **OCI labels on every image:** `org.opencontainers.image.{source,revision,created,version,title}`
  and `dev.veneer.fingerprint`.
- **SBOM:** generated from the §5.1 manifest, so it's authoritative rather than scanned.
  CycloneDX 1.6 is the default, and SPDX 2.3 is available on request.
- **Provenance:** an in-toto Statement v1 with a SLSA Provenance v1 predicate. It records the
  resolved spec digest, lock digest, veneer version, and builder identity.
- **Attachment:** pushed as OCI 1.1 referrers (`subject` = image digest) using the existing
  registry client. If a registry doesn't support referrers, the fallback is the tag schema
  (`sha256-<digest>.sbom`).
- **Signing:** veneer shells out to `cosign`, which handles both keyless OIDC and key
  signing. veneer doesn't implement any cryptography. `--sign` without cosign installed fails
  in preflight (§5.4).

**Tests:** in a local `registry:2`:

- push, then check that `cosign verify` and `cosign verify-attestation` succeed
- the SBOM validates against the CycloneDX schema
- each referrer is listed under the image digest

**Effort:** ~1.5 days.

---

### 5.4 Resilience: full lock, retries, preflight, offline bundles

**Why:** agent fleets and benchmarks fail on flaky networks, registry drift, and missing host
tools. veneer should either fail before doing any work, or not fail at all.

**5.4.1 Lock everything by digest.** Today `veneer.lock` pins only remote spec sources. It will
now also pin:

| Input | Locked as |
|---|---|
| Tools | resolved version + per-platform artifact sha256 (written to `mise.lock` too, for mise's own verification) |
| Images | manifest digest (`name@sha256:…`) |
| Repos | commit SHA per ref |
| Features / modules | OCI digest (already) |
| OS packages | recorded in the manifest. Exact pinning only for distros that support snapshot mirrors (`snapshot.debian.org`, `snapshot.ubuntu.com`), opt-in. |

`veneer lock` refreshes the lock, and `--frozen` fails on any drift. CI and benchmarks use
`--frozen`.

**5.4.2 Retries and timeouts.**

- Every network step retries transient failures 4 times with exponential backoff and jitter
  (1s → 8s), honoring `Retry-After`.
- Transient means connection errors, 5xx, and 429. 4xx and checksum mismatches fail
  immediately.
- Spec: `timeout:` per step and a `defaults.retries:` override. The CLI gets `--timeout` for
  the whole run.
- Retries and timeouts are reported in the NDJSON events.

**5.4.3 Preflight (`veneer doctor`).**

- `apply` and `bake` first compute which host dependencies the plan needs, for example docker
  for compose, nix for `nix:` tools, cosign for `--sign`, and a CA bundle for crane.
- If any are missing, it reports them all at once with diagnostic codes (V5xxx) before
  touching anything.
- `veneer doctor [spec]` prints the same report on its own.

**5.4.4 Offline bundles.**

```sh
veneer fetch [spec] -o bundle/          # or -o oci://registry/acme/agent-env-bundle:v3
veneer apply [spec] --offline --from bundle/
```

- The bundle is an **OCI Image Layout** directory, so it can be copied as files or pushed to a
  registry as one artifact. It contains:
  - tool artifacts per target platform
  - images as OCI layouts
  - repos as `git bundle` files
  - remote spec sources and the lock
- `--offline` sets mise's offline behavior, points crane at the local layout, and clones from
  bundles. Any network access is a hard error, which makes air-gapped benchmark runs provable.

**Tests**

- Run `apply --offline` under `docker run --network none` from a fetched bundle, and check its
  fingerprint equals an online run.
- A toxiproxy container injects resets and 503s into the registry, and the run still
  converges.
- With docker missing, the doctor lists the missing dependency, and the journal shows that no
  step ran.

**Effort:** ~3 days. Offline bundles are about half of that.

---

### 5.5 Generic microVM bundle and standard disk formats

**Why:** a kernel, an ext4 rootfs, and a kernel command line boot on almost every VMM. Only the
config file is Firecracker-specific.

**Surface**

```sh
veneer bake --target microvm --vmm firecracker|cloud-hypervisor|qemu --kernel vmlinux -o vm/
veneer bake --target firecracker ...    # stays, as an alias for --target microvm --vmm firecracker
veneer bake --target disk -o env.ova    # adds OVA; raw/qcow2/vmdk/vhd/vdi already work
veneer bake --target disk -o env.vhd --vhd-fixed   # fixed, 1 MiB-aligned VHD (Hyper-V/Azure-compatible)
```

**Design**

- The bundle is always `vmlinux`, `rootfs.ext4` (with `/sbin/veneer-init`), and `cmdline`.
  Each VMM gets a config or launch script:
  - Firecracker: `vm-config.json`
  - Cloud Hypervisor: `cloud-hypervisor.sh`, with `console=hvc0`
  - QEMU: `qemu.sh` using `-M microvm` on x86_64 and `-M virt` on arm64
- `veneer vm boot --vmm …` drives any of them. `snapshot`/`restore` stays Firecracker-only for
  now. Cloud Hypervisor snapshots (`ch-remote snapshot`) are a later follow-up.
- OVA is a DMTF OVF 2.x descriptor plus a streamOptimized VMDK, packed as tar with the OVF
  first.

**Tests**

- Under Lima KVM, boot the same bundle in Firecracker, Cloud Hypervisor, and QEMU, and check
  that all three reach the ready marker with equal fingerprints.
- Check the OVA with `ovftool` if it's available, otherwise with a structural check.
- Boot the qcow2 image under QEMU. This closes the gap where qcow2 was only checked
  structurally.

**Effort:** ~1.5 days.

---

### 5.6 Kubernetes manifests

**Why:** Kubernetes is the most common fleet runtime for agents and benchmarks. Indexed Jobs fit
"N identical workspaces" directly.

**Surface**

```sh
veneer render k8s [spec] --image ghcr.io/acme/agent-env@sha256:… [--count 50] > env.yaml
veneer render k8s [spec] --stage-at-start --image ubuntu:24.04   # no baked image
```

**Design**

- Output is plain YAML (Pod, or Job with `completionMode: Indexed` when `--count` > 1), with
  no CRDs. It works with `kubectl apply`, Kustomize, Helm, and Argo.
- **Workspace id** comes from `JOB_COMPLETION_INDEX` (`<name>-<index>`).
- **Secrets** become `secretKeyRef`/`envFrom` references to a named Secret. Values are never
  rendered.
- **Image-backed services** become native sidecars (an init container with
  `restartPolicy: Always`, which needs K8s ≥ 1.29). Native services keep running inside the
  workspace container.
- **`--stage-at-start`:** an init container copies veneer from the published GHCR image into an
  `emptyDir`, and the main container runs `veneer entrypoint`.
- Readiness probe: `/run/veneer/ready` exists.
- Later, not in this proposal: `veneer up --provider kubernetes`, on top of the same renderer.

**Tests**

- In a `kind` cluster inside Docker, apply an Indexed Job with `--count 3`. Check that all 3
  are ready with distinct workspace ids, that `kubectl exec … veneer verify` passes, and that
  sidecar services answer.
- Validate the output with `kubeconform`.

**Effort:** ~1.5 days.

---

### 5.7 OpenTelemetry

**Why:** benchmarks need per-step timing across fleets. OTLP is what every observability backend
accepts.

**Design**

- Export is enabled only when the standard `OTEL_EXPORTER_OTLP_ENDPOINT` (or `…_TRACES_…`) is
  set. It honors `OTEL_SERVICE_NAME`, `OTEL_RESOURCE_ATTRIBUTES`, and `OTEL_EXPORTER_OTLP_HEADERS`.
- **Spans:**
  - One run span, with a child span per step.
  - Attributes: step id, kind, action (`skip`/`apply`), retries, and the workspace id.
  - Span names never contain secret values. Attributes go through the existing redaction.
- **Context propagation:** if `TRACEPARENT` is set (W3C Trace Context), the run becomes a child
  of the caller's trace. Hooks and services receive `TRACEPARENT`, so their own spans nest under
  the step.
- **Transport:** OTLP/HTTP with JSON, using the existing HTTP client, so no gRPC stack is
  added. It's batched, and export failures never fail the run.

**Tests:** an `otel/opentelemetry-collector` container with the file exporter. Check the span
tree shape, the parent from `TRACEPARENT`, and that the redacted secret doesn't appear in the
exported file.

**Effort:** ~1 day.

---

### 5.8 More standard inputs: Devfile and agent configuration

**5.8.1 Devfile 2.x import** (`extends: [devfile.yaml]`), mapped the same way devcontainer.json
is mapped:

| Devfile | veneer |
|---|---|
| `components[].container.image` (first or `mountSources`) | `base` |
| `components[].container.env` | `env` |
| other container components | `services` (image) |
| `projects[]` (git remotes, checkoutFrom) | `workspace.repos` |
| `commands[]` with `group: build` | `hooks.build` |
| `events.postStart` | `hooks.workspace` |
| `components[].kubernetes/openshift` | warning W3003, ignored |

**5.8.2 Agent configuration.** This covers the harness setup from the original brief.

**Supported harnesses**

| Harness | Module | Install |
|---|---|---|
| Claude Code | `veneer/harness/claude-code@1` (exists) | npm `@anthropic-ai/claude-code` |
| Codex | `veneer/harness/codex@1` (exists) | npm `@openai/codex` |
| Pi | `veneer/harness/pi@1` (new) | npm `@earendil-works/pi-coding-agent` |
| OMP (oh-my-pi) | `veneer/harness/omp@1` (new) | npm `@oh-my-pi/pi-coding-agent` (binary `omp`). Upstream recommends Bun, so the module installs `bun` through mise. |
| OpenCode | `veneer/harness/opencode@1` (new) | npm `opencode-ai` |

All of them install through mise, so versions are locked by §5.4.1 like any other tool.

**New spec block `mcp:`.** MCP servers are declared once and rendered by each harness module:

```yaml
mcp:
  github:
    command: github-mcp-server
    args: [stdio]
    env: { GITHUB_TOKEN: "{{ secrets.github_token }}" }
  docs:
    url: https://mcp.example.com/mcp
    headers: { Authorization: "Bearer {{ secrets.docs_token }}" }
```

| Harness | Rendered file | Shape | Secret reference |
|---|---|---|---|
| Claude Code | `<repo>/.mcp.json` | `mcpServers.<name>`: `command`/`args`/`env`, or `type: http` + `url`/`headers` | `${VAR}` |
| Codex | `$CODEX_HOME/config.toml` | `[mcp_servers.<name>]`: `command`/`args`/`env`, or `url` | literal value, file mode 0600 |
| Pi | `~/.pi/agent/mcp.json` | `mcpServers.<name>`: `command`/`args`/`env`/`cwd`, or `url`/`headers`. HTTP only, no SSE. | `${VAR}` |
| OMP | `<repo>/.omp/mcp.json` | `mcpServers.<name>`: stdio by default, or `type: http` + `url`/`headers` | `${VAR}` |
| OpenCode | `<repo>/opencode.json` (`mcp` key, merged into an existing file) | `type: local` + `command: [cmd, ...args]` + `environment`, or `type: remote` + `url`/`headers` | `{env:VAR}` |

**Notes on the table**

- **Pi is rendered at user level** because Pi reads project `.pi/mcp.json` only after the user
  grants project trust. That would block a headless agent run. The workspace's `$HOME` is
  already per-workspace, so user level is still isolated.
- **OMP also auto-imports** other harnesses' configs (`.mcp.json`, OpenCode, Codex). The module
  still writes `.omp/mcp.json` so nothing depends on import order (first definition wins).
- **Secrets:** values come from the existing `secrets:` mechanism. Rendered files use each
  harness's env-reference syntax, and the secret is exported only to the harness's process
  environment (`veneer exec`). Codex gets the literal value in a 0600 file. Values never
  appear in events or logs (existing redaction).
- **No `!command` values.** Pi and OMP can run shell commands from config values. veneer never
  renders that form, because it would run code whenever config loads.

**`AGENTS.md`:** `workspace.files` already places files.

- Codex, Pi, OMP, and OpenCode read `AGENTS.md` from the repo natively.
- The Claude Code module adds a `CLAUDE.md → AGENTS.md` symlink, unless a `CLAUDE.md` already
  exists.
- The result is one instruction file for all five harnesses.

**Tests**

- Import a Devfile fixture, apply it in the distros suite, and check the repos, hooks, and env.
- Render one `mcp:` block (one stdio server, one HTTP server) for all five harnesses:
  - Validate each output against the harness's schema where one is published (OpenCode
    publishes `https://opencode.ai/config.json`).
  - Then use each harness's own listing command to confirm both servers load (`claude mcp list`,
    `codex mcp list`, and the Pi, OMP, and OpenCode equivalents). No model API key is needed.
- A secret-scan check confirms that no secret value appears in any rendered file except the
  0600 Codex file.

**Effort:** Devfile ~1 day; agent config ~2 days (3 new harness modules + 5 renderers).

## 6. Delivery plan

Ordered so each milestone builds on the previous one. Effort is in focused agent-days.

| Milestone | Contents | Depends on | Effort |
|---|---|---|---|
| **M8 Foundation** | §5.1 manifest + fingerprint + `verify` | none | 1 d |
| **M9 Portability** | §5.2 cloud-init | M8 | 1 d |
| **M10 Resilience** | §5.4 full lock, retries, doctor, offline | M8 | 3 d |
| **M11 Supply chain** | §5.3 labels, SBOM, provenance, cosign | M8 | 1.5 d |
| **M12 VMs** | §5.5 generic microVM, OVA, fixed VHD, qcow2 boot test | none | 1.5 d |
| **M13 Kubernetes** | §5.6 render k8s | M8 | 1.5 d |
| **M14 Telemetry** | §5.7 OTLP + Trace Context | none | 1 d |
| **M15 Inputs** | §5.8 Devfile, `mcp:` + `AGENTS.md`, Pi / OMP / OpenCode modules | none | 3 d |

**Total:** about 13.5 agent-days. M8 and M9 alone (2 days) deliver the biggest gains:
provable environments, and every VM cloud without publishing.

**Compatibility:**

- Every addition is opt-in or backward compatible.
- `--target firecracker` stays as an alias.
- The spec version stays `1`. The new keys (`mcp:`, `timeout:`, `defaults:`) are additive and
  validated by the JSON Schema.

## 7. Risks

| Risk | Mitigation |
|---|---|
| User-data size limits (16 KiB on EC2) | Size guard + OCI-artifact project delivery (§5.2) |
| OS packages aren't reproducible without snapshot mirrors | Record them exactly in the manifest. Pinning via snapshot mirrors is opt-in. The fingerprint still detects drift. |
| Registries without OCI 1.1 referrers | Fall back to the tag schema |
| MCP config formats keep changing | Rendering lives in versioned harness modules (`@1`, `@2`), not in core |
| More external tools (cosign, cloud-hypervisor, kind) | Each is optional and checked in preflight. None of them is needed to run `apply`. |
| Offline bundle size for multi-platform tools | `fetch --platform` picks the targets. The default is the host platform. |

## 8. Decisions (resolved 2026-11-05)

1. **SBOM format:** CycloneDX 1.6 is the default. SPDX 2.3 is available with `--sbom spdx`.
2. **Kubernetes:** M13 ships the renderer only. `veneer up --provider kubernetes` comes later.
3. **OS package pinning:** pinning through snapshot mirrors is deferred. Packages are recorded
   exactly in the manifest, and the fingerprint detects drift.
4. **Harnesses:** Claude Code and Codex (existing), plus Pi, OMP, and OpenCode (new in M15).
