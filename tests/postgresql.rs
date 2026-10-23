//! Tests against a PostgreSQL server with TLS. Run them in Docker with `scripts/test-integration.sh`.

use std::{
    env, fs,
    future::{self, Future},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use svalbard::{
    config::{Kind, Password, Safety, Source, Tls},
    db::{self, Dump, Error, Failure, Postgres},
    staging::Run,
};
use tokio_postgres::{Client, Config, NoTls, config::Host};

/// The password of the backup role. `:` and `\` need escapes in the libpq password file.
const PASSWORD: &str = r"pa:ss\word";

const FIXTURE: &str = r#"
CREATE TABLE accounts (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  email text NOT NULL UNIQUE,
  balance numeric(12, 2) NOT NULL DEFAULT 0,
  tags text[],
  profile jsonb
);
CREATE TABLE orders (
  id serial PRIMARY KEY,
  account_id bigint NOT NULL REFERENCES accounts,
  placed timestamptz NOT NULL,
  note text
);
CREATE INDEX orders_placed ON orders (placed);
CREATE VIEW balances AS SELECT email, balance FROM accounts WHERE balance > 0;
CREATE TABLE events (id bigserial PRIMARY KEY);
-- The tests lock this table to stall pg_dump. pg_dump locks the older tables first.
CREATE TABLE stall (id int);

INSERT INTO accounts (email, balance, tags, profile)
SELECT format('user%s@example.com', g), g * 1.5, ARRAY['a', 'b' || g],
  jsonb_build_object('n', g, 'quote', 'it''s "x"')
FROM generate_series(1, 1000) g;
INSERT INTO orders (account_id, placed, note)
SELECT 1 + g % 1000, '2026-01-01'::timestamptz + g * interval '1 minute',
  CASE WHEN g % 7 > 0 THEN format(E'line\nbreak\t%s', g) END
FROM generate_series(1, 5000) g;
INSERT INTO events SELECT nextval('events_id_seq') FROM generate_series(1, 10);
"#;

/// The schema, and the data of the tables that no test changes.
const FINGERPRINT: &str = r#"
SELECT
  (SELECT string_agg(format('%s.%s %s %s %s', table_name, column_name, data_type,
      is_nullable, column_default), E'\n' ORDER BY table_name, column_name)
    FROM information_schema.columns WHERE table_schema = 'public'),
  (SELECT string_agg(indexdef, E'\n' ORDER BY indexdef)
    FROM pg_indexes WHERE schemaname = 'public'),
  (SELECT string_agg(format('%s %s', conrelid::regclass, pg_get_constraintdef(oid)), E'\n'
      ORDER BY conrelid::regclass::text, conname)
    FROM pg_constraint WHERE connamespace = 'public'::regnamespace),
  (SELECT string_agg(viewname || ' ' || definition, E'\n' ORDER BY viewname)
    FROM pg_views WHERE schemaname = 'public'),
  (SELECT md5(string_agg(a::text, E'\n' ORDER BY a.id)) FROM accounts a),
  (SELECT md5(string_agg(o::text, E'\n' ORDER BY o.id)) FROM orders o)
"#;

/// Backup sessions in the current database. The control connection is `svalbard`.
const BACKUP_SESSIONS: &str = "application_name ~ '^svalbard-[0-9a-f]{32}$'
AND datname = current_database()";

fn admin() -> Config {
    env::var("SVALBARD_TEST_POSTGRESQL")
        .expect("SVALBARD_TEST_POSTGRESQL must be set: run scripts/test-integration.sh")
        .parse()
        .expect("SVALBARD_TEST_POSTGRESQL must be a connection URL")
}

fn verify_full() -> Tls {
    let ca = env::var_os("SVALBARD_TEST_POSTGRESQL_CA")
        .expect("SVALBARD_TEST_POSTGRESQL_CA must be set: run scripts/test-integration.sh");
    Tls::VerifyFull(Some(ca.into()))
}

async fn connect(config: &Config) -> Client {
    let (client, connection) = config
        .connect(NoTls)
        .await
        .expect("the test connection must open");
    tokio::spawn(connection);
    client
}

fn safety() -> Safety {
    Safety {
        lock_timeout: Duration::from_secs(30),
        contention_timeout: Duration::from_secs(1),
        dump_timeout: Duration::from_secs(60),
        max_dump_size: 1 << 30,
        min_free_space: 0,
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

/// Waits until a backup session waits for the lock on the `stall` table.
async fn stalled(client: &Client) {
    let query = format!(
        "SELECT EXISTS (
  SELECT FROM pg_locks l JOIN pg_stat_activity a USING (pid)
  WHERE NOT l.granted AND l.relation = 'stall'::regclass AND {BACKUP_SESSIONS})"
    );
    for _ in 0..100 {
        if client.query_one(&query, &[]).await.unwrap().get(0) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the backup did not wait for the stall table");
}

/// A database with fixture data, and a backup role with the same name and `pg_read_all_data`.
struct Fixture {
    /// The admin connection to the database.
    admin: Config,
    database: String,
}

impl Fixture {
    async fn create(name: &str) -> Fixture {
        let database = format!("svalbard_{name}");
        let server = connect(&admin()).await;
        // The first statements remove what an earlier failed run left.
        for statement in [
            format!("DROP DATABASE IF EXISTS {database} WITH (FORCE)"),
            format!("DROP DATABASE IF EXISTS {database}_restored WITH (FORCE)"),
            format!("DROP ROLE IF EXISTS {database}"),
            format!("CREATE ROLE {database} LOGIN PASSWORD '{PASSWORD}' IN ROLE pg_read_all_data"),
            format!("CREATE DATABASE {database}"),
        ] {
            let result = server.batch_execute(&statement).await;
            result.unwrap_or_else(|error| panic!("{statement}: {error}"));
        }
        let mut admin = admin();
        admin.dbname(&database);
        connect(&admin).await.batch_execute(FIXTURE).await.unwrap();
        let fixture = Fixture { admin, database };
        fs::write(fixture.password_file(), format!("{PASSWORD}\n")).unwrap();
        fixture
    }

    fn password_file(&self) -> PathBuf {
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{}-password", self.database))
    }

    fn source(&self, tls: Tls) -> Source {
        let Host::Tcp(host) = &self.admin.get_hosts()[0] else {
            panic!("SVALBARD_TEST_POSTGRESQL must use TCP");
        };
        Source {
            kind: Kind::Postgresql,
            host: host.clone(),
            port: self.admin.get_ports().first().copied().unwrap_or(5432),
            database: self.database.clone(),
            user: self.database.clone(),
            password: Password::File(self.password_file()),
            tls,
        }
    }

    async fn client(&self) -> Client {
        connect(&self.admin).await
    }

    /// Holds an exclusive lock on the `stall` table until the client is dropped.
    async fn hold_stall_lock(&self) -> Client {
        let client = self.client().await;
        client
            .batch_execute("BEGIN; LOCK TABLE stall IN ACCESS EXCLUSIVE MODE")
            .await
            .unwrap();
        client
    }

    async fn backup_sessions(&self) -> i64 {
        let query = format!("SELECT count(*) FROM pg_stat_activity WHERE {BACKUP_SESSIONS}");
        self.client()
            .await
            .query_one(&query, &[])
            .await
            .unwrap()
            .get(0)
    }

    /// Restores `dump` into a new database with `pg_restore`, and connects to it.
    async fn restore(&self, dump: &Path) -> Client {
        let restored = format!("{}_restored", self.database);
        let server = connect(&admin()).await;
        for statement in [
            format!("DROP DATABASE IF EXISTS {restored} WITH (FORCE)"),
            format!("CREATE DATABASE {restored} TEMPLATE template0"),
        ] {
            server.batch_execute(&statement).await.unwrap();
        }
        let Host::Tcp(host) = &self.admin.get_hosts()[0] else {
            panic!("SVALBARD_TEST_POSTGRESQL must use TCP");
        };
        let password = self.admin.get_password().unwrap_or_default();
        let output = Command::new("pg_restore")
            .args(["--exit-on-error", "--dbname", &restored])
            .arg(dump)
            .env("PGHOST", host)
            .env("PGPORT", self.source(Tls::Disable).port.to_string())
            .env("PGUSER", self.admin.get_user().unwrap_or("postgres"))
            .env("PGPASSWORD", String::from_utf8_lossy(password).as_ref())
            .output()
            .expect("pg_restore must start");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut config = self.admin.clone();
        config.dbname(&restored);
        connect(&config).await
    }
}

async fn fingerprint(client: &Client) -> Vec<Option<String>> {
    let row = client.query_one(FINGERPRINT, &[]).await.unwrap();
    (0..row.len()).map(|column| row.get(column)).collect()
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
    assert!(error.message.contains("table public.orders"), "{error}");
    assert!(error.message.contains("and 1 more"), "{error}");
    assert!(!error.message.contains("table public.accounts"), "{error}");
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
