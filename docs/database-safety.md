# Database safety contract

Design note for [#6](https://github.com/paulrose/svalbard/issues/6).
The rules apply to the PostgreSQL adapter ([#9](https://github.com/paulrose/svalbard/issues/9)) and the MySQL adapter ([#10](https://github.com/paulrose/svalbard/issues/10)).
The limits are the `[safety]` settings of the configuration.
The facts in this note were checked on 2026-10-21 against the manuals and the client source code in [References](#references).

## Purpose and limits

Svalbard protects ordinary reads and writes on the source database. It does not guarantee zero impact.

A logical dump reads every row. It uses CPU, disk I/O, memory and network on the database server.
It keeps a snapshot open until the dump ends, and other sessions can wait for locks that the dump holds.
Svalbard limits the duration of this impact and stops the backup when it detects contention.
It cannot remove the impact.

Each rule in this note has one of three classes:

- **Enforced**: Svalbard checks or limits the condition on every run. A violation fails the run.
- **Assumption**: Svalbard depends on the condition but cannot check it. The documentation tells the operator.
- **Experiment**: a target that tests must measure before the first release ([#15](https://github.com/paulrose/svalbard/issues/15)).

A failed run never reports success. Svalbard never uploads a staged file from a failed or cancelled dump.

## Supported versions

| Source | Servers | Client |
| --- | --- | --- |
| PostgreSQL | 15, 16, 17, 18 | `pg_dump` 15 to 18, of the same major version as the server or newer |
| MySQL | 8.4 LTS, 9.7 LTS | `mysqldump` of the same LTS series as the server |

`pg_dump` refuses to dump a server that is newer than its own major version.
An archive from `pg_dump` 17 or newer has archive format 1.16, which `pg_restore` 16 and older cannot read.
Restore an archive with a `pg_restore` of the same major version as the `pg_dump` that wrote it, or newer.
`pg_restore` 17 and newer send `SET transaction_timeout = 0`, which servers 15 and 16 reject. `pg_restore` reports this error, continues, and exits with status 1.
For a clean restore into servers 15 and 16, dump them with `pg_dump` of the same major version.

The MySQL manual gives no compatibility matrix for clients and servers of different series, so Svalbard requires the same series.
Use the newest patch release. `mysqldump` 8.4.13 and 9.7.4 correct faults with malicious server metadata.

Before the dump starts, Svalbard checks the client and server versions against this table. It records both versions in the artifact metadata.

## Where to run the dump

Use a source in this order of preference:

1. A dedicated replica that serves no application traffic.
2. A replica that serves application traffic.
3. The primary.

### PostgreSQL standby

`pg_dump` can run on a hot standby. WAL replay can conflict with the dump:

- An `ACCESS EXCLUSIVE` lock from the primary, for example from `ALTER TABLE`, conflicts with the dump's table locks.
- Vacuum cleanup records conflict with the dump's snapshot.

The standby delays replay up to `max_standby_streaming_delay` (default 30 s), then cancels the dump. The backup fails.
While replay waits, replication lag grows. If the delay is `-1`, replay can wait until the dump ends.
The watchdog treats WAL replay that waits for a backup lock as contention (Experiment: confirm that detection sees the startup process).

`hot_standby_feedback = on` prevents cleanup conflicts. It also holds back vacuum on the primary for all databases, which can cause bloat.
Svalbard does not change this setting.
A dump from a standby always excludes the data in unlogged tables.

### MySQL replica

Svalbard never uses `--dump-replica` and never stops replication.
The replication applier needs an exclusive metadata lock for each replicated schema change.
If the dump holds a lock on the same table, the applier waits, later transactions wait in commit order, and replication lag grows.
The watchdog treats the waiting applier as contention (Experiment: confirm that detection sees the applier threads).
A replicated schema change on a table that the dump did not read yet fails the run with `schema_changed` (see [Schema changes](#schema-changes)).

### Primary

On a primary, the dump competes with application traffic for CPU, I/O and buffer cache.
Its snapshot delays the cleanup of old row versions for the full dump duration.
Schema changes from deployments conflict with the dump. Schedule backups outside migration windows.
Contention detection cancels the backup when a schema change waits for it.

## Limits

| Limit | Key | Default | Range | Enforcement | Failure |
| --- | --- | --- | --- | --- | --- |
| Dump workers | none | 1 | 1 | Svalbard never passes `--jobs` to `pg_dump`. `mysqldump` uses one connection. | — |
| Lock wait for the backup | `lock_timeout` | 10 s | 1 s to 5 min | The watchdog. PostgreSQL also gets `--lock-wait-timeout`, and MySQL gets `lock_wait_timeout` through `--init-command`. | `lock_timeout` |
| Wait that the backup causes | `contention_timeout` | 2 s | 1 s to 60 s | The watchdog. | `contention` |
| Dump duration | `dump_timeout` | 1 h | 1 min to 24 h | A timer from client start to client exit. | `dump_timeout` |
| Staged file size | `max_dump_size` | 20 GiB | 1 MiB to 16 TiB | Svalbard counts the bytes that it writes. | `staging_limit` |
| Free space on the staging filesystem | `min_free_space` | 2 GiB | 0 to 1 TiB | A free-space check before the dump and after each 64 MiB written. | `staging_limit` |

The watchdog interval (1 s) and the cancellation grace period (10 s) are fixed. They are not configuration values.

The failure names are error kinds for the JSON result. The CLI issues assign the exit codes.

The upload has separate deadlines, which the storage adapters define ([#11](https://github.com/paulrose/svalbard/issues/11)).
The dump deadline limits only the time that the snapshot stays open.

## Run sequence

### Client process

- Svalbard starts the native client directly, without a shell.
- Svalbard builds the full argument list. The configuration cannot add raw client options.
- The child environment contains only `PATH` and the variables that Svalbard sets. Inherited `PGOPTIONS`, `PG*` and `MYSQL_*` variables do not reach the client.
- Credentials go in a private file in the run directory. They are never in arguments or in the environment.
- On Linux, Svalbard sets `PR_SET_PDEATHSIG`. Then the client receives `SIGTERM` if Svalbard stops without cleanup.

### Control connection

Before the client starts, Svalbard opens a control connection with the same credentials.
The control connection runs short autocommit statements. It holds no locks and no snapshot.
It runs the preflight checks, the watchdog, the cancellation and the closure check.
A backup therefore uses two database connections (Assumption: the server has connection headroom).

### Preflight

Before the dump starts, Svalbard checks:

- The client and server versions.
- The privileges for the dump, for contention detection and for cancellation.
- The source-specific conditions, such as storage engines.
- The free space in the staging directory.
- That one detection query succeeds.

If a check fails, no dump starts. A failed detection check gives `detection_unavailable`.

### Watchdog

While the client runs, the watchdog runs once per second. It cancels the backup when one of these conditions occurs:

| Condition | Failure |
| --- | --- |
| A backup session waits for a lock for longer than `lock_timeout`. | `lock_timeout` |
| Another session waits for a lock that a backup session holds, for longer than `contention_timeout`. | `contention` |
| The dump runs for longer than `dump_timeout`. | `dump_timeout` |
| Detection fails for longer than `contention_timeout`, or the backup session does not appear within `lock_timeout`. | `detection_lost` |

If the client exits with an error while a backup session waits for a lock, the run fails with `lock_timeout`.
This happens when the server-side lock timeout of the client ends the wait before the watchdog.

Svalbard does not wait for contention to stop.
A schema change that waits for the dump also blocks later reads and writes on the same table. In PostgreSQL, it can wait until the dump ends.

The watchdog counts every waiting session, including replication appliers and WAL replay.

### Cancellation

Cancellation targets backup sessions only. Svalbard identifies a backup session by a value that is unique to the run.
It never selects sessions by user name, database or query text alone.

1. Svalbard ends each backup session from the control connection. If the control connection failed, Svalbard opens a new one.
2. Svalbard sends `SIGTERM` to the client. After 10 s, it sends `SIGKILL`. Svalbard waits for the process to exit.
3. Svalbard checks that no backup session remains. If one remains, the run reports `cleanup_failed` with the session IDs.
4. Svalbard deletes the run directory.

The end of a session also ends its transaction, its locks and its snapshot.
Svalbard ends the sessions on the server because a session that waits for a lock does not detect a lost client in either database.

If Svalbard receives `SIGINT` or `SIGTERM`, for example when a CI job is cancelled, it runs the same steps and exits with an error.
If Svalbard is killed with `SIGKILL`, no cleanup runs. `PR_SET_PDEATHSIG` stops the client, and the server-side backstops for each source apply.
If the host stops, the server ends the sessions only after TCP detects the lost connection (Assumption).

### Private staging

- Svalbard creates a new run directory with mode `0700` in the staging directory. If the directory exists, the run fails.
- Svalbard creates each file with mode `0600` and fails if the file exists.
- The client writes the dump to standard output. Svalbard writes the staged file, counts its bytes and calculates its checksum.
- Svalbard copies the stream through a fixed-size buffer. Its memory use does not depend on the size of the database.
- Svalbard deletes the credential file when the client exits, before the upload starts.

The staging directory is not on the filesystem that holds the database files (Assumption).
If it is, set a larger `min_free_space`, because a full disk can stop the database.

### Snapshot closure before upload

The dump step succeeds only when all of these conditions are true:

1. The client exited with status 0.
2. Svalbard read the end of the client's standard output.
3. Svalbard flushed and closed the staged file.
4. The control connection shows no backup session.
5. For MySQL, the schema did not change during the dump.

Svalbard uses the exit status, not the dump content, because `mysqldump` can write its `Dump completed` line after an error.
After the checks, Svalbard closes the control connection and the upload starts.
The upload reads the staged file. It never reads the client stream, because a slow upload keeps the snapshot open.

### Cleanup

- After a failure or cancellation, Svalbard deletes the run directory before it exits.
- After a successful upload, Svalbard deletes the run directory.
- If a deletion fails, the run reports `cleanup_failed` with the path.
- Svalbard does not delete the directories of other runs, because another run can still use them.

## PostgreSQL rules

### Snapshot and locks

`pg_dump` starts a `REPEATABLE READ, READ ONLY` transaction.
Then it locks each table that it dumps in `ACCESS SHARE` mode.
It keeps the snapshot and the locks until it exits. This includes the time when it waits to write to standard output.

`ACCESS SHARE` conflicts only with `ACCESS EXCLUSIVE`.
`DROP TABLE`, `TRUNCATE`, `REINDEX`, `CLUSTER`, `VACUUM FULL`, `REFRESH MATERIALIZED VIEW`, `LOCK TABLE` and most forms of `ALTER TABLE` take `ACCESS EXCLUSIVE`.
Vacuum truncation of empty pages also needs it.
Each of these statements waits until the dump ends.
While one waits, later statements on the same table, including `SELECT`, wait behind it.
`pg_dump` does not block ordinary reads and writes directly. It can block them through a waiting schema change.

The table locks block schema changes on the dumped tables until the dump ends.
`pg_dump` does not lock views, sequences or functions.
It also reads some definitions through server functions that see the latest committed state, not the snapshot.
A schema change before the table locks, or on an object without a lock, can make `pg_dump` fail (Experiment: confirm that these cases fail and do not give an incorrect dump).

### Timeouts

`pg_dump` sets `statement_timeout`, `lock_timeout`, `idle_in_transaction_session_timeout` and `transaction_timeout` to 0 after it connects.
Server, role and `PGOPTIONS` values for these settings have no effect on the dump.
Svalbard enforces all deadlines from outside.

Svalbard passes `--lock-wait-timeout=<lock_timeout>`.
`pg_dump` applies this value as `statement_timeout` to each `LOCK TABLE` batch, at the start of the dump only.
When this timeout ends the dump, the run fails with `lock_timeout`. The watchdog limits all other lock waits.

`pg_dump` does not reset `client_connection_check_interval`.
If the server accepts the setting, Svalbard sets it to 10 s through `PGOPTIONS`.
Then the server ends the session within 10 s after the client disconnects (Experiment: confirm this during a lock wait).
The setting needs a Linux, macOS, illumos or BSD server.

### Arguments

Svalbard passes `--format=custom`, `--no-password` and `--lock-wait-timeout`.
The connection settings go through `PGHOST`, `PGPORT`, `PGDATABASE`, `PGUSER`, `PGSSLMODE` and `PGSSLROOTCERT`, and the password through `PGPASSFILE`.

Svalbard never passes these options:

| Option | Reason |
| --- | --- |
| `--jobs` | More connections and load. Workers abort when they meet a waiting `ACCESS EXCLUSIVE` request. |
| `--serializable-deferrable` | The start of the dump can wait for an indeterminate time. It fails on a standby. |
| `--enable-row-security` | It dumps only the rows that the role can see. That is a partial backup. |
| `--role` | Cancellation matches sessions by their login role. |
| Table, schema and data filters | A filtered dump is a partial backup. |

Svalbard sets the session label through `PGAPPNAME`, for example `svalbard-<run id>`.
The value has less than 64 ASCII characters and contains no secrets, because all users can see it.

### Contention detection

The watchdog uses `pg_locks` and `pg_blocking_pids()`. Both need no special privileges.
`pg_stat_activity` hides `wait_event` and `query` for the sessions of other roles, so the watchdog does not use these columns.

```sql
-- Backup sessions of this run
SELECT pid FROM pg_stat_activity
WHERE application_name = $1 AND usename = current_user;

-- Sessions that wait, with the backup sessions that block them
WITH waiting AS MATERIALIZED (
  SELECT DISTINCT pid, waitstart FROM pg_locks WHERE NOT granted
)
SELECT pid, now() - waitstart AS waited
FROM waiting
WHERE pg_blocking_pids(pid) && $2;
```

`pg_blocking_pids()` reports the sessions that hold a conflicting lock and the sessions ahead in the wait queue.
Frequent calls need short exclusive access to the lock manager. The materialized CTE calls the function only for sessions that wait.
`waitstart` can be null for a short time after a wait starts. The watchdog treats null as a new wait.
The same `pg_locks` query, filtered on the backup PIDs, finds the lock waits of the backup itself.

### Cancellation

Svalbard runs `pg_terminate_backend(pid)` for each session where `application_name` is the run label and `usename` is `current_user`.
A role can end its own sessions without `pg_signal_backend` or superuser.

`pg_dump` also sends a cancel request when it receives `SIGINT`, `SIGTERM` or `SIGQUIT`, in serial and parallel mode.
Without `client_connection_check_interval`, a backend that waits for a lock detects a lost client only after it gets the lock.

### Privileges

- The dump needs read access to every object that it dumps. The predefined role `pg_read_all_data` gives `SELECT` on tables and sequences and `USAGE` on schemas.
- `pg_read_all_data` does not include large objects. A database with large objects needs the privileges on them.
- `pg_read_all_data` does not bypass row-level security. If a table has policies, the role needs `BYPASSRLS`, or `pg_dump` fails.
- Detection and cancellation need no additional privileges.

The preflight lists the tables and sequences that the role cannot read, and the tables with row-level security that applies to the role.
It does not check large objects.

### Scope

`pg_dump` dumps one database.
It does not dump roles, tablespaces or other global objects.
The roles that own objects or have grants in the dump must exist before a restore.

## MySQL rules

### Snapshot and locks

`mysqldump --single-transaction` sets `REPEATABLE READ` and sends `START TRANSACTION WITH CONSISTENT SNAPSHOT`.
The snapshot is consistent only for InnoDB tables. The transaction stays open until `mysqldump` disconnects.

`mysqldump` holds a shared metadata lock on a table while it reads the table.
After each table, it runs `ROLLBACK TO SAVEPOINT` to release the lock.
A statement that needs an exclusive metadata lock on that table, for example `ALTER TABLE`, waits until the table is complete.
While it waits, later reads and writes on the table wait behind it.

### Storage engines

Before the dump, Svalbard reads `information_schema.TABLES` for the database.
If a base table uses an engine other than InnoDB, the run fails before the dump.
With `--single-transaction`, `mysqldump` reads MyISAM and MEMORY tables without a lock and without a consistent snapshot.

### Schema changes

The MySQL manual tells operators not to run `ALTER TABLE`, `CREATE TABLE`, `DROP TABLE`, `RENAME TABLE` or `TRUNCATE TABLE` in other connections during the dump.
The consistent read is not isolated from these statements. The dump can fail, or it can contain incorrect data without an error.

Concurrent schema changes are not supported. Svalbard cannot prevent them, but it detects them:

- A statement that waits for a metadata lock of the dump is contention. The watchdog cancels the backup.
- Svalbard records the table and column definitions before the client starts and after it exits. If they differ, the run fails with `schema_changed`.

The definitions come from `information_schema.TABLES` and `information_schema.COLUMNS`, with `information_schema_stats_expiry = 0` for the control session.
Experiment: confirm which forms of these statements the definition check detects.

### Arguments

Svalbard passes these options:

| Option | Reason |
| --- | --- |
| `--defaults-file=<private file>` | It is the first argument and contains the credentials. Option files such as `/etc/my.cnf` cannot add options. |
| `--single-transaction` | InnoDB snapshot. It also turns off `--lock-tables`. |
| `--skip-lock-tables` | Makes the absence of `LOCK TABLES` explicit. |
| `--quick` | Streams rows. Without it, `mysqldump` keeps each table in client memory. |
| `--set-gtid-purged=OFF` | Prevents `FLUSH TABLES WITH READ LOCK` on servers that use GTIDs. |
| `--no-tablespaces` | Removes the need for the `PROCESS` privilege. |
| `--init-command="SET SESSION lock_wait_timeout = <lock_timeout>, @svalbard_run = '<run id>'"` | Limits each metadata lock wait of the dump on the server, in whole seconds. Labels the session for detection and cancellation. |
| `--routines`, `--events` | Includes stored routines and events. Triggers are included by default. |

Svalbard never passes these options:

| Option | Reason |
| --- | --- |
| `--lock-all-tables`, `-x` | A global read lock for the whole dump. |
| `--lock-tables` | `LOCK TABLES ... READ` blocks writes to InnoDB tables. |
| `--flush-logs` | Runs `FLUSH LOGS`. With `--single-transaction`, it also takes `FLUSH TABLES WITH READ LOCK`. |
| `--source-data`, `--master-data` | Takes `FLUSH TABLES WITH READ LOCK` at the start. |
| `--dump-replica`, `--dump-slave` | Stops the replica SQL thread. After a failed dump, `mysqldump` does not start it again. |
| `--delete-source-logs` | Purges binary logs on the server. |
| `--set-gtid-purged` with a value other than `OFF` | Takes `FLUSH TABLES WITH READ LOCK` when `gtid_mode` is not `OFF`, including the permissive modes. |
| `--skip-quick` | Keeps whole tables in client memory. |
| `--force` | Continues after SQL errors, which can give a partial dump. |

`FLUSH TABLES WITH READ LOCK` is short, but it waits for running statements. While it waits, it blocks new writes on the whole server.

With `--set-gtid-purged=OFF`, the dump does not contain the GTID set. A restore creates new GTIDs.

### Contention detection

Detection needs these conditions. The preflight checks them:

- `performance_schema` is on.
- The consumer `global_instrumentation` and the instrument `wait/lock/metadata/sql/mdl` are enabled. Both are enabled by default.
- The account has `SELECT` on `performance_schema.metadata_locks`, `threads`, `user_variables_by_thread`, `setup_consumers` and `setup_instruments`.

`mysqldump` has no option for a session label. The `--init-command` sets the user variable `@svalbard_run` in the dump session instead.
`user_variables_by_thread` shows the variables of all sessions, so the control connection finds the backup session:

```sql
SELECT t.THREAD_ID, t.PROCESSLIST_ID
FROM performance_schema.user_variables_by_thread AS v
JOIN performance_schema.threads AS t USING (THREAD_ID)
WHERE v.VARIABLE_NAME = 'svalbard_run' AND v.VARIABLE_VALUE = ?;
```

The client process ID in the connection attributes is not unique. Two containers on one host can use the same process ID.

The watchdog finds the sessions that wait for an object that the backup session locks:

```sql
SELECT w.OWNER_THREAD_ID, w.LOCK_TYPE, w.OBJECT_SCHEMA, w.OBJECT_NAME
FROM performance_schema.metadata_locks AS w
JOIN performance_schema.metadata_locks AS h
  ON h.OBJECT_TYPE = w.OBJECT_TYPE
 AND h.OBJECT_SCHEMA <=> w.OBJECT_SCHEMA
 AND h.OBJECT_NAME <=> w.OBJECT_NAME
WHERE h.OWNER_THREAD_ID = ? AND h.LOCK_STATUS = 'GRANTED'
  AND w.LOCK_STATUS = 'PENDING' AND w.OWNER_THREAD_ID <> h.OWNER_THREAD_ID;
```

`metadata_locks` does not show when a wait started. The watchdog measures each wait from the first poll that sees it.
The query counts every pending request on a locked object, also when another session blocks it too. A false match cancels only the backup.
The pending requests of the backup session itself are its lock waits.

### Cancellation

Svalbard runs `KILL CONNECTION <processlist id>` for the backup session. An account can kill its own connections without `CONNECTION_ADMIN`.

If only the client stops, a server thread that waits for a metadata lock continues to wait.
It stops when it gets the lock, when `lock_wait_timeout` expires, or after `KILL`.
`lock_wait_timeout` from `--init-command` limits this wait to `lock_timeout` when Svalbard cannot run `KILL`.

### Privileges

- The dump needs `SELECT`, `SHOW VIEW` and `TRIGGER` on the database, and `EVENT` for events.
- Routines need global `SELECT` or `SHOW_ROUTINE` (Experiment: confirm that `SHOW_ROUTINE` is sufficient).
- With the arguments above, the dump needs no `LOCK TABLES`, `RELOAD`, `FLUSH_TABLES`, `PROCESS` or replication privileges.
- Detection needs `SELECT` on the five `performance_schema` tables. Cancellation needs no additional privileges.

Use a dedicated backup account without `LOCK TABLES`, `RELOAD`, `FLUSH_TABLES` and `CONNECTION_ADMIN`.
Then the account cannot take global or table locks, even if an argument is wrong.

### Workload impact

A long snapshot with concurrent writes increases the InnoDB history list length.
InnoDB cannot purge old row versions for the snapshot, and the undo tablespace can grow.
If `innodb_max_purge_lag` is not 0, InnoDB delays `INSERT`, `UPDATE` and `DELETE` statements when the purge lag is more than that value.

`mysqldump` sets `net_read_timeout` and `net_write_timeout` to 86400 s for its session.
If the reader of the output stops, the server can keep the snapshot open for one day.
Svalbard reads the output continuously, and `dump_timeout` ends the session.

A non-zero global `max_execution_time` can stop a long table read. The run then fails (Assumption: the operator sets it to 0 or a large value).

### Scope

`mysqldump` dumps one database.
It does not dump accounts, grants or the `mysql` system schema.
The accounts in `DEFINER` clauses must exist before a restore.

## Enforced, assumed and measured

### Enforced on every run

- The supported versions and the version rules for client and server.
- One dump worker, a fixed argument list and no inherited client options.
- InnoDB tables only, for MySQL.
- Contention detection is available before the dump starts.
- `lock_timeout`, `contention_timeout`, `dump_timeout`, `max_dump_size` and `min_free_space`.
- Cancellation of the sessions of this run only.
- No backup session remains before the upload starts.
- No schema change during a MySQL dump.
- Private staging, deletion of credential files, and cleanup on every handled exit.

### Assumptions

- The staging directory is not on the filesystem of the database, or `min_free_space` is large enough for both.
- The server has headroom for two more connections and spare CPU and I/O for a full read.
- In PostgreSQL, `max_locks_per_transaction` is large enough for one lock on each table.
- Server settings do not stop long reads, for example a MySQL `max_execution_time`.
- The operator sets replica settings such as `max_standby_streaming_delay` and `hot_standby_feedback`.
- Migrations set their own `lock_timeout`. Svalbard cannot stop a schema change from waiting. It can only cancel the backup.
- After `SIGKILL` or the loss of the host, cleanup depends on the server-side backstops.

### Experiments

- The workload budgets in [Measurement plan](#measurement-plan).
- Detection sees PostgreSQL WAL replay and the MySQL replication applier.
- `client_connection_check_interval` ends a PostgreSQL session that waits for a lock.
- A PostgreSQL schema change before the table locks, or on an object without a lock, makes `pg_dump` fail.
- The MySQL definition check detects each form of the unsupported schema changes.
- `SHOW_ROUTINE` is sufficient for MySQL routines.

## Measurement plan

### Setup

- Use containers with pinned server versions: PostgreSQL 15 to 18, MySQL 8.4 and 9.7.
- Give each container fixed CPU and memory limits.
- Make the dataset four times larger than `shared_buffers` or the InnoDB buffer pool, so that the dump reads from disk.
- Create load with `pgbench` for PostgreSQL and `sysbench oltp_read_write` for MySQL.
- Run the load at a fixed rate of 50% of the maximum throughput on the same setup.
- Measure five minutes without a backup, then the same period with a backup.

Record application throughput, p50 and p99 latency, and errors.
Record server CPU and I/O, replication lag, PostgreSQL `age(backend_xmin)`, and the InnoDB history list length.
Record the memory of Svalbard and the staged bytes.

### Budgets

These budgets are initial targets. Review them after the first measurements. A release needs all budgets met, or an approved change to a budget.

| ID | Measurement | Budget |
| --- | --- | --- |
| B1 | Application errors that the backup causes | 0 |
| B2 | p99 latency during the dump | Not more than 2 × the baseline p99 |
| B3 | Throughput at the fixed rate | At least 95% of the rate |
| B4 | Stall of reads and writes on a table after a schema change starts to wait for the backup | Not more than `contention_timeout` + 3 s |
| B5 | Age of the backup snapshot | Not more than `dump_timeout` + 15 s |
| B6 | Backup sessions and staged files after a failure | 0, within 15 s |
| B7 | Memory of the Svalbard process | Not more than 64 MiB, for all database sizes |
| B8 | Duration of one detection query | Not more than 50 ms at p99, with 500 client sessions |

### Scenarios

The adapter issues run the scenarios for their source. [#15](https://github.com/paulrose/svalbard/issues/15) runs the full set with load and budgets.

| ID | Scenario | Expected result |
| --- | --- | --- |
| S1 | Dump under concurrent reads and writes, on a primary and on a replica | Success. B1, B2, B3, B7 and B8 met. A native restore matches the fixture data. |
| S2 | `ALTER TABLE` on a table that the dump locks | `contention`. B4 met. No success record. |
| S3 | `ALTER TABLE`, `TRUNCATE TABLE` and `RENAME TABLE` on a MySQL table that the dump did not read yet | `schema_changed`. |
| S4 | The backup starts while a schema change holds an exclusive lock | `lock_timeout`. The backup does not block application traffic. |
| S5 | A dump that takes longer than `dump_timeout` | `dump_timeout`. B5 and B6 met. |
| S6 | A staging filesystem that is too small | `staging_limit`. B6 met. |
| S7 | `SIGINT` and `SIGTERM` to Svalbard during preflight, the dump and the closure check | Error exit. B6 met. |
| S8 | `SIGKILL` to Svalbard during a lock wait and during the data transfer | Record how long the backup sessions stay open. |
| S9 | Missing detection privileges, and `performance_schema` off | `detection_unavailable`. No dump starts. |
| S10 | A MySQL database with a MyISAM table | The run fails before the dump. |
| S11 | Other sessions of the same database user, including another Svalbard run | Svalbard never ends them. |
| S12 | A PostgreSQL replay conflict, with the default delay and with `-1`. A replicated `ALTER TABLE` on MySQL. | The backup fails or is cancelled. Record the replication lag. |
| S13 | The control connection fails during the dump | `detection_lost`. B6 met. |

## References

PostgreSQL:

- [`pg_dump`](https://www.postgresql.org/docs/current/app-pgdump.html), [`pg_restore`](https://www.postgresql.org/docs/current/app-pgrestore.html), [explicit locking](https://www.postgresql.org/docs/current/explicit-locking.html), [`pg_locks`](https://www.postgresql.org/docs/current/view-pg-locks.html), [system information functions](https://www.postgresql.org/docs/current/functions-info.html)
- [Server signaling functions](https://www.postgresql.org/docs/current/functions-admin.html#FUNCTIONS-ADMIN-SIGNAL), [cumulative statistics](https://www.postgresql.org/docs/current/monitoring-stats.html), [hot standby](https://www.postgresql.org/docs/current/hot-standby.html), [connection settings](https://www.postgresql.org/docs/current/runtime-config-connection.html), [predefined roles](https://www.postgresql.org/docs/current/predefined-roles.html), [versioning policy](https://www.postgresql.org/support/versioning/)
- Release 18 source: `setup_connection()` and `getTables()` in [`pg_dump.c`](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/bin/pg_dump/pg_dump.c), the signal handler in [`parallel.c`](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/bin/pg_dump/parallel.c), and `K_VERS_MINOR` in [`pg_backup_archiver.h`](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/bin/pg_dump/pg_backup_archiver.h)

MySQL:

- [`mysqldump`](https://dev.mysql.com/doc/refman/8.4/en/mysqldump.html), [metadata locking](https://dev.mysql.com/doc/refman/8.4/en/metadata-locking.html), [online DDL performance](https://dev.mysql.com/doc/refman/8.4/en/innodb-online-ddl-performance.html), [consistent reads](https://dev.mysql.com/doc/refman/8.4/en/innodb-consistent-read.html), [`KILL`](https://dev.mysql.com/doc/refman/8.4/en/kill.html), [`lock_wait_timeout`](https://dev.mysql.com/doc/refman/8.4/en/server-system-variables.html#sysvar_lock_wait_timeout)
- [`metadata_locks`](https://dev.mysql.com/doc/refman/8.4/en/performance-schema-metadata-locks-table.html), [user variables by thread](https://dev.mysql.com/doc/refman/8.4/en/performance-schema-user-variable-tables.html), [purge configuration](https://dev.mysql.com/doc/refman/8.4/en/innodb-purge-configuration.html), [replica backups](https://dev.mysql.com/doc/refman/8.4/en/replication-solutions-backups-mysqldump.html), [release model](https://dev.mysql.com/doc/refman/9.7/en/mysql-releases.html)
- 8.4 source: `main()`, `do_flush_tables_read_lock()`, `dump_all_tables_in_db()` and `connect_to_db()` in [`mysqldump.cc`](https://github.com/mysql/mysql-server/blob/8.4/client/mysqldump.cc), `MDL_context::acquire_lock()` in [`mdl.cc`](https://github.com/mysql/mysql-server/blob/8.4/sql/mdl.cc), and `pfs_create_metadata_lock_vc()` in [`pfs.cc`](https://github.com/mysql/mysql-server/blob/8.4/storage/perfschema/pfs.cc)
- `FLUSH TABLES WITH READ LOCK` with GTIDs: [8.0.32](https://dev.mysql.com/doc/relnotes/mysql/8.0/en/news-8-0-32.html) and [8.0.33](https://dev.mysql.com/doc/relnotes/mysql/8.0/en/news-8-0-33.html) release notes
