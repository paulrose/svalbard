//! Tests against a MySQL server with TLS. Run them in Docker with `scripts/test-integration.sh`.

mod common;

use std::{
    env, fs,
    future::{self, Future},
    path::{Path, PathBuf},
    time::Duration,
};

use common::mysql::*;
use mysql_async::prelude::Queryable;
use svalbard::{
    config::{Password, Safety, Source, Tls},
    db::{self, Dump, Error, Failure, Mysql, Source as _, Wait},
    staging::Run,
};

fn safety() -> Safety {
    Safety {
        lock_timeout: Duration::from_secs(30),
        contention_timeout: Duration::from_secs(1),
        dump_timeout: Duration::from_secs(60),
        max_dump_size: 1 << 30,
        min_free_space: 0,
        staging_dir: PathBuf::from(env!("CARGO_TARGET_TMPDIR")),
    }
}

/// Runs a dump. Returns the result and the run directory.
async fn dump(
    source: Source,
    safety: &Safety,
    cancel: impl Future<Output = ()>,
) -> (Result<Dump, Error>, Run) {
    let run = Run::create(Path::new(env!("CARGO_TARGET_TMPDIR"))).expect("run directory");
    let result = match Mysql::new(source) {
        Ok(mut mysql) => db::dump(&mut mysql, safety, &run, cancel).await,
        Err(error) => Err(error),
    };
    (result, run)
}

/// Runs a dump that must fail, and checks that it leaves no files.
async fn failure(source: Source, safety: &Safety, cancel: impl Future<Output = ()>) -> Error {
    let (result, run) = dump(source, safety, cancel).await;
    let error = result.expect_err("the dump must fail");
    assert_eq!(fs::read_dir(&run.dir).unwrap().count(), 0, "{error}");
    run.remove().unwrap();
    error
}

#[tokio::test]
#[ignore = "needs MySQL: run scripts/test-integration.sh"]
async fn dump_restores_with_the_same_schema_and_data() {
    let _shared = SERVER.read().await;
    let fixture = Fixture::create("restore").await;
    let mut source = fixture.client().await;
    let mut writer = fixture.client().await;
    let writes = tokio::spawn(async move {
        loop {
            writer
                .query_drop("INSERT INTO events () VALUES ()")
                .await
                .unwrap();
        }
    });

    for tls in [verify_full(), Tls::Disable] {
        let (result, run) = dump(fixture.source(tls), &safety(), future::pending()).await;
        let dump = result.expect("the dump must succeed");
        let mut restored = fixture.restore(&dump.path).await;

        assert_eq!(dump.format, "mysqldump-sql");
        assert!(dump.client_version.starts_with("mysqldump  Ver "));
        assert_eq!(
            fingerprint(&mut restored).await,
            fingerprint(&mut source).await
        );
        // The rows of the concurrent writes form a prefix, because the dump uses one snapshot.
        let (count, last): (u64, u64) = restored
            .query_first("SELECT COUNT(*), IFNULL(MAX(id), 0) FROM events")
            .await
            .unwrap()
            .unwrap();
        assert!(count >= 10 && count == last, "{count} rows, last ID {last}");
        assert!(fixture.backup_sessions(&mut source).await.is_empty());
        run.remove().unwrap();
    }
    writes.abort();
}

#[tokio::test]
#[ignore = "needs MySQL: run scripts/test-integration.sh"]
async fn lock_held_at_the_start_fails_with_lock_timeout() {
    let _shared = SERVER.read().await;
    let fixture = Fixture::create("lock_timeout").await;
    let _holder = fixture.hold_stall_lock().await;
    let short = Safety {
        lock_timeout: Duration::from_secs(2),
        ..safety()
    };
    let error = failure(fixture.source(verify_full()), &short, future::pending()).await;

    assert_eq!(error.kind, Failure::LockTimeout, "{error}");
    assert!(error.message.contains("stall"), "{error}");
    let mut client = fixture.client().await;
    assert!(fixture.backup_sessions(&mut client).await.is_empty());
}

