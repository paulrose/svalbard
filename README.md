# Svalbard

Svalbard is an open-source Rust CLI for database backups.
It backs up PostgreSQL or MySQL to Amazon S3, Cloudflare R2, another S3-compatible service, or an SSH server with SFTP or SCP.

## Install

Each release has an archive for Linux x86_64 and a `SHA256SUMS` file.
The archive `svalbard-<version>-x86_64-unknown-linux-gnu.tar.gz` holds the `svalbard` binary, `LICENSE` and this README.
The binary runs on Linux with glibc 2.39 or newer, such as Ubuntu 24.04.

```sh
version=0.1.0
gh release download "v$version" --repo paulrose/svalbard \
  --pattern "svalbard-$version-x86_64-unknown-linux-gnu.tar.gz" --pattern SHA256SUMS
sha256sum --check SHA256SUMS
tar --extract --gzip --file "svalbard-$version-x86_64-unknown-linux-gnu.tar.gz"
./svalbard-$version-x86_64-unknown-linux-gnu/svalbard --version
```

`SHA256SUMS` from the same release finds a damaged download, but not a changed release.
For CI, keep the SHA-256 of the archive in the workflow, as in the [CI example](#run-in-ci).
The repository is private, so the download needs a token: see [Private access](#private-access).

### Native clients

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

## Build and run

Install Rust with [rustup](https://rustup.rs/).
The repository pins Rust 1.99.1, which is also the minimum supported Rust version.
Rustup selects this toolchain from `rust-toolchain.toml`.

```sh
cargo build --locked --release
./target/release/svalbard --help
./target/release/svalbard --version
```

The CLI does not prompt for input.

## Configuration

`svalbard config check` validates the configuration.
It makes no network connections, so it cannot check connectivity, credentials or permissions.

```sh
./target/release/svalbard config check --config examples/svalbard.toml
./target/release/svalbard config check --config examples/svalbard.toml --json
```

[`examples/svalbard.toml`](examples/svalbard.toml) is a complete example.
`SVALBARD_CONFIG` can also give the path of the configuration file.

Each setting has a TOML key, a flag and an environment variable.
For example, `source.host` is also `--source-host` and `SVALBARD_SOURCE_HOST`.
A flag takes precedence over its environment variable, the variable over the configuration file, and the file over the default.
Unknown keys, empty values and invalid values are errors.

| Key | Default | Values |
| --- | --- | --- |
| `source.type` | Required | `postgresql` or `mysql` |
| `source.host` | Required | Host name |
| `source.port` | `5432` for PostgreSQL, `3306` for MySQL | `1` to `65535` |
| `source.database` | Required | Database name |
| `source.user` | Required | User name |
| `source.password_file` | None | Path to a file that holds the password |
| `source.tls` | `verify-full` | `verify-full` or `disable` |
| `source.tls_ca_file` | The system trust store | Path to a PEM file with the CA certificates |
| `destination.type` | Required | `s3`, `r2`, `sftp` or `scp` |
| `destination.prefix` | Empty | Prefix for object keys or file names. For SSH: letters, digits, `.`, `_`, `-` and `/`, and its directories must exist |
| `destination.upload_timeout` | `1h` | `1min` to `24h` |
| `destination.bucket` | Required for S3 and R2 | Bucket name |
| `destination.region` | From the AWS configuration, `auto` for R2 | AWS Region |
| `destination.endpoint` | AWS, required for R2 | URL of an S3-compatible service, such as `https://s3.example.com` |
| `destination.host` | Required for SSH | Host name or IP address |
| `destination.port` | `22` | `1` to `65535` |
| `destination.user` | Required for SSH | User name |
| `destination.path` | Required for SSH | Existing absolute directory on the server, without `\`, `"` or line breaks |
| `destination.identity_file` | The SSH agent and the default keys | Path to an unencrypted private key |
| `destination.known_hosts_file` | `~/.ssh/known_hosts` and `/etc/ssh/ssh_known_hosts` | Path to a known_hosts file |
| `safety.lock_timeout` | `10s` | `1s` to `5min` |
| `safety.contention_timeout` | `2s` | `1s` to `60s` |
| `safety.dump_timeout` | `1h` | `1min` to `24h` |
| `safety.max_dump_size` | `20GiB` | `1MiB` to `16TiB` |
| `safety.min_free_space` | `2GiB` | `0B` to `1TiB` |
| `safety.staging_dir` | The system temp directory (`TMPDIR`) | An existing directory for the staged dumps, not on the filesystem of the database files |

With `verify-full`, the database connections require TLS, and Svalbard checks the server certificate and host name.
Without `source.tls_ca_file`, Svalbard uses the system trust store. Then `pg_dump` needs libpq 16 or newer, and MySQL uses the CA bundle file of the system OpenSSL.

A duration is a whole number with `s`, `min` or `h`. A size is a whole number with `B`, `KiB`, `MiB`, `GiB` or `TiB`.
The [database safety contract](docs/database-safety.md) explains the safety limits.

### Secrets

The configuration file and flags never hold secrets. The file rejects keys such as `password`.

- Set the database password in `SVALBARD_SOURCE_PASSWORD`, or set `source.password_file` to a mounted secret file. Set one of them, not both.
- If the database needs no password, set neither. An empty `SVALBARD_SOURCE_PASSWORD` is an error, because CI systems give an empty value for a missing secret.
- For S3, Svalbard uses the AWS credential chain: environment variables such as `AWS_ACCESS_KEY_ID`, a shared AWS profile, a web identity token from CI, or an instance or container role. Static keys are not necessary.
- For R2, Svalbard reads only `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`. It never sends AWS credentials to Cloudflare.
- For SSH, Svalbard uses the SSH agent in `SSH_AUTH_SOCK`, or `destination.identity_file`. A key file must be unencrypted, because Svalbard never prompts for a passphrase.

`config check` does not read the password. It checks that the password file opens.
Secret values do not appear in errors or in the JSON result.

```yaml
# GitHub Actions
- name: Check the Svalbard configuration
  run: svalbard config check --config svalbard.toml --json
  env:
    SVALBARD_SOURCE_PASSWORD: ${{ secrets.DB_PASSWORD }}
```

### Storage

Each backup is two objects with the same name stem, under `destination.prefix`:

```text
app/20261022T134500Z-<run id>.dump   the dump (.dump for PostgreSQL, .sql for MySQL)
app/20261022T134500Z-<run id>.json   the completion record
```

The time is the UTC start of the run. Svalbard uploads the record only after the dump is complete.
A dump without a record is an incomplete backup. Svalbard never deletes it, because a backup bucket can deny deletes.

The record holds `version` (1), `created`, `run_id`, `data` (the key of the dump), `format`, `client_version`, `server_version`, `size` and `sha256` (hexadecimal).
The dump object also carries its SHA-256 as the metadata `x-amz-meta-sha256`.

S3 checks every upload against a SHA-256: a small dump in one request, a larger dump in parts of 64 MiB or more.
Svalbard also checks that the parts form the staged file. Each request has up to 3 attempts. `destination.upload_timeout` limits the whole upload.
After an error or cancellation, Svalbard aborts the unfinished multipart upload.

The S3 credentials need these permissions on the prefix:

- `s3:PutObject` for the dump and the record.
- `s3:AbortMultipartUpload` for cleanup after an error.
- `s3:ListBucketMultipartUploads` on the bucket, recommended. It finds an upload that was cancelled before S3 returned its ID.

Add a lifecycle rule with `AbortIncompleteMultipartUpload` to the bucket. It removes the parts of an upload that Svalbard could not abort, for example after `SIGKILL`.

#### Cloudflare R2

```toml
[destination]
type = "r2"
bucket = "example-backups"
prefix = "app/"
endpoint = "https://<account-id>.r2.cloudflarestorage.com"
```

Use the S3 API endpoint of the account, without the bucket. For a bucket in a jurisdiction, the endpoint includes it, such as `https://<account-id>.eu.r2.cloudflarestorage.com`.
The region is `auto`. [`examples/r2.toml`](examples/r2.toml) is a complete example.

Create an R2 API token with Object Read & Write on the bucket.
Set its Access Key ID in `AWS_ACCESS_KEY_ID` and its Secret Access Key in `AWS_SECRET_ACCESS_KEY`.

R2 checks a small dump against its SHA-256, as S3 does. For the parts of a larger dump, R2 checks MD5 instead of SHA-256.
Svalbard then checks the MD5 ETag of the complete object, and that the parts form the staged file.
The record and `x-amz-meta-sha256` hold the SHA-256 for every dump.

R2 aborts incomplete multipart uploads after 7 days by default.

#### SSH: SFTP and SCP

```toml
[destination]
type = "sftp"          # or "scp"
host = "backup.example.com"
user = "svalbard"
path = "/srv/backups"
known_hosts_file = "/etc/svalbard/known_hosts"
```

[`examples/sftp.toml`](examples/sftp.toml) and [`examples/scp.toml`](examples/scp.toml) are complete examples.
Svalbard runs the OpenSSH clients `sftp`, `scp` and `ssh`. `scp` must accept `-O`: OpenSSH 9.0 or newer, or a distribution that backports it.
Svalbard ignores `ssh_config`, never prompts, and checks the host key strictly. The key must already be in the known_hosts file, and Svalbard never adds it.

Each file goes to a hidden temporary name, such as `.<name>.<random>.partial`, in the same directory.
Svalbard renames it only after the server has the complete file, so the dump and the record appear complete or not at all.
After an error or cancellation, Svalbard stops the client and removes the temporary file. If the removal fails, the result is `cleanup_failed`.
The files keep the permissions of the staged files, `0600`.

SSH protects every byte in transit, and the server confirms the writes. Svalbard also checks the size of the remote file before the rename.
The record holds the SHA-256 of the dump.

- `sftp` uses the SFTP subsystem. The server must offer `fsync@openssh.com`, so that the file is on disk before the rename. With `posix-rename@openssh.com`, a retry of the same run replaces the files. OpenSSH's `sftp-server` offers both.
- `scp` uses the legacy SCP protocol for servers without SFTP. The account needs a POSIX login shell that prints nothing, and `scp`, `mv`, `rm` and `wc` on the server. SCP confirms each file, not each write, and it cannot sync the file to disk.

A host key that is missing or does not match gives `host_key_rejected`. A rejected key or a denied write gives `access_denied`.

## Back up

```sh
export SVALBARD_SOURCE_PASSWORD='…'
./target/release/svalbard backup --config svalbard.toml
./target/release/svalbard backup --config svalbard.toml --json
```

`svalbard backup` uses the same configuration, flags and environment variables as `config check`. It runs these stages in order:

1. `configuration`: Svalbard validates the configuration.
2. `destination`: Svalbard checks what it can without writing. For S3 and R2, these are the region and the credentials. For SSH, these are the connection, the host key, the authentication and the directory. The check has 30 s.
3. `staging`: Svalbard creates a private run directory in `safety.staging_dir`.
4. `dump`: Svalbard checks the source and dumps it under the [database safety contract](docs/database-safety.md). Then it closes the snapshot and the database connections.
5. `transfer`: Svalbard uploads the staged dump, and then its completion record. After an `upload` failure, Svalbard tries once more after 10 s, with the same staged dump and the same names, within `destination.upload_timeout`.
6. `cleanup`: Svalbard deletes the run directory, after a success and after a failure.

A backup is complete when the dump and its record are both stored. Only then does Svalbard report success.
Progress lines go to stderr.

SIGINT and SIGTERM cancel the backup. Svalbard ends the backup sessions, stops the clients, removes unfinished uploads and temporary files, and deletes the run directory.
This cleanup can take up to about a minute. If a CI system sends SIGKILL sooner, no cleanup runs.
Then `PR_SET_PDEATHSIG` stops the clients, the server-side backstops end the database sessions, and the lifecycle rule (S3) or the `.partial` names (SSH) mark what remains.
The run directory `svalbard-<run id>` also remains in `safety.staging_dir`, with the partial dump and, if the dump was running, the file with the database password. Delete it.
When GitHub Actions cancels a job, it sends SIGINT and then SIGTERM to the entry process of the running step only, and kills the step's processes after 10 s.
Start Svalbard with `exec` there, as in the [CI example](#run-in-ci).

## Output and exit codes

`config check` prints `Configuration is valid.` on stdout.
With `--json`, stdout gets `{"valid":true}` or `{"valid":false,"error":"<message>"}`.

`backup` prints `Backup complete: <data> and <record>.` on stdout. With `--json`, stdout gets one object:

```json
{"result":"complete","run_id":"…","data":"app/….dump","record":"app/….json","size":1048576,"sha256":"…","format":"pg_dump-custom","client_version":"pg_dump (PostgreSQL) 18.0","server_version":"18.0"}
{"result":"failed","stage":"transfer","error":"access_denied","message":"…","run_id":"…"}
```

`stage` is one of the stages above. `error` is a failure kind, such as `lock_timeout`, `access_denied` or `cancelled`.
Errors always go to stderr. No output contains a secret.

| Exit code | Meaning |
| --- | --- |
| `0` | The configuration is valid, the backup is complete, or the CLI printed help or version output. |
| `2` | Usage error, such as an unknown flag. The output is not JSON. |
| `3` | The configuration is invalid. |
| `4` | The backup failed, and Svalbard cleaned up. The backup can run again. |
| `5` | The backup failed, and the cleanup failed too (`cleanup_failed`). A backup session, the run directory or an unfinished upload may remain. The message says which. |

## Run in CI

A CI job installs the archive and the native clients, gives the secrets in environment variables, and runs `svalbard backup --json`.
The exit code fails the job when the backup fails.
[`examples/github-actions/backup-cli.yml`](examples/github-actions/backup-cli.yml) is a complete GitHub Actions workflow for PostgreSQL to SFTP.
The [GitHub Action](#github-action) does the installation and the backup in one step.
The runner must reach the database and the destination.

### Secrets

- Database password: `SVALBARD_SOURCE_PASSWORD: ${{ secrets.DB_PASSWORD }}` in the `env` of the backup step.
- R2: `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` from secrets, in the same way.
- S3: no stored keys. Give the workflow `id-token: write`, and let it assume an IAM role through OIDC before the backup:

  ```yaml
  - uses: aws-actions/configure-aws-credentials@e1253824e5c10ff9df46874f81ed3ec929e19cfd # v6.3.0
    with:
      role-to-assume: arn:aws:iam::123456789012:role/svalbard-backup
      aws-region: eu-west-1
  ```

  The role needs the permissions in [Storage](#storage).
- SSH: write the private key from a secret to a file that only the job's user can read, and set `SVALBARD_DESTINATION_IDENTITY_FILE` to it.
  The runner deletes `$RUNNER_TEMP` after the job.
- known_hosts: commit the server's line next to the configuration. It is public, and a review shows every change.
  Take the line from the server, for example from `/etc/ssh/ssh_host_ed25519_key.pub`, not from an unchecked `ssh-keyscan`.
  For a port other than 22, the host is `[backup.example.com]:2222`.

GitHub masks secrets in the log, and no Svalbard output contains them.

### One backup per database

```yaml
concurrency:
  group: svalbard-db.example.com-app
  cancel-in-progress: false
```

GitHub runs one job of a concurrency group at a time, in one repository.
A new run waits for the running backup and never cancels it. A newer run replaces a run that waits, so at most one backup waits.
Use the same group in every workflow of the repository that backs up the database.

### Cancellation and timeouts

When GitHub cancels a job, or a job or step reaches `timeout-minutes`, it sends SIGINT and then SIGTERM to the entry process of the running step only.
After 10 s, it kills the processes of the step.
Start Svalbard with `exec`, so that the entry process is Svalbard and it can clean up. Without `exec`, the shell gets the signals, and Svalbard stops without cleanup.

Give the job a `timeout-minutes` above `safety.dump_timeout` and `destination.upload_timeout` together, so that the limits of Svalbard end a slow backup first.

### Results and failures

With `--json`, stdout gets the result, and the log shows it. Progress and errors go to stderr.
To use the result in later steps, write stdout to a file:

```yaml
- name: Back up
  id: backup
  run: exec svalbard backup --config svalbard.toml --json >"$RUNNER_TEMP/backup.json"
- name: Show the result
  if: always() && steps.backup.outcome != 'skipped'
  run: jq . "$RUNNER_TEMP/backup.json"
```

A failed backup exits with 4 or 5 (see [Output and exit codes](#output-and-exit-codes)), so the step and the job fail.
After 4, the next run can start normally. After 5, read the message: a backup session, the run directory or an unfinished upload may remain.

### GitHub Action

The Action in [`action/`](action/action.yml) installs a released Svalbard and checks the archive against its SHA-256 in [`action/checksums.txt`](action/checksums.txt).
It can install the database clients, and then it runs one backup with `exec`.
[`examples/github-actions/backup-action.yml`](examples/github-actions/backup-action.yml) is a complete workflow.

```yaml
- uses: paulrose/svalbard/action@<commit> # v0.1.0
  with:
    version: 0.1.0
    config: svalbard.toml
    postgresql: "18"
    token: ${{ secrets.SVALBARD_TOKEN }}
  env:
    SVALBARD_SOURCE_PASSWORD: ${{ secrets.DB_PASSWORD }}
```

Pin the Action to a commit. Each commit of the Action installs only the Svalbard versions in its `checksums.txt`.

| Input | Meaning |
| --- | --- |
| `version` | Required. The Svalbard version, such as `0.1.0`. |
| `config` | Required. The configuration file. |
| `postgresql` | Installs `pg_dump` of this major version: `15` to `18`. |
| `mysql` | Installs `mysqldump` of this series: `8.4` or `9.7`. |
| `token` | A token that can read the Svalbard releases. The default is `github.token`. See [Private access](#private-access). |

`postgresql` and `mysql` change the system packages as in [Native clients](#native-clients), so they work only on GitHub's `ubuntu-24.04` runner.
On another runner, install the clients before the Action.
The other settings and the secrets go in the `env` of the step, as for the CLI. The Action has no inputs for secrets.

The outputs come from the JSON result: `result`, `run-id`, `data`, `record`, `size`, `sha256`, `stage` and `error`, and `json` with the complete result.
A failed backup fails the step with the exit code of Svalbard, and the outputs describe the failure.
The Action also writes the result to `$RUNNER_TEMP/svalbard-result.json`.
When the step reaches its `timeout-minutes`, GitHub stops the Action before it sets the outputs, so only the file has the result. A later step with `if: always()` can read it.

### Private access

The repository is private, so the release download needs a token with read access to it.
Create a fine-grained personal access token for the resource owner `paulrose`, with access to the repository `paulrose/svalbard` only and the permission Contents: Read-only.
Store it as a secret such as `SVALBARD_TOKEN`. Give it only to the download step as `GH_TOKEN`, or to the Action as `token`.

Keep this token apart from the database and storage credentials. It reads the Svalbard source and releases, not the backups.
A workflow in `paulrose/svalbard` itself can use `github.token` instead.

Other private repositories of `paulrose` can use the Action when Settings → Actions → General → Access of `paulrose/svalbard` is "Accessible from repositories owned by 'paulrose' user".
This setting shares the Action, not the releases, so these workflows need the token too.

When the repository is public, the release download needs no personal token, and workflows reference the Action as `paulrose/svalbard/action@<commit>`.
If the Action moves to its own repository for the GitHub Marketplace, the reference names that repository instead.

## Restore

A backup is the dump and its completion record (see [Storage](#storage)).

1. Download both, for example with `aws s3 cp s3://<bucket>/<key> .` or with `sftp`.
2. Check the dump against the record:

   ```sh
   echo "$(jq --raw-output .sha256 20261022T134500Z-<run id>.json)  20261022T134500Z-<run id>.dump" |
     sha256sum --check
   ```

3. Restore it into a new database with the native client.

PostgreSQL:

```sh
createdb --template=template0 app_restored
pg_restore --exit-on-error --dbname=app_restored 20261022T134500Z-<run id>.dump
```

The dump has no roles, tablespaces or other global objects. The roles that own objects or have grants in the dump must exist before the restore.

MySQL:

```sh
mysql --defaults-extra-file=restore.cnf --execute='CREATE DATABASE app_restored'
mysql --defaults-extra-file=restore.cnf app_restored <20261022T134500Z-<run id>.sql
```

`restore.cnf` is an option file with `[client]`, `host`, `user` and `password`, readable only by you, so that no argument holds the password.
The dump names no database, so it restores into the database that you give.
It has no accounts or grants, and the accounts in its `DEFINER` clauses must exist.
A dump from a replica restores its events disabled: see [MySQL replica](docs/database-safety.md#mysql-replica).

## Development

Run these checks before you submit a change:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

The [CI workflow](.github/workflows/ci.yml) runs the same checks on Ubuntu for each pull request and each push to `main`.
It uses the toolchain in `rust-toolchain.toml`. The `check` and `integration` jobs need no credentials or paid services.

The PostgreSQL, MySQL, S3 and SSH tests run in Docker, which is their only requirement.
[`scripts/test-integration.sh`](scripts/test-integration.sh) starts both database servers with TLS, a PostgreSQL hot standby, a MySQL replica, a MySQL server without `performance_schema`, the [Floci](https://github.com/floci-io/floci) S3 emulator and an OpenSSH server on a private network, and a runner container that installs Rust and the native clients.
The arguments are the PostgreSQL version and the MySQL series. The defaults are 18 and 8.4.
The script writes nothing to the host. At the end, it removes the containers, the volumes and the images that it pulled.

```sh
scripts/test-integration.sh
scripts/test-integration.sh 15 9.7
```

CI runs these tests against PostgreSQL 15, 16, 17 and 18, MySQL 8.4 and 9.7, Floci, and OpenSSH on Alpine Linux.

`tests/matrix.rs` runs `svalbard backup` for each source with each container destination: S3 (Floci), SFTP and SCP.
Each test downloads the backup without Svalbard, restores it with `pg_restore` or `mysql`, and compares the schema and the data, including an empty table and a larger one.
`tests/backup.rs` runs the CLI into failures: a wrong password, denied writes, a missing record, signals, a full staging filesystem, rejected credentials, and broken transfers.
Every failure must exit with an error, leave no completion record and no staged files, and keep secrets out of the output.
The matrix tests print one `matrix:` line each, with the client and server versions, in the `successes` output of the test run.

`tests/scenarios.rs` runs the [safety scenarios](docs/database-safety.md#scenarios) that need real servers: contention on the primary, the standby and the replica, `dump_timeout`, signals during preflight and the dump, `SIGKILL`, detection without `performance_schema`, two runs on one database, and a lost control connection.
Each test prints a `scenario:` line with the results and the timings against budgets B5 and B6.

[`scripts/measure.sh`](scripts/measure.sh) measures the workload budgets of the contract for one PostgreSQL version and one MySQL series.
It loads the primary with pgbench or sysbench at half of the throughput that the servers sustain, runs backups back to back on the primary and on the standby or replica, and restores the last dump of each.
It prints Markdown tables: latency, throughput, errors, server CPU and I/O, the memory of Svalbard and the duration of the detection query.
On a host with 8 or more CPUs, the servers, the load and the backup get their own CPUs.
A pair takes about an hour and needs about 15 GB of Docker disk. Like the tests, the script leaves nothing behind.

```sh
scripts/measure.sh 18 8.4
```

The [Measure workflow](.github/workflows/measure.yml) runs the script for each version pair on demand. Its runners have 2 CPUs, which the servers, the load and the backup share.

The R2 tests need a real R2 bucket. [`scripts/test-r2.sh`](scripts/test-r2.sh) runs them in Docker.
Set `SVALBARD_TEST_R2_ENDPOINT` and `SVALBARD_TEST_R2_BUCKET`, and the keys of an R2 API token with Object Read & Write on the bucket in `SVALBARD_TEST_R2_ACCESS_KEY_ID` and `SVALBARD_TEST_R2_SECRET_ACCESS_KEY`.
The tests write under `svalbard-test/` and remove what they wrote.
The script also starts PostgreSQL and MySQL, because the R2 tests include backups through the CLI with native restores.
Give the bucket a lifecycle rule that deletes objects under `svalbard-test/` after a day, for tests that fail before their cleanup.

```sh
scripts/test-r2.sh
```

The `r2` CI job runs these tests in the `svalbard-ci` bucket for pull requests from this repository, for pushes to `main`, and on demand.
It reads the keys from the repository secrets `R2_ACCESS_KEY_ID` and `R2_SECRET_ACCESS_KEY`, and the endpoint and bucket from the variables `R2_ENDPOINT` and `R2_BUCKET`.

The `package` CI job builds the release archive with [`scripts/package.sh`](scripts/package.sh).
The `clean-runner` job installs it on a fresh runner that builds nothing, with the native clients from [`action/clients.sh`](action/clients.sh), which runs the commands in [Native clients](#native-clients). Then it runs the steps of the [CI example](#run-in-ci).
Its servers are those of the Docker tests, which [`tests/docker/ports.yaml`](tests/docker/ports.yaml) publishes on 127.0.0.1.
It backs up PostgreSQL to SFTP and MySQL to S3, restores both as in [Restore](#restore), and checks a wrong password, a cancellation by a step timeout, and that no output contains a password.
The `action` job runs the Action of the commit with a released Svalbard on the same servers. It checks a backup, a wrong password and a cancellation through the outputs of the Action.

Database adapters must obey the [database safety contract](docs/database-safety.md).

Create a branch for each change. Submit changes through a pull request.
Keep PRs focused and include relevant check results. Review the diff before a solo merge.
GitHub deletes merged head branches automatically.

### Releases

1. Change `version` in `Cargo.toml` in a pull request.
2. After the merge, when CI is green on `main`, tag the merge commit and push the tag:

   ```sh
   git tag v0.1.0 <commit>
   git push origin v0.1.0
   ```

3. The [Release workflow](.github/workflows/release.yml) builds the archive, checks that the tag matches the version, and creates the GitHub release with the archive and `SHA256SUMS`.
4. In a pull request, add the line of the archive from `SHA256SUMS` to [`action/checksums.txt`](action/checksums.txt), and change the version in the `action` CI job and the examples.

## Website preview

The splash page and future documentation live in `site/`. They use plain HTML, CSS, and JavaScript, with no build step or runtime dependencies.

```sh
python3 -m http.server 4173 --bind 127.0.0.1 --directory site
```

Open <http://127.0.0.1:4173>. The backup animation is a simulation of the planned workflow.
The colours follow the estimated solar elevation in Longyearbyen. The estimate excludes weather and terrain.
Reduced-motion settings show one frame of the upload, after the snapshot has closed.
For previews, `?theme=light` or `?theme=dark` overrides daylight, and `?at=<seconds>` holds the animation at one moment.

Run the site checks with Node.js 18 or later:

```sh
node --test site/tests/*.test.mjs
```

## License

[MIT](LICENSE).
