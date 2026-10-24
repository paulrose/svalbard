//! The safety scenarios of docs/database-safety.md ("Measurement plan") that need real servers,
//! through the real CLI. Run the tests with `scripts/test-integration.sh`. Each test prints one
//! evidence line that starts with `scenario:`.
//!
//! S1 and the load parts of S2 and S4 are in tests/workload.rs. S3, S4, S10 and the PostgreSQL
//! part of S2 are in tests/postgresql.rs and tests/mysql.rs, and S6 is in tests/backup.rs.

mod common;

use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

use common::{
    Fault, Proxy,
    cli::{self, MYSQL_SECRETS, POSTGRESQL_SECRETS, Setup, children, within},
    floci, mysql, postgresql, proxy, ssh,
};
use mysql_async::prelude::Queryable;
use rustix::process::{Pid, Signal, kill_process, kill_process_group};
use svalbard::config::Source;
use tokio::{process::Child, time};

/// A fixture of either source, for the scenarios that both sources share.
enum Fixture {
    Postgresql(Box<postgresql::Fixture>),
    Mysql(mysql::Fixture),
}

/// A connection that holds the lock on the `stall` table.
enum Lock {
    Postgresql(tokio_postgres::Client),
    Mysql(mysql_async::Conn),
}

impl Lock {
    async fn release(self) {
        match self {
            Lock::Postgresql(client) => client.batch_execute("COMMIT").await.unwrap(),
            Lock::Mysql(mut conn) => conn.query_drop("UNLOCK TABLES").await.unwrap(),
        }
    }
}

impl Fixture {
    async fn postgresql(name: &str) -> Fixture {
        Fixture::Postgresql(Box::new(postgresql::Fixture::create(name).await))
    }

    async fn mysql(name: &str) -> Fixture {
        Fixture::Mysql(mysql::Fixture::create(name).await)
    }

