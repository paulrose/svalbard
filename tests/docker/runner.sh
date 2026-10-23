#!/usr/bin/env bash
# Turns the MySQL image into the test runner, then runs the given command. It installs Rust and
# the PostgreSQL clients of POSTGRESQL_VERSION inside the container, so nothing is built or
# cached outside it. The MySQL image is the base because Oracle publishes the MySQL clients for
# arm64 only in it.
set -euo pipefail

rpm --install --quiet \
  "https://download.postgresql.org/pub/repos/yum/reporpms/EL-9-$(uname -m)/pgdg-redhat-repo-latest.noarch.rpm"
microdnf install --assumeyes gcc make tar gzip "postgresql${POSTGRESQL_VERSION}" >/dev/null
curl --proto '=https' --tlsv1.2 --silent --show-error --fail https://sh.rustup.rs |
  sh -s -- -y --quiet --no-modify-path --profile minimal --default-toolchain 1.99.1 \
    --component clippy --component rustfmt >/dev/null

export PATH="/usr/pgsql-${POSTGRESQL_VERSION}/bin:$HOME/.cargo/bin:$PATH"
exec "$@"
