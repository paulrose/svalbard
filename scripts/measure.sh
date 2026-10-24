#!/usr/bin/env bash
# Measures the workload impact of backups against the budgets in docs/database-safety.md
# ("Measurement plan"), for one PostgreSQL version and one MySQL series. It runs the steps of
# tests/workload.rs in one runner container, and prints Markdown tables. A pair takes about 75
# minutes and needs about 15 GB of Docker disk.
#
# Docker is the only requirement. On a host with 8 or more CPUs, each server, the load and the
# backup get their own CPUs. At the end, the script removes the containers, the volumes and the
# images that it pulled, so a run leaves nothing behind.
#
# Usage: scripts/measure.sh [postgresql version] [mysql series]
# A trial run: SVALBARD_MEASURE_WINDOW=30 SVALBARD_MEASURE_SCALE=10 scripts/measure.sh
set -euo pipefail

export POSTGRESQL_VERSION=${1:-18} MYSQL_SERIES=${2:-8.4} FLOCI_VERSION=${FLOCI_VERSION:-2.3.0}
root=$(cd "$(dirname "$0")/.." && pwd)
compose=(docker compose --progress quiet --file "$root/tests/docker/compose.yaml"
  --file "$root/tests/docker/measure.yaml")
runner=svalbard-measure

pulled=()
for image in "postgres:$POSTGRESQL_VERSION" "mysql:$MYSQL_SERIES" "floci/floci:$FLOCI_VERSION" \
  alpine:3.24; do
  docker image inspect "$image" >/dev/null 2>&1 || pulled+=("$image")
done
samples=$(mktemp -d)
cleanup() {
  if [ -n "${sampler:-}" ]; then kill "$sampler" && wait "$sampler" || true; fi 2>/dev/null
  docker rm --force "$runner" >/dev/null 2>&1 || true
  "${compose[@]}" down --volumes --timeout 1 >/dev/null 2>&1 || true
  if [ ${#pulled[@]} -gt 0 ]; then docker image rm "${pulled[@]}" >/dev/null || true; fi
  rm -rf "$samples"
}
trap cleanup EXIT

cpus=$(docker info --format '{{.NCPU}}')
if [ "$cpus" -ge 8 ]; then
  export MEASURE_PRIMARY_CPUS=0-1 MEASURE_REPLICA_CPUS=2-3 MEASURE_RUNNER_CPUS=4-7 \
    MEASURE_LOAD_CPUS=4-5 MEASURE_BACKUP_CPUS=6-7
  export MEASURE_LAYOUT="pinned on $cpus CPUs: primary 0-1, replica 2-3, load 4-5, backup 6-7"
else
  export MEASURE_LAYOUT="shared, $cpus CPUs"
fi

"${compose[@]}" run --detach --name "$runner" tests \
  bash -c 'touch /tmp/ready && exec sleep infinity' >/dev/null
for _ in $(seq 600); do
  docker exec "$runner" test -e /tmp/ready 2>/dev/null && break
  sleep 1
done
free=$(docker exec "$runner" df --output=avail --block-size=1G / | tail -1 | tr -d ' ')
if [ "$free" -lt 15 ]; then
  echo "error: Docker has ${free} GB free; the measurements need about 15 GB." >&2
  exit 1
fi

run() {
  docker exec --env "PATH=/usr/pgsql-$POSTGRESQL_VERSION/bin:/root/.cargo/bin:/usr/bin:/bin" \
    "$runner" "$@"
}

# The tests run on the backup CPUs, so Svalbard and its clients do too.
step() {
  echo "measure.sh: $1" >&2
  run ${MEASURE_BACKUP_CPUS:+taskset --cpu-list "$MEASURE_BACKUP_CPUS"} \
    cargo test --release --locked --quiet --test workload -- --ignored --exact "$1" --nocapture \
    --test-threads 1
}

# Every 2 s: the CPU time and the bytes read and written of each server, from its cgroup.
sample() {
  local ids=() service
  for service in "$@"; do ids+=("$service=$("${compose[@]}" ps --quiet "$service")"); done
  while sleep 2; do
    for id in "${ids[@]}"; do
      docker exec "${id#*=}" cat /sys/fs/cgroup/cpu.stat /sys/fs/cgroup/io.stat 2>/dev/null |
        awk -v t="$(date +%s)" -v n="${id%%=*}" '
          $1 == "usage_usec" { cpu = $2 }
          { for (i = 2; i <= NF; i++) { split($i, f, "="); if (f[1] == "rbytes") r += f[2];
            if (f[1] == "wbytes") w += f[2] } }
          END { print t, n, cpu, r + 0, w + 0 }'
    done | docker exec --interactive "$runner" sh -c 'mkdir -p /tmp/measure &&
      cat >>/tmp/measure/cgroup.log'
  done
}

# The Markdown of a step, without the lines of the test harness.
report() {
  grep --invert-match --extended-regexp '^(running [0-9]+ tests?|test result: .*|\.+)$' || true
}

measure() {
  local source=$1 primary=$2 replica=$3
  step "${source}_prepare" >&2
  sample "$primary" "$replica" >/dev/null &
  sampler=$!
  step "${source}_measure" | report
  kill "$sampler"
  wait "$sampler" 2>/dev/null || true
  sampler=
  # B8 opens 500 sessions, which need more memory than the measured server has.
  docker update --memory 3g --memory-swap 3g "$("${compose[@]}" ps --quiet "$primary")" >/dev/null
  step "${source}_detection" | report
  # Frees the disk for the next source.
  "${compose[@]}" rm --stop --force "$primary" "$replica" >/dev/null 2>&1
  docker volume rm "svalbard-tests_$primary-data" "svalbard-tests_${4}-data" >/dev/null
}

# The build uses all CPUs.
run cargo test --release --locked --quiet --test workload --no-run
"${compose[@]}" stop mysql mysql-replica mysql-nops >/dev/null
# Not in a pipeline, so the trap sees the sampler of a failed step.
measure postgresql postgresql postgresql-standby standby >"$samples/postgresql.md"
cat "$samples/postgresql.md"
"${compose[@]}" up --wait --no-deps --no-recreate mysql mysql-replica >/dev/null
measure mysql mysql mysql-replica replica >"$samples/mysql.md"
cat "$samples/mysql.md"

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  cat "$samples/postgresql.md" "$samples/mysql.md" >>"$GITHUB_STEP_SUMMARY"
fi
