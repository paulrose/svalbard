---
title: "Back up"
description: "The stages of a backup, cancellation, the output and the exit codes."
---

```sh
export SVALBARD_SOURCE_PASSWORD='…'
svalbard backup --config svalbard.toml
svalbard backup --config svalbard.toml --json
```

## Stages

`svalbard backup` uses the same configuration, flags and environment variables as `config check`. It runs these stages in order:

1. `configuration`: Svalbard validates the configuration.
2. `destination`: Svalbard checks what it can without writing. For S3 and R2, these are the region and the credentials. For SSH, these are the connection, the host key, the authentication and the directory. The check has 30 s.
3. `staging`: Svalbard creates a private run directory in `safety.staging_dir`.
4. `dump`: Svalbard checks the source and dumps it under the [database safety contract](database-safety.md). Then it closes the snapshot and the database connections.
5. `transfer`: Svalbard uploads the staged dump, and then its completion record. After an `upload` failure, Svalbard tries once more after 10 s, with the same staged dump and the same names, within `destination.upload_timeout`.
6. `cleanup`: Svalbard deletes the run directory, after a success and after a failure.

A backup is complete when the dump and its record are both stored. Only then does Svalbard report success.
Progress lines go to stderr.

## Cancellation

SIGINT and SIGTERM cancel the backup. Svalbard ends the backup sessions, stops the clients, removes unfinished uploads and temporary files, and deletes the run directory.
This cleanup can take up to about a minute. If a CI system sends SIGKILL sooner, no cleanup runs.
Then `PR_SET_PDEATHSIG` stops the clients, the server-side backstops end the database sessions, and the lifecycle rule (S3) or the `.partial` names (SSH) mark what remains.
The run directory `svalbard-<run id>` also remains in `safety.staging_dir`, with the partial dump and, if the dump was running, the file with the database password. Delete it.
When GitHub Actions cancels a job, it sends SIGINT and then SIGTERM to the entry process of the running step only, and kills the step's processes after 10 s.
Start Svalbard with `exec` there: see [Cancellation and timeouts](ci.md#cancellation-and-timeouts).

## Output and exit codes

`config check` prints `Configuration is valid.` on stdout, and then the [excluded tables](configuration.md#excluded-tables) if there are any.
With `--json`, stdout gets `{"excluded_tables":[],"valid":true}` or `{"valid":false,"error":"<message>"}`.

`backup` prints `Backup complete: <data> and <record>.` on stdout, and then the excluded tables if there are any. With `--json`, stdout gets one object:

```json
{"result":"complete","run_id":"…","data":"app/….dump","record":"app/….json","size":1048576,"sha256":"…","format":"pg_dump-custom","client_version":"pg_dump (PostgreSQL) 18.0","server_version":"18.0","excluded_tables":[]}
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
