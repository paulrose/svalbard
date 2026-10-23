# Svalbard

Svalbard is an open-source Rust CLI for database backups.
This version validates the configuration for a PostgreSQL or MySQL backup to Amazon S3, Cloudflare R2 or another S3-compatible service.

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
| `destination.type` | Required | `s3` or `r2` |
| `destination.bucket` | Required | Bucket name |
| `destination.prefix` | Empty | Prefix for object keys |
| `destination.region` | From the AWS configuration, `auto` for R2 | AWS Region |
| `destination.endpoint` | AWS, required for R2 | URL of an S3-compatible service, such as `https://s3.example.com` |
| `destination.upload_timeout` | `1h` | `1min` to `24h` |
| `safety.lock_timeout` | `10s` | `1s` to `5min` |
| `safety.contention_timeout` | `2s` | `1s` to `60s` |
| `safety.dump_timeout` | `1h` | `1min` to `24h` |
| `safety.max_dump_size` | `20GiB` | `1MiB` to `16TiB` |
| `safety.min_free_space` | `2GiB` | `0B` to `1TiB` |

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

### Output and exit codes

Without `--json`, a valid configuration prints `Configuration is valid.` on stdout.
With `--json`, stdout gets `{"valid":true}` or `{"valid":false,"error":"<message>"}`.
Errors always go to stderr.

| Exit code | Meaning |
| --- | --- |
| `0` | The configuration is valid, or the CLI printed help or version output. |
| `2` | Usage error, such as an unknown flag. The output is not JSON. |
| `3` | The configuration is invalid. |

## Development

Run these checks before you submit a change:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

The [CI workflow](.github/workflows/ci.yml) runs the same checks on Ubuntu for each pull request and each push to `main`.
It uses the toolchain in `rust-toolchain.toml`. The `check` and `integration` jobs need no credentials or paid services.

The PostgreSQL, MySQL and S3 tests run in Docker, which is their only requirement.
[`scripts/test-integration.sh`](scripts/test-integration.sh) starts both database servers with TLS and the [Floci](https://github.com/floci-io/floci) S3 emulator on a private network, and a runner container that installs Rust and the native clients.
The arguments are the PostgreSQL version and the MySQL series. The defaults are 18 and 8.4.
The script writes nothing to the host. At the end, it removes the containers, the volumes and the images that it pulled.

```sh
scripts/test-integration.sh
scripts/test-integration.sh 15 9.7
```

CI runs these tests against PostgreSQL 15, 16, 17 and 18, MySQL 8.4 and 9.7, and Floci.

The R2 tests need a real R2 bucket. [`scripts/test-r2.sh`](scripts/test-r2.sh) runs them in Docker.
Set `SVALBARD_TEST_R2_ENDPOINT` and `SVALBARD_TEST_R2_BUCKET`, and the keys of an R2 API token with Object Read & Write on the bucket in `SVALBARD_TEST_R2_ACCESS_KEY_ID` and `SVALBARD_TEST_R2_SECRET_ACCESS_KEY`.
The tests write under `svalbard-test/` and remove what they wrote.
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
