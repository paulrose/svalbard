#!/usr/bin/env bash
# Installs the native database clients for the Svalbard Action on GitHub's ubuntu-24.04 runner:
# pg_dump POSTGRESQL (15 to 18) from the PostgreSQL APT repository, and mysqldump MYSQL (8.4 or
# 9.7) from the MySQL APT repository. The commands are those of "Native clients" in the README.
set -euo pipefail

case "$POSTGRESQL" in
  "" | 15 | 16 | 17 | 18) ;;
  *) echo "::error::postgresql is \`$POSTGRESQL\`. Use 15, 16, 17 or 18." && exit 1 ;;
esac
case "$MYSQL" in
  "" | 8.4 | 9.7) ;;
  *) echo "::error::mysql is \`$MYSQL\`. Use 8.4 or 9.7." && exit 1 ;;
esac
# The commands change the system packages, which is safe only on a runner for one job.
# shellcheck source=/dev/null
. /etc/os-release
if [ "$RUNNER_ENVIRONMENT" != github-hosted ] || [ "$VERSION_ID" != 24.04 ]; then
  echo "::error::The postgresql and mysql inputs need GitHub's ubuntu-24.04 runner. Install the clients yourself."
  exit 1
fi

if [ -n "$POSTGRESQL" ]; then
  sudo /usr/share/postgresql-common/pgdg/apt.postgresql.org.sh -y
  sudo apt-get install --yes --no-install-recommends "postgresql-client-$POSTGRESQL"
  # /usr/bin/pg_dump can select another installed version.
  echo "/usr/lib/postgresql/$POSTGRESQL/bin" >>"$GITHUB_PATH"
fi

if [ -n "$MYSQL" ]; then
  key="$RUNNER_TEMP/mysql.asc"
  curl --fail --silent --show-error --location --output "$key" \
    https://repo.mysql.com/RPM-GPG-KEY-mysql-2025
  # The key of MySQL Release Engineering.
  fingerprint=$(gpg --show-keys --with-colons "$key" | awk -F: '$1 == "pub" { getline; print $10 }')
  if [ "$fingerprint" != BCA43417C3B485DD128EC6D4B7B3B788A8D3785C ]; then
    echo "::error::The MySQL signing key has the fingerprint $fingerprint"
    exit 1
  fi
  gpg --dearmor <"$key" | sudo tee /etc/apt/keyrings/mysql.gpg >/dev/null
  echo "deb [signed-by=/etc/apt/keyrings/mysql.gpg] https://repo.mysql.com/apt/ubuntu noble mysql-$MYSQL-lts" |
    sudo tee /etc/apt/sources.list.d/mysql.list >/dev/null
  # Otherwise apt replaces the runner's MySQL 8.0 server with Oracle's server.
  sudo apt-get remove --yes mysql-server mysql-server-8.0 mysql-server-core-8.0
  sudo apt-get update
  sudo apt-get install --yes --no-install-recommends mysql-community-client
fi
