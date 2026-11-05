#!/usr/bin/env bash
# End-to-end tests in real containers.
#
#   tests/containers/run.sh                 # everything
#   SUITES="distros bake" tests/containers/run.sh
#   DISTROS="alpine:3.20" SUITES=distros tests/containers/run.sh
#
# Suites:
#   distros  apply on each distro: assertions, idempotency, workspace isolation,
#            kill -9 chaos + convergence, down
#   bake     OCI bake, entrypoint, layer caching, bake/apply parity, rootfs
#   compose  compose services through the host Docker socket
#   install  the one-command installer against a local release server
#   devcontainer  extends a devcontainer.json: env, lifecycle hooks, a local and a ghcr.io Feature
#   oci      oci:// spec sources and extends through a local registry (pushed with oras)
#   nix      `nix:` tools through Nix (nixos/nix), plus the musl loader guard (V4002)
#   sandbox  `veneer up/ls/enter/rm`: parallel sandboxes from the baked image (run after bake)
#   resilience  veneer.lock pins + --frozen, hook retries/timeouts, doctor preflight, and an
#            offline bundle applied in a container with no network (fingerprint parity)
#   cloudinit  `bake --target cloud-init` output validated by `cloud-init schema`
#   vmformats  `--target microvm` launchers, OVA (OVF + streamOptimized VMDK + manifest), and a
#            fixed 1 MiB-aligned VHD
#   k8s      `veneer render k8s` validated by kubeconform, then applied to a kind cluster: an
#            Indexed Job of 3 workspaces with a native-sidecar service and a Secret
#   otel     OTLP/HTTP traces into an OpenTelemetry Collector: span tree, TRACEPARENT parenting
#            and propagation into hooks, secret redaction
#   supply   OCI labels + fingerprint label, SBOMs (CycloneDX validated) and SLSA provenance as
#            OCI referrers, and cosign sign/attest + verify against a local registry
#   vm       Firecracker boot + memory snapshot + 2 restores, and the bootable disk under QEMU/KVM.
#            Needs a KVM host sharing $VM_SHARED at the same path, e.g. on macOS (M3+):
#              limactl start --name veneer-kvm (vz, nestedVirtualization: true, mount /tmp/veneer-fc rw)
#              KVM_EXEC="limactl shell veneer-kvm --" SUITES=vm tests/containers/run.sh
#            On a Linux host with /dev/kvm, KVM_EXEC can stay empty.
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
bin="$repo/target/linux/release/veneer"
host_bin="${HOST_VENEER:-$repo/target/debug/veneer}"
SUITES="${SUITES:-distros bake compose install devcontainer oci nix sandbox resilience cloudinit supply vmformats k8s otel vm}"
DISTROS="${DISTROS:-ubuntu:24.04 debian:12 alpine:3.20 fedora:41}"
TOKEN="tok-s3cret-value"

work="$(mktemp -d "${TMPDIR:-/tmp}/veneer-ct.XXXXXX")"
logs="$work/logs"
mkdir -p "$logs"
pass=0
fail=0
failures=()

ok() { pass=$((pass + 1)); printf '  \033[32m✓\033[0m %s\n' "$1"; }
bad() { fail=$((fail + 1)); failures+=("$1"); printf '  \033[31m✗\033[0m %s\n' "$1"; [ -n "${2:-}" ] && sed 's/^/      /' "$2" | tail -n 25; }
check() { # check "<label>" <command...>
  local _label="$1"; shift
  local _out="$logs/$(echo "$_label" | tr -c 'a-zA-Z0-9' '_').log"
  if "$@" >"$_out" 2>&1; then ok "$_label"; else bad "$_label" "$_out"; fi
}

[ -x "$bin" ] || "$here/build-linux.sh"
[ -x "$host_bin" ] || (cd "$repo" && cargo build -q -p veneer)

# A bare upstream repo for `workspace.repos`.
git init -q -b main "$work/upstream-src"
echo "upstream readme" > "$work/upstream-src/README.md"
git -C "$work/upstream-src" add .
git -C "$work/upstream-src" -c user.email=t@t -c user.name=t -c commit.gpgsign=false commit -qm init
git clone -q --bare "$work/upstream-src" "$work/upstream.git"

start() { # start <name> <image> [docker run args...]
  local name="$1" image="$2"; shift 2
  docker rm -f "$name" >/dev/null 2>&1
  docker run -d --name "$name" -e TEST_API_TOKEN="$TOKEN" -v "$work/upstream.git:/srv/upstream.git:ro" "$@" \
    "$image" sleep infinity >/dev/null
  docker cp "$bin" "$name:/usr/local/bin/veneer" >/dev/null
}

fingerprints() { # build.json -> sorted "step status fingerprint" lines
  python3 -c 'import json,sys; d=json.load(open(sys.argv[1]))["steps"]; [print(k, v["status"], v["fingerprint"]) for k,v in sorted(d.items())]' "$1"
}
export -f fingerprints

