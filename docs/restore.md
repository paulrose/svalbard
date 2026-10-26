---
title: "Restore"
description: "Download a backup, check it against its record, and restore it with the native clients."
---

A backup is the dump and its completion record (see [Storage](storage.md)).

1. Download both, for example with `aws s3 cp s3://<bucket>/<key> .` or with `sftp`.
2. Check the dump against the record:

   ```sh
   echo "$(jq --raw-output .sha256 20261022T134500Z-<run id>.json)  20261022T134500Z-<run id>.dump" |
     sha256sum --check
   ```

3. Restore it into a new database with the native client.

## PostgreSQL

```sh
createdb --template=template0 app_restored
pg_restore --exit-on-error --dbname=app_restored 20261022T134500Z-<run id>.dump
```

The dump has no roles, tablespaces or other global objects. The roles that own objects or have grants in the dump must exist before the restore.

## MySQL

```sh
mysql --defaults-extra-file=restore.cnf --execute='CREATE DATABASE app_restored'
mysql --defaults-extra-file=restore.cnf app_restored <20261022T134500Z-<run id>.sql
```

`restore.cnf` is an option file with `[client]`, `host`, `user` and `password`, readable only by you, so that no argument holds the password.
The dump names no database, so it restores into the database that you give.
It has no accounts or grants, and the accounts in its `DEFINER` clauses must exist.
A dump from a replica restores its events disabled: see [MySQL replica](database-safety.md#mysql-replica).

## Excluded tables

The dump does not contain the tables in the record's `excluded_tables`. Every other table restores completely.

- Create the excluded tables after the restore, before the application uses them, for example with the application's migrations.
- Functions, procedures, triggers and events that use an excluded table restore without an error, but fail when they run.
- For PostgreSQL, the dump also leaves out the sequences that an excluded table owns, such as the sequence of a `serial` or identity column.
