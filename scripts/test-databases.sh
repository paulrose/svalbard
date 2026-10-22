#!/usr/bin/env bash
# Runs the PostgreSQL and MySQL tests in Docker, against servers with TLS. Docker is the only
# requirement: the runner installs Rust and the native clients of the server versions inside
# its container. At the end, the script removes the containers, the volumes and the images
# that it pulled, so a run leaves nothing behind.
#
# Usage: scripts/test-databases.sh [postgresql version] [mysql series] [test filter]
# Example: scripts/test-databases.sh 15 9.7
set -euo pipefail

export POSTGRESQL_VERSION=${1:-18} MYSQL_SERIES=${2:-8.4}
compose=(docker compose --file "$(cd "$(dirname "$0")/.." && pwd)/tests/docker/compose.yaml")

# Images that were here before the run stay.
pulled=()
for image in "postgres:$POSTGRESQL_VERSION" "mysql:$MYSQL_SERIES"; do
  docker image inspect "$image" >/dev/null 2>&1 || pulled+=("$image")
done
cleanup() {
  "${compose[@]}" down --volumes --timeout 1 >/dev/null 2>&1 || true
  if [ ${#pulled[@]} -gt 0 ]; then docker image rm "${pulled[@]}" >/dev/null || true; fi
}
trap cleanup EXIT

"${compose[@]}" run --rm tests \
  cargo test --locked --test postgresql --test mysql -- --ignored "${@:3}"