# ------------------------------------------------------------------ distros
suite_distros() {
  for image in $DISTROS; do
    local name="veneer-ct-$(echo "$image" | tr -c 'a-z0-9' '-')"
    local ov=""
    case "$image" in debian*|fedora*) ov="--overlay /fixture/.veneer/overlays/node.overlay.yaml" ;; esac
    echo "▸ $image"
    start "$name" "$image"
    docker cp "$here/fixture" "$name:/fixture" >/dev/null
    x() { docker exec "$name" sh -c "$*"; }
    local A="veneer apply /fixture $ov --json"

    check "$image: cold apply" sh -c "docker exec $name sh -c '$A --id t1' | tee $logs/$name-cold.ndjson | grep -q '\"event\":\"run.done\",\"ok\":true'"
    check "$image: jq from .tool-versions on PATH" sh -c "docker exec $name veneer exec /fixture $ov --id t1 -- jq --version | grep -q 1.7.1"
    check "$image: yq (aqua) on PATH" sh -c "docker exec $name veneer exec /fixture $ov --id t1 -- yq --version | grep -q 4.44.3"
    check "$image: templated file with mode 0600 and secret" x "test \"\$(stat -c %a /work/t1/app/.env)\" = 600 && grep -q 'TOKEN=$TOKEN' /work/t1/app/.env && grep -q 'GREETING=hello from' /work/t1/app/.env"
    check "$image: clone + worktree checked out" x "test -f /work/t1/src/README.md && test -f /work/t1/wt/README.md && git -C /work/t1/src remote get-url origin | grep -q upstream.git"
    check "$image: build and workspace hooks ran" x "test -s /fixture/.jq-version && grep -q 4.44.3 /work/t1/yq.txt"
    check "$image: image pre-pulled with crane (no daemon)" x "test -s /var/lib/veneer/data/images/busybox_1.36.tar"
    check "$image: env masks the secret" sh -c "docker exec $name veneer env /fixture $ov --id t1 --format dotenv | tee $logs/$name-env.txt | grep -q 'API_TOKEN=\*\*\*\*' && ! grep -q $TOKEN $logs/$name-env.txt"
    check "$image: native service answers on its port" x "port=\$(sed -n 's/.*\"web\": *\\([0-9]*\\).*/\\1/p' /var/lib/veneer/workspaces/t1/ports.json); curl -fsS http://127.0.0.1:\$port/ >/dev/null"
    if [ -n "$ov" ]; then
      check "$image: node via overlay + npm ci from lockfile" sh -c "docker exec $name veneer exec /fixture $ov --id t1 -- node --version | grep -q v22.11.0 && docker exec $name test -f /fixture/node_modules/is-number/package.json"
    fi
    check "$image: second apply is a no-op" sh -c "docker exec $name sh -c '$A --id t1' | grep -q '\"applied\":0'"
    check "$image: diff reports everything up to date" sh -c "! docker exec $name veneer diff /fixture $ov --id t1 | grep -q '^+'"
    check "$image: second workspace is isolated" sh -c "docker exec $name sh -c '$A --id t2' | grep -q '\"ok\":true' && docker exec $name test -f /work/t2/app/.env && [ \"\$(docker exec $name cat /var/lib/veneer/workspaces/t1/ports.json)\" != \"\$(docker exec $name cat /var/lib/veneer/workspaces/t2/ports.json)\" ]"
    docker exec "$name" cat /var/lib/veneer/build.json >"$work/$name-clean-build.json"
    local fp1
    fp1=$(docker exec "$name" python3 -c 'import json; print(json.load(open("/var/lib/veneer/workspaces/t1/manifest.json"))["fingerprint"])')
    check "$image: t1 and t2 share one environment fingerprint" sh -c "[ -n '$fp1' ] && docker exec $name python3 -c 'import json; print(json.load(open(\"/var/lib/veneer/workspaces/t2/manifest.json\"))[\"fingerprint\"])' | grep -qx '$fp1'"
    check "$image: verify passes, and catches a tampered file" sh -c "docker exec $name veneer verify /fixture $ov --id t1 && docker exec $name sh -c 'echo x >> /work/t1/app/.env' && ! docker exec $name veneer verify /fixture $ov --id t1 2>$logs/$name-verify.txt && grep -q '/files/0/sha256' $logs/$name-verify.txt && docker exec $name veneer apply /fixture $ov --id t1 >/dev/null 2>&1 && docker exec $name veneer verify /fixture $ov --id t1 --expect '$fp1'"
    check "$image: manifest has no secret values" sh -c "! docker exec $name grep -rq '$TOKEN' /var/lib/veneer/workspaces/t1/manifest.json"

    # chaos: wipe state, kill -9 the run (and its children) at a random point, twice
    x "veneer down /fixture $ov --id t1 --rm; veneer down /fixture $ov --id t2 --rm; rm -rf /var/lib/veneer /fixture/node_modules /fixture/.jq-version" >/dev/null 2>&1
    cat >"$work/chaos.sh" <<'EOF'
for p in /proc/[0-9]*; do
  pid=${p#/proc/}
  [ "$pid" = "$$" ] && continue
  cmd=$(tr '\0' ' ' < "$p/cmdline" 2>/dev/null) || continue
  case "$cmd" in
    *veneer\ apply*|*mise*|*crane*|*apt-get*|*dpkg*|*apk\ add*|*dnf*|*npm*|*git\ *) kill -9 "$pid" 2>/dev/null ;;
  esac
done
EOF
    docker cp "$work/chaos.sh" "$name:/tmp/chaos.sh" >/dev/null
    local kills=""
    for round in 1 2; do
      local k=$(( (RANDOM % 6) + 1 ))
      kills="$kills ${k}s"
      docker exec -d "$name" sh -c "$A --id t3 > /tmp/chaos-$round.log 2>&1"
      sleep "$k"
      docker exec "$name" sh /tmp/chaos.sh
      sleep 1
    done
    check "$image: converges after kill -9 at$kills" sh -c "docker exec $name sh -c '$A --id t3' | grep -q '\"event\":\"run.done\",\"ok\":true'"
    check "$image: post-chaos rerun is a no-op" sh -c "docker exec $name sh -c '$A --id t3' | grep -q '\"applied\":0'"
    docker exec "$name" cat /var/lib/veneer/build.json >"$work/$name-chaos-build.json"
    check "$image: chaos fingerprints equal a clean run" bash -c "diff <(fingerprints '$work/$name-clean-build.json') <(fingerprints '$work/$name-chaos-build.json')"
    check "$image: post-chaos workspace has the clean run's environment fingerprint" sh -c "docker exec $name veneer verify /fixture $ov --id t3 --expect '$fp1'"
    check "$image: down stops services" sh -c "docker exec $name veneer down /fixture $ov --id t3 && docker exec $name sh -c 'port=\$(sed -n \"s/.*\\\"web\\\": *\\([0-9]*\\).*/\\1/p\" /var/lib/veneer/workspaces/t3/ports.json); ! curl -fsS --max-time 2 http://127.0.0.1:\$port/ >/dev/null 2>&1'"
    docker rm -f "$name" >/dev/null
  done
}

