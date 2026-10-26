---
title: "Storage"
description: "Where backups go, how Svalbard checks each upload, and the S3, R2 and SSH destinations."
---

Each backup is two objects with the same name stem, under `destination.prefix`:

```text
app/20261022T134500Z-<run id>.dump   the dump (.dump for PostgreSQL, .sql for MySQL)
app/20261022T134500Z-<run id>.json   the completion record
```

The time is the UTC start of the run. Svalbard uploads the record only after the dump is complete.
A dump without a record is an incomplete backup. Svalbard never deletes it, because a backup bucket can deny deletes.

The record holds `version` (1), `created`, `run_id`, `data` (the key of the dump), `format`, `client_version`, `server_version`, `size`, `sha256` (hexadecimal) and `excluded_tables`.
`excluded_tables` lists the [excluded tables](configuration.md#excluded-tables), and is empty if there are none. A record without it is from Svalbard 0.1.0, and its dump has all tables.
The dump object also carries its SHA-256 as the metadata `x-amz-meta-sha256`.

## Amazon S3 and S3-compatible services

S3 checks every upload against a SHA-256: a small dump in one request, a larger dump in parts of 64 MiB or more.
Svalbard also checks that the parts form the staged file. Each request has up to 3 attempts. `destination.upload_timeout` limits the whole upload.
After an error or cancellation, Svalbard aborts the unfinished multipart upload.

The S3 credentials need these permissions on the prefix:

- `s3:PutObject` for the dump and the record.
- `s3:AbortMultipartUpload` for cleanup after an error.
- `s3:ListBucketMultipartUploads` on the bucket, recommended. It finds an upload that was cancelled before S3 returned its ID.

Add a lifecycle rule with `AbortIncompleteMultipartUpload` to the bucket. It removes the parts of an upload that Svalbard could not abort, for example after `SIGKILL`.

## Cloudflare R2

```toml
[destination]
type = "r2"
bucket = "example-backups"
prefix = "app/"
endpoint = "https://<account-id>.r2.cloudflarestorage.com"
```

Use the S3 API endpoint of the account, without the bucket. For a bucket in a jurisdiction, the endpoint includes it, such as `https://<account-id>.eu.r2.cloudflarestorage.com`.
The region is `auto`. [`examples/r2.toml`](../examples/r2.toml) is a complete example.

Create an R2 API token with Object Read & Write on the bucket.
Set its Access Key ID in `AWS_ACCESS_KEY_ID` and its Secret Access Key in `AWS_SECRET_ACCESS_KEY`.

R2 checks a small dump against its SHA-256, as S3 does. For the parts of a larger dump, R2 checks MD5 instead of SHA-256.
Svalbard then checks the MD5 ETag of the complete object, and that the parts form the staged file.
The record and `x-amz-meta-sha256` hold the SHA-256 for every dump.

R2 aborts incomplete multipart uploads after 7 days by default.

## SSH: SFTP and SCP

```toml
[destination]
type = "sftp"          # or "scp"
host = "backup.example.com"
user = "svalbard"
path = "/srv/backups"
known_hosts_file = "/etc/svalbard/known_hosts"
```

[`examples/sftp.toml`](../examples/sftp.toml) and [`examples/scp.toml`](../examples/scp.toml) are complete examples.
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
