# veneer — design spec

- **Date:** 2026-11-03
- **Status:** approved in brainstorming, pending written-spec review
- **Scope:** v1 of the veneer CLI and spec format

## 1. Purpose

veneer defines an agent workspace environment once, in a YAML spec that lives
in the repo, and creates that environment consistently and quickly in every
workspace an agent (or benchmark worker) needs — on any runtime.

A workspace environment covers:

- environment variables and templated configuration
- tool installation (runtimes, CLIs)
- OS packages
- project dependency installs (from existing lockfiles)
- agent harness setup (Claude Code, Codex, OpenHands, …)
- git clones / worktrees and workspace folder layout
- pre-pulled container images
- service initialization (databases, caches, …)

Target runtimes: plain VM / bare metal / macOS, Docker / Podman, Kubernetes,
Firecracker and other microVMs, and hosted sandboxes (Modal, E2B, Daytona,
Fly).

### Originally deferred, now implemented

- Sandbox creation: `veneer up/ls/enter/rm --provider docker|podman` (parallel `--count`).
- Nix tool backend: `tools: { "nix:<attr>": latest | <nixpkgs rev> }`.
- Bake targets `qcow2`, `firecracker` (bundle), `disk` (bootable, via d2vm) and `ami`
  (S3 + EC2 VM Import + register-image, or `--snapshot-id`).
- Firecracker memory snapshots: `veneer vm snapshot|restore|boot`.
- Language server: `veneer lsp` (diagnostics incl. unsaved buffers, hover, completion).

### Non-goals

- Long-lived workstation convergence / dotfiles sync.

## 2. Principles

1. **One spec, two modes.** `apply` (inside the environment, at runtime) and
   `bake` (ahead of time, into a snapshot) run the *same* step engine, so they
   cannot diverge.
2. **Orchestrate, don't reimplement.** Every step delegates real work to a
   battle-tested tool: mise, apt/apk/dnf/brew, git, docker/podman compose,
   BuildKit/buildah, pnpm/uv/cargo. veneer owns ordering, hashing,
   idempotency and reporting.
3. **Existing files stay the source of truth.** `compose.yaml`, `mise.toml`,
   lockfiles and `devcontainer.json` are read in place, never duplicated.
4. **Fully idempotent and interrupt-safe.** Any run may be killed at any
   point; rerunning converges to the same state as a clean run.
5. **One command.** Install + fetch spec + set up is a single shell line.

## 3. Execution model

The spec compiles to a DAG of typed, idempotent steps. Each step's identity
is a blake3 hash of its resolved inputs.

Steps belong to one of two phases:

| Phase | Blocks | Bakeable | Runs |
|---|---|---|---|
| **build** | `packages`, `tools`, `deps`, `images`, `hooks.build`, module build parts | yes | once per machine/image; shared by all workspaces |
| **workspace** | `secrets`, `env`, `workspace`, `services`, `hooks.workspace`, harness config | never | once per workspace id |

Phase is inferred from the block. A template in a build-phase block that
references `workspace.*` or `secrets.*` (detected with minijinja's
`undeclared_variables()`) is a validation error (`V2001`), so secrets and
per-workspace values can never end up in a baked image.

`apply` runs the whole DAG and skips steps whose input hash and live check
both match. `bake` runs only the build phase inside a build target and
snapshots the result. On a baked image, `apply` therefore runs only the
workspace phase.

## 4. Spec format

### 4.1 Location and discovery

The environment lives in the repo:

```
app/
├── .veneer/
│   ├── veneer.yaml          # entrypoint
│   ├── veneer.lock          # pinned remote refs (generated)
│   ├── tools.yaml
│   ├── harness/claude.yaml
│   ├── overlays/bench.overlay.yaml
│   └── templates/.env.j2
├── compose.yaml
├── package.json + pnpm-lock.yaml
└── mise.toml
```

- `veneer <cmd>` with no spec argument walks up from the cwd to the first
  `.veneer/veneer.yaml`.
