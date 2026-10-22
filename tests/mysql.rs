//! Tests against a MySQL server with TLS. Run them in Docker with `scripts/test-databases.sh`.

use std::{
    env, fs,
    fs::File,
    future::{self, Future},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use mysql_async::{Conn, Opts, OptsBuilder, Row, prelude::Queryable};
use svalbard::{
    config::{Kind, Password, Safety, Source, Tls},
    db::{self, Dump, Error, Failure, Mysql, Source as _, Wait},
    staging::Run,
};
use tokio::sync::RwLock;

/// The password of the backup account. Option files need escapes for `"`, `\` and line breaks,
/// keep spaces only inside quotes, and treat `#` as a comment.
const PASSWORD: &str = " p\"a\\ss#w'o\nrd ";

/// A plain `FLUSH TABLES` waits for every open table on the server, so the test that runs one
/// must not overlap the others.
static SERVER: RwLock<()> = RwLock::const_new(());

const FIXTURE: &str = r#"
SET SESSION cte_max_recursion_depth = 10000;
CREATE TABLE accounts (
  id BIGINT AUTO_INCREMENT PRIMARY KEY,
  email VARCHAR(255) NOT NULL UNIQUE,
  balance DECIMAL(12, 2) NOT NULL DEFAULT 0,
  profile JSON,
  token VARBINARY(32),
  emoji VARCHAR(16) CHARACTER SET utf8mb4,
  domain VARCHAR(255) AS (SUBSTRING_INDEX(email, '@', -1)) STORED
);
CREATE TABLE orders (
  id INT AUTO_INCREMENT PRIMARY KEY,
  account_id BIGINT NOT NULL,
  placed DATETIME(6) NOT NULL,
  note TEXT,
  FOREIGN KEY (account_id) REFERENCES accounts (id)
);
CREATE INDEX orders_placed ON orders (placed);
CREATE VIEW balances AS SELECT email, balance FROM accounts WHERE balance > 0;
CREATE TRIGGER orders_note BEFORE INSERT ON orders FOR EACH ROW SET NEW.note = IFNULL(NEW.note, '-');
CREATE PROCEDURE account_count(OUT n BIGINT) SELECT COUNT(*) INTO n FROM accounts;
CREATE EVENT purge_orders ON SCHEDULE EVERY 1 DAY DISABLE
  DO DELETE FROM orders WHERE placed < '2000-01-01';
CREATE TABLE events (id BIGINT AUTO_INCREMENT PRIMARY KEY);
-- The tests lock this table to stall mysqldump, which dumps tables in name order.
CREATE TABLE stall (id INT);
CREATE TABLE zz_last (id INT);

INSERT INTO accounts (email, balance, profile, token, emoji)
WITH RECURSIVE g (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g WHERE n < 1000)
SELECT CONCAT('user', n, '@example.com'), n * 1.5, JSON_OBJECT('n', n, 'quote', 'it''s "x"'),
  UNHEX(SHA2(n, 256)), '🦭'
FROM g;
INSERT INTO orders (account_id, placed, note)
WITH RECURSIVE g (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g WHERE n < 5000)
SELECT 1 + n % 1000, TIMESTAMP '2026-01-01 00:00:00' + INTERVAL n MINUTE,
  IF(n % 7 > 0, CONCAT('line\nbreak\t', n), NULL)
FROM g;
INSERT INTO events () VALUES (), (), (), (), (), (), (), (), (), ();
INSERT INTO zz_last VALUES (1);
"#;

/// The schema, and the data of the tables that no test changes.
const FINGERPRINT: &str = r#"
SELECT
  (SELECT GROUP_CONCAT(CONCAT_WS(' ', TABLE_NAME, COLUMN_NAME, ORDINAL_POSITION, COLUMN_TYPE,
      IS_NULLABLE, QUOTE(COLUMN_DEFAULT), EXTRA, GENERATION_EXPRESSION, COLLATION_NAME)
      ORDER BY TABLE_NAME, ORDINAL_POSITION SEPARATOR '\n')
    FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE()),
  (SELECT GROUP_CONCAT(CONCAT_WS(' ', TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX, COLUMN_NAME, NON_UNIQUE)
      ORDER BY TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX SEPARATOR '\n')
    FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = DATABASE()),
  (SELECT GROUP_CONCAT(CONCAT_WS(' ', TABLE_NAME, CONSTRAINT_NAME, COLUMN_NAME,
      REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME)
      ORDER BY TABLE_NAME, CONSTRAINT_NAME, ORDINAL_POSITION SEPARATOR '\n')
    FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA = DATABASE()),
  (SELECT GROUP_CONCAT(CONCAT_WS(' ', TABLE_NAME,
      REPLACE(VIEW_DEFINITION, CONCAT('`', TABLE_SCHEMA, '`.'), ''))
      ORDER BY TABLE_NAME SEPARATOR '\n')
    FROM information_schema.VIEWS WHERE TABLE_SCHEMA = DATABASE()),
  (SELECT GROUP_CONCAT(CONCAT_WS(' ', TRIGGER_NAME, ACTION_TIMING, EVENT_MANIPULATION,
      EVENT_OBJECT_TABLE, ACTION_STATEMENT) ORDER BY TRIGGER_NAME SEPARATOR '\n')
    FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = DATABASE()),
  (SELECT GROUP_CONCAT(CONCAT_WS(' ', ROUTINE_NAME, ROUTINE_TYPE, ROUTINE_DEFINITION)
      ORDER BY ROUTINE_NAME SEPARATOR '\n')
    FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = DATABASE()),
  (SELECT GROUP_CONCAT(CONCAT_WS(' ', EVENT_NAME, STATUS, INTERVAL_VALUE, INTERVAL_FIELD,
      EVENT_DEFINITION) ORDER BY EVENT_NAME SEPARATOR '\n')
    FROM information_schema.EVENTS WHERE EVENT_SCHEMA = DATABASE()),
  (SELECT SHA2(GROUP_CONCAT(CONCAT_WS('|', id, email, balance, profile, HEX(token), emoji, domain)
      ORDER BY id SEPARATOR '\n'), 256) FROM accounts),
  (SELECT SHA2(GROUP_CONCAT(CONCAT_WS('|', id, account_id, placed, HEX(note))
      ORDER BY id SEPARATOR '\n'), 256) FROM orders)
"#;

fn admin() -> Opts {
    let url = env::var("SVALBARD_TEST_MYSQL")
        .expect("SVALBARD_TEST_MYSQL must be set: run scripts/test-databases.sh");
    Opts::from_url(&url).expect("SVALBARD_TEST_MYSQL must be a connection URL")
}

fn verify_full() -> Tls {
    let ca = env::var_os("SVALBARD_TEST_MYSQL_CA")
        .expect("SVALBARD_TEST_MYSQL_CA must be set: run scripts/test-databases.sh");
    Tls::VerifyFull(Some(ca.into()))
}

async fn connect(options: OptsBuilder) -> Conn {
    Conn::new(options.prefer_socket(false))
        .await
        .expect("the test connection must open")
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

/// A string literal for MySQL statements that take no parameters.
fn literal(value: &str) -> String {
    format!(
        "'{}'",
        value
            .replace('\\', "\\\\")
            .replace('\'', "\\'")
            .replace('\n', "\\n")
    )
}

/// A database with fixture data, and a backup account with the same name and the grants from
/// docs/database-safety.md.
struct Fixture {
    database: String,
}

impl Fixture {
    async fn create(name: &str) -> Fixture {
        let database = format!("svalbard_{name}");
        let mut server = connect(OptsBuilder::from_opts(admin())).await;
        // The first statements remove what an earlier failed run left.
        server
            .query_drop(format!(
                "DROP DATABASE IF EXISTS {database};
                 DROP DATABASE IF EXISTS {database}_restored;
                 DROP USER IF EXISTS {database};
                 CREATE DATABASE {database};
                 CREATE USER {database} IDENTIFIED BY {};
                 GRANT SELECT, SHOW VIEW, TRIGGER, EVENT ON {database}.* TO {database};
                 GRANT SHOW_ROUTINE ON *.* TO {database};
                 GRANT SELECT ON performance_schema.metadata_locks TO {database};
                 GRANT SELECT ON performance_schema.threads TO {database};
                 GRANT SELECT ON performance_schema.user_variables_by_thread TO {database};",
                literal(PASSWORD)
            ))
            .await
            .unwrap();
        let fixture = Fixture { database };
        // One statement at a time: in a multi-statement query, MySQL keeps the `;` in the
        // trigger body, and the dump of that body cannot be restored.
        let mut client = fixture.client().await;
        for statement in FIXTURE.split(";\n").filter(|s| !s.trim().is_empty()) {
            client.query_drop(statement).await.unwrap();
        }
        fs::write(fixture.password_file(), format!("{PASSWORD}\n")).unwrap();
        fixture
    }

    fn password_file(&self) -> PathBuf {
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{}-password", self.database))
    }

    fn source(&self, tls: Tls) -> Source {
        let admin = admin();
        Source {
            kind: Kind::Mysql,
            host: admin.ip_or_hostname().to_owned(),
            port: admin.tcp_port(),
            database: self.database.clone(),
            user: self.database.clone(),
            password: Password::File(self.password_file()),
            tls,
        }
    }

    /// An admin connection to the database.
    async fn client(&self) -> Conn {
        connect(OptsBuilder::from_opts(admin()).db_name(Some(&self.database))).await
    }

    /// A connection of the backup account, without TLS.
    async fn account(&self) -> Conn {
        let options = OptsBuilder::from_opts(admin())
            .db_name(Some(&self.database))
            .user(Some(&self.database))
            .pass(Some(PASSWORD));
        connect(options).await
    }

    /// Holds a write lock on the `stall` table until the connection is dropped.
    async fn hold_stall_lock(&self) -> Conn {
        let mut client = self.client().await;
        client.query_drop("LOCK TABLES stall WRITE").await.unwrap();
        client
    }

    /// Waits until a session of the backup account waits for the lock on the `stall` table.
    async fn stalled(&self, client: &mut Conn) {
        for _ in 0..100 {
            let waiting: Option<u64> = client
                .exec_first(
                    "SELECT COUNT(*) FROM performance_schema.metadata_locks l
                     JOIN performance_schema.threads t ON t.THREAD_ID = l.OWNER_THREAD_ID
                     WHERE l.LOCK_STATUS = 'PENDING' AND l.OBJECT_SCHEMA = ?
                       AND l.OBJECT_NAME = 'stall' AND t.PROCESSLIST_USER = ?",
                    (&self.database, &self.database),
                )
                .await
                .unwrap();
            if waiting > Some(0) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("the backup did not wait for the stall table");
    }

    /// The sessions of the backup account with a run ID from Svalbard, and their connection types.
    async fn backup_sessions(&self, client: &mut Conn) -> Vec<String> {
        client
            .exec(
                "SELECT t.CONNECTION_TYPE FROM performance_schema.user_variables_by_thread v
                 JOIN performance_schema.threads t USING (THREAD_ID)
                 WHERE v.VARIABLE_NAME = 'svalbard_run' AND LENGTH(v.VARIABLE_VALUE) = 32
                   AND t.PROCESSLIST_USER = ?",
                (&self.database,),
            )
            .await
            .unwrap()
    }

    /// Restores `dump` into a new database with the `mysql` client, and connects to it.
    async fn restore(&self, dump: &Path) -> Conn {
        let restored = format!("{}_restored", self.database);
        let mut server = connect(OptsBuilder::from_opts(admin())).await;
        server
            .query_drop(format!(
                "DROP DATABASE IF EXISTS {restored}; CREATE DATABASE {restored}"
            ))
            .await
            .unwrap();
        let admin = admin();
        let defaults = Path::new(env!("CARGO_TARGET_TMPDIR")).join("mysql-admin.cnf");
        fs::write(
            &defaults,
            format!(
                "[client]\nhost={}\nport={}\nuser={}\npassword={}\nprotocol=TCP\nssl-mode=REQUIRED\n",
                admin.ip_or_hostname(),
                admin.tcp_port(),
                admin.user().unwrap_or("root"),
                admin.pass().unwrap_or_default()
            ),
        )
        .unwrap();
        let output = Command::new("mysql")
            .arg(format!("--defaults-file={}", defaults.display()))
            .args(["--", &restored])
            .stdin(File::open(dump).unwrap())
            .output()
            .expect("mysql must start");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        connect(OptsBuilder::from_opts(admin).db_name(Some(restored))).await
    }
}

async fn fingerprint(client: &mut Conn) -> Vec<mysql_async::Value> {
    client
        .query_drop("SET SESSION group_concat_max_len = 16777216")
        .await
        .unwrap();
    let row: Option<Row> = client.query_first(FINGERPRINT).await.unwrap();
    row.unwrap().unwrap()
}

#[tokio::test]
#[ignore = "needs MySQL: run scripts/test-databases.sh"]
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
#[ignore = "needs MySQL: run scripts/test-databases.sh"]
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
#[ignore = "needs MySQL: run scripts/test-databases.sh"]
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
#[ignore = "needs MySQL: run scripts/test-databases.sh"]
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
#[ignore = "needs MySQL: run scripts/test-databases.sh"]
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
#[ignore = "needs MySQL: run scripts/test-databases.sh"]
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
#[ignore = "needs MySQL: run scripts/test-databases.sh"]
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
