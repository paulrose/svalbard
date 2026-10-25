#!/usr/bin/env bash
# Runs the PostgreSQL, MySQL, S3 and SSH tests in Docker, against database servers with TLS, a
# PostgreSQL standby, a MySQL replica, the Floci S3 emulator and an OpenSSH server. Docker is the
# only requirement: the runner installs Rust and the native clients inside its container. At the
# end, the script removes the containers, the volumes and the images that it pulled, so a run
# leaves nothing behind.
#
# Usage: scripts/test-integration.sh [postgresql version] [mysql series] [test filter]
# Example: scripts/test-integration.sh 15 9.7
set -euo pipefail

export POSTGRESQL_VERSION=${1:-18} MYSQL_SERIES=${2:-8.4} FLOCI_VERSION=${FLOCI_VERSION:-2.3.0}
compose=(docker compose --file "$(cd "$(dirname "$0")/.." && pwd)/tests/docker/compose.yaml")

# Images that were here before the run stay.
pulled=()
for image in "postgres:$POSTGRESQL_VERSION" "mysql:$MYSQL_SERIES" "floci/floci:$FLOCI_VERSION" \
  alpine:3.24; do
  docker image inspect "$image" >/dev/null 2>&1 || pulled+=("$image")
done
cleanup() {
  "${compose[@]}" down --volumes --timeout 1 >/dev/null 2>&1 || true
  if [ ${#pulled[@]} -gt 0 ]; then docker image rm "${pulled[@]}" >/dev/null || true; fi
}
trap cleanup EXIT

"${compose[@]}" run --rm tests \
  cargo test --locked --test postgresql --test mysql --test s3 --test ssh --test backup \
  --test matrix --test scenarios -- --ignored --show-output "${@:3}"