- A spec argument may be a local path, `github:owner/repo[@ref]`,
  `git+https://…[@ref]`, `https://…/veneer.yaml`, or `oci://registry/repo:tag`.
  Repo-style sources resolve to `.veneer/veneer.yaml` at that ref.

### 4.2 Top-level shape

```yaml
version: 1
base: ubuntu:24.04                           # bake base image
extends: [github:acme/envs//base.yaml@v3]    # layered deep merge; later wins
use:                                         # modules (reusable fragments)
  - veneer/harness/claude-code@1

vars:                                        # minijinja (Jinja2) templating
  python: "3.12"
  db_name: "bench_{{ workspace.id }}"

secrets:                                     # workspace-time only, never baked
  ANTHROPIC_API_KEY: { from: env }
  GH_TOKEN:          { from: file, path: /run/secrets/gh }
  DB_PASSWORD:       { from: command, run: "op read op://dev/db/password" }

env:
  DATABASE_URL: "postgres://localhost/{{ vars.db_name }}"

packages: [git, build-essential, libpq-dev]  # apt/apk/dnf/brew, auto-detected

tools:                                       # passed to mise
  python: "{{ vars.python }}"
  node: 22
  "aqua:BurntSushi/ripgrep": latest

deps:
  - from: ../package.json                    # → pnpm install --frozen-lockfile
  - from: ../pyproject.toml                  # → uv sync --frozen

images: [postgres:16, redis:7]               # pre-pulled at build time

workspace:
  root: "{{ env.HOME }}/work/{{ workspace.id }}"
  repos:
    - url: https://github.com/acme/app
      ref: main
      path: app
      mode: worktree                         # clone | worktree | copy
  files:
    - { src: templates/.env.j2, dest: app/.env, mode: "0600" }

services:                                    # Compose-compatible subset
  db:
    image: postgres:16
    env: { POSTGRES_DB: "{{ vars.db_name }}" }
    healthcheck: { cmd: pg_isready, interval: 1s, retries: 30 }
  worker:
    run: "python -m app.worker"              # native mode (no container engine)
    healthcheck: { http: "http://localhost:8001/health" }

hooks:
  build:
    - run: pip install -r app/requirements.txt
      creates: .venv/bin/python
  workspace:
    - run: make -C app migrate
      unless: "make -C app migrate-status"
```

Template context available to every template: `vars.*`, `env.*` (host env at
apply time), `workspace.id`, `workspace.root`, `workspace.ports.<service>`, `secrets.*` (workspace phase
only), `os.family`, `os.arch`, `veneer.version`.

### 4.3 `$ref` — splitting veneer's own files

Follows JSON Reference as used by OpenAPI 3.1 / JSON Schema 2020-12:

```yaml
tools:   { $ref: ./tools.yaml }
harness: { $ref: ./harness/claude.yaml#/claude }   # RFC 6901 JSON Pointer
env:
  $ref: ./env.yaml
  EXTRA: "1"                                       # siblings deep-merge over target
```

Rules:

- Relative refs resolve against the referencing document's URI.
- Cycles are an error (`V1003`), reported with the full chain.
- Remote refs must be pinned; resolved digests are recorded in
  `.veneer/veneer.lock` (`sha256-<hex>` per document, as in suspect's acquire
  manifest). `veneer apply` fails on digest mismatch (`V1005`);
  `veneer lock --update` refreshes.
- A remote spec may only reference documents inside its own repo/artifact or
  other pinned remotes — never host absolute paths (`V1006`).

### 4.4 `from:` — importing foreign formats

`from:` reads a file in another tool's native format through a typed adapter:

| Block | `from:` accepts | Effect |
|---|---|---|
| `tools` | `mise.toml`, `.tool-versions`, `.nvmrc`, `.node-version`, `.python-version` | merged into the generated mise config |
| `services` | `compose.yaml` / `docker-compose.yml` (optionally `#/services/<name>`) | Compose engine, or native fallback where possible |
| `packages` | `Brewfile`, `apt.txt` | OS package manager |
| `deps` | `package.json`, `pyproject.toml`, `requirements.txt`, `Cargo.toml`, `go.mod`, `Gemfile` | lockfile + manager detection, frozen install |
| `extends` | `devcontainer.json` | maps `features`, `containerEnv`, `postCreateCommand` |

