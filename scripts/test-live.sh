#!/usr/bin/env bash
# Runs the live storage tests in Docker against a real bucket in Cloudflare R2 (`r2`) or Amazon S3
# (`aws`), with PostgreSQL and MySQL servers for the backups through the CLI. Docker is the only
# requirement. The tests write under svalbard-test/ in the bucket and remove what they wrote. At
# the end, the script removes the containers, the volumes and the images that it pulled.
#
# r2: set SVALBARD_TEST_R2_ENDPOINT (https://<account-id>.r2.cloudflarestorage.com) and
# SVALBARD_TEST_R2_BUCKET, and the keys of an R2 API token with Object Read & Write on the bucket
# in SVALBARD_TEST_R2_ACCESS_KEY_ID and SVALBARD_TEST_R2_SECRET_ACCESS_KEY.
#
# aws: set SVALBARD_TEST_AWS_BUCKET and AWS_REGION, and the credentials in AWS_ACCESS_KEY_ID and
# AWS_SECRET_ACCESS_KEY, with AWS_SESSION_TOKEN for temporary credentials.
#
# Usage: scripts/test-live.sh r2|aws [test filter]
set -euo pipefail

export SVALBARD_TEST_LIVE=${1:-}
case "$SVALBARD_TEST_LIVE" in
  r2)
    : "${SVALBARD_TEST_R2_ENDPOINT:?must be set}" "${SVALBARD_TEST_R2_BUCKET:?must be set}"
    export SVALBARD_TEST_R2_ENDPOINT SVALBARD_TEST_R2_BUCKET
    export AWS_ACCESS_KEY_ID=${SVALBARD_TEST_R2_ACCESS_KEY_ID:?must be set}
    export AWS_SECRET_ACCESS_KEY=${SVALBARD_TEST_R2_SECRET_ACCESS_KEY:?must be set}
    # An AWS session token belongs to other keys.
    unset AWS_SESSION_TOKEN
    ;;
  aws)
    : "${SVALBARD_TEST_AWS_BUCKET:?must be set}" "${AWS_REGION:?must be set}"
    : "${AWS_ACCESS_KEY_ID:?must be set}" "${AWS_SECRET_ACCESS_KEY:?must be set}"
    export SVALBARD_TEST_AWS_BUCKET AWS_REGION AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY
    ;;
  *)
    echo "Usage: scripts/test-live.sh r2|aws [test filter]" >&2
    exit 2
    ;;
esac
export MYSQL_SERIES=${MYSQL_SERIES:-8.4} POSTGRESQL_VERSION=${POSTGRESQL_VERSION:-18}
# A project of its own, so that the cleanup never touches another run.
compose=(docker compose --project-name "svalbard-$SVALBARD_TEST_LIVE"
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

"${compose[@]}" run --rm live cargo test --locked --test live -- --ignored --show-output "${@:2}"
