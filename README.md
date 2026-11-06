<img src="assets/icon.svg" width="64" height="64" alt="">

# veneer

Define an agent workspace environment once in `.veneer/veneer.yaml`. veneer stages it the
same way, quickly, in every workspace an agent or benchmark worker needs: a plain VM, a
container, Kubernetes, a microVM, or a hosted sandbox.

```sh
curl -fsSL https://raw.githubusercontent.com/LukasParke/veneer/main/install.sh | sh -s -- apply github:acme/app@main
```

That one line installs a checksum-verified static binary, fetches the spec, and stages the
environment. On a pre-baked image only the per-workspace phase runs.

## What a spec covers

| Block | What it does | Built on |
|---|---|---|
| `packages` | OS packages | apt / apk / dnf / brew |
| `tools` | runtimes and CLIs | [mise](https://mise.jdx.dev) (pinned, isolated); `nix:<attr>` via Nix |
| `deps` | project installs from lockfiles | npm/pnpm/yarn/bun, uv/poetry/pip, cargo, go, bundler |
| `images` | pre-pulled container images | docker pull, or crane tarballs when there is no daemon |
| `secrets`, `env` | templated environment | minijinja |
| `workspace` | git clone / worktree / copy, templated files | git, mirror cache |
| `services` | databases, caches, workers | Docker Compose, or supervised native processes |
| `hooks` | build- and workspace-time commands | `creates:` / `unless:` guards |
| `use` | reusable modules: `veneer/harness/{claude-code,codex,pi,omp,opencode}@1` | |
| `mcp`, `harnesses` | one MCP server list, rendered into each harness's own config | |
| `defaults` | retries and timeouts (hooks also take `retries:` / `timeout:`) | |

Specs split across files with `$ref` (JSON Reference), layer with `extends`, vary with
[Overlay 1.0](https://spec.openapis.org/overlay/v1.0.0.html) files, and import existing
`mise.toml`, `.tool-versions`, `compose.yaml`, lockfiles, and `Brewfile`s through `from:`.
`extends: [../.devcontainer/devcontainer.json]` reuses a Dev Container definition: image,
env, lifecycle commands, and [Features](https://containers.dev/features) (pulled over OCI).
`extends: [../devfile.yaml]` does the same for a [Devfile](https://devfile.io) 2.x.

Specs and layers can live anywhere: a local path, `github:org/repo//path@ref`,
`git+https://…`, `https://…`, or an OCI artifact (`oci://ghcr.io/org/envs:v1`, push with
`oras push`). Remote sources are pinned by commit or manifest digest in `.veneer/veneer.lock`.

## Commands

```sh
veneer init                         # scaffold .veneer/ from files the project already has
veneer check [--resolved]           # validate; print the merged spec
veneer apply [--id W] [--phase build|workspace]   # stage (idempotent, safe to interrupt)
veneer diff                         # what would run, and why
veneer exec -- <cmd>                # run with the workspace env and tools on PATH
veneer env --format sh|json|dotenv  # print the env (secrets masked)
veneer down [--rm]                  # stop services (and delete the workspace)
veneer bake --target oci|rootfs|qcow2|disk|microvm|cloud-init
                                    #   oci: OCI labels + fingerprint; --push --sbom cyclonedx,spdx
                                    #        --provenance (SLSA v1) --sign (cosign), as OCI referrers
                                    #   disk: -o x.raw|qcow2|vmdk|vdi|vhd [--vhd-fixed]|ova
                                    #   microvm: kernel + rootfs + Firecracker/Cloud Hypervisor/QEMU launchers
                                    #   cloud-init: user-data [--seed seed.iso] for stock cloud images
veneer render k8s --image I [--count N]   # Pod, or Indexed Job, with sidecar services
veneer up --count N [--image I]     # create N sandboxes (docker|podman), stage each
veneer ls | enter <id> | rm --all   # manage sandboxes
veneer vm snapshot|restore <bundle> # Firecracker: snapshot a staged microVM, resume copies in ms
veneer vm boot --vmm firecracker|cloud-hypervisor|qemu <bundle>
veneer lsp                          # language server: diagnostics, hover, completion
veneer entrypoint -- <cmd>          # container entrypoint: workspace phase, then exec
veneer lock [--update]              # pin sources, tool versions, image digests, repo commits
veneer apply --frozen               # fail on anything not pinned
veneer verify [--expect sha256:…]   # re-observe the workspace; compare its fingerprint
veneer sbom --format cyclonedx|spdx # SBOM of a workspace, from its manifest
veneer doctor                       # host requirements, checked before any step runs
veneer fetch -o DIR                 # offline bundle (OCI image layout)
veneer apply --from DIR             # restore it and run with the network forbidden
veneer schema                       # JSON Schema for editors
```

In other Dockerfiles: `COPY --from=ghcr.io/lukasparke/veneer:1 /veneer /usr/local/bin/veneer`.

Every step goes check → apply → verify, with a write-ahead journal. If a run is killed at
any point (Ctrl-C, `kill -9`, OOM), rerunning `veneer apply` converges to the same state as
a clean run. The container suite checks this on every distro.

Every apply records a manifest of what the workspace actually contains (tool and package
versions, image digests, repo commits, file digests) and an RFC 8785 fingerprint that is the
same for any two identical workspaces, whatever their id, root, ports, or secrets. Traces go
to any OpenTelemetry collector when `OTEL_EXPORTER_OTLP_ENDPOINT` is set (W3C `TRACEPARENT`
is honored and passed to every step).

## Development

```sh
cargo test --workspace                 # unit + integration tests
tests/containers/build-linux.sh        # static musl binary for containers
tests/containers/run.sh                # 17 suites incl. kind, cosign, OTel, and KVM boots (see header)
```

Design: [spec](docs/superpowers/specs/2026-11-03-veneer-design.md) ·
[M1 plan](docs/superpowers/plans/2026-11-03-veneer-m1-spec-core.md) ·
[M2–M7 plan](docs/superpowers/plans/2026-11-03-veneer-m2-m7-engine-bake-dist.md) ·
[standards proposal (M8–M15)](docs/superpowers/specs/2026-11-05-veneer-standards-proposal.md)