`deps` manager detection is by lockfile: `pnpm-lock.yaml` → pnpm,
`package-lock.json` → npm, `yarn.lock` → yarn, `bun.lock` → bun, `uv.lock` →
uv, `poetry.lock` → poetry, `Cargo.lock` → cargo fetch, `go.sum` → go mod
download, `Gemfile.lock` → bundler. Missing lockfile is a warning (`W3001`)
and installs unfrozen. The step hash is the lockfile digest.

### 4.5 Modules (`use:`)

A module is a spec fragment with the same schema as `veneer.yaml` plus an
`inputs:` block. Modules are the only extension mechanism: harnesses
(`veneer/harness/claude-code`, `…/codex`, `…/openhands`), common services, and
org baselines are all modules. Modules resolve like `extends` (git ref or OCI
artifact, pinned in the lock) and merge before the root spec.

```yaml
use:
  - module: veneer/harness/claude-code@1
    with: { settings: { $ref: ./harness/claude.yaml } }
```

### 4.6 Overlays — variants without forking

Variants (benchmark configurations, CI vs local, GPU vs CPU) use the
**OpenAPI Overlay 1.0** standard: an ordered list of `update` / `remove`
actions targeting RFC 9535 JSONPath expressions on the resolved spec.

```yaml
# .veneer/overlays/bench.overlay.yaml
overlay: 1.0.0
info: { title: bench variant, version: 1 }
actions:
  - target: $.services.worker
    remove: true
  - target: $.env
    update: { BENCH_MODE: "1" }
```

`veneer apply --overlay .veneer/overlays/bench.overlay.yaml` (repeatable,
applied in order after `$ref`/`extends`/`use` resolution).

### 4.7 Merge semantics

Resolution order: `use` modules → `extends` (in list order) → root document →
`$ref` sibling merges → overlays → `--set k=v` (vars only).

- Maps deep-merge; later wins per key.
- Lists replace, except `packages`, `images`, `deps`, `hooks.*`,
  `workspace.repos`, `workspace.files`, which concatenate and de-duplicate by
  identity (package name, image ref, `from` path, hook `run`, repo `path`, file
  `dest`).
- `null` deletes a key inherited from a lower layer.

### 4.8 Validation and diagnostics

- A JSON Schema (2020-12) is generated from the Rust types with `schemars`,
  committed at `schema/veneer.schema.json`, and published so editors validate
  via `# yaml-language-server: $schema=…`.
- Unknown keys are errors.
- Every diagnostic has a stable code (`V1xxx` resolve, `V2xxx` template/phase,
  `V3xxx` adapters, `V4xxx` steps, `W…` warnings), a severity, and a source
  range in the originating file — including files reached through `$ref`.
  Output is deterministic, sorted by `(doc, range, code)`.
- `veneer check` validates; `veneer check --resolved` prints the fully merged
  spec with every `$ref`, `from:`, module and overlay expanded, plus the step
  DAG.

## 5. CLI

### 5.1 One-command flow

```sh
curl -fsSL https://veneer.sh/i | sh -s -- apply github:acme/app@main
```

1. Installer detects OS/arch, downloads the static binary from GitHub
   Releases, verifies sha256 + GitHub artifact attestation, installs to
   `~/.local/bin` (or `/usr/local/bin` as root), then execs the remaining
   args.
2. `apply` fetches and resolves the spec.
3. `apply` reads the state journal and runs only steps that are not already
   verified-done. On a baked image this is the workspace phase only.

Other installs: `brew install veneer`, `cargo binstall veneer`,
`npx veneer` / `uvx veneer` (cargo-dist wrappers), and
`COPY --from=ghcr.io/veneer/veneer:1 /veneer /usr/local/bin/`.

