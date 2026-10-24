#!/usr/bin/env bash
# Runs the R2 compatibility tests in Docker against a real Cloudflare R2 bucket, with PostgreSQL
# and MySQL servers for the backups through the CLI. Docker is the only requirement. The tests
# write under svalbard-test/ in the bucket and remove what they wrote. At the end, the script
# removes the containers, the volumes and the images that it pulled.
#
# Set SVALBARD_TEST_R2_ENDPOINT (https://<account-id>.r2.cloudflarestorage.com) and
# SVALBARD_TEST_R2_BUCKET, and the keys of an R2 API token with Object Read & Write on the bucket
# in SVALBARD_TEST_R2_ACCESS_KEY_ID and SVALBARD_TEST_R2_SECRET_ACCESS_KEY.
#
# Usage: scripts/test-r2.sh [test filter]
set -euo pipefail

: "${SVALBARD_TEST_R2_ENDPOINT:?must be set}" "${SVALBARD_TEST_R2_BUCKET:?must be set}"
export AWS_ACCESS_KEY_ID=${SVALBARD_TEST_R2_ACCESS_KEY_ID:?must be set}
export AWS_SECRET_ACCESS_KEY=${SVALBARD_TEST_R2_SECRET_ACCESS_KEY:?must be set}
export SVALBARD_TEST_R2_ENDPOINT SVALBARD_TEST_R2_BUCKET MYSQL_SERIES=${MYSQL_SERIES:-8.4} \
  POSTGRESQL_VERSION=${POSTGRESQL_VERSION:-18}
# A project of its own, so that the cleanup never touches a run of test-integration.sh.
compose=(docker compose --project-name svalbard-r2
  --file "$(cd "$(dirname "$0")/.." && pwd)/tests/docker/compose.yaml")

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

"${compose[@]}" run --rm r2 cargo test --locked --test r2 -- --ignored --show-output "$@"