    fn name(&self) -> &'static str {
        match self {
            Fixture::Postgresql(_) => "postgresql",
            Fixture::Mysql(_) => "mysql",
        }
    }

    fn database(&self) -> &str {
        match self {
            Fixture::Postgresql(fixture) => &fixture.database,
            Fixture::Mysql(fixture) => &fixture.database,
        }
    }

    fn source(&self) -> Source {
        match self {
            Fixture::Postgresql(fixture) => fixture.source(postgresql::verify_full()),
            Fixture::Mysql(fixture) => fixture.source(mysql::verify_full()),
        }
    }

    fn upstream(&self) -> String {
        match self {
            Fixture::Postgresql(fixture) => fixture.upstream(),
            Fixture::Mysql(fixture) => fixture.upstream(),
        }
    }

    fn password(&self) -> &'static str {
        match self {
            Fixture::Postgresql(_) => postgresql::PASSWORD,
            Fixture::Mysql(_) => mysql::PASSWORD,
        }
    }

    fn secrets(&self) -> &'static [&'static str] {
        match self {
            Fixture::Postgresql(_) => POSTGRESQL_SECRETS,
            Fixture::Mysql(_) => MYSQL_SECRETS,
        }
    }

    /// A configuration with this source and an S3 bucket, and the bucket.
    async fn setup(&self, name: &str, safety: &str) -> (Setup, String) {
        let bucket = floci::bucket(&name.replace('_', "-"), None).await;
        let setup = Setup::new(
            name,
            &cli::source(&self.source()),
            &cli::floci(&bucket),
            safety,
        );
        (setup, bucket)
    }

    async fn version(&self) -> String {
        match self {
            Fixture::Postgresql(fixture) => {
                postgresql::server_version(&fixture.client().await).await
            }
            Fixture::Mysql(fixture) => mysql::server_version(&mut fixture.client().await).await,
        }
    }

    async fn hold_stall_lock(&self) -> Lock {
        match self {
            Fixture::Postgresql(fixture) => Lock::Postgresql(fixture.hold_stall_lock().await),
            Fixture::Mysql(fixture) => Lock::Mysql(fixture.hold_stall_lock().await),
        }
    }

    /// The number of backup sessions that wait for the lock on the `stall` table.
    async fn waiting(&self) -> i64 {
        match self {
            Fixture::Postgresql(fixture) => {
                let query = format!(
                    "SELECT count(*) FROM pg_locks l JOIN pg_stat_activity a USING (pid)
                     WHERE NOT l.granted AND l.relation = 'stall'::regclass
                       AND {}",
                    postgresql::BACKUP_SESSIONS
                );
                let client = fixture.client().await;
                client.query_one(&query, &[]).await.unwrap().get(0)
            }
            Fixture::Mysql(fixture) => {
                let count: Option<i64> = fixture
                    .client()
                    .await
                    .exec_first(
                        "SELECT COUNT(*) FROM performance_schema.metadata_locks l
                         JOIN performance_schema.threads t ON t.THREAD_ID = l.OWNER_THREAD_ID
                         WHERE l.LOCK_STATUS = 'PENDING' AND l.OBJECT_SCHEMA = ?
                           AND l.OBJECT_NAME = 'stall' AND t.PROCESSLIST_USER = ?",
                        (&fixture.database, &fixture.database),
                    )
                    .await
                    .unwrap();
                count.unwrap()
            }
        }
    }

    /// Waits until `count` backup sessions wait for the lock on the `stall` table.
    async fn stalled(&self, count: i64) {
        until(Duration::from_secs(10), async || {
            self.waiting().await >= count
        })
        .await;
    }

    async fn sessions(&self) -> usize {
        match self {
            Fixture::Postgresql(fixture) => fixture.backup_sessions().await as usize,
            Fixture::Mysql(fixture) => fixture
                .backup_sessions(&mut fixture.client().await)
                .await
                .len(),
        }
    }

    /// The age of the oldest backup snapshot in seconds, or `None` without a backup session.
    async fn snapshot_age(&self) -> Option<f64> {
        match self {
            Fixture::Postgresql(fixture) => {
                let query = format!(
                    "SELECT extract(epoch FROM max(now() - xact_start))::float8
                     FROM pg_stat_activity WHERE {}",
                    postgresql::BACKUP_SESSIONS
                );
                let client = fixture.client().await;
                client.query_one(&query, &[]).await.unwrap().get(0)
            }
            Fixture::Mysql(fixture) => {
                // information_schema.innodb_trx fails on MySQL 9 while another session runs a
                // statement with 4-byte characters. TIMER_WAIT of an active transaction is its
                // age in picoseconds.
                let age: Option<Option<f64>> = fixture
                    .client()
                    .await
                    .exec_first(
                        "SELECT CAST(MAX(x.TIMER_WAIT) / 1e12 AS DOUBLE)
                         FROM performance_schema.events_transactions_current x
                         JOIN performance_schema.threads t USING (THREAD_ID)
                         JOIN performance_schema.user_variables_by_thread v USING (THREAD_ID)
                         WHERE x.STATE = 'ACTIVE' AND v.VARIABLE_NAME = 'svalbard_run'
                           AND t.PROCESSLIST_USER = ?",
                        (&fixture.database,),
                    )
                    .await
                    .unwrap();
                age.flatten()
            }
        }
    }

    /// Ends the control connection of the backup account, and refuses new connections of it.
    async fn lock_out(&self) {
        let account = self.database();
        match self {
            Fixture::Postgresql(fixture) => {
                let statement = format!(
                    "ALTER ROLE {account} NOLOGIN;
                     SELECT pg_terminate_backend(pid) FROM pg_stat_activity
                     WHERE application_name = 'svalbard' AND usename = '{account}'"
                );
                fixture
                    .client()
                    .await
                    .batch_execute(&statement)
                    .await
                    .unwrap();
            }
            Fixture::Mysql(fixture) => {
                let mut client = fixture.client().await;
                client
                    .query_drop(format!("ALTER USER {account} ACCOUNT LOCK"))
                    .await
                    .unwrap();
                // The control connection is the session of the account without a run ID.
                let control: Vec<u64> = client
                    .exec(
                        "SELECT t.PROCESSLIST_ID FROM performance_schema.threads t
                         WHERE t.PROCESSLIST_USER = ? AND NOT EXISTS (
                           SELECT 1 FROM performance_schema.user_variables_by_thread v
                           WHERE v.THREAD_ID = t.THREAD_ID AND v.VARIABLE_NAME = 'svalbard_run')",
                        (account,),
                    )
                    .await
                    .unwrap();
                assert!(!control.is_empty(), "no control connection to end");
                for id in control {
                    client.query_drop(format!("KILL {id}")).await.unwrap();
                }
            }
        }
    }

    async fn let_in(&self) {
        let account = self.database();
        match self {
            Fixture::Postgresql(fixture) => fixture
                .client()
                .await
                .batch_execute(&format!("ALTER ROLE {account} LOGIN"))
                .await
                .unwrap(),
            Fixture::Mysql(fixture) => fixture
                .client()
                .await
                .query_drop(format!("ALTER USER {account} ACCOUNT UNLOCK"))
                .await
                .unwrap(),
        }
    }
}

