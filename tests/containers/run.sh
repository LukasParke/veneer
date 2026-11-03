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
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
bin="$repo/target/linux/release/veneer"
host_bin="${HOST_VENEER:-$repo/target/debug/veneer}"
SUITES="${SUITES:-distros bake compose install}"
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
  check "bake: rootfs ext4 image" sh -c "'$host_bin' bake '$proj' --veneer-bin '$bin' --tag '$tag' --target rootfs -o '$work/rootfs.ext4' && file '$work/rootfs.ext4' | grep -qi ext4"
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

for s in $SUITES; do "suite_$s"; done

echo
echo "passed: $pass  failed: $fail  (logs: $logs)"
if [ "$fail" -gt 0 ]; then
  printf '  - %s\n' "${failures[@]}"
  exit 1
fi