#[tokio::test]
#[ignore = "needs MySQL: run scripts/test-integration.sh"]
async fn cancellation_ends_only_the_backup_session() {
    let _shared = SERVER.read().await;
    let fixture = Fixture::create("cancel").await;
    let _holder = fixture.hold_stall_lock().await;
    // Sessions of the same account: one without a run ID, and one of another run.
    let mut plain = fixture.account().await;
    let mut other = fixture.account().await;
    other
        .query_drop("SET @svalbard_run = 'another-run'")
        .await
        .unwrap();
    let mut observer = fixture.client().await;
    let cancel = async {
        fixture.stalled(&mut observer).await;
        let types = fixture.backup_sessions(&mut observer).await;
        assert_eq!(types, ["SSL/TLS"], "mysqldump uses TLS");
    };
    let error = failure(fixture.source(verify_full()), &safety(), cancel).await;

    assert_eq!(error.kind, Failure::Cancelled, "{error}");
    assert!(fixture.backup_sessions(&mut observer).await.is_empty());
    plain
        .query_drop("DO 1")
        .await
        .expect("a plain session remains");
    other.query_drop("DO 1").await.expect("another run remains");
}

#[tokio::test]
#[ignore = "needs MySQL: run scripts/test-integration.sh"]
async fn observe_reports_waits_and_terminate_ends_only_the_run() {
    // The FLUSH TABLES below waits for every open table on the server.
    let _exclusive = SERVER.write().await;
    let fixture = Fixture::create("observe").await;
    let mut mysql = Mysql::new(fixture.source(Tls::Disable)).unwrap();
    let observe = async |mysql: &mut Mysql, expected: &Wait| {
        for _ in 0..50 {
            let observation = mysql.observe("run").await.unwrap();
            if observation.waits.contains(expected) {
                return observation;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("detection did not report {expected:?}");
    };

    // A backup session that holds a metadata lock in its snapshot, like mysqldump on a table.
    let mut backup = fixture.account().await;
    backup
        .query_drop("SET @svalbard_run = 'run'; START TRANSACTION; SELECT * FROM accounts LIMIT 1")
        .await
        .unwrap();
    let mut other_run = fixture.account().await;
    other_run
        .query_drop("SET @svalbard_run = 'other'")
        .await
        .unwrap();
    let mut schema_change = fixture.client().await;
    let alter_id = schema_change.id();
    let alter = tokio::spawn(async move {
        schema_change
            .query_drop("ALTER TABLE accounts ADD COLUMN extra INT")
            .await
    });
    let blocked = Wait::Blocked(format!("connection {alter_id}"));
    let observation = observe(&mut mysql, &blocked).await;
    assert_eq!(observation.sessions, [backup.id().to_string()]);

    // A backup session that waits for a lock.
    let holder = fixture.hold_stall_lock().await;
    let mut waiting = fixture.account().await;
    let waiting_id = waiting.id();
    let wait = tokio::spawn(async move {
        waiting
            .query_drop("SET @svalbard_run = 'run'; SELECT * FROM stall")
            .await
    });
    let lock = format!("SHARED_READ on TABLE {}.stall", fixture.database);
    observe(&mut mysql, &Wait::Backup(lock)).await;

    // A flush that waits for a table in use. No metadata lock shows it.
    let mut reader = fixture.client().await;
    let reader_id = reader.id();
    let read = tokio::spawn(async move { reader.query_drop("SELECT SLEEP(5) FROM zz_last").await });
    let mut client = fixture.client().await;
    let mut sleeping = false;
    for _ in 0..100 {
        let state: Option<String> = client
            .exec_first(
                "SELECT PROCESSLIST_STATE FROM performance_schema.threads WHERE PROCESSLIST_ID = ?",
                (reader_id,),
            )
            .await
            .unwrap();
        sleeping = state.as_deref() == Some("User sleep");
        if sleeping {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(sleeping, "the SELECT must run before the flush");
    let mut flush = fixture.client().await;
    let flush_id = flush.id();
    let flushed = tokio::spawn(async move { flush.query_drop("FLUSH TABLES").await });
    observe(&mut mysql, &Wait::Blocked(format!("connection {flush_id}"))).await;

    mysql.terminate("run").await.unwrap();
    for _ in 0..50 {
        if mysql.observe("run").await.unwrap().sessions.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(mysql.observe("run").await.unwrap().sessions.is_empty());
    assert!(
        backup.query_drop("DO 1").await.is_err(),
        "the run's session ended"
    );
    assert!(
        wait.await.unwrap().is_err(),
        "connection {waiting_id} ended"
    );
    other_run
        .query_drop("DO 1")
        .await
        .expect("another run remains");
    // The flush also waits for the table that LOCK TABLES holds open.
    drop(holder);
    alter.await.unwrap().expect("the schema change completes");
    read.await.unwrap().unwrap();
    flushed.await.unwrap().unwrap();
}

#[tokio::test]
#[ignore = "needs MySQL: run scripts/test-integration.sh"]
async fn schema_change_during_the_dump_fails() {
    let _shared = SERVER.read().await;
    let fixture = Fixture::create("schema_change").await;
    let safety = safety();
    // Tables after `stall` in name order, which mysqldump has not read when it stalls.
    for (statement, kind) in [
        ("CREATE TABLE zz_new (id INT)", Failure::SchemaChanged),
        ("ALTER TABLE zz_last ADD COLUMN extra INT", Failure::Client),
        ("TRUNCATE TABLE zz_last", Failure::Client),
        ("RENAME TABLE zz_last TO zz_renamed", Failure::Client),
    ] {
        let mut holder = fixture.hold_stall_lock().await;
        let mut client = fixture.client().await;
        let change = async {
            fixture.stalled(&mut client).await;
            client.query_drop(statement).await.unwrap();
            holder.query_drop("UNLOCK TABLES").await.unwrap();
            future::pending::<()>().await;
        };
        let error = tokio::select! {
            error = failure(fixture.source(Tls::Disable), &safety, future::pending()) => error,
            () = change => unreachable!(),
        };

        assert_eq!(error.kind, kind, "{statement}: {error}");
        if kind == Failure::SchemaChanged {
            assert!(error.message.contains("zz_"), "{error}");
        }
    }
}

#[tokio::test]
#[ignore = "needs MySQL: run scripts/test-integration.sh"]
async fn unsupported_tables_and_missing_privileges_fail_before_the_dump() {
    let _shared = SERVER.read().await;
    let fixture = Fixture::create("privileges").await;
    let mut client = fixture.client().await;
    let version: String = client
        .query_first("SELECT VERSION()")
        .await
        .unwrap()
        .unwrap();
    let client_version = format!("mysqldump  Ver {version} for Linux");
    let user = &fixture.database;

    for (change, message) in [
        (
            "CREATE TABLE legacy (id INT) ENGINE = MyISAM",
            Some("legacy (MyISAM)"),
        ),
        ("DROP TABLE legacy", None),
        (
            &*format!("REVOKE TRIGGER ON {user}.* FROM {user}"),
            Some("TRIGGER on"),
        ),
        (
            &*format!(
                "REVOKE SELECT ON {user}.* FROM {user}; GRANT TRIGGER, SELECT ON {user}.accounts TO {user}"
            ),
            Some("SELECT on"),
        ),
        // Privileges through a default role count.
        (
            &*format!(
                "DROP ROLE IF EXISTS {user}_role; CREATE ROLE {user}_role;
                 GRANT SELECT, TRIGGER ON {user}.* TO {user}_role;
                 GRANT {user}_role TO {user}; SET DEFAULT ROLE {user}_role TO {user}"
            ),
            None,
        ),
    ] {
        client.query_drop(change).await.unwrap();
        let mut mysql = Mysql::new(fixture.source(Tls::Disable)).unwrap();
        let result = mysql.preflight(&client_version).await;

        match message {
            None => assert_eq!(result, Ok(version.clone()), "{change}"),
            Some(message) => {
                let error = result.expect_err(change);
                assert!(error.contains(message), "{change}: {error}");
            }
        }
    }

    client
        .query_drop(format!(
            "REVOKE SELECT ON performance_schema.metadata_locks FROM {user}"
        ))
        .await
        .unwrap();
    let error = failure(fixture.source(Tls::Disable), &safety(), future::pending()).await;
    assert_eq!(error.kind, Failure::DetectionUnavailable, "{error}");
}

#[tokio::test]
#[ignore = "needs MySQL: run scripts/test-integration.sh"]
async fn excluded_table_is_left_out_and_the_rest_restores() {
    let _shared = SERVER.read().await;
    let fixture = Fixture::create("exclude").await;
    let mut source = fixture.client().await;
    source
        .query_drop(
            "CREATE TABLE cache (id INT PRIMARY KEY, body TEXT) ENGINE = MyISAM;
             INSERT INTO cache VALUES (1, 'a'), (2, 'b')",
        )
        .await
        .unwrap();
    // A schema change on an excluded table during the dump does not fail the run.
    let mut holder = fixture.hold_stall_lock().await;
    let mut client = fixture.client().await;
    let change = async {
        fixture.stalled(&mut client).await;
        client
            .query_drop("ALTER TABLE cache ADD COLUMN extra INT")
            .await
            .unwrap();
        holder.query_drop("UNLOCK TABLES").await.unwrap();
        future::pending::<()>().await;
    };
    let excluded = Source {
        exclude_tables: vec!["cache".to_owned()],
        ..fixture.source(Tls::Disable)
    };

    let safety = safety();
    let (result, run) = tokio::select! {
        result = dump(excluded, &safety, future::pending()) => result,
        () = change => unreachable!(),
    };
    let dump = result.expect("the dump must succeed");
    let mut restored = fixture.restore(&dump.path).await;

    assert_eq!(dump.excluded_tables, ["cache"]);
    let cache: Option<u64> = restored
        .query_first(
            "SELECT COUNT(*) FROM information_schema.TABLES
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'cache'",
        )
        .await
        .unwrap();
    assert_eq!(cache, Some(0));
    source.query_drop("DROP TABLE cache").await.unwrap();
    assert_eq!(
        fingerprint(&mut restored).await,
        fingerprint(&mut source).await
    );
    run.remove().unwrap();
}

#[tokio::test]
#[ignore = "needs MySQL: run scripts/test-integration.sh"]
async fn exclusions_that_break_a_restore_fail_before_the_dump() {
    let _shared = SERVER.read().await;
    let fixture = Fixture::create("exclude_checks").await;
    let mut client = fixture.client().await;
    let version: String = client
        .query_first("SELECT VERSION()")
        .await
        .unwrap()
        .unwrap();
    let client_version = format!("mysqldump  Ver {version} for Linux");
    client
        .query_drop("CREATE TABLE Cache (id INT); CREATE TABLE cache (id INT)")
        .await
        .unwrap();

    for (tables, message) in [
        (&["missing", "Cache"][..], Some("names no table: missing")),
        (&["accounts"], Some("foreign key orders_ibfk_1 on orders")),
        (&["accounts"], Some("view balances")),
        // mysqldump would leave out `cache` too.
        (&["Cache"], Some("cache (like Cache)")),
        (&["Cache", "cache"], None),
    ] {
        let source = Source {
            exclude_tables: tables.iter().map(|table| table.to_string()).collect(),
            ..fixture.source(Tls::Disable)
        };
        let result = Mysql::new(source).unwrap().preflight(&client_version).await;

        match message {
            None => assert_eq!(result, Ok(version.clone()), "{tables:?}"),
            Some(message) => {
                let error = result.expect_err(message);
                assert!(error.contains(message), "{tables:?}: {error}");
            }
        }
    }
}

#[tokio::test]
#[ignore = "needs MySQL: run scripts/test-integration.sh"]
async fn connection_problems_fail_before_the_dump() {
    let _shared = SERVER.read().await;
    let fixture = Fixture::create("connection").await;
    let wrong = Path::new(env!("CARGO_TARGET_TMPDIR")).join("svalbard_mysql-wrong");
    fs::write(&wrong, "s3cr3t-wrong").unwrap();
    let nul = Path::new(env!("CARGO_TARGET_TMPDIR")).join("svalbard_mysql-nul");
    fs::write(&nul, "s3cr3t\0").unwrap();

    for (source, message) in [
        (
            Source {
                password: Password::File(wrong),
                ..fixture.source(verify_full())
            },
            "Access denied",
        ),
        (
            Source {
                password: Password::File(nul),
                ..fixture.source(verify_full())
            },
            "NUL",
        ),
        // The system CA bundle does not hold the test CA.
        (fixture.source(Tls::VerifyFull(None)), "certificate"),
    ] {
        let error = failure(source, &safety(), future::pending()).await;

        assert_eq!(error.kind, Failure::Preflight, "{error}");
        assert!(error.message.contains(message), "{error}");
        assert!(!error.message.contains("s3cr3t"), "{error}");
    }
}
