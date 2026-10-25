---
title: "Overview"
description: "Svalbard is an open-source CLI for database backups. It backs up PostgreSQL or MySQL to S3, R2 or an SSH server."
---

Svalbard is an open-source command-line tool for database backups, written in Rust.
It backs up one PostgreSQL or MySQL database per run to Amazon S3, Cloudflare R2, another S3-compatible service, or an SSH server with SFTP or SCP.
CI or cron owns the schedule. Svalbard has no server and no hosted control plane.

| Sources | Destinations |
| --- | --- |
| PostgreSQL 15, 16, 17 and 18 | Amazon S3 and S3-compatible services |
| MySQL 8.4 and 9.7, InnoDB | Cloudflare R2 |
| | SSH servers, with SFTP or SCP |

## How a backup runs

1. Svalbard checks the configuration and the destination, without writing.
2. It dumps the database from a snapshot with the native client, `pg_dump` or `mysqldump`, into a private staging directory.
   A watchdog ends the backup sessions when they block other sessions.
3. It closes the snapshot and the database connections, and only then uploads the dump and its completion record.
4. It deletes the staging directory. A backup is complete only when the dump and its record are both stored.

## Pages

- [Install](install.md): the release archive, the native clients, or a build from source.
- [Configuration](configuration.md): the settings, their flags and environment variables, TLS and secrets.
- [Storage](storage.md): the stored objects, and the S3, R2 and SSH destinations.
- [Back up](backup.md): the stages, cancellation, the output and the exit codes.
- [Restore](restore.md): the native restore of a backup.
- [Run in CI](ci.md) and the [GitHub Action](github-action.md).
- [Database safety contract](database-safety.md): the rules that protect the source database.
- [Development](development.md): checks, tests, releases and this website.

Svalbard is [MIT licensed](../LICENSE). The license covers the code, not the name or the logos: see [trademarks](../TRADEMARKS.md). The source is on [GitHub](https://github.com/paulrose/svalbard).
