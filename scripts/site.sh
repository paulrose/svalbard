#!/usr/bin/env bash
# Runs the website in Docker: the splash page and the documentation in docs/, built with Astro and
# Starlight. Docker is the only requirement. The sources are mounted read-only, and the
# dependencies, caches and builds stay in the container. At the end, the script removes the
# container, and the image if it pulled it.
#
# Usage:
#   scripts/site.sh dev          serves the site on http://localhost:7246 and reloads on changes.
#                                hatch.yml serves the same at https://svalbard.test.
#   scripts/site.sh build [dir]  builds the site, checks its links and runs the site tests. With
#                                dir, it copies the built site there.
#   scripts/site.sh lock         writes site/package-lock.json for site/package.json.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
image=node:24-bookworm-slim
mode=${1:-}
name="svalbard-site-$$"
# The same layout as the repository, so that relative paths such as ../docs work. Only the
# container's own /work/site is writable: node_modules, .astro, the caches and dist.
# FONTAWESOME_API_TOKEN, if set, passes by name only: site/brand.mjs fetches the logo with it.
args=(--rm --name "$name" --workdir /work/site --env NPM_CONFIG_UPDATE_NOTIFIER=false
  --env ASTRO_TELEMETRY_DISABLED=1 --env FONTAWESOME_API_TOKEN)
for path in docs site/src site/tests site/astro.config.mjs site/brand.mjs site/package.json \
  site/package-lock.json; do
  args+=(--volume "$root/$path:/work/$path:ro")
done
install='npm ci --no-audit --no-fund --loglevel=error && node brand.mjs'
case "$mode" in
  dev)
    args+=(--publish 127.0.0.1:7246:7246 --env SVALBARD_SITE_POLL=1)
    script="$install && exec npx astro dev --host 0.0.0.0 --port 7246"
    ;;
  build)
    if [ -n "${2:-}" ]; then
      mkdir -p "$2"
      args+=(--volume "$(cd "$2" && pwd):/out")
    fi
    script="$install && npx astro build && node --test tests/*.test.mjs &&
      if [ -d /out ]; then cp -R dist/. /out/; fi"
    ;;
  lock)
    # The only mode that writes to the repository: site/package-lock.json.
    args=(--rm --name "$name" --workdir /work --env NPM_CONFIG_UPDATE_NOTIFIER=false
      --volume "$root/site:/out")
    script='cp /out/package.json . && npm install --package-lock-only --no-audit --no-fund \
      --loglevel=error && cp package-lock.json /out/'
    ;;
  *)
    echo "Usage: scripts/site.sh dev|build [dir]|lock" >&2
    exit 2
    ;;
esac

# The image stays if it was here before the run.
pulled=false
docker image inspect "$image" >/dev/null 2>&1 || pulled=true
cleanup() {
  docker rm --force "$name" >/dev/null 2>&1 || true
  if $pulled; then docker image rm "$image" >/dev/null 2>&1 || true; fi
}
trap cleanup EXIT

# In the background, so that SIGINT and SIGTERM (from Hatch, for example) reach the trap at once.
docker run "${args[@]}" "$image" bash -ec "$script" &
wait $!