# ------------------------------------------------------------------ bake
suite_bake() {
  echo "▸ bake"
  local proj="$work/bakeproj" tag="veneer-ct/fixture:latest"
  cp -R "$here/fixture" "$proj"
  export VENEER_STATE_DIR="$work/host-state"
  check "bake: oci image builds" "$host_bin" bake "$proj" --veneer-bin "$bin" --tag "$tag" --context "$work/bake-ctx"
  check "bake: one layer per group in Containerfile" sh -c "grep -c 'RUN \[\"veneer\", \"apply\", \"--phase\", \"build\", \"--only\"' '$work/bake-ctx/Containerfile' | grep -qx 3"
  local t0=$(date +%s)
  check "bake: entrypoint runs workspace phase then the command" sh -c "docker run --rm -e TEST_API_TOKEN=$TOKEN -v $work/upstream.git:/srv/upstream.git:ro $tag sh -c 'jq --version && yq --version && cat /work/default/app/.env && test -f /work/default/src/README.md' | tee $logs/bake-run.txt | grep -q 'TOKEN=$TOKEN'"
  local secs=$(( $(date +%s) - t0 ))
  check "bake: warm start took ${secs}s (< 30s)" test "$secs" -lt 30
  check "bake: baked tools need no download at start" sh -c "grep -q 1.7.1 $logs/bake-run.txt"
  docker run --rm --entrypoint cat "$tag" /var/lib/veneer/build.json >"$work/baked-build.json"
  if [ -f "$work/veneer-ct-ubuntu-24-04--clean-build.json" ]; then
    check "bake: fingerprints match a cold apply on ubuntu" bash -c "diff <(fingerprints '$work/veneer-ct-ubuntu-24-04--clean-build.json') <(fingerprints '$work/baked-build.json')"
  fi
  echo "touch" >"$proj/NOTES.md"
  check "bake: rebuild after an unrelated edit reuses cached layers" sh -c "'$host_bin' bake '$proj' --veneer-bin '$bin' --tag '$tag' && grep -c CACHED \$(ls -t '$work/host-state/logs'/bake-*.log | head -n1) | awk '{exit !(\$1 >= 5)}'"
  check "bake: --overlay reaches every in-image step (node in image)" sh -c "'$host_bin' bake '$proj' --veneer-bin '$bin' --tag veneer-ct/fixture:node --overlay '$proj/.veneer/overlays/node.overlay.yaml' && docker run --rm -e TEST_API_TOKEN=$TOKEN -v $work/upstream.git:/srv/upstream.git:ro veneer-ct/fixture:node sh -c 'node --version && test -f /opt/veneer/project/node_modules/is-number/package.json' | grep -q v22.11.0"
  check "bake: rootfs ext4 image" sh -c "'$host_bin' bake '$proj' --veneer-bin '$bin' --tag '$tag' --target rootfs -o '$work/rootfs.ext4' && file '$work/rootfs.ext4' | grep -qi ext4"
  printf 'fake-vmlinux' >"$work/vmlinux"
  check "bake: firecracker bundle (rootfs + init shim + kernel + vm-config)" sh -c "'$host_bin' bake '$proj' --veneer-bin '$bin' --tag '$tag' --target firecracker --kernel '$work/vmlinux' -o '$work/fc' && python3 -c 'import json,sys; c=json.load(open(sys.argv[1])); assert c[\"drives\"][0][\"path_on_host\"]==\"rootfs.ext4\" and \"init=/sbin/veneer-init\" in c[\"boot-source\"][\"boot_args\"]' '$work/fc/vm-config.json' && cmp -s '$work/vmlinux' '$work/fc/vmlinux' && docker run --rm -v '$work/fc:/fc' alpine:3.20 sh -c 'apk add --no-cache e2fsprogs-extra >/dev/null && debugfs -R \"cat /sbin/veneer-init\" /fc/rootfs.ext4 2>/dev/null > /tmp/init && sh -n /tmp/init && grep -q \"veneer apply /opt/veneer/project/.veneer/veneer.yaml --phase workspace\" /tmp/init && debugfs -R \"stat /usr/local/bin/veneer\" /fc/rootfs.ext4 2>/dev/null | grep -q Inode'"
  check "bake: qcow2 VM disk" sh -c "'$host_bin' bake '$proj' --veneer-bin '$bin' --tag '$tag' --target qcow2 -o '$work/disk.qcow2' && file '$work/disk.qcow2' | grep -qi qcow"
  unset VENEER_STATE_DIR
}

# ------------------------------------------------------------------ compose
suite_compose() {
  echo "▸ compose"
  local name="veneer-ct-compose"
  start "$name" docker:27-cli -v /var/run/docker.sock:/var/run/docker.sock
  docker exec -i "$name" sh -c 'mkdir -p /cf/.veneer && cat > /cf/.veneer/veneer.yaml' <<'EOF'
version: 1
services:
  cache:
    image: redis:7-alpine
    healthcheck: { cmd: "redis-cli ping", interval: 1s, retries: 30 }
EOF
  check "compose: apply starts healthy services" sh -c "docker exec $name veneer apply /cf --id ct --json | grep -q '\"ok\":true' && docker ps --filter label=com.docker.compose.project=veneer-ct --format '{{.Status}}' | grep -q healthy"
  check "compose: second apply is a no-op" sh -c "docker exec $name veneer apply /cf --id ct --json | grep -q '\"applied\":0'"
  check "compose: down removes the project" sh -c "docker exec $name veneer down /cf --id ct && [ -z \"\$(docker ps -q --filter label=com.docker.compose.project=veneer-ct)\" ]"
  docker rm -f "$name" >/dev/null
}

# ------------------------------------------------------------------ install
suite_install() {
  echo "▸ install"
  local name="veneer-ct-install" arch
  arch="$(docker version --format '{{.Server.Arch}}')"
  case "$arch" in arm64|aarch64) arch=aarch64 ;; *) arch=x86_64 ;; esac
  local target="$arch-unknown-linux-musl" rel="$work/release"
  mkdir -p "$rel/veneer-$target"
  cp "$bin" "$rel/veneer-$target/veneer"
  (cd "$rel" && tar -cJf "veneer-$target.tar.xz" "veneer-$target" && shasum -a 256 "veneer-$target.tar.xz" >"veneer-$target.tar.xz.sha256")
  cp "$repo/install.sh" "$rel/install.sh"
  start "$name" python:3.12-slim -v "$rel:/release:ro"
  docker exec "$name" rm -f /usr/local/bin/veneer
  docker cp "$here/fixture" "$name:/fixture" >/dev/null
  docker exec -d "$name" python3 -m http.server 8765 -d /release
  sleep 1
  check "install: one command installs, verifies, and runs veneer" sh -c "docker exec $name sh -c 'apt-get -qq update >/dev/null && apt-get -qq install -y curl xz-utils >/dev/null && curl -fsSL http://127.0.0.1:8765/install.sh | VENEER_RELEASE_URL=http://127.0.0.1:8765 sh -s -- check /fixture' | grep -q '^ok '"
  check "install: tampered archive is rejected" sh -c "docker exec $name sh -c 'mkdir -p /bad && cp -r /release/. /bad/ && echo junk >> /bad/veneer-$target.tar.xz && cd /bad && (python3 -m http.server 8766 >/dev/null 2>&1 &) && sleep 1 && ! curl -fsSL http://127.0.0.1:8766/install.sh | VENEER_RELEASE_URL=http://127.0.0.1:8766 VENEER_INSTALL_DIR=/tmp/v sh -s -- --version'"
  docker rm -f "$name" >/dev/null
}

