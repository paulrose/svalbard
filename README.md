# Svalbard

Svalbard is an open-source Rust CLI for database backups.
It backs up PostgreSQL or MySQL to Amazon S3, Cloudflare R2, another S3-compatible service, or an SSH server with SFTP or SCP.

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
| `destination.prefix` | Empty | Prefix for object keys or file names. For SSH: letters, digits, `.`, `_`, `-` and `/` |
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
GitHub Actions sends SIGINT, then SIGTERM, and then SIGKILL about 10 s after it cancels a job.

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

Database adapters must obey the [database safety contract](docs/database-safety.md).

Create a branch for each change. Submit changes through a pull request.
Keep PRs focused and include relevant check results. Review the diff before a solo merge.
GitHub deletes merged head branches automatically.

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
