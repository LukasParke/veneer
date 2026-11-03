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
| `tools` | runtimes and CLIs | [mise](https://mise.jdx.dev) (pinned, isolated) |
| `deps` | project installs from lockfiles | npm/pnpm/yarn/bun, uv/poetry/pip, cargo, go, bundler |
| `images` | pre-pulled container images | docker pull, or crane tarballs when there is no daemon |
| `secrets`, `env` | templated environment | minijinja |
| `workspace` | git clone / worktree / copy, templated files | git, mirror cache |
| `services` | databases, caches, workers | Docker Compose, or supervised native processes |
| `hooks` | build- and workspace-time commands | `creates:` / `unless:` guards |
| `use` | reusable modules (e.g. `veneer/harness/claude-code@1`) | |

Specs split across files with `$ref` (JSON Reference), layer with `extends`, vary with
[Overlay 1.0](https://spec.openapis.org/overlay/v1.0.0.html) files, and import existing
`mise.toml`, `.tool-versions`, `compose.yaml`, lockfiles, and `Brewfile`s through `from:`.

## Commands

```sh
veneer init                         # scaffold .veneer/ from files the project already has
veneer check [--resolved]           # validate; print the merged spec
veneer apply [--id W] [--phase build|workspace]   # stage (idempotent, safe to interrupt)
veneer diff                         # what would run, and why
veneer exec -- <cmd>                # run with the workspace env and tools on PATH
veneer env --format sh|json|dotenv  # print the env (secrets masked)
veneer down [--rm]                  # stop services (and delete the workspace)
veneer bake --target oci|rootfs     # bake the build phase into an image or ext4 rootfs
veneer entrypoint -- <cmd>          # container entrypoint: workspace phase, then exec
veneer lock [--update]              # pin remote sources in .veneer/veneer.lock
veneer schema                       # JSON Schema for editors
```

Every step goes check → apply → verify, with a write-ahead journal. If a run is killed at
any point (Ctrl-C, `kill -9`, OOM), rerunning `veneer apply` converges to the same state as
a clean run. The container suite checks this on every distro.

## Development

```sh
cargo test --workspace                 # unit + integration tests
tests/containers/build-linux.sh        # static musl binary for containers
tests/containers/run.sh                # ubuntu/debian/alpine/fedora, bake, compose, installer
```

Design: [spec](docs/superpowers/specs/2026-11-03-veneer-design.md) ·
[M1 plan](docs/superpowers/plans/2026-11-03-veneer-m1-spec-core.md) ·
[M2–M7 plan](docs/superpowers/plans/2026-11-03-veneer-m2-m7-engine-bake-dist.md)