# ------------------------------------------------------------------ devcontainer
suite_devcontainer() {
  echo "▸ devcontainer"
  local name="veneer-ct-devcontainer"
  start "$name" ubuntu:24.04
  docker cp "$here/devcontainer-fixture" "$name:/dc" >/dev/null
  check "devcontainer: apply installs features and runs lifecycle hooks" sh -c "docker exec $name veneer apply /dc --json | tee $logs/dc.ndjson | grep -q '\"event\":\"run.done\",\"ok\":true'"
  check "devcontainer: local feature with options and containerEnv PATH" sh -c "docker exec $name veneer exec /dc -- hello | grep -q 'hi from a feature (loud=false, user=root)'"
  check "devcontainer: ghcr.io feature (github-cli) installed over OCI" sh -c "docker exec $name veneer exec /dc -- gh --version | grep -q 'gh version'"
  check "devcontainer: containerEnv mapped, \${containerWorkspaceFolder} rendered" sh -c "docker exec $name veneer env /dc --format dotenv | tee $logs/dc-env.txt | grep -q '^FROM_DEVCONTAINER=yes' && grep -q '^WS=/work/dc' $logs/dc-env.txt && grep -q '^HELLO_HOME=/opt/hello' $logs/dc-env.txt"
  check "devcontainer: onCreate (build) and postCreate (workspace) commands ran" sh -c "docker exec $name sh -c 'grep -q created /tmp/oncreate.txt && grep -q post /work/dc/postcreate.txt'"
  check "devcontainer: second apply is a no-op" sh -c "docker exec $name veneer apply /dc --json | grep -q '\"applied\":0'"
  docker rm -f "$name" >/dev/null
}

# ------------------------------------------------------------------ oci
suite_oci() {
  echo "▸ oci"
  local net="veneer-ct-net" reg="veneer-ct-registry" name="veneer-ct-oci"
  docker network create "$net" >/dev/null 2>&1
  docker rm -f "$reg" >/dev/null 2>&1
  docker run -d --name "$reg" --network "$net" registry:2 >/dev/null
  mkdir -p "$work/ocispec/.veneer"
  printf 'version: 1\nextends: [./base.yaml]\nenv:\n  FROM_OCI: "yes"\nworkspace:\n  files:\n    - { src: hello.txt.j2, dest: hello.txt }\n' >"$work/ocispec/.veneer/veneer.yaml"
  printf 'version: 1\nenv:\n  LAYER: base\n' >"$work/ocispec/.veneer/base.yaml"
  printf 'hello {{ workspace.id }}\n' >"$work/ocispec/.veneer/hello.txt.j2"
  sleep 2
  check "oci: oras pushes the spec directory" docker run --rm --network "$net" -v "$work/ocispec:/w" -w /w ghcr.io/oras-project/oras:v1.2.0 push --plain-http "$reg:5000/envs/app:v1" .veneer
  check "oci: oras pushes a single-file base layer" docker run --rm --network "$net" -v "$work/ocispec/.veneer:/w" -w /w ghcr.io/oras-project/oras:v1.2.0 push --plain-http "$reg:5000/envs/base:v1" base.yaml
  start "$name" alpine:3.20 --network "$net" -e VENEER_OCI_INSECURE=1
  check "oci: apply oci:// spec (with its own extends and template file)" sh -c "docker exec $name veneer apply oci://$reg:5000/envs/app:v1 --id o1 --json | grep -q '\"ok\":true' && docker exec $name sh -c 'grep -q \"hello o1\" /var/lib/veneer/roots/o1/hello.txt && veneer env oci://$reg:5000/envs/app:v1 --id o1 --format dotenv | grep -q LAYER=base'"
  docker exec "$name" sh -c "mkdir -p /p/.veneer && printf 'version: 1\nextends: [\"oci://$reg:5000/envs/base:v1//base.yaml\"]\n' > /p/.veneer/veneer.yaml"
  check "oci: extends oci:// is pinned by manifest digest in veneer.lock" sh -c "docker exec $name veneer check /p && docker exec $name grep -q '\"commit\": \"sha256:' /p/.veneer/veneer.lock"
  docker run --rm --network "$net" -v "$work/ocispec/.veneer:/w" -w /w alpine:3.20 sh -c "printf 'version: 1\nenv:\n  LAYER: changed\n' > /w/base.yaml" >/dev/null
  check "oci: upstream retag keeps resolving the pinned digest" sh -c "docker run --rm --network $net -v $work/ocispec/.veneer:/w -w /w ghcr.io/oras-project/oras:v1.2.0 push --plain-http $reg:5000/envs/base:v1 base.yaml >/dev/null && docker exec $name sh -c 'cd /p && veneer env --format dotenv | grep -q LAYER=base'"
  docker rm -f "$name" "$reg" >/dev/null
  docker network rm "$net" >/dev/null 2>&1
}

# ------------------------------------------------------------------ nix
suite_nix() {
  echo "▸ nix"
  local name="veneer-ct-nix"
  docker rm -f "$name" >/dev/null 2>&1
  docker run -d --name "$name" nixos/nix:2.24.10 sleep infinity >/dev/null
  docker exec "$name" mkdir -p /usr/local/bin /p/.veneer
  docker cp "$bin" "$name:/usr/local/bin/veneer" >/dev/null
  docker exec "$name" sh -c 'printf "version: 1\ntools:\n  \"nix:hello\": latest\n" > /p/.veneer/veneer.yaml'
  check "nix: nix:hello installs into veneer's profile" sh -c "docker exec $name /usr/local/bin/veneer apply /p --json | grep -q '\"ok\":true'"
  check "nix: tool on PATH through veneer exec" sh -c "docker exec $name /usr/local/bin/veneer exec /p -- hello | grep -q 'Hello, world!'"
  check "nix: second apply is a no-op" sh -c "docker exec $name /usr/local/bin/veneer apply /p --json | grep -q '\"applied\":0'"
  docker rm -f "$name" >/dev/null

  local g="veneer-ct-guard"
  start "$g" alpine:3.20
  docker exec "$g" sh -c 'mkdir -p /g/.veneer && printf "version: 1\ntools:\n  \"aqua:BurntSushi/ripgrep\": \"14.1.1\"\n" > /g/.veneer/veneer.yaml'
  if [ "$(docker exec "$g" uname -m)" = aarch64 ]; then
    check "nix: glibc-only tool on musl fails early with V4002" sh -c "! docker exec $g veneer apply /g > $logs/guard.txt 2>&1 && grep -q V4002 $logs/guard.txt"
  fi
  docker rm -f "$g" >/dev/null
}

# ------------------------------------------------------------------ sandbox
suite_sandbox() {
  echo "▸ sandbox"
  local image="veneer-ct/fixture:latest"
  if ! docker image inspect "$image" >/dev/null 2>&1; then
    export VENEER_STATE_DIR="$work/host-state"
    cp -R "$here/fixture" "$work/sbproj" 2>/dev/null
    "$host_bin" bake "$work/sbproj" --veneer-bin "$bin" --tag "$image" >/dev/null 2>&1
    unset VENEER_STATE_DIR
  fi
  local t0=$(date +%s)
  check "sandbox: up --count 3 from the baked image" sh -c "'$host_bin' up '$here/fixture' --image $image --count 3 --name ct-sb --json --env TEST_API_TOKEN=$TOKEN -v $work/upstream.git:/srv/upstream.git:ro | tee $logs/sb.ndjson | grep -c '\"ok\":true' | grep -qx 3"
  local secs=$(( $(date +%s) - t0 ))
  check "sandbox: 3 warm sandboxes ready in ${secs}s (< 60s)" test "$secs" -lt 60
  check "sandbox: ls lists them" sh -c "'$host_bin' ls | grep -c '^ct-sb-' | grep -qx 3"
  check "sandbox: enter runs with that sandbox's workspace env" sh -c "'$host_bin' enter ct-sb-2 -- sh -c 'echo \$VENEER_WORKSPACE_ID; jq --version' | tr '\n' ' ' | grep -q 'ct-sb-2 jq-1.7.1'"
  check "sandbox: rm --all removes them" sh -c "'$host_bin' rm --all && [ -z \"\$('$host_bin' ls)\" ]"
}

