//! A MySQL database with fixture data and a backup account.

use std::{
    env,
    fs::{self, File},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use mysql_async::{Conn, Opts, OptsBuilder, Row, prelude::Queryable};
use svalbard::config::{Kind, Password, Source, Tls};
use tokio::sync::RwLock;

/// The password of the backup account. Option files need escapes for `"`, `\` and line breaks,
/// keep spaces only inside quotes, and treat `#` as a comment.
pub const PASSWORD: &str = " p\"a\\ss#w'o\nrd ";

/// A plain `FLUSH TABLES` waits for every open table on the server, so the test that runs one
/// must not overlap the others.
pub static SERVER: RwLock<()> = RwLock::const_new(());

pub const FIXTURE: &str = r#"
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
CREATE TABLE blank (id INT PRIMARY KEY, note TEXT);
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
pub const FINGERPRINT: &str = r#"
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
      ORDER BY id SEPARATOR '\n'), 256) FROM orders),
  (SELECT COUNT(*) FROM blank)
"#;

/// About 50k rows of about 200 bytes, for the tests that need more data.
const LARGER: &str = r#"
CREATE TABLE larger (
  id INT PRIMARY KEY,
  name VARCHAR(200) NOT NULL,
  amount DECIMAL(14, 4),
  created DATETIME(6) NOT NULL,
  flags SET('a', 'b', 'c'),
  payload VARBINARY(32)
);
SET SESSION cte_max_recursion_depth = 50000;
INSERT INTO larger
WITH RECURSIVE g (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g WHERE n < 50000)
SELECT n, REPEAT(SHA2(n, 256), 2), n * 0.0001, TIMESTAMP '2026-01-01 00:00:00' + INTERVAL n SECOND,
  ELT(1 + n % 3, 'a', 'b', 'a,c'), UNHEX(SHA2(n, 256))
FROM g;
"#;

pub fn admin() -> Opts {
    let url = env::var("SVALBARD_TEST_MYSQL")
        .expect("SVALBARD_TEST_MYSQL must be set: run scripts/test-integration.sh");
    Opts::from_url(&url).expect("SVALBARD_TEST_MYSQL must be a connection URL")
}

pub fn verify_full() -> Tls {
    let ca = env::var_os("SVALBARD_TEST_MYSQL_CA")
        .expect("SVALBARD_TEST_MYSQL_CA must be set: run scripts/test-integration.sh");
    Tls::VerifyFull(Some(ca.into()))
}

pub async fn connect(options: OptsBuilder) -> Conn {
    Conn::new(options.prefer_socket(false))
        .await
        .expect("the test connection must open")
}

/// A string literal for MySQL statements that take no parameters.
pub fn literal(value: &str) -> String {
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
pub struct Fixture {
    pub database: String,
    /// Whether [`Fixture::add_larger`] ran.
    pub larger: bool,
}

impl Fixture {
    pub async fn create(name: &str) -> Fixture {
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
        let fixture = Fixture {
            database,
            larger: false,
        };
        // One statement at a time: in a multi-statement query, MySQL keeps the `;` in the
        // trigger body, and the dump of that body cannot be restored.
        let mut client = fixture.client().await;
        for statement in FIXTURE.split(";\n").filter(|s| !s.trim().is_empty()) {
            client.query_drop(statement).await.unwrap();
        }
        fs::write(fixture.password_file(), format!("{PASSWORD}\n")).unwrap();
        fixture
    }

    /// Adds the `larger` table.
    pub async fn add_larger(&mut self) {
        let mut client = self.client().await;
        for statement in LARGER.split(";\n").filter(|s| !s.trim().is_empty()) {
            client.query_drop(statement).await.unwrap();
        }
        self.larger = true;
    }

    /// Restores `dump` and checks that the schema and the data match the source.
    pub async fn assert_restores(&self, dump: &[u8]) {
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{}.sql", self.database));
        fs::write(&path, dump).unwrap();
        let (mut restored, mut source) = (self.restore(&path).await, self.client().await);
        assert_eq!(
            fingerprint(&mut restored).await,
            fingerprint(&mut source).await
        );
        if self.larger {
            assert_eq!(
                larger_fingerprint(&mut restored).await,
                larger_fingerprint(&mut source).await
            );
        }
    }

    pub fn password_file(&self) -> PathBuf {
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{}-password", self.database))
    }

    pub fn source(&self, tls: Tls) -> Source {
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
    pub async fn client(&self) -> Conn {
        connect(OptsBuilder::from_opts(admin()).db_name(Some(&self.database))).await
    }

    /// A connection of the backup account, without TLS.
    pub async fn account(&self) -> Conn {
        let options = OptsBuilder::from_opts(admin())
            .db_name(Some(&self.database))
            .user(Some(&self.database))
            .pass(Some(PASSWORD));
        connect(options).await
    }

    /// Holds a write lock on the `stall` table until the connection is dropped.
    pub async fn hold_stall_lock(&self) -> Conn {
        let mut client = self.client().await;
        client.query_drop("LOCK TABLES stall WRITE").await.unwrap();
        client
    }

    /// Waits until a session of the backup account waits for the lock on the `stall` table.
    pub async fn stalled(&self, client: &mut Conn) {
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
    pub async fn backup_sessions(&self, client: &mut Conn) -> Vec<String> {
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
    pub async fn restore(&self, dump: &Path) -> Conn {
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

pub async fn fingerprint(client: &mut Conn) -> Vec<mysql_async::Value> {
    client
        .query_drop("SET SESSION group_concat_max_len = 16777216")
        .await
        .unwrap();
    let row: Option<Row> = client.query_first(FINGERPRINT).await.unwrap();
    row.unwrap().unwrap()
}

/// The hash of the row hashes of the `larger` table.
pub async fn larger_fingerprint(client: &mut Conn) -> String {
    client
        .query_drop("SET SESSION group_concat_max_len = 16777216")
        .await
        .unwrap();
    let query = "SELECT SHA2(GROUP_CONCAT(SHA2(CONCAT_WS('|', id, name, amount, created, flags,
        HEX(payload)), 256) ORDER BY id SEPARATOR ''), 256) FROM larger";
    let hash: Option<String> = client.query_first(query).await.unwrap();
    hash.unwrap()
}
