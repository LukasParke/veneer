# veneer M2–M7: Engine, Steps, Bake, Remote, Distribution

> Executed inline after M1. Implements spec §3, §4.4–4.5, §5, §6, §7, §10 on top of the M1 resolver.

**Goal:** `veneer apply`, `bake`, `entrypoint`, `exec`, `env`, `diff`, `down`, `lock`, and `init` work end-to-end, are idempotent and safe to interrupt, and are verified in real containers.

**Spec:** `docs/superpowers/specs/2026-11-03-veneer-design.md`

## Global constraints

- Everything from the M1 plan still applies.
- The engine is synchronous. It uses `std::process`, not tokio. Child processes run in their own process group, and SIGINT/SIGTERM are forwarded (spec §6.5).
- mise is pinned to `v2026.11.0`. Linux always uses the `-musl` asset. Every download is checked against the release `SHASUMS256.txt`.
- mise runs in isolation: `MISE_DATA_DIR=<data>/mise`, `MISE_CONFIG_DIR`, `MISE_CACHE_DIR`, `MISE_GLOBAL_CONFIG_FILE=<state>/mise.toml`, and a cwd of `<state>`. The user's own mise is never touched.
- State lives in `VENEER_STATE_DIR` if set. Otherwise it's `/var/lib/veneer` as root, or `$XDG_STATE_HOME/veneer` (default `~/.local/state/veneer`). Data and cache live under the state dir (`data/`, `cache/`), so a bake captures everything.
- Exit codes are `0`, `1`, `2`, `3`, `4`, `5` and `130`, as in spec §5.4.

## Architecture

| Crate | Responsibility |
|---|---|
| `veneer-plan` | `Plan`, `Step`, `StepKind`, `Phase`. Renders templates (minijinja) with context `vars/env/workspace/secrets/os/veneer`. Resolves paths relative to the declaring file using node spans. Expands `from:` adapters. Hashes step inputs with blake3. |
| `veneer-exec` | State dirs, the journal (atomic JSON), flock locks, signal forwarding, child process runner with per-step logs, the event sink (human or NDJSON) with secret redaction, and the runner loop (check → apply → verify). |
| `veneer-steps` | One module per step kind: `packages`, `mise` (tools), `deps`, `images`, `hooks`, `repos`, `files`, `env`, `services` (compose and native). |
| `veneer-bake` | Context staging, Containerfile generation (layer per group), the buildx/buildah driver, and the `rootfs` export. |
| `veneer-resolve` (extended) | Remote sources (`github:`, `git+`, `https:`), `veneer.lock`, built-in modules, and `use:` merge. |
| `veneer` (extended) | `apply`, `bake`, `entrypoint`, `exec`, `env`, `diff`, `down`, `lock`, `init`. |

Step order (a linear topological order of the DAG). Groups in brackets are bake layers:
`[packages] → [tools] → [deps:N…] → [images] → [hooks.build] → secrets → env → repos → files → images.load → services → hooks.workspace`.

## Tasks

1. **Plan:** context, template rendering, path resolution, `from:` adapters (tools from mise.toml, .tool-versions, .nvmrc, .node-version, .python-version; packages from Brewfile, apt.txt; services from compose; deps with lockfile → manager detection), step list, hashes. Unit tests.
2. **Exec:** state, journal, locks, runner, events, redaction, signals. Unit tests with fake steps, including an interrupted step that is re-run.
3. **Steps:** files, env, hooks, secrets, repos (clone, worktree, copy via a mirror cache). Unit tests in temp dirs with local git repos.
4. **Steps:** packages (apt/apk/dnf/brew, `sudo -n`, `dpkg --configure -a`), mise bootstrap and tools, deps, images (docker pull or crane tarball, then load).
5. **Steps:** services, both compose (`docker compose -p veneer-<id> up -d --wait`) and native (setsid, pidfile, cmd/http healthcheck, allocated ports).
6. **CLI:** `apply`, `diff`, `exec`, `env`, `down`, `entrypoint`, `--set`, `--json`.
7. **Bake:** `oci` (layered Containerfile, `--veneer-bin` for cross-OS), `rootfs` (docker export → mkfs.ext4 in a helper container).
8. **Remote and modules:** fetch/cache, lockfile, built-in harness modules (`veneer/harness/claude-code`, `veneer/harness/codex`), `use:` with `with:` merging into vars.
9. **init and distribution:** `veneer init` detection, cargo-dist config, `install.sh`, release workflow.
10. **Container test harness** (`tests/containers/run.sh`):
    - build a static linux binary in `rust:alpine`
    - per distro (ubuntu:24.04, debian:12, alpine:3.20, fedora:41): apply, check the results, apply again (all skipped), kill -9 mid-run and rerun (converges), compare fingerprints
    - bake an OCI image and run its entrypoint
    - native services
    - compose services through the docker socket
    - remote `github:` fetch