# ------------------------------------------------------------------ resilience
suite_resilience() {
  echo "▸ resilience"
  local img="buildpack-deps:bookworm-scm" a="veneer-ct-res-a" b="veneer-ct-res-b"
  start "$a" "$img"
  docker cp "$here/offline-fixture" "$a:/p" >/dev/null
  docker cp "$here/resilience-fixture" "$a:/r" >/dev/null
  x() { docker exec "$a" sh -c "$*"; }
  check "lock: pins tools, images, and repos in veneer.lock" x "veneer lock /p && python3 -c 'import json,re; l=json.load(open(\"/p/.veneer/veneer.lock\")); assert re.fullmatch(r\"1\\.7(\\.\\d+)?\", l[\"tools\"][\"jq@1.7\"]), l; assert l[\"images\"][\"busybox:1.36\"].startswith(\"sha256:\"); assert len(l[\"repos\"][\"/srv/upstream.git#main\"]) == 40'"
  check "lock: --frozen apply uses the pins" x "veneer apply /p --frozen --id t1 --json | tee /tmp/frozen.ndjson | grep -q '\"event\":\"run.done\",\"ok\":true' && test \"\$(git -C /work/t1/src rev-parse HEAD)\" = \"\$(python3 -c 'import json; print(json.load(open(\"/p/.veneer/veneer.lock\"))[\"repos\"][\"/srv/upstream.git#main\"])')\""
  check "lock: --frozen refuses an unpinned image before doing anything" x "sed -i 's/images: \[busybox:1.36\]/images: [busybox:1.36, alpine:3.20]/' /p/.veneer/veneer.yaml; veneer apply /p --frozen --id t9 2>/tmp/f.err; rc=\$?; sed -i 's/, alpine:3.20//' /p/.veneer/veneer.yaml; [ \$rc = 2 ] && grep -q 'V1006' /tmp/f.err && ! test -e /var/lib/veneer/workspaces/t9/journal.json"
  check "retry: a flaky hook is retried with backoff until it passes" x "rm -f /tmp/flaky; veneer apply /r --id r1 --json > /tmp/retry.ndjson; grep -q '\"event\":\"run.done\",\"ok\":true' /tmp/retry.ndjson && [ \$(grep -c '\"event\":\"step.retry\"' /tmp/retry.ndjson) = 2 ]"
  check "timeout: a hung hook is killed at its timeout" x "s=\$(date +%s); ! veneer apply /r/.veneer/timeout.yaml --id r2 2>/tmp/t.err; [ \$((\$(date +%s) - s)) -lt 15 ] && grep -q 'timed out' /tmp/t.err"
  check "doctor: a missing host requirement fails before any step runs" x "! veneer doctor /r/.veneer/nix.yaml > /tmp/doc.txt; grep -q 'V5004' /tmp/doc.txt; veneer apply /r/.veneer/nix.yaml --id r3 2>/tmp/d.err; [ \$? = 2 ] && grep -q 'nothing was changed' /tmp/d.err && ! test -e /var/lib/veneer/build.json -a -n \"\$(grep nix /var/lib/veneer/build.json 2>/dev/null)\""
  check "fetch: writes an OCI layout bundle" x "veneer fetch /p --frozen -o /bundle && test -f /bundle/oci-layout && python3 -c 'import json; i=json.load(open(\"/bundle/index.json\")); assert i[\"manifests\"][0][\"artifactType\"]==\"application/vnd.veneer.bundle.v1+json\"'"
  docker exec "$a" veneer apply /p --frozen --id off >/dev/null 2>&1
  local fpa
  fpa=$(docker exec "$a" python3 -c 'import json; print(json.load(open("/var/lib/veneer/workspaces/off/manifest.json"))["fingerprint"])')
  rm -rf "$work/bundle" "$work/proj-off"; docker cp "$a:/bundle" "$work/bundle" >/dev/null; docker cp "$a:/p" "$work/proj-off" >/dev/null
  docker rm -f "$b" >/dev/null 2>&1
  docker run -d --name "$b" --network none "$img" sleep infinity >/dev/null
  docker cp "$bin" "$b:/usr/local/bin/veneer" >/dev/null
  docker cp "$work/bundle" "$b:/bundle" >/dev/null; docker cp "$work/proj-off" "$b:/p" >/dev/null
  check "offline: apply --from a bundle with no network" sh -c "docker exec $b veneer apply /p --frozen --from /bundle --id off --json | tee $logs/offline.ndjson | grep -q '\"event\":\"run.done\",\"ok\":true' && docker exec $b sh -c 'grep -q jq-1.7 /work/off/jq.txt && test -f /work/off/src/README.md && test -s /var/lib/veneer/data/images/busybox_1.36.tar'"
  check "offline: the environment fingerprint equals the online run" sh -c "[ -n '$fpa' ] && docker exec $b veneer verify /p --frozen --id off --expect '$fpa'"
  check "offline: a corrupt bundle blob is rejected" sh -c "docker exec $b sh -c 'f=\$(ls -S /bundle/blobs/sha256/* | head -1); echo x >> \$f; ! veneer apply /p --from /bundle --id off2 2>/tmp/c.err && grep -q corrupt /tmp/c.err'"
  docker rm -f "$a" "$b" >/dev/null
}

# ------------------------------------------------------------------ cloudinit
suite_cloudinit() {
  echo "▸ cloudinit"
  local ud="$work/ci"; mkdir -p "$ud"
  check "cloudinit: bake writes user-data and a NoCloud seed ISO" env VENEER_STATE_DIR="$work/host-state" "$host_bin" bake "$here/vm-fixture" --target cloud-init --embed-veneer --veneer-bin "$bin" -o "$ud/user-data" --seed "$ud/seed.iso"
  check "cloudinit: user-data passes \`cloud-init schema\`" docker run --rm -v "$ud:/t" ubuntu:24.04 sh -c 'apt-get update -qq >/dev/null && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq cloud-init >/dev/null 2>&1 && cloud-init schema --config-file /t/user-data'
  check "cloudinit: the seed ISO is labelled cidata and holds both files" docker run --rm -v "$ud:/t" alpine:3.20 sh -c 'apk add --no-cache xorriso >/dev/null 2>&1 && xorriso -indev /t/seed.iso -pvd_info 2>/dev/null | grep -qi "Volume Id.*cidata" && xorriso -indev /t/seed.iso -ls / 2>/dev/null | grep -qi user-data && xorriso -indev /t/seed.iso -ls / 2>/dev/null | grep -qi meta-data'
}