/// Polls `done` every 100 ms until it is true. Returns the time it took, and fails the test
/// after `limit`.
async fn until(limit: Duration, mut done: impl AsyncFnMut() -> bool) -> Duration {
    let started = Instant::now();
    while !done().await {
        assert!(
            started.elapsed() < limit,
            "the condition did not hold within {limit:?}"
        );
        time::sleep(Duration::from_millis(100)).await;
    }
    started.elapsed()
}

/// `source` through `proxy`. The test certificate is valid for 127.0.0.1.
fn via(mut source: Source, proxy: &Proxy) -> String {
    source.host = "127.0.0.1".to_owned();
    source.port = proxy.address.port();
    cli::source(&source)
}

fn signal(child: &Child, signal: Signal) {
    let pid = Pid::from_raw(child.id().unwrap() as i32).unwrap();
    kill_process(pid, signal).unwrap();
}

fn empty(dir: &Path) -> bool {
    fs::read_dir(dir).unwrap().next().is_none()
}

/// The paths below `dir`, relative to it, and then removes them.
fn take_leftovers(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if path.is_dir() {
            for inner in fs::read_dir(&path).unwrap() {
                let inner = inner.unwrap().file_name();
                found.push(format!("{name}/{}", inner.to_string_lossy()));
            }
            fs::remove_dir_all(&path).unwrap();
        } else {
            found.push(name);
            fs::remove_file(&path).unwrap();
        }
    }
    found.sort();
    found
}

