#!/usr/bin/env bash
set -euo pipefail

# Puts the working tree's app changes into the running app on this machine
# (minibuntu) by the quickest path that covers them, then waits for /healthz.
#
#   static  static/*.css, svg, fonts: copied into the running container
#           (ServeDir reads them from disk, so no restart).
#   web     web/src: app.js and htmx.js built in a throwaway node container,
#           then copied in like static.
#   full    anything else (src, templates, migrations, pi, Cargo, Dockerfile):
#           image rebuilt and the container recreated. The live database is
#           backed up first only when migrations changed.
#   auto    (default) picks one of the above by comparing the tree with the
#           state recorded after the last full rebuild.
#
# Usage: .claude/skills/t3-show/show.sh [auto|static|web|full]
# Prints the chosen mode and ends with "ready: http://minibuntu:3000".

cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"

STATE=target/.show-state
NODE_IMAGE=node:22.23.4-bookworm-slim
HEALTH=http://localhost:3000/healthz

# Checksums of every file a full rebuild depends on, one tree per line.
fingerprint() {
  local part
  for part in "$@"; do
    printf '%s ' "$part"
    find $part -type f 2>/dev/null | LC_ALL=C sort | xargs -r sha256sum | sha256sum | cut -c1-16
  done
}
server_parts() { fingerprint "src templates" migrations pi "Cargo.toml Cargo.lock Dockerfile compose.yaml package.json package-lock.json"; }
web_parts() { fingerprint web; }

wait_healthy() {
  for _ in $(seq 1 90); do
    if curl -fsS "$HEALTH" >/dev/null 2>&1; then return 0; fi
    sleep 2
  done
  echo "Timed out waiting for $HEALTH; see 'docker compose logs recipes'" >&2
  exit 1
}

running() {
  local id
  id=$(docker compose ps -q recipes 2>/dev/null || true)
  [ -n "$id" ] && [ "$(docker inspect -f '{{.State.Running}}' "$id")" = true ]
}

copy_static() {
  docker compose cp static/. recipes:/app/static/ >/dev/null 2>&1
}

build_web() {
  docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp \
    -v "$PWD":/app -w /app "$NODE_IMAGE" \
    sh -c 'node_modules/.bin/tsc -p web && node_modules/.bin/tsc -p web/tsconfig.htmx.json'
}

full() {
  local backup=--no-backup
  if [ -f "$STATE" ] && ! grep -qxF "$(fingerprint migrations)" "$STATE"; then
    backup=
  fi
  if [ -x ./deploy.sh ]; then
    ./deploy.sh "$(hostname)" $backup
  else
    docker compose up --build -d
  fi
  wait_healthy
  { server_parts; web_parts; } > "$STATE"
}

mode=${1:-auto}
if [ "$mode" = auto ]; then
  if ! running || [ ! -f "$STATE" ] || [ "$(server_parts)" != "$(grep -v '^web ' "$STATE")" ]; then
    mode=full
  elif [ "$(web_parts)" != "$(grep '^web ' "$STATE")" ]; then
    mode=web
  else
    mode=static
  fi
fi
echo "mode: $mode"

case "$mode" in
  static) copy_static ;;
  web) build_web; copy_static ;;
  full) full ;;
  *) echo "Usage: $0 [auto|static|web|full]" >&2; exit 2 ;;
esac

wait_healthy
echo "ready: http://minibuntu:3000"
