//! Tests against a PostgreSQL server with TLS. Run them in Docker with `scripts/test-integration.sh`.

mod common;

use std::{
    env, fs,
    future::{self, Future},
    path::{Path, PathBuf},
    time::Duration,
};

use common::postgresql::*;
use svalbard::{
    config::{Password, Safety, Source, Tls},
    db::{self, Dump, Error, Failure, Postgres},
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
    let result = match Postgres::new(source) {
        Ok(mut postgres) => db::dump(&mut postgres, safety, &run, cancel).await,
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
#[ignore = "needs PostgreSQL: run scripts/test-integration.sh"]
async fn dump_restores_with_the_same_schema_and_data() {
    let fixture = Fixture::create("restore").await;
    let source = fixture.client().await;
    let writer = fixture.client().await;
    let writes = tokio::spawn(async move {
        loop {
            writer
                .execute("INSERT INTO events DEFAULT VALUES", &[])
                .await
                .unwrap();
        }
    });

    for tls in [verify_full(), Tls::Disable] {
        let (result, run) = dump(fixture.source(tls), &safety(), future::pending()).await;
        let dump = result.expect("the dump must succeed");
        let restored = fixture.restore(&dump.path).await;

        assert_eq!(dump.format, "pg_dump-custom");
        assert!(dump.client_version.starts_with("pg_dump (PostgreSQL) "));
        assert_eq!(fingerprint(&restored).await, fingerprint(&source).await);
        // The rows of the concurrent writes form a prefix, because the dump uses one snapshot.
        let row = restored
            .query_one("SELECT count(*), coalesce(max(id), 0) FROM events", &[])
            .await
            .unwrap();
        let (count, last): (i64, i64) = (row.get(0), row.get(1));
        assert!(count >= 10 && count == last, "{count} rows, last ID {last}");
        assert_eq!(fixture.backup_sessions().await, 0);
        run.remove().unwrap();
    }
    writes.abort();
}

#[tokio::test]
#[ignore = "needs PostgreSQL: run scripts/test-integration.sh"]
async fn schema_change_waiting_for_the_backup_stops_it() {
    let fixture = Fixture::create("contention").await;
    let _holder = fixture.hold_stall_lock().await;
    let other = fixture.client().await;
    let schema_change = async {
        stalled(&other).await;
        other
            .batch_execute("ALTER TABLE accounts ADD COLUMN extra int")
            .await
    };
    let safety = safety();
    let (error, altered) = tokio::join!(
        failure(fixture.source(Tls::Disable), &safety, future::pending()),
        schema_change
    );

    assert_eq!(error.kind, Failure::Contention, "{error}");
    altered.expect("the schema change must run after the backup ends");
    assert_eq!(fixture.backup_sessions().await, 0);
}

#[tokio::test]
#[ignore = "needs PostgreSQL: run scripts/test-integration.sh"]
async fn lock_held_at_the_start_fails_with_lock_timeout() {
    let fixture = Fixture::create("lock_timeout").await;
    let _holder = fixture.hold_stall_lock().await;
    let short = Safety {
        lock_timeout: Duration::from_secs(2),
        ..safety()
    };
    let error = failure(fixture.source(verify_full()), &short, future::pending()).await;

    assert_eq!(error.kind, Failure::LockTimeout, "{error}");
    assert!(error.message.contains("stall"), "{error}");
    assert_eq!(fixture.backup_sessions().await, 0);
}

#[tokio::test]
#[ignore = "needs PostgreSQL: run scripts/test-integration.sh"]
async fn cancellation_ends_only_the_backup_session() {
    let fixture = Fixture::create("cancel").await;
    let _holder = fixture.hold_stall_lock().await;
    let mut role = fixture.admin.clone();
    role.user(&fixture.database)
        .password(PASSWORD)
        .application_name("other");
    let other = connect(&role).await;
    let observer = fixture.client().await;
    let cancel = async {
        stalled(&observer).await;
        let query = format!(
            "SELECT bool_and(ssl) FROM pg_stat_ssl JOIN pg_stat_activity USING (pid)
WHERE {BACKUP_SESSIONS}"
        );
        let row = observer.query_one(&query, &[]).await.unwrap();
        assert_eq!(
            row.get::<_, Option<bool>>(0),
            Some(true),
            "pg_dump uses TLS"
        );
    };
    let error = failure(fixture.source(verify_full()), &safety(), cancel).await;

    assert_eq!(error.kind, Failure::Cancelled, "{error}");
    assert_eq!(fixture.backup_sessions().await, 0);
    other
        .batch_execute("SELECT 1")
        .await
        .expect("another session of the role must remain");
}

#[tokio::test]
#[ignore = "needs PostgreSQL: run scripts/test-integration.sh"]
async fn unreadable_objects_fail_before_the_dump() {
    let fixture = Fixture::create("privileges").await;
    let admin = fixture.client().await;
    admin
        .batch_execute(
            "CREATE TABLE notes (body text); ALTER TABLE notes ENABLE ROW LEVEL SECURITY",
        )
        .await
        .unwrap();
    let error = failure(fixture.source(Tls::Disable), &safety(), future::pending()).await;

    assert_eq!(error.kind, Failure::Preflight, "{error}");
    assert!(error.message.contains("table public.notes"), "{error}");

    let role = &fixture.database;
    admin
        .batch_execute(&format!(
            "DROP TABLE notes; REVOKE pg_read_all_data FROM {role}; GRANT SELECT ON accounts TO {role}"
        ))
        .await
        .unwrap();
    let error = failure(fixture.source(Tls::Disable), &safety(), future::pending()).await;

    assert_eq!(error.kind, Failure::Preflight, "{error}");
    // The first five of the seven unreadable objects, in name order, and a count of the rest.
    assert!(error.message.contains("table public.blank"), "{error}");
    assert!(error.message.contains("and 2 more"), "{error}");
    assert!(!error.message.contains("table public.accounts"), "{error}");
}

#[tokio::test]
#[ignore = "needs PostgreSQL: run scripts/test-integration.sh"]
async fn excluded_table_is_left_out_and_the_rest_restores() {
    let fixture = Fixture::create("exclude").await;
    let admin = fixture.client().await;
    // Row-level security makes the table unreadable for the backup role.
    admin
        .batch_execute(
            r#"CREATE TABLE "Cache.Entries" (id serial PRIMARY KEY, body text);
            INSERT INTO "Cache.Entries" (body) SELECT md5(g::text) FROM generate_series(1, 100) g;
            ALTER TABLE "Cache.Entries" ENABLE ROW LEVEL SECURITY"#,
        )
        .await
        .unwrap();
    // pg_dump does not lock an excluded table, or read it for its serial sequence, so this lock
    // does not stop the dump.
    let holder = fixture.client().await;
    holder
        .batch_execute(r#"BEGIN; LOCK TABLE "Cache.Entries" IN ACCESS EXCLUSIVE MODE"#)
        .await
        .unwrap();
    let source = Source {
        exclude_tables: vec!["public.Cache.Entries".to_owned()],
        ..fixture.source(Tls::Disable)
    };

    let (result, run) = dump(source, &safety(), future::pending()).await;
    let dump = result.expect("the dump must succeed");
    holder.batch_execute("ROLLBACK").await.unwrap();
    let restored = fixture.restore(&dump.path).await;

    assert_eq!(dump.excluded_tables, ["public.Cache.Entries"]);
    let query = r#"SELECT to_regclass('public."Cache.Entries"') IS NULL,
      to_regclass('public."Cache.Entries_id_seq"') IS NULL"#;
    let row = restored.query_one(query, &[]).await.unwrap();
    assert!(
        row.get::<_, bool>(0) && row.get::<_, bool>(1),
        "the table and its sequence are absent"
    );
    admin
        .batch_execute(r#"DROP TABLE "Cache.Entries""#)
        .await
        .unwrap();
    assert_eq!(fingerprint(&restored).await, fingerprint(&admin).await);
    run.remove().unwrap();
}

#[tokio::test]
#[ignore = "needs PostgreSQL: run scripts/test-integration.sh"]
async fn exclusions_that_break_a_restore_fail_before_the_dump() {
    let fixture = Fixture::create("exclude_checks").await;
    fixture
        .client()
        .await
        .batch_execute(
            "CREATE TABLE logs (id int, at date) PARTITION BY RANGE (at);
            CREATE TABLE logs_2026 PARTITION OF logs FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');
            CREATE TABLE cache (id serial);
            CREATE TABLE copies (id int DEFAULT nextval('cache_id_seq'));
            CREATE FUNCTION cache_size(c cache) RETURNS int LANGUAGE sql AS 'SELECT 1'",
        )
        .await
        .unwrap();

    for (tables, message) in [
        (
            &["public.missing", "public.cache"][..],
            "names no table: public.missing",
        ),
        (
            &["public.accounts"],
            "constraint orders_account_id_fkey on table orders",
        ),
        (&["public.accounts"], "view balances"),
        (&["public.cache"], "function cache_size(cache)"),
        (
            &["public.cache"],
            "default value for column id of table copies",
        ),
        (&["public.logs"], "only ordinary tables"),
        (&["public.logs_2026"], "only ordinary tables"),
    ] {
        let source = Source {
            exclude_tables: tables.iter().map(|table| table.to_string()).collect(),
            ..fixture.source(Tls::Disable)
        };
        let error = failure(source, &safety(), future::pending()).await;

        assert_eq!(error.kind, Failure::Preflight, "{tables:?}: {error}");
        assert!(error.message.contains(message), "{tables:?}: {error}");
    }
}

#[tokio::test]
#[ignore = "needs PostgreSQL: run scripts/test-integration.sh"]
async fn connection_problems_fail_before_the_dump() {
    let fixture = Fixture::create("connection").await;
    let wrong = Path::new(env!("CARGO_TARGET_TMPDIR")).join("svalbard_connection-wrong");
    fs::write(&wrong, "s3cr3t-wrong").unwrap();
    let multiline = Path::new(env!("CARGO_TARGET_TMPDIR")).join("svalbard_connection-lines");
    fs::write(&multiline, "s3cr3t\nsecond line").unwrap();

    for (source, message) in [
        (
            Source {
                password: Password::File(wrong),
                ..fixture.source(verify_full())
            },
            "password authentication failed",
        ),
        (
            Source {
                password: Password::File(multiline),
                ..fixture.source(verify_full())
            },
            "line break",
        ),
        // The system trust store does not hold the test CA.
        (fixture.source(Tls::VerifyFull(None)), "certificate"),
    ] {
        let error = failure(source, &safety(), future::pending()).await;

        assert_eq!(error.kind, Failure::Preflight, "{error}");
        assert!(error.message.contains(message), "{error}");
        assert!(!error.message.contains("s3cr3t"), "{error}");
    }
}
