//! A PostgreSQL database with fixture data and a backup role.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use svalbard::config::{Kind, Password, Source, Tls};
use tokio_postgres::{Client, Config, NoTls, config::Host};

/// The password of the backup role. `:` and `\` need escapes in the libpq password file.
pub const PASSWORD: &str = r"pa:ss\word";

pub const FIXTURE: &str = r#"
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
pub const FINGERPRINT: &str = r#"
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
pub const BACKUP_SESSIONS: &str = "application_name ~ '^svalbard-[0-9a-f]{32}$'
AND datname = current_database()";

pub fn admin() -> Config {
    env::var("SVALBARD_TEST_POSTGRESQL")
        .expect("SVALBARD_TEST_POSTGRESQL must be set: run scripts/test-integration.sh")
        .parse()
        .expect("SVALBARD_TEST_POSTGRESQL must be a connection URL")
}

pub fn verify_full() -> Tls {
    let ca = env::var_os("SVALBARD_TEST_POSTGRESQL_CA")
        .expect("SVALBARD_TEST_POSTGRESQL_CA must be set: run scripts/test-integration.sh");
    Tls::VerifyFull(Some(ca.into()))
}

pub async fn connect(config: &Config) -> Client {
    let (client, connection) = config
        .connect(NoTls)
        .await
        .expect("the test connection must open");
    tokio::spawn(connection);
    client
}

/// Waits until a backup session waits for the lock on the `stall` table.
pub async fn stalled(client: &Client) {
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
pub struct Fixture {
    /// The admin connection to the database.
    pub admin: Config,
    pub database: String,
}

impl Fixture {
    pub async fn create(name: &str) -> Fixture {
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

    pub fn password_file(&self) -> PathBuf {
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{}-password", self.database))
    }

    pub fn source(&self, tls: Tls) -> Source {
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

    pub async fn client(&self) -> Client {
        connect(&self.admin).await
    }

    /// Holds an exclusive lock on the `stall` table until the client is dropped.
    pub async fn hold_stall_lock(&self) -> Client {
        let client = self.client().await;
        client
            .batch_execute("BEGIN; LOCK TABLE stall IN ACCESS EXCLUSIVE MODE")
            .await
            .unwrap();
        client
    }

    pub async fn backup_sessions(&self) -> i64 {
        let query = format!("SELECT count(*) FROM pg_stat_activity WHERE {BACKUP_SESSIONS}");
        self.client()
            .await
            .query_one(&query, &[])
            .await
            .unwrap()
            .get(0)
    }

    /// Restores `dump` into a new database with `pg_restore`, and connects to it.
    pub async fn restore(&self, dump: &Path) -> Client {
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

pub async fn fingerprint(client: &Client) -> Vec<Option<String>> {
    let row = client.query_one(FINGERPRINT, &[]).await.unwrap();
    (0..row.len()).map(|column| row.get(column)).collect()
}
