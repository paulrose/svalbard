# Svalbard

Svalbard is an open-source Rust CLI for database backups.
It backs up PostgreSQL or MySQL to Amazon S3, Cloudflare R2, another S3-compatible service, or an SSH server with SFTP or SCP.
Svalbard dumps the database from a snapshot, closes the snapshot, and only then uploads the dump and its completion record.

The documentation is at [withsvalbard.com/docs](https://withsvalbard.com/docs/), and the same pages are in [`docs/`](docs/index.md).

```sh
svalbard config check --config svalbard.toml
export SVALBARD_SOURCE_PASSWORD='…'
svalbard backup --config svalbard.toml --json
```

- [Install](docs/install.md): the release archive, the native clients, or a build from source.
- [Configuration](docs/configuration.md), [Storage](docs/storage.md), [Back up](docs/backup.md) and [Restore](docs/restore.md).
- [Run in CI](docs/ci.md) and the [GitHub Action](docs/github-action.md).
- [Database safety contract](docs/database-safety.md): how Svalbard protects the reads and writes of the source database.
- [Development](docs/development.md): checks, tests, releases and the website.

## License

[MIT](LICENSE). The license covers the code, not the Svalbard name or logos: see [TRADEMARKS.md](TRADEMARKS.md).