### 5.2 Commands

| Command | Purpose |
|---|---|
| `veneer apply [spec]` | Run the DAG. `--phase build\|workspace`, `--id <ws>`, `--set k=v`, `--overlay f`, `--dry-run` |
| `veneer bake [spec] --target oci\|rootfs` | Run build phase into a snapshot. `--tag`, `--push`, `--platform` |
| `veneer entrypoint [spec] -- <cmd>` | Container entrypoint: workspace phase, then `exec` cmd |
| `veneer check [spec] [--resolved]` | Validate; print merged spec and DAG |
| `veneer diff [spec]` | Show steps that would run and why (input change vs live-check failure) |
| `veneer exec [--id] -- <cmd>` | Run cmd with workspace env + tool PATH |
| `veneer env [--id] --format sh\|json\|dotenv` | Print resolved env (secrets redacted unless `--reveal`) |
| `veneer down [--id] [--rm]` | Stop services; optionally remove workspace root |
| `veneer lock [--update]` | Write/refresh `.veneer/veneer.lock` |
| `veneer init` | Detect existing files, scaffold `.veneer/` |

### 5.3 Configuration precedence

Tool behaviour (not environment content) follows one rule, borrowed from
suspect-config: **flag > `VENEER_*` env var > `~/.config/veneer/config.toml`
> built-in default.** The spec (`veneer.yaml`) is a manifest, not
configuration: it never changes how the CLI behaves.

### 5.4 Machine interface

- `--json` emits NDJSON events: `run.start`, `step.start`, `step.skip`,
  `step.done`, `step.fail`, `run.done`, each with step id, phase, kind,
  duration and reason. Events pass through a redactor that masks every
  resolved secret value before writing.
- Exit codes: `0` ok, `1` internal error, `2` invalid spec, `3` step failed,
  `4` missing secret, `5` lock/digest mismatch, `130` interrupted.
- Failure output names the failing step and the last 50 lines of its log;
  full logs are at `<state>/logs/<run-id>/<step-id>.log`.

### 5.5 Parallel workspaces

- Each `--id` gets its own workspace root, journal, and service namespace
  (compose project name `veneer-<id>`; native services get ports allocated
  and exposed as `workspace.ports.<service>` in templates).
- Build-phase state is shared and guarded by an exclusive `flock`.
- N benchmark workers on one host share baked/build state and do not collide.

### 5.6 Privileges

OS package steps need root. veneer uses `sudo -n` when not root and sudo is
available; otherwise the step fails with `V4001: step packages needs root`.
Everything else runs rootless. State lives in `/var/lib/veneer` as root,
`$XDG_STATE_HOME/veneer` otherwise; caches in `$XDG_CACHE_HOME/veneer`.

## 6. Idempotency and interrupt safety

Contract: after any interruption (Ctrl-C, SIGKILL, OOM, power loss), rerunning
`veneer apply` converges to the same state as a clean run.

1. **Step protocol: `check → apply → verify`.** A step is stamped done only
   after `verify` passes. Stamps are hints: on rerun, a cheap live `check`
   (tool on PATH at expected version, file hash matches, worktree exists,
   service healthy) runs before skipping.
2. **Write-ahead journal.** `state.json` records each step as `pending`,
   `running`, `done` (with input hash + output fingerprint). It is rewritten
   atomically (temp file in same dir → fsync → rename → fsync dir). A step
   found `running` is re-checked and redone.
3. **No partial results visible.**
   - Files: render to a temp file in the destination directory, then rename.
     Unchanged files are not rewritten. A per-workspace ownership manifest
     records files veneer wrote (path + digest) so files removed from the
     spec are deleted only if their content is still what veneer wrote
     (pattern from suspect-artifact).
   - Downloads: content-addressed cache keyed by sha256; `.part` files are
     resumed or discarded.
   - Git clone: into a temp dir, then rename. Worktree: detect existing,
     `git worktree prune` stale entries, then `git worktree add`.
   - dpkg/apt: wait on the dpkg lock; run `dpkg --configure -a` if a previous
     run was interrupted.
