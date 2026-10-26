---
title: "Install"
description: "Download the release archive, install the native clients, or build from source."
---

Each release has an archive for Linux x86_64 and a `SHA256SUMS` file.
The archive `svalbard-<version>-x86_64-unknown-linux-gnu.tar.gz` holds the `svalbard` binary, `LICENSE` and the README.
The binary runs on Linux with glibc 2.39 or newer, such as Ubuntu 24.04.

```sh
version=0.2.0
archive="svalbard-$version-x86_64-unknown-linux-gnu.tar.gz"
for file in "$archive" SHA256SUMS; do
  curl --fail --silent --show-error --location --remote-name \
    "https://github.com/paulrose/svalbard/releases/download/v$version/$file"
done
sha256sum --check SHA256SUMS
tar --extract --gzip --file "$archive"
"./${archive%.tar.gz}/svalbard" --version
```

`SHA256SUMS` from the same release finds a damaged download, but not a changed release.
For CI, keep the SHA-256 of the archive in the workflow, as in the [CI example](ci.md).
The [releases](https://github.com/paulrose/svalbard/releases) list the versions.

## Native clients

Svalbard runs the native clients from `PATH`, and checks their versions before the dump.

| For | Client |
| --- | --- |
| PostgreSQL 15 to 18 | `pg_dump` 15 to 18, of the server's major version or newer. A restore needs `pg_restore` of the same version or newer. |
| MySQL 8.4 or 9.7 | `mysqldump` from Oracle MySQL, of the server's series: 8.4 for MySQL 8.4, 9.7 for MySQL 9.7. Svalbard rejects the MariaDB client. A restore needs `mysql`. |
| SFTP or SCP | OpenSSH `ssh`, `sftp` and `scp`. `scp` must accept `-O`: OpenSSH 9.0 or newer, or a distribution that backports it. |

On Ubuntu 24.04, install the database clients from the PostgreSQL and MySQL APT repositories.
GitHub's `ubuntu-24.04` runner has `pg_dump` 16 and `mysqldump` 8.0, and Svalbard rejects `mysqldump` 8.0.

```sh
# pg_dump 18. The runner has postgresql-common. Elsewhere, install it first.
sudo /usr/share/postgresql-common/pgdg/apt.postgresql.org.sh -y
sudo apt-get install --yes --no-install-recommends postgresql-client-18
# /usr/bin/pg_dump can select another installed version.
export PATH="/usr/lib/postgresql/18/bin:$PATH"
```

```sh
# mysqldump 8.4. For 9.7, use mysql-9.7-lts.
curl --fail --silent --show-error --location --output mysql.asc \
  https://repo.mysql.com/RPM-GPG-KEY-mysql-2025
gpg --show-keys --with-colons mysql.asc | awk -F: '$1 == "pub" { getline; print $10 }'
# Continue only if the fingerprint is BCA43417C3B485DD128EC6D4B7B3B788A8D3785C.
gpg --dearmor <mysql.asc | sudo tee /etc/apt/keyrings/mysql.gpg >/dev/null
echo "deb [signed-by=/etc/apt/keyrings/mysql.gpg] https://repo.mysql.com/apt/ubuntu noble mysql-8.4-lts" |
  sudo tee /etc/apt/sources.list.d/mysql.list
# On the runner. Otherwise apt replaces its MySQL 8.0 server with Oracle's server too.
sudo apt-get remove --yes mysql-server mysql-server-8.0 mysql-server-core-8.0
sudo apt-get update
sudo apt-get install --yes --no-install-recommends mysql-community-client
```

The fingerprint is the key of MySQL Release Engineering.
If Oracle publishes a new key, compare its fingerprint with the one in the MySQL documentation before you change it here.

## Build from source

Install Rust with [rustup](https://rustup.rs/).
The repository pins Rust 1.99.1, which is also the minimum supported Rust version.
Rustup selects this toolchain from `rust-toolchain.toml`.

```sh
cargo build --locked --release
./target/release/svalbard --help
./target/release/svalbard --version
```

The CLI does not prompt for input.