/// The leftovers without the random run ID.
fn shapes(leftovers: &[String]) -> String {
    let shapes: Vec<_> = leftovers
        .iter()
        .map(|path| match path.split_once('/') {
            Some((_, file)) => format!("svalbard-<run>/{file}"),
            None => path.clone(),
        })
        .collect();
    shapes.join(", ")
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn mysql_schema_change_waiting_for_the_backup_stops_it() {
    let mut fixture = mysql::Fixture::create("sc_contention").await;
    fixture.add_larger().await;
    // mysqldump is in the middle of `larger`, the only table of more than 1 MiB.
    let fault = Fault::PauseResponses {
        connection: 2,
        after: 1 << 20,
    };
    let proxy = proxy(&fixture.upstream(), fault).await;
    let bucket = floci::bucket("sc-contention", None).await;
    let source = via(fixture.source(mysql::verify_full()), &proxy);
    let setup = Setup::new("sc-contention", &source, &cli::floci(&bucket), "");
    let child = setup.spawn(mysql::PASSWORD);
    within(proxy.stalled.notified()).await;
    let mut admin = fixture.client().await;
    holds_larger(&fixture, &mut admin).await;
    let started = Instant::now();
    let alter = tokio::spawn(async move {
        admin
            .query_drop("ALTER TABLE larger ADD COLUMN contention INT")
            .await
    });
    let outcome = setup.finish(child, MYSQL_SECRETS).await;
    within(alter).await.unwrap().unwrap();
    let waited = started.elapsed();

    assert_eq!(outcome.code, Some(4), "{outcome:?}");
    assert_eq!(outcome.result["stage"], "dump", "{outcome:?}");
    assert_eq!(outcome.result["error"], "contention", "{outcome:?}");
    assert!(floci::objects(&bucket).await.is_empty());
    let version = mysql::server_version(&mut fixture.client().await).await;
    println!(
        "scenario: S2 mysql {version}: contention; the ALTER TABLE waited {:.1} s \
         (contention_timeout 2 s)",
        waited.as_secs_f64()
    );
}

/// Waits until a backup session of `fixture` holds its shared metadata lock on `larger`.
async fn holds_larger(fixture: &mysql::Fixture, client: &mut mysql_async::Conn) {
    until(Duration::from_secs(10), async || {
        let count: Option<i64> = client
            .exec_first(
                "SELECT COUNT(*) FROM performance_schema.metadata_locks l
                 JOIN performance_schema.user_variables_by_thread v
                   ON v.THREAD_ID = l.OWNER_THREAD_ID AND v.VARIABLE_NAME = 'svalbard_run'
                 WHERE l.OBJECT_SCHEMA = ? AND l.OBJECT_NAME = 'larger'
                   AND l.LOCK_STATUS = 'GRANTED' AND l.LOCK_TYPE = 'SHARED_READ'",
                (&fixture.database,),
            )
            .await
            .unwrap();
        count > Some(0)
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn dump_timeout_ends_the_snapshot_and_cleans_up() {
    tokio::join!(
        dump_timeout(Fixture::postgresql("sc_deadline_pg").await),
        dump_timeout(Fixture::mysql("sc_deadline_my").await)
    );
}

/// S5 with B5 and B6: the dump waits for a lock for longer than `dump_timeout`.
async fn dump_timeout(fixture: Fixture) {
    let name = format!("{}-deadline", fixture.name());
    // The lock wait would end only after the deadline.
    let safety = "dump_timeout = \"1min\"\nlock_timeout = \"5min\"\n";
    let (setup, bucket) = fixture.setup(&name, safety).await;
    let lock = fixture.hold_stall_lock().await;
    let child = setup.spawn(fixture.password());
    fixture.stalled(1).await;

    // The time when the oldest snapshot reaches the deadline, from the latest age seen.
    let (mut oldest, mut deadline) = (0.0_f64, None);
    until(Duration::from_secs(120), async || {
        match fixture.snapshot_age().await {
            Some(age) => {
                oldest = oldest.max(age);
                deadline = Some(Instant::now() + Duration::from_secs_f64((60.0 - age).max(0.0)));
                false
            }
            None => deadline.is_some() && empty(&setup.staging),
        }
    })
    .await;
    let cleaned = deadline.unwrap().elapsed();
    let outcome = setup.finish(child, fixture.secrets()).await;
    lock.release().await;

    assert_eq!(outcome.code, Some(4), "{outcome:?}");
    assert_eq!(outcome.result["error"], "dump_timeout", "{outcome:?}");
    assert!(floci::objects(&bucket).await.is_empty());
    assert!(oldest <= 75.0, "B5: the snapshot was {oldest:.1} s old");
    assert!(cleaned <= Duration::from_secs(15), "B6: {cleaned:?}");
    println!(
        "scenario: S5 {} {}: dump_timeout; snapshot age at most {oldest:.1} s (B5: 75 s); \
         no backup session and no staged file {:.1} s after the deadline (B6: 15 s)",
        fixture.name(),
        fixture.version().await,
        cleaned.as_secs_f64()
    );
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn signals_during_preflight_and_dump_cancel_and_clean_up() {
    tokio::join!(
        signals(Fixture::postgresql("sc_signals_pg").await),
        signals(Fixture::mysql("sc_signals_my").await)
    );
}

/// S7 with B6: SIGINT and SIGTERM while preflight waits for the server, and while the dump
/// waits for a lock.
async fn signals(fixture: Fixture) {
    let mut evidence = Vec::new();
    for (stage, signal_name, sent) in [
        ("preflight", "SIGINT", Signal::INT),
        ("preflight", "SIGTERM", Signal::TERM),
        ("dump", "SIGINT", Signal::INT),
        ("dump", "SIGTERM", Signal::TERM),
    ] {
        let name = format!("{}-{stage}-{signal_name}", fixture.name()).to_lowercase();
        let (setup, bucket, child, lock) = if stage == "preflight" {
            // The server never answers the control connection.
            let proxy = proxy(&fixture.upstream(), Fault::HoldFirstResponse).await;
            let bucket = floci::bucket(&name, None).await;
            let source = via(fixture.source(), &proxy);
            let setup = Setup::new(&name, &source, &cli::floci(&bucket), "");
            let child = setup.spawn(fixture.password());
            within(proxy.stalled.notified()).await;
            (setup, bucket, child, None)
        } else {
            let (setup, bucket) = fixture.setup(&name, "lock_timeout = \"5min\"\n").await;
            let lock = fixture.hold_stall_lock().await;
            let child = setup.spawn(fixture.password());
            fixture.stalled(1).await;
            (setup, bucket, child, Some(lock))
        };
        let started = Instant::now();
        signal(&child, sent);
        let outcome = setup.finish(child, fixture.secrets()).await;
        let elapsed = started.elapsed();
        if let Some(lock) = lock {
            lock.release().await;
        }

        assert_eq!(outcome.code, Some(4), "{name}: {outcome:?}");
        assert_eq!(outcome.result["error"], "cancelled", "{name}: {outcome:?}");
        assert_eq!(fixture.sessions().await, 0, "{name}");
        assert!(floci::objects(&bucket).await.is_empty(), "{name}");
        assert!(
            elapsed <= Duration::from_secs(15),
            "B6: {name}: {elapsed:?}"
        );
        evidence.push(format!(
            "{signal_name} in {stage} {:.2} s",
            elapsed.as_secs_f64()
        ));
    }
    println!(
        "scenario: S7 {} {}: cancelled and cleaned up after {} (B6: 15 s)",
        fixture.name(),
        fixture.version().await,
        evidence.join(", ")
    );
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn silent_server_fails_preflight() {
    tokio::join!(
        silent(Fixture::postgresql("sc_silent_pg").await),
        silent(Fixture::mysql("sc_silent_my").await)
    );
}

/// A server that accepts the connection and never answers fails preflight on its own.
async fn silent(fixture: Fixture) {
    let name = format!("{}-silent", fixture.name());
    let proxy = proxy(&fixture.upstream(), Fault::HoldFirstResponse).await;
    let bucket = floci::bucket(&name, None).await;
    let setup = Setup::new(
        &name,
        &via(fixture.source(), &proxy),
        &cli::floci(&bucket),
        "",
    );
    let started = Instant::now();
    let outcome = setup.run(fixture.password(), fixture.secrets()).await;
    let elapsed = started.elapsed();

    assert_eq!(outcome.code, Some(4), "{outcome:?}");
    assert_eq!(outcome.result["error"], "preflight", "{outcome:?}");
    let message = outcome.result["message"].as_str().unwrap();
    assert!(message.contains("the connection timed out"), "{outcome:?}");
    assert!(elapsed < Duration::from_secs(20), "{elapsed:?}");
    println!(
        "scenario: S7 {} {}: a silent server fails preflight after {:.1} s",
        fixture.name(),
        fixture.version().await,
        elapsed.as_secs_f64()
    );
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn sigkill_leaves_what_the_backstops_remove() {
    tokio::join!(
        sigkill(Fixture::postgresql("sc_kill_pg").await),
        sigkill(Fixture::mysql("sc_kill_my").await)
    );
}

/// S8: SIGKILL to Svalbard during a lock wait, the same with the dump client killed too as on
/// the loss of the host, and during the transfer. Records how long the backup sessions stay.
async fn sigkill(fixture: Fixture) {
    // The server-side limit of the lock wait: `lock_timeout` + 10 s. On PostgreSQL, the
    // longer value leaves `client_connection_check_interval` (10 s) to end the session.
    let (safety, limit) = match fixture {
        Fixture::Postgresql(_) => ("lock_timeout = \"30s\"\n", 15),
        Fixture::Mysql(_) => ("lock_timeout = \"5s\"\n", 20),
    };
    let mut evidence = Vec::new();
    for host_lost in [false, true] {
        let name = format!("{}-kill-{host_lost}", fixture.name());
        let (setup, _) = fixture.setup(&name, safety).await;
        let lock = fixture.hold_stall_lock().await;
        let mut child = setup.spawn(fixture.password());
        fixture.stalled(1).await;
        if host_lost {
            for client in children(child.id().unwrap()) {
                // The client runs in its own process group.
                let _ = kill_process_group(Pid::from_raw(client as i32).unwrap(), Signal::KILL);
            }
        }
        child.kill().await.unwrap();
        let ended = until(Duration::from_secs(60), async || {
            fixture.sessions().await == 0
        })
        .await;
        lock.release().await;
        let leftovers = take_leftovers(&setup.staging);

        assert!(ended <= Duration::from_secs(limit), "{name}: {ended:?}");
        let case = if host_lost {
            "with the client killed too"
        } else {
            "during a lock wait"
        };
        evidence.push(format!(
            "{case}, the backup session ended after {:.1} s and {} remained",
            ended.as_secs_f64(),
            shapes(&leftovers)
        ));
    }

    // During the transfer the snapshot is closed already. The SFTP upload never finishes.
    let name = format!("{}-kill-transfer", fixture.name());
    let dir = ssh::directory("/srv/backups", &name);
    let proxy = proxy(&ssh::upstream(), Fault::StallAfter(64 << 10)).await;
    let destination =
        cli::ssh_destination("sftp", "127.0.0.1", proxy.address.port(), &dir, "client");
    let setup = Setup::new(&name, &cli::source(&fixture.source()), &destination, "");
    let mut child = setup.spawn(fixture.password());
    within(proxy.stalled.notified()).await;
    let sessions = fixture.sessions().await;
    child.kill().await.unwrap();
    let leftovers = take_leftovers(&setup.staging);
    let remote = ssh::files(&dir);
    ssh::shell(&format!("rm -f {}/.*.partial", ssh::quote(&dir)));

    assert_eq!(
        sessions, 0,
        "the transfer started with a backup session open"
    );
    assert!(
        remote.len() == 1 && remote[0].ends_with(".partial"),
        "{remote:?}"
    );
    evidence.push(format!(
        "during the transfer, no backup session was open, and {} and the SFTP file \
         .<name>.<random>.partial remained",
        shapes(&leftovers)
    ));
    println!(
        "scenario: S8 {} {}: {}",
        fixture.name(),
        fixture.version().await,
        evidence.join("; ")
    );
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn mysql_without_performance_schema_fails_before_the_dump() {
    let fixture = mysql::Fixture::create_at(mysql::nops(), "sc_nops").await;
    let bucket = floci::bucket("sc-nops", None).await;
    let setup = Setup::new(
        "sc-nops",
        &cli::mysql_source(&fixture),
        &cli::floci(&bucket),
        "",
    );
    let outcome = setup.run(mysql::PASSWORD, MYSQL_SECRETS).await;

    assert_eq!(outcome.code, Some(4), "{outcome:?}");
    assert_eq!(outcome.result["stage"], "dump", "{outcome:?}");
    assert_eq!(
        outcome.result["error"], "detection_unavailable",
        "{outcome:?}"
    );
    assert!(floci::objects(&bucket).await.is_empty());
    let version = mysql::server_version(&mut fixture.client().await).await;
    println!("scenario: S9 mysql {version} with performance_schema off: detection_unavailable");
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn cancelling_one_run_leaves_another_run() {
    tokio::join!(
        two_runs(Fixture::postgresql("sc_two_pg").await),
        two_runs(Fixture::mysql("sc_two_my").await)
    );
}

/// S11: two runs on one database. Cancelling one leaves the session of the other.
async fn two_runs(fixture: Fixture) {
    let bucket = floci::bucket(&format!("{}-two", fixture.name()), None).await;
    let setup = |run: &str| {
        Setup::new(
            &format!("{}-two-{run}", fixture.name()),
            &cli::source(&fixture.source()),
            &cli::floci(&bucket),
            "lock_timeout = \"1min\"\n",
        )
    };
    let (first, second) = (setup("a"), setup("b"));
    let lock = fixture.hold_stall_lock().await;
    let (cancelled, kept) = (
        first.spawn(fixture.password()),
        second.spawn(fixture.password()),
    );
    fixture.stalled(2).await;
    signal(&cancelled, Signal::TERM);
    let cancelled = first.finish(cancelled, fixture.secrets()).await;
    let waiting = fixture.waiting().await;
    lock.release().await;
    let kept = second.finish(kept, fixture.secrets()).await;

    assert_eq!(cancelled.result["error"], "cancelled", "{cancelled:?}");
    assert_eq!(waiting, 1, "the other run must still wait");
    assert_eq!(kept.code, Some(0), "{kept:?}");
    let mut stored = floci::objects(&bucket).await;
    stored.sort();
    let mut expected = vec![
        kept.result["data"].as_str().unwrap().to_owned(),
        kept.result["record"].as_str().unwrap().to_owned(),
    ];
    expected.sort();
    assert_eq!(stored, expected);
    println!(
        "scenario: S11 {} {}: SIGTERM cancelled one run, and the other run completed",
        fixture.name(),
        fixture.version().await
    );
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn replay_waiting_on_a_postgresql_standby_stops_the_backup() {
    let mut fixture = postgresql::Fixture::create("sc_replay").await;
    fixture.add_larger().await;
    let standby = fixture.on(&postgresql::standby());
    let observer = standby.client().await;
    postgresql::caught_up(&fixture.client().await, &observer, Duration::from_secs(30)).await;
    let settings = postgresql::connect(&postgresql::standby()).await;
    let mut evidence = Vec::new();
    for (index, delay) in ["-1", "30s"].into_iter().enumerate() {
        // ALTER SYSTEM cannot run in the implicit transaction of a batch.
        let statement = format!("ALTER SYSTEM SET max_standby_streaming_delay = '{delay}'");
        settings.batch_execute(&statement).await.unwrap();
        settings
            .batch_execute("SELECT pg_reload_conf()")
            .await
            .unwrap();
        until(Duration::from_secs(10), async || {
            let row = settings
                .query_one("SHOW max_standby_streaming_delay", &[])
                .await
                .unwrap();
            row.get::<_, String>(0) == delay
        })
        .await;

        // pg_dump on the standby is in the middle of `larger` and holds all its table locks.
        let fault = Fault::PauseResponses {
            connection: 2,
            after: 1 << 20,
        };
        let proxy = proxy(&standby.upstream(), fault).await;
        let name = format!("sc-replay-{index}");
        let bucket = floci::bucket(&name, None).await;
        let source = via(standby.source(postgresql::verify_full()), &proxy);
        let setup = Setup::new(&name, &source, &cli::floci(&bucket), "");
        let child = setup.spawn(postgresql::PASSWORD);
        within(proxy.stalled.notified()).await;
        let locked = format!(
            "SELECT EXISTS (SELECT FROM pg_locks l JOIN pg_stat_activity a USING (pid)
             WHERE l.granted AND l.relation = 'accounts'::regclass AND {})",
            postgresql::BACKUP_SESSIONS
        );
        until(Duration::from_secs(10), async || {
            observer.query_one(&locked, &[]).await.unwrap().get(0)
        })
        .await;

        let column = format!("replayed_{index}");
        fixture
            .client()
            .await
            .batch_execute(&format!("ALTER TABLE accounts ADD COLUMN {column} int"))
            .await
            .unwrap();
        let replayed = format!(
            "SELECT EXISTS (SELECT FROM information_schema.columns
             WHERE table_name = 'accounts' AND column_name = '{column}')"
        );
        let (outcome, delayed) = tokio::join!(
            setup.finish(child, POSTGRESQL_SECRETS),
            until(Duration::from_secs(60), async || {
                observer.query_one(&replayed, &[]).await.unwrap().get(0)
            })
        );

        assert_eq!(outcome.code, Some(4), "{delay}: {outcome:?}");
        assert_eq!(
            outcome.result["error"], "contention",
            "{delay}: {outcome:?}"
        );
        assert!(floci::objects(&bucket).await.is_empty());
        evidence.push(format!(
            "max_standby_streaming_delay {delay}: contention, and the ALTER TABLE was replayed \
             {:.1} s after its commit",
            delayed.as_secs_f64()
        ));
    }
    settings
        .batch_execute("ALTER SYSTEM RESET max_standby_streaming_delay")
        .await
        .unwrap();
    settings
        .batch_execute("SELECT pg_reload_conf()")
        .await
        .unwrap();
    println!(
        "scenario: S12 postgresql {} standby: {}",
        postgresql::server_version(&observer).await,
        evidence.join("; ")
    );
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn replicated_schema_change_on_a_mysql_replica_stops_the_backup() {
    let mut fixture = mysql::Fixture::create("sc_applier").await;
    fixture.add_larger().await;
    let replica = fixture.on(mysql::replica());
    let mut observer = replica.client().await;
    mysql::caught_up(
        &mut fixture.client().await,
        &mut observer,
        Duration::from_secs(30),
    )
    .await;
    let fault = Fault::PauseResponses {
        connection: 2,
        after: 1 << 20,
    };
    let proxy = proxy(&replica.upstream(), fault).await;
    let bucket = floci::bucket("sc-applier", None).await;
    let source = via(replica.source(mysql::verify_full()), &proxy);
    let setup = Setup::new("sc-applier", &source, &cli::floci(&bucket), "");
    let child = setup.spawn(mysql::PASSWORD);
    within(proxy.stalled.notified()).await;
    holds_larger(&replica, &mut observer).await;

    fixture
        .client()
        .await
        .query_drop("ALTER TABLE larger ADD COLUMN applied INT")
        .await
        .unwrap();
    let (outcome, delayed) = tokio::join!(
        setup.finish(child, MYSQL_SECRETS),
        until(Duration::from_secs(60), async || {
            let count: Option<i64> = observer
                .exec_first(
                    "SELECT COUNT(*) FROM information_schema.COLUMNS
                     WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'larger' AND COLUMN_NAME = 'applied'",
                    (&replica.database,),
                )
                .await
                .unwrap();
            count > Some(0)
        })
    );

    assert_eq!(outcome.code, Some(4), "{outcome:?}");
    assert_eq!(outcome.result["error"], "contention", "{outcome:?}");
    assert!(floci::objects(&bucket).await.is_empty());
    println!(
        "scenario: S12 mysql {} replica: contention, and the replicated ALTER TABLE was applied \
         {:.1} s after its commit",
        mysql::server_version(&mut observer).await,
        delayed.as_secs_f64()
    );
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn lost_control_connection_stops_the_backup() {
    tokio::join!(
        lost_control(Fixture::postgresql("sc_control_pg").await),
        lost_control(Fixture::mysql("sc_control_my").await)
    );
}

/// S13 with B6: the control connection breaks and cannot reconnect until the dump client has
/// stopped.
async fn lost_control(fixture: Fixture) {
    let name = format!("{}-control", fixture.name());
    let (setup, bucket) = fixture.setup(&name, "lock_timeout = \"5min\"\n").await;
    let lock = fixture.hold_stall_lock().await;
    let child = setup.spawn(fixture.password());
    fixture.stalled(1).await;
    fixture.lock_out().await;
    let broken = Instant::now();
    let svalbard = child.id().unwrap();
    until(Duration::from_secs(30), async || {
        children(svalbard).is_empty()
    })
    .await;
    fixture.let_in().await;
    until(Duration::from_secs(30), async || {
        fixture.sessions().await == 0 && empty(&setup.staging)
    })
    .await;
    let cleaned = broken.elapsed();
    let outcome = setup.finish(child, fixture.secrets()).await;
    lock.release().await;

    assert_eq!(outcome.code, Some(4), "{outcome:?}");
    assert_eq!(outcome.result["error"], "detection_lost", "{outcome:?}");
    assert!(floci::objects(&bucket).await.is_empty());
    assert!(cleaned <= Duration::from_secs(15), "B6: {cleaned:?}");
    println!(
        "scenario: S13 {} {}: detection_lost; no backup session and no staged file {:.1} s \
         after the control connection broke (B6: 15 s)",
        fixture.name(),
        fixture.version().await,
        cleaned.as_secs_f64()
    );
}