4. **Locks.** `flock` (via `fs4`) on the build phase and on each workspace.
   Kernel-released on process death, so no stale locks.
5. **Signals.** On SIGINT/SIGTERM, veneer forwards the signal to the child
   process group, waits up to 10 s, then SIGKILLs, marks the step
   `interrupted`, and exits 130.
6. **User hooks.** veneer cannot make arbitrary shell idempotent. Hooks run
   once per input hash and support Ansible-style guards `creates: <path>` and
   `unless: <cmd>`. Docs require hooks to be rerun-safe.
7. **Services.** "Start" means "ensure running and healthy": `compose up -d
   --wait` is idempotent; native services use a pidfile + healthcheck and are
   restarted only if not healthy.

## 7. Bake targets and runtimes

### 7.1 Targets (v1)

| Target | Output | Mechanism |
|---|---|---|
| `oci` | image ref + digest; optional push | generated Containerfile built with BuildKit (`docker buildx`) or rootless buildah |
| `rootfs` | ext4 image | export the `oci` result, flatten, `mkfs.ext4 -d` |

Layering (one `RUN veneer apply --phase build --only <group>` each, so BuildKit
caches per group):

1. `base`
2. veneer binary
3. `packages`
4. mise + `tools`
5. one layer per `deps` entry (cache key = lockfile digest; only the manifest
   and lockfile are copied in for that layer)
6. `images` (saved as OCI tarballs under `/var/lib/veneer/images`, loaded with
   `docker load` / `ctr import` at workspace time — no registry pull)
7. `hooks.build`
8. journal stamp + `ENTRYPOINT ["veneer","entrypoint","--"]`

### 7.2 Runtime matrix

| Runtime | Workspace start |
|---|---|
| VM / bare metal / macOS | one-command `apply` (cold) or `apply` on a pre-baked VM image |
| Docker / Podman | baked `oci` image; `veneer entrypoint -- <cmd>` |
| Kubernetes | same image + entrypoint; services in native mode |
| Firecracker / microVM | `rootfs` target; systemd unit runs `veneer apply --phase workspace` at boot |
| Modal / E2B / Daytona / Fly | push `oci` image; `exec("veneer apply --phase workspace")` |

### 7.3 Platform support (v1)

- `apply`: Linux x86_64 / aarch64 (glibc and musl distros: Ubuntu, Debian,
  Alpine, Fedora), macOS arm64.
- `bake`: Linux hosts; macOS via Docker Desktop / OrbStack.

## 8. Rust architecture

Cargo workspace, edition 2024, MSRV 1.88 (matching suspect).

| Crate | Responsibility | Built on |
|---|---|---|
| `veneer-spec` | typed spec model, schema generation, phase classification | `serde`, `schemars` |
| `veneer-resolve` | document workspace, `$ref`, `extends`, `use`, overlays, lockfile, remote fetch, `from:` adapters | suspect spec-layer crates (below), `gix`, `reqwest` (rustls), `oci-client`, `toml`, `serde_json` |
| `veneer-template` | minijinja environment, context, phase-violation detection | `minijinja` |
| `veneer-plan` | resolved spec → step DAG, input hashing | `petgraph`, `blake3` |
| `veneer-exec` | DAG runner, journal, locks, signals, atomic writes, event stream, redaction | `tokio`, `fs4`, `tempfile`, `nix` |
| `veneer-steps` | step kinds: packages, mise, deps, git, files, images, services, hooks | shells out to apt/apk/dnf/brew, pinned mise, `git`, docker/podman, package managers |
| `veneer-bake` | Containerfile generation, buildx/buildah driver, rootfs export | — |
| `veneer` (bin) | CLI, TTY progress, NDJSON, diagnostics rendering | `clap`, `indicatif`, `miette` |

### 8.1 Spec layer: reuse suspect