# ------------------------------------------------------------------ vmformats
suite_vmformats() {
  echo "▸ vmformats"
  local o="$work/vmf" hs="$work/host-state"; mkdir -p "$o"; printf 'not a kernel' > "$o/vmlinux"
  local B="env VENEER_STATE_DIR=$hs $host_bin bake $here/vm-fixture --veneer-bin $bin --tag veneer-ct/vm:latest"
  check "vmformats: microvm bundle has Firecracker, Cloud Hypervisor, and QEMU launchers" sh -c "$B --target microvm --kernel $o/vmlinux -o $o/mvm >/dev/null 2>&1 && for f in vm-config.json cloud-hypervisor.sh qemu.sh cmdline rootfs.ext4 vmlinux; do test -s $o/mvm/\$f || exit 1; done && sh -n $o/mvm/qemu.sh && sh -n $o/mvm/cloud-hypervisor.sh && test -x $o/mvm/qemu.sh"
  check "vmformats: OVA = OVF first, stream-optimized VMDK, matching SHA-256 manifest" sh -c "$B --target disk -o $o/agent.ova >$logs/ova.log 2>&1 && tar -tf $o/agent.ova > $o/ova.list && sed -n 1p $o/ova.list | grep -qx agent.ovf && grep -qx agent-disk1.vmdk $o/ova.list && grep -qx agent.mf $o/ova.list && mkdir -p $o/x && tar -xf $o/agent.ova -C $o/x && python3 -c 'import xml.dom.minidom,sys; xml.dom.minidom.parse(sys.argv[1])' $o/x/agent.ovf && (cd $o/x && sed -E 's/^SHA256\(([^)]*)\)= (.*)/\2  \1/' agent.mf | shasum -a 256 -c -) && docker run --rm -v $o/x:/w alpine:3.20 sh -c 'apk add --no-cache qemu-img >/dev/null && qemu-img info /w/agent-disk1.vmdk' | grep -qi 'create type: streamOptimized'"
  check "vmformats: fixed VHD with a whole-MiB virtual size" sh -c "$B --target disk -o $o/agent.vhd --vhd-fixed >$logs/vhd.log 2>&1 && docker run --rm -v $o:/w alpine:3.20 sh -c 'apk add --no-cache qemu-img >/dev/null && qemu-img info -f vpc --output json /w/agent.vhd' > $o/vhd.json && python3 -c 'import json,os,sys; i=json.load(open(sys.argv[1])); v=i[\"virtual-size\"]; assert i[\"format\"]==\"vpc\", i; assert v % 1048576 == 0, v; assert os.path.getsize(sys.argv[2]) == v + 512, (os.path.getsize(sys.argv[2]), v)' $o/vhd.json $o/agent.vhd"
  rm -rf "$o"
}

# ------------------------------------------------------------------ k8s
suite_k8s() {
  echo "▸ k8s"
  local hs="$work/host-state" cl="veneer-ct" kube="$work/kubeconfig"
  local M="env MISE_DATA_DIR=$work/mise MISE_CACHE_DIR=$work/mise-cache MISE_YES=1 mise x aqua:kubernetes-sigs/kind@0.30.0 aqua:kubernetes/kubectl@1.34.1 --"
  check "k8s: bake the workspace image" env VENEER_STATE_DIR="$hs" "$host_bin" bake "$here/k8s-fixture" --veneer-bin "$bin" --tag veneer-ct/k8s:v1
  check "k8s: render an Indexed Job of 3" sh -c "env VENEER_STATE_DIR=$hs '$host_bin' render k8s '$here/k8s-fixture' --image veneer-ct/k8s:v1 --count 3 --name agent -o $work/job.yaml && grep -q 'completionMode: \"Indexed\"' $work/job.yaml && grep -q 'restartPolicy: \"Always\"' $work/job.yaml && grep -q secretKeyRef $work/job.yaml"
  check "k8s: kubeconform accepts the manifest" docker run --rm -v "$work:/w" ghcr.io/yannh/kubeconform:v0.7.0 -strict -summary /w/job.yaml
  $M kind delete cluster --name "$cl" >/dev/null 2>&1
  check "k8s: kind cluster up" $M kind create cluster --name "$cl" --kubeconfig "$kube" --wait 120s
  local K="$M kubectl --kubeconfig $kube"
  $M kind load docker-image veneer-ct/k8s:v1 --name "$cl" >/dev/null 2>&1
  $K create secret generic veneer-secrets --from-literal=api_token=k8s-$TOKEN >/dev/null 2>&1
  check "k8s: all 3 workspaces become Ready" sh -c "$K apply -f $work/job.yaml >/dev/null && $K wait --for=condition=Ready pod -l app.kubernetes.io/name=agent --timeout=300s"
  local pod; pod=$($K get pod -l app.kubernetes.io/name=agent -o name 2>/dev/null | head -1)
  local idx; idx=$($K get "$pod" -o jsonpath='{.metadata.annotations.batch\.kubernetes\.io/job-completion-index}' 2>/dev/null)
  check "k8s: each pod has its own workspace id, the secret, and the sidecar" sh -c "$K exec $pod -c workspace -- sh -c 'cat /work/agent-$idx/hello.txt' | grep -q 'hello agent-$idx token-len=$(( ${#TOKEN} + 4 ))' && $K exec $pod -c workspace -- grep -qi nginx /work/agent-$idx/web.html"
  check "k8s: veneer verify passes inside the pod" $K exec "$pod" -c workspace -- sh -c "veneer verify /opt/veneer/project/.veneer/veneer.yaml --id agent-$idx"
  check "k8s: the secret value is not in the rendered manifest" sh -c "! grep -q '$TOKEN' $work/job.yaml"
  $M kind delete cluster --name "$cl" >/dev/null 2>&1
  docker rmi -f veneer-ct/k8s:v1 >/dev/null 2>&1
}

