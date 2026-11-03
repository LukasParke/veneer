<img src="assets/icon.svg" width="64" height="64" alt="">

# veneer

Created in [T3 Code](https://t3.codes).

Define an agent workspace environment once in `.veneer/veneer.yaml`; veneer stages it
consistently in every workspace. See the [design spec](docs/superpowers/specs/2026-11-03-veneer-design.md).

## Status

Milestone 1: spec core. `veneer check` validates and resolves specs (`$ref`, `extends`,
overlays, strict schema, template phase checks).

```sh
cargo run -p veneer -- check                       # nearest .veneer/veneer.yaml
cargo run -p veneer -- check --resolved            # print the merged spec
cargo run -p veneer -- check --overlay .veneer/overlays/bench.overlay.yaml --resolved
cargo run -p veneer -- schema > veneer.schema.json # JSON Schema for editors
```