The `$ref`/source/overlay layer reuses suspect's generic crates as git
dependencies pinned to a revision:

- `suspect-source` — loading, encodings, line indexes, canonical URIs
- `suspect-syntax` + `suspect-low` — lossless YAML CST and ordered model with
  source maps, JSON Pointer navigation, alias/merge-key handling,
  duplicate-key reporting
- `suspect-ref` — `$ref` workspace graph, memoized resolution, cycle census,
  `DocumentProvider` with sha256-pinned documents
- `suspect-jsonpath` + `suspect-overlay` — RFC 9535 JSONPath and Overlay 1.0
  apply engine

Flow: suspect loads and resolves documents with source maps → veneer merges
layers into an owned value tree that keeps a JSON-Pointer → source-range map →
deserializes into `veneer-spec` types → diagnostics map back to the original
file and line.

Patterns adopted from suspect without a code dependency: stable diagnostic
codes with deterministic ordering (suspect-validate), single configuration
precedence (suspect-config), JSONL event journal with redactor
(suspect-journal), confined atomic writes with ownership manifests
(suspect-artifact).

This keeps the door open for a later `veneer lsp` (goto-definition across
`$ref`, hover previews, completions) built the same way as `suspect lsp`.

### 8.2 Step trait

```rust
trait Step {
    fn id(&self) -> StepId;
    fn phase(&self) -> Phase;
    fn inputs_hash(&self) -> blake3::Hash;
    async fn check(&self, cx: &Cx) -> Result<CheckOutcome>;   // Satisfied | Needed(reason)
    async fn apply(&self, cx: &Cx) -> Result<()>;
    async fn verify(&self, cx: &Cx) -> Result<Fingerprint>;
}
```

### 8.3 mise handling

veneer pins a mise version, downloads it once (checksum-verified) into
`<data>/veneer/bin/mise`, writes a generated `mise.toml` per spec, and runs
it with isolated `MISE_DATA_DIR` / `MISE_CONFIG_DIR` under veneer's data dir.
A user's own mise installation and config are never read or modified.
`veneer exec` / `env` put mise's shims/bin paths on `PATH`.

## 9. Testing

1. **Unit + golden.** Parsing, merge semantics, `$ref` / `from:` / module /
   overlay resolution, phase inference. `insta` snapshots of
   `check --resolved` and diagnostics output.
2. **Schema drift.** CI regenerates `schema/veneer.schema.json` and fails on
   diff.
3. **Distro matrix.** Integration tests run `apply` in containers for Ubuntu,
   Debian, Alpine, Fedora; plus a macOS runner.
4. **Idempotency.** Second `apply` must report every step `skip`.
5. **Chaos.** For each step kind, kill -9 at randomized points, rerun, assert
   the final fingerprint equals a clean run's.
6. **Bake parity.** Cold `apply` vs `bake` + workspace-phase `apply` must
   produce identical fingerprints.

## 10. Release

cargo-dist builds static musl Linux and macOS binaries, the curl/sh
installer, Homebrew formula, npm and PyPI wrappers, and GitHub artifact
attestations. A minimal `ghcr.io/veneer/veneer` image ships the binary for
`COPY --from`.

## 11. v1 milestones

1. **Spec core:** `veneer-spec`, `veneer-resolve` (local `$ref`, `extends`,
   overlays), `veneer check [--resolved]`, schema publishing.
2. **Engine:** `veneer-plan`, `veneer-exec` (journal, locks, signals, events),
   `files`, `hooks`, `env`/`secrets` steps; `apply`, `diff`, `exec`, `env`.
3. **Toolchain steps:** `packages`, `mise` tools, `deps`, `git` repos.
4. **Services + images:** compose and native services, image pre-pull, `down`.
5. **Remote + modules:** git/https/OCI sources, lockfile, `use:` modules,
   first harness modules (Claude Code, Codex).
6. **Bake:** `oci` and `rootfs` targets, `entrypoint`.
7. **Distribution:** cargo-dist installers, one-command flow, `init`.