# ------------------------------------------------------------------ otel
suite_otel() {
  echo "▸ otel"
  local net="veneer-ct-otel" col="veneer-ct-collector" name="veneer-ct-otel-ws" out="$work/otel"
  local tid="4bf92f3577b34da6a3ce929d0e0e4736" psid="00f067aa0ba902b7"
  mkdir -p "$out"; chmod 777 "$out"
  docker network create "$net" >/dev/null 2>&1
  docker rm -f "$col" >/dev/null 2>&1
  docker run -d --name "$col" --network "$net" -v "$here/otel-collector.yaml:/etc/otelcol-contrib/config.yaml:ro" -v "$out:/out" otel/opentelemetry-collector-contrib:0.164.0 >/dev/null
  start "$name" alpine:3.20 --network "$net" -e OTEL_EXPORTER_OTLP_ENDPOINT=http://$col:4318 -e OTEL_SERVICE_NAME=veneer-ct -e TRACEPARENT=00-$tid-$psid-01
  docker cp "$here/otel-fixture" "$name:/p" >/dev/null
  sleep 3
  check "otel: apply exports a trace" sh -c "docker exec $name veneer apply /p --id o1 --json | grep -q '\"ok\":true' && for i in \$(seq 1 20); do [ -s $out/traces.json ] && break; sleep 1; done; [ -s $out/traces.json ]"
  check "otel: run span is a child of TRACEPARENT; step spans are children of the run" python3 -c "
import json
spans=[s for l in open('$out/traces.json') for r in json.loads(l)['resourceSpans'] for ss in r['scopeSpans'] for s in ss['spans']]
res=[a for l in open('$out/traces.json') for r in json.loads(l)['resourceSpans'] for a in r['resource']['attributes']]
assert {'key':'service.name','value':{'stringValue':'veneer-ct'}} in res, res
run=[s for s in spans if s['name']=='veneer apply'][0]
assert all(s['traceId']=='$tid' for s in spans)
assert run['parentSpanId']=='$psid', run
steps=[s for s in spans if s is not run]
assert len(steps)>=3 and all(s['parentSpanId']==run['spanId'] for s in steps), steps
assert any(a['key']=='veneer.step.action' for a in steps[0]['attributes'])
"
  check "otel: hooks receive a TRACEPARENT for their own step span" sh -c "tp=\$(docker exec $name cat /work/o1/tp.txt); echo \$tp | grep -q '^00-$tid-[0-9a-f]\{16\}-01\$' && grep -q \"\$(echo \$tp | cut -d- -f3)\" $out/traces.json"
  check "otel: no secret values in exported spans" sh -c "! grep -q '$TOKEN' $out/traces.json"
  check "otel: an unreachable collector never fails the run" sh -c "docker exec -e OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:9 $name veneer apply /p --id o2 --json | grep -q '\"ok\":true'"
  docker rm -f "$name" "$col" >/dev/null
  docker network rm "$net" >/dev/null 2>&1
}

# ------------------------------------------------------------------ supply
suite_supply() {
  echo "▸ supply"
  local reg="veneer-ct-supply-reg" att="$work/att" ref="localhost:5005/veneer-ct/supply"
  local oras="ghcr.io/oras-project/oras:v1.2.0" cosign="gcr.io/projectsigstore/cosign:v2.5.3"
  docker rm -f "$reg" >/dev/null 2>&1
  docker run -d --name "$reg" -p 5005:5000 registry:2 >/dev/null
  sleep 2
  mkdir -p "$att" "$work/keys"
  docker run --rm -e COSIGN_PASSWORD= -v "$work/keys:/k" -w /k "$cosign" generate-key-pair >/dev/null 2>&1
  local hs="$work/host-state"
  check "supply: bake --push with SBOMs and provenance" sh -c "env VENEER_STATE_DIR=$hs '$host_bin' bake '$here/vm-fixture' --veneer-bin '$bin' --tag $ref:v1 --push --sbom cyclonedx,spdx --provenance --insecure-registry --attest-dir $att > $work/digest1 2>$logs/supply-bake1.log && grep -q '^$ref@sha256:' $work/digest1"
  local d1; d1=$(cat "$work/digest1" 2>/dev/null)
  check "supply: OCI labels and the environment fingerprint label" sh -c "docker image inspect $ref:v1 --format '{{json .Config.Labels}}' | python3 -c 'import json,sys; l=json.load(sys.stdin); assert l[\"org.opencontainers.image.created\"].endswith(\"Z\"); assert l[\"org.opencontainers.image.version\"]==\"v1\"; assert l[\"dev.veneer.fingerprint\"].startswith(\"sha256:\"); assert l[\"dev.veneer.spec\"].startswith(\"sha256:\")'"
  check "supply: SBOMs and provenance are OCI referrers of the image" sh -c "docker run --rm --network host $oras discover --plain-http --format json $d1 > $work/refs.json && python3 -c 'import json; r=json.load(open(\"$work/refs.json\")); t={m[\"artifactType\"] for m in r.get(\"manifests\", r.get(\"referrers\", []))}; assert {\"application/vnd.cyclonedx+json\",\"application/spdx+json\",\"application/vnd.in-toto+json\"} <= t, t'"
  check "supply: the CycloneDX SBOM validates against the 1.6 schema" docker run --rm -v "$att:/a" cyclonedx/cyclonedx-cli validate --input-file /a/sbom.cdx.json --input-version v1_6 --fail-on-errors
  check "supply: the SPDX SBOM and SLSA statement are well formed" python3 -c "import json; s=json.load(open('$att/sbom.spdx.json')); assert s['spdxVersion']=='SPDX-2.3' and len(s['packages'])>1; p=json.load(open('$att/provenance.intoto.json')); assert p['_type']=='https://in-toto.io/Statement/v1' and p['predicateType']=='https://slsa.dev/provenance/v1'; assert p['subject'][0]['digest']['sha256']=='${d1##*sha256:}'"
  check "supply: bake --sign signs and attests with cosign" sh -c "env VENEER_STATE_DIR=$hs COSIGN_PASSWORD= '$host_bin' bake '$here/vm-fixture' --veneer-bin '$bin' --tag $ref:v2 --push --sbom cyclonedx --provenance --sign --sign-key $work/keys/cosign.key --no-tlog --insecure-registry --attest-dir $att/signed > $work/digest2 2>$logs/supply-bake2.log"
  local d2; d2=$(cat "$work/digest2" 2>/dev/null)
  check "supply: cosign verify accepts the signature" docker run --rm --network host -v "$work/keys:/k" "$cosign" verify --key /k/cosign.pub --insecure-ignore-tlog=true --allow-insecure-registry "$d2"
  check "supply: cosign verify-attestation accepts the SLSA provenance and SBOM" sh -c "docker run --rm --network host -v $work/keys:/k $cosign verify-attestation --key /k/cosign.pub --insecure-ignore-tlog=true --allow-insecure-registry --type slsaprovenance1 $d2 >/dev/null && docker run --rm --network host -v $work/keys:/k $cosign verify-attestation --key /k/cosign.pub --insecure-ignore-tlog=true --allow-insecure-registry --type cyclonedx $d2 >/dev/null"
  check "supply: an unsigned image is rejected" sh -c "! docker run --rm --network host -v $work/keys:/k $cosign verify --key /k/cosign.pub --insecure-ignore-tlog=true --allow-insecure-registry $ref:v1"
  docker rm -f "$reg" >/dev/null
}

