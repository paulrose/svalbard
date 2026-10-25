---
title: "Configuration"
description: "Settings, their flags and environment variables, precedence, TLS and secrets."
---

`svalbard config check` validates the configuration.
It makes no network connections, so it cannot check connectivity, credentials or permissions.

```sh
svalbard config check --config svalbard.toml
svalbard config check --config svalbard.toml --json
```

[`examples/svalbard.toml`](../examples/svalbard.toml) is a complete example for S3.
[`examples/r2.toml`](../examples/r2.toml), [`examples/sftp.toml`](../examples/sftp.toml) and [`examples/scp.toml`](../examples/scp.toml) cover the other destinations.
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
The [database safety contract](database-safety.md) explains the safety limits.

## Secrets

The configuration file and flags never hold secrets. The file rejects keys such as `password`.

- Set the database password in `SVALBARD_SOURCE_PASSWORD`, or set `source.password_file` to a mounted secret file. Set one of them, not both.
- If the database needs no password, set neither. An empty `SVALBARD_SOURCE_PASSWORD` is an error, because CI systems give an empty value for a missing secret.
- For S3, Svalbard uses the AWS credential chain: access keys in `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`, temporary credentials with `AWS_SESSION_TOKEN`, a shared AWS profile, a web identity token from CI, or an instance or container role.
- For R2, Svalbard reads only `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`. It never sends AWS credentials to Cloudflare.
- For SSH, Svalbard uses the SSH agent in `SSH_AUTH_SOCK`, or `destination.identity_file`. A key file must be unencrypted, because Svalbard never prompts for a passphrase.

`config check` does not read the password. It checks that the password file opens.
Secret values do not appear in errors or in the JSON result.

[Run in CI](ci.md) shows how a CI job gives these secrets. In GitHub Actions:

```yaml
- name: Check the Svalbard configuration
  run: svalbard config check --config svalbard.toml --json
  env:
    SVALBARD_SOURCE_PASSWORD: ${{ secrets.DB_PASSWORD }}
```
