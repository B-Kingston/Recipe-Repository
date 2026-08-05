#!/usr/bin/env bash
set -euo pipefail

# Deploy Recipe-Repository locally via Docker Compose: build the image, start
# detached, wait for /healthz, print the URL. Data persists in the
# recipe-data volume; optional env overrides come from .env.
# Usage: ./deploy-local.sh [-f|--foreground]   (default: detached)

FOREGROUND=0
case "${1:-}" in
  -f|--foreground) FOREGROUND=1 ;;
  "") ;;
  *)
    echo "Usage: $0 [-f|--foreground]" >&2
    exit 2
    ;;
esac

if ! command -v docker >/dev/null 2>&1; then
  echo "docker not found in PATH" >&2
  exit 1
fi

if [ "$FOREGROUND" -eq 1 ]; then
  echo "Building and starting in foreground (Ctrl-C to stop)..."
  exec docker compose up --build
fi

echo "Building image and starting detached..."
docker compose up --build -d

echo "Waiting for http://localhost:3000/healthz ..."
for _ in $(seq 1 60); do
  if curl -fsS http://localhost:3000/healthz >/dev/null 2>&1; then
    echo "Done. App is at http://localhost:3000"
    echo "Logs: docker compose logs -f"
    echo "Stop: docker compose down"
    exit 0
  fi
  sleep 1
done

echo "Timed out waiting for http://localhost:3000/healthz — check 'docker compose logs'" >&2
exit 1