# ------------------------------------------------------------------ vm
suite_vm() {
  echo "▸ vm"
  local shared="${VM_SHARED:-/tmp/veneer-fc}" kx="${KVM_EXEC:-}"
  if ! $kx test -c /dev/kvm 2>/dev/null; then
    bad "vm: no KVM host (set KVM_EXEC, see the header)"
    return
  fi
  mkdir -p "$shared"
  local fcv
  fcv=$(curl -fsSL https://api.github.com/repos/firecracker-microvm/firecracker/releases/latest | sed -n 's/.*"tag_name": *"\(v[^"]*\)".*/\1/p' | head -n1)
  $kx bash -c "set -e; cd $shared; arch=\$(uname -m)
    [ -x firecracker ] || { curl -fsSL https://github.com/firecracker-microvm/firecracker/releases/download/$fcv/firecracker-$fcv-\$arch.tgz | tar -xz; cp release-$fcv-\$arch/firecracker-$fcv-\$arch firecracker; }
    [ -s vmlinux ] && file vmlinux | grep -q 'Linux kernel' || curl -fsSL -o vmlinux https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.15/\$arch/vmlinux-6.1.155
    command -v qemu-system-\$arch >/dev/null || sudo DEBIAN_FRONTEND=noninteractive apt-get -qq install -y qemu-system-arm qemu-system-x86 qemu-efi-aarch64 >/dev/null" >/dev/null 2>&1
  local vb="$shared/veneer-linux"; cp "$bin" "$vb"; cp "$here/boot-cloudinit.sh" "$shared/boot-cloudinit.sh"
  rm -rf "$shared/bundle"
  check "vm: bake firecracker bundle with a real kernel" env VENEER_STATE_DIR="$work/host-state" "$host_bin" bake "$here/vm-fixture" --veneer-bin "$bin" --tag veneer-ct/vm:latest --target firecracker --kernel "$shared/vmlinux" -o "$shared/bundle"
  check "vm: firecracker boots, stages the workspace, and snapshots memory" $kx bash -c "sudo rm -rf /var/tmp/vfc /var/tmp/vfc-run-* && cp -r $shared/bundle /var/tmp/vfc && sudo VENEER_FIRECRACKER=$shared/firecracker $vb vm snapshot /var/tmp/vfc --timeout 240 && test -s /var/tmp/vfc/snapshot/mem && test -s /var/tmp/vfc/snapshot/vmstate"
  for i in 1 2; do
    check "vm: restore #$i resumes the snapshot (no reboot) and runs commands" $kx bash -c "(sleep 1; printf 'echo R$i=\$IN_VM-\$((6*7)); cat /work/default/hello.txt; echo b > /proc/sysrq-trigger\n') | timeout 60 sudo VENEER_FIRECRACKER=$shared/firecracker $vb vm restore /var/tmp/vfc --run-dir /var/tmp/vfc-run-$i > /var/tmp/vfc-r$i.log 2>&1; grep -q 'R$i=yes-42' /var/tmp/vfc-r$i.log && grep -q 'hello from alpine microvm' /var/tmp/vfc-r$i.log && grep -q 'resumed from snapshot' /var/tmp/vfc-r$i.log && ! grep -q 'Run /init' /var/tmp/vfc-r$i.log"
  done
  # The same bundle on the other VMMs, a qcow2 root disk, and cloud-init on a stock cloud image.
  $kx bash -c "set -e; cd $shared; arch=\$(uname -m)
    [ -x cloud-hypervisor ] || { [ \$arch = aarch64 ] && n=cloud-hypervisor-static-aarch64 || n=cloud-hypervisor-static; curl -fsSL -o cloud-hypervisor https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v48.0/\$n && chmod +x cloud-hypervisor; }
    command -v xorriso >/dev/null || sudo DEBIAN_FRONTEND=noninteractive apt-get -qq install -y xorriso >/dev/null
    [ -s ubuntu.img ] || curl -fsSL -o ubuntu.img https://cloud-images.ubuntu.com/releases/24.04/release/ubuntu-24.04-server-cloudimg-\$([ \$arch = aarch64 ] && echo arm64 || echo amd64).img" >/dev/null 2>&1
  check "vm: QEMU boots the bundle and runs the workspace phase" $kx bash -c "sudo rm -rf /var/tmp/vq && cp -r $shared/bundle /var/tmp/vq && cd /var/tmp/vq && sudo timeout 180 sh qemu.sh </dev/null > /var/tmp/vq.log 2>&1; grep -q 'veneer: workspace ready' /var/tmp/vq.log"
  check "vm: Cloud Hypervisor boots the bundle and runs the workspace phase" $kx bash -c "sudo rm -rf /var/tmp/vch && cp -r $shared/bundle /var/tmp/vch && cd /var/tmp/vch && sudo CLOUD_HYPERVISOR=$shared/cloud-hypervisor timeout 180 sh cloud-hypervisor.sh </dev/null > /var/tmp/vch.log 2>&1; grep -q 'veneer: workspace ready' /var/tmp/vch.log"
  check "vm: a --target qcow2 root disk boots through the QEMU launcher" sh -c "env VENEER_STATE_DIR='$work/host-state' '$host_bin' bake '$here/vm-fixture' --veneer-bin '$bin' --tag veneer-ct/vm:latest --target qcow2 -o '$shared/root.qcow2' >/dev/null 2>&1 && $kx bash -c 'cd /var/tmp/vq && sudo DISK=$shared/root.qcow2 FORMAT=qcow2 timeout 180 sh qemu.sh </dev/null > /var/tmp/vqc.log 2>&1; grep -q \"veneer: workspace ready\" /var/tmp/vqc.log'"
  check "vm: cloud-init stages the workspace on a stock Ubuntu cloud image" sh -c "env VENEER_STATE_DIR='$work/host-state' '$host_bin' bake '$here/vm-fixture' --target cloud-init --embed-veneer --veneer-bin '$bin' -o '$shared/user-data' --seed '$shared/seed.iso' >/dev/null 2>&1 && $kx bash $shared/boot-cloudinit.sh $shared"
  cp "$here/boot-disk.sh" "$shared/boot-disk.sh"
  check "vm: bootable disk (d2vm) boots under QEMU/KVM and runs the workspace phase" sh -c "env VENEER_STATE_DIR='$work/host-state' '$host_bin' bake '$here/vm-fixture' --veneer-bin '$bin' --tag veneer-ct/vm:latest --target disk -o '$shared/vm.raw' >/dev/null 2>&1 && $kx bash $shared/boot-disk.sh $shared/vm.raw"
}

for s in $SUITES; do "suite_$s"; done

echo
echo "passed: $pass  failed: $fail  (logs: $logs)"
if [ "$fail" -gt 0 ]; then
  printf '  - %s\n' "${failures[@]}"
  exit 1
fi
