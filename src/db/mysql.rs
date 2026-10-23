//! The MySQL source. See "MySQL rules" in docs/database-safety.md.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

use mysql_async::{Conn, OptsBuilder, SslOpts, prelude::Queryable};
use sha2::{Digest, Sha256};
use tokio::time;

use super::{Client, Error, Failure, Observation, Source, Wait, listing};
use crate::config::{self, Safety, Tls};

/// The supported release series of `mysqldump` and of the server.
const SERIES: [(u32, u32); 2] = [(8, 4), (9, 7)];
/// The limit for a connection attempt and for each control statement.
const TIMEOUT: Duration = Duration::from_secs(10);
/// Server error for `KILL` of a connection that has ended.
const UNKNOWN_THREAD: u16 = 1094;

/// The privileges on the database without which `mysqldump` silently leaves objects out.
const DATABASE_PRIVILEGES: [&str; 4] = ["SELECT", "SHOW VIEW", "TRIGGER", "EVENT"];

/// The control session sees its own metadata lock only if `performance_schema`, its
/// metadata-lock instrumentation and the account's privileges allow detection.
const SELF_CHECK: &str = "SELECT COUNT(*) FROM performance_schema.metadata_locks
WHERE OWNER_THREAD_ID = PS_CURRENT_THREAD_ID()
  AND OBJECT_SCHEMA = 'performance_schema' AND OBJECT_NAME = 'metadata_locks'";

const SESSIONS: &str = "SELECT t.THREAD_ID, t.PROCESSLIST_ID
FROM performance_schema.user_variables_by_thread v
JOIN performance_schema.threads t USING (THREAD_ID)
WHERE v.VARIABLE_NAME = 'svalbard_run' AND v.VARIABLE_VALUE = ?
  AND t.PROCESSLIST_USER = SUBSTRING_INDEX(USER(), '@', 1) AND t.PROCESSLIST_ID IS NOT NULL";

/// The metadata-lock waits of the backup threads in `{ids}`, the waits for locks that they
/// hold, and the waits for a table flush, which no metadata lock shows.
const WAITS: &str = "SELECT w.OWNER_THREAD_ID IN ({ids}),
  CONCAT(w.LOCK_TYPE, ' on ', w.OBJECT_TYPE, ' ', CONCAT_WS('.', w.OBJECT_SCHEMA, w.OBJECT_NAME)),
  IFNULL(CONCAT('connection ', t.PROCESSLIST_ID), CONCAT('thread ', t.THREAD_ID))
FROM performance_schema.metadata_locks w
JOIN performance_schema.threads t ON t.THREAD_ID = w.OWNER_THREAD_ID
WHERE w.LOCK_STATUS = 'PENDING' AND (w.OWNER_THREAD_ID IN ({ids}) OR EXISTS (
  SELECT 1 FROM performance_schema.metadata_locks h
  WHERE h.OWNER_THREAD_ID IN ({ids}) AND h.LOCK_STATUS = 'GRANTED'
    AND h.OBJECT_TYPE = w.OBJECT_TYPE
    AND h.OBJECT_SCHEMA <=> w.OBJECT_SCHEMA AND h.OBJECT_NAME <=> w.OBJECT_NAME))
UNION ALL
SELECT 0, PROCESSLIST_STATE,
  IFNULL(CONCAT('connection ', PROCESSLIST_ID), CONCAT('thread ', THREAD_ID))
FROM performance_schema.threads
WHERE PROCESSLIST_STATE = 'Waiting for table flush'";

const ENGINES: &str = "SELECT CONCAT(TABLE_NAME, ' (', IFNULL(ENGINE, 'no engine'), ')')
FROM information_schema.TABLES
WHERE TABLE_SCHEMA = DATABASE() AND TABLE_TYPE = 'BASE TABLE' AND NOT (ENGINE <=> 'InnoDB')
ORDER BY TABLE_NAME";

/// The definition of each table and view. Writes change none of these columns.
const DEFINITIONS: &str = "SELECT t.TABLE_NAME, CONCAT_WS(',',
  QUOTE(t.TABLE_TYPE), QUOTE(t.ENGINE), QUOTE(t.CREATE_TIME), QUOTE(c.ORDINAL_POSITION),
  QUOTE(c.COLUMN_NAME), QUOTE(c.COLUMN_TYPE), QUOTE(c.IS_NULLABLE), QUOTE(c.COLUMN_DEFAULT),
  QUOTE(c.EXTRA), QUOTE(c.GENERATION_EXPRESSION), QUOTE(c.COLLATION_NAME))
FROM information_schema.TABLES t
LEFT JOIN information_schema.COLUMNS c
  ON c.TABLE_SCHEMA = t.TABLE_SCHEMA AND c.TABLE_NAME = t.TABLE_NAME
WHERE t.TABLE_SCHEMA = DATABASE()
ORDER BY t.TABLE_NAME, c.ORDINAL_POSITION";

/// A MySQL database. It holds the password in memory.
pub struct Mysql {
    source: config::Source,
    password: Option<String>,
    /// The trusted certificates, or `None` if TLS is disabled.
    ca_file: Option<PathBuf>,
    control: Option<Conn>,
    /// The SHA-256 of each table definition before the dump.
    definitions: Option<BTreeMap<String, Vec<u8>>>,
}

impl Mysql {
    /// Reads the password and finds the trusted certificates. Makes no connection.
    pub fn new(source: config::Source) -> Result<Mysql, Error> {
        let preflight = |message: String| Error::new(Failure::Preflight, message);
        let password = source.password.read().map_err(preflight)?;
        if password
            .as_ref()
            .is_some_and(|password| password.contains('\0'))
        {
            return Err(preflight(
                "the source password contains a NUL character, which mysqldump cannot read."
                    .to_owned(),
            ));
        }
        // mysqldump needs a CA file for VERIFY_IDENTITY, so both connections use one.
        let ca_file = match &source.tls {
            Tls::VerifyFull(Some(path)) => Some(path.clone()),
            Tls::VerifyFull(None) => Some(openssl_probe::probe().cert_file.ok_or_else(|| {
                preflight("no system CA bundle was found. Set source.tls_ca_file.".to_owned())
            })?),
            Tls::Disable => None,
        };
        Ok(Mysql {
            source,
            password,
            ca_file,
            control: None,
            definitions: None,
        })
    }

    async fn connect(&self) -> Result<Conn, String> {
        let source = &self.source;
        let mut options = OptsBuilder::default()
            .ip_or_hostname(source.host.clone())
            .tcp_port(source.port)
            .user(Some(source.user.clone()))
            .pass(self.password.clone())
            .db_name(Some(source.database.clone()))
            .prefer_socket(false)
            // An empty sql_mode keeps the quoting of SHOW GRANTS stable.
            .init(vec![format!(
                "SET SESSION sql_mode = '', max_execution_time = {}, information_schema_stats_expiry = 0",
                TIMEOUT.as_millis()
            )]);
        if let Some(ca_file) = &self.ca_file {
            options = options.ssl_opts(
                SslOpts::default()
                    .with_root_certs(vec![ca_file.clone().into()])
                    .with_disable_built_in_roots(true),
            );
        }
        time::timeout(TIMEOUT, Conn::new(options))
            .await
            .map_err(|_| "cannot connect to MySQL: the connection timed out".to_owned())?
            .map_err(|error| format!("cannot connect to MySQL: {error}"))
    }

    /// Runs `queries` on the control connection, and opens one if there is none. A connection
    /// is reused only after its queries succeeded, because a dropped query can leave it broken.
    async fn control<T>(
        &mut self,
        queries: impl AsyncFnOnce(&mut Conn) -> mysql_async::Result<T>,
    ) -> Result<T, String> {
        let mut conn = match self.control.take() {
            Some(conn) => conn,
            None => self.connect().await?,
        };
        let result = queries(&mut conn)
            .await
            .map_err(|error| format!("MySQL query failed: {error}"))?;
        self.control = Some(conn);
        Ok(result)
    }

    /// The SHA-256 of each table definition.
    async fn definitions(&mut self) -> Result<BTreeMap<String, Vec<u8>>, String> {
        self.control(async |conn| {
            conn.query_iter(DEFINITIONS)
                .await?
                .reduce_and_drop(
                    BTreeMap::new(),
                    |mut tables: BTreeMap<String, Sha256>, (table, column): (String, String)| {
                        tables.entry(table).or_default().update(column + "\n");
                        tables
                    },
                )
                .await
        })
        .await
        .map(|tables| {
            tables
                .into_iter()
                .map(|(table, sha256)| (table, sha256.finalize().to_vec()))
                .collect()
        })
    }
}

impl Source for Mysql {
    const CLIENT: &'static str = "mysqldump";
    const FORMAT: &'static str = "mysqldump-sql";
    const EXTENSION: &'static str = "sql";

    async fn preflight(&mut self, client_version: &str) -> Result<String, String> {
        let client = client_series(client_version)
            .filter(|series| SERIES.contains(series))
            .ok_or_else(|| {
                format!(
                    "`{client_version}` is not a supported mysqldump. Use mysqldump 8.4 or 9.7."
                )
            })?;
        let (version, database, roles) = self
            .control(async |conn| {
                conn.query_first::<(String, Option<String>, String), _>(
                    "SELECT VERSION(), DATABASE(), CURRENT_ROLE()",
                )
                .await
            })
            .await?
            .ok_or("MySQL returned no version")?;
        let server = series(&version)
            .filter(|series| SERIES.contains(series))
            .ok_or_else(|| format!("MySQL {version} is not supported. Use MySQL 8.4 or 9.7."))?;
        if client != server {
            return Err(format!(
                "mysqldump {}.{} cannot dump MySQL {}.{}. Use mysqldump {1}.{2}.",
                client.0, client.1, server.0, server.1
            ));
        }
        let database = database.ok_or("MySQL selected no database")?;

        let show_grants = match roles.as_str() {
            "NONE" => "SHOW GRANTS FOR CURRENT_USER()".to_owned(),
            roles => format!("SHOW GRANTS FOR CURRENT_USER() USING {roles}"),
        };
        let grants = self
            .control(async |conn| conn.query::<String, _>(show_grants).await)
            .await?;
        let missing = missing_privileges(&grants, &database);
        if !missing.is_empty() {
            return Err(format!(
                "the account lacks {}. mysqldump leaves out objects that the account cannot see, \
                 so it needs {} on `{database}`.* and SHOW_ROUTINE on *.*.",
                missing.join(", "),
                DATABASE_PRIVILEGES.join(", ")
            ));
        }

        let other_engines = self
            .control(async |conn| conn.query::<String, _>(ENGINES).await)
            .await?;
        if !other_engines.is_empty() {
            return Err(format!(
                "these tables do not use InnoDB, so the dump cannot read them consistently: {}",
                listing(&other_engines)
            ));
        }

        self.definitions = Some(self.definitions().await?);
        Ok(version)
    }

    fn client(&self, run_id: &str, credentials: &Path, safety: &Safety) -> Client {
        let source = &self.source;
        // env_clear removes HOME and MYSQL_TEST_LOGIN_FILE, so no login path file is read.
        let mut file = format!(
            "[client]\nhost={}\nport={}\nuser={}\nprotocol=TCP\n",
            option_value(&source.host),
            source.port,
            option_value(&source.user)
        );
        if let Some(password) = &self.password {
            file += &format!("password={}\n", option_value(password));
        }
        match &self.ca_file {
            Some(ca_file) => {
                file += &format!(
                    "ssl-mode=VERIFY_IDENTITY\nssl-ca={}\n",
                    option_value(&ca_file.to_string_lossy())
                );
            }
            // caching_sha2_password needs the server's key to send a password without TLS.
            None => file += "ssl-mode=DISABLED\nget-server-public-key=ON\n",
        }
        let mut defaults_file = OsString::from("--defaults-file=");
        defaults_file.push(credentials);
        Client {
            args: vec![
                // mysqldump reads --defaults-file only as the first argument.
                defaults_file,
                "--single-transaction".into(),
                "--skip-lock-tables".into(),
                "--quick".into(),
                "--set-gtid-purged=OFF".into(),
                "--no-tablespaces".into(),
                "--routines".into(),
                "--events".into(),
                // A backstop if Svalbard stops. The watchdog ends a lock wait first, as lock_timeout.
                format!(
                    "--init-command=SET SESSION lock_wait_timeout = {}, @svalbard_run = '{run_id}'",
                    (safety.lock_timeout + TIMEOUT).as_secs()
                )
                .into(),
                "--".into(),
                source.database.clone().into(),
            ],
            env: Vec::new(),
            credentials: Some(file),
        }
    }

    async fn observe(&mut self, run_id: &str) -> Result<Observation, String> {
        let run_id = run_id.to_owned();
        let (checked, sessions, waits) = self
            .control(async |conn| {
                let checked = conn.query_first::<u64, _>(SELF_CHECK).await?;
                let sessions: Vec<(u64, u64)> = conn.exec(SESSIONS, (run_id,)).await?;
                if sessions.is_empty() {
                    return Ok((checked, sessions, Vec::new()));
                }
                let ids: Vec<String> = sessions.iter().map(|(id, _)| id.to_string()).collect();
                let waits: Vec<(bool, String, String)> =
                    conn.query(WAITS.replace("{ids}", &ids.join(","))).await?;
                Ok((checked, sessions, waits))
            })
            .await?;
        if checked != Some(1) {
            return Err(
                "performance_schema does not show metadata locks to this account. \
                 Enable performance_schema with the wait/lock/metadata/sql/mdl instrument, \
                 and grant SELECT on metadata_locks, threads and user_variables_by_thread."
                    .to_owned(),
            );
        }
        Ok(Observation {
            sessions: sessions.iter().map(|(_, id)| id.to_string()).collect(),
            waits: waits
                .into_iter()
                .map(|(backup, lock, session)| match backup {
                    true => Wait::Backup(lock),
                    false => Wait::Blocked(session),
                })
                .collect(),
        })
    }

    async fn terminate(&mut self, run_id: &str) -> Result<(), String> {
        let run_id = run_id.to_owned();
        self.control(async |conn| {
            let sessions: Vec<(u64, u64)> = conn.exec(SESSIONS, (run_id,)).await?;
            for (_, id) in sessions {
                match conn.query_drop(format!("KILL CONNECTION {id}")).await {
                    Err(mysql_async::Error::Server(error)) if error.code == UNKNOWN_THREAD => {}
                    result => result?,
                }
            }
            Ok(())
        })
        .await
    }

    async fn verify(&mut self) -> Result<(), Error> {
        let before = self.definitions.take().unwrap_or_default();
        let after = self.definitions().await.map_err(|error| {
            Error::new(
                Failure::DetectionLost,
                format!("the schema check failed: {error}"),
            )
        })?;
        let changed: Vec<String> = before
            .keys()
            .chain(after.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|table| before.get(*table) != after.get(*table))
            .cloned()
            .collect();
        if changed.is_empty() {
            return Ok(());
        }
        Err(Error::new(
            Failure::SchemaChanged,
            format!(
                "these tables were created, dropped or changed during the dump: {}",
                listing(&changed)
            ),
        ))
    }
}

/// The release series in a first line such as `mysqldump  Ver 8.4.8 for Linux on x86_64`.
fn client_series(version: &str) -> Option<(u32, u32)> {
    if version.contains("MariaDB") || version.contains("Distrib") {
        return None;
    }
    let (_, number) = version.split_once(" Ver ")?;
    series(number)
}

/// The major and minor version at the start of `version`, such as `8.4.8-log`.
fn series(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.split(|c: char| !c.is_ascii_digit());
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// The privileges in [`DATABASE_PRIVILEGES`] and `SHOW_ROUTINE` that `grants` (the lines of
/// `SHOW GRANTS`) do not give on all of `database`. Only grants on `*.*` and on the database
/// count. Table-level and wildcard grants do not cover objects that are created later.
fn missing_privileges(grants: &[String], database: &str) -> Vec<String> {
    let (mut global, mut on_database, mut revoked) =
        (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    for line in grants {
        let (revoke, rest, to) = match (line.strip_prefix("GRANT "), line.strip_prefix("REVOKE ")) {
            (Some(rest), _) => (false, rest, " TO "),
            (_, Some(rest)) => (true, rest, " FROM "),
            _ => continue,
        };
        let Some((privileges, rest)) = rest.split_once(" ON ") else {
            continue;
        };
        let Some((object, _)) = rest.split_once(to) else {
            continue;
        };
        let set = match (revoke, object) {
            (false, "*.*") => &mut global,
            (false, _) if names_database(object, database) => &mut on_database,
            (true, _) if names_database(object, database) => &mut revoked,
            _ => continue,
        };
        set.extend(
            privileges
                .split(',')
                .map(|privilege| privilege.trim().to_owned()),
        );
    }
    let has = |set: &BTreeSet<String>, privilege: &str| {
        set.contains(privilege) || set.contains("ALL PRIVILEGES")
    };
    let mut missing: Vec<String> = DATABASE_PRIVILEGES
        .into_iter()
        .filter(|&privilege| {
            !has(&on_database, privilege) && (!has(&global, privilege) || has(&revoked, privilege))
        })
        .map(|privilege| format!("{privilege} on `{database}`.*"))
        .collect();
    if !has(&global, "SHOW_ROUTINE") && !has(&global, "SELECT") {
        missing.push("SHOW_ROUTINE on *.*".to_owned());
    }
    missing
}

/// Whether a grant object such as `` `app\_db`.* `` names exactly `database`.
fn names_database(object: &str, database: &str) -> bool {
    let Some(name) = object
        .strip_prefix('`')
        .and_then(|object| object.strip_suffix("`.*"))
    else {
        return false;
    };
    let name = name
        .replace("``", "`")
        .replace("\\_", "_")
        .replace("\\%", "%");
    // An unescaped % is a wildcard, which can match other databases too.
    name == database && !object.replace("\\%", "").contains('%')
}

/// A quoted value for a MySQL option file, which reads `\\`, `\"`, `\n` and `\r` as escapes.
fn option_value(value: &str) -> String {
    let mut quoted = String::from('"');
    for c in value.chars() {
        match c {
            '\\' => quoted += "\\\\",
            '"' => quoted += "\\\"",
            '\n' => quoted += "\\n",
            '\r' => quoted += "\\r",
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_series_reads_oracle_builds_only() {
        assert_eq!(
            client_series("mysqldump  Ver 8.4.8 for macos26.2 on arm64 (Homebrew)"),
            Some((8, 4))
        );
        assert_eq!(
            client_series(
                "mysqldump  Ver 9.7.4 for Linux on aarch64 (MySQL Community Server - GPL)"
            ),
            Some((9, 7))
        );
        assert_eq!(
            client_series("mysqldump from 11.4.2-MariaDB, client 10.19"),
            None
        );
        assert_eq!(
            client_series("mysqldump  Ver 10.19 Distrib 10.6.18-MariaDB, for Linux"),
            None
        );
        assert_eq!(series("8.4.6-log"), Some((8, 4)));
    }

    #[test]
    fn missing_privileges_counts_only_database_wide_grants() {
        let full = [
            "GRANT USAGE ON *.* TO `backup`@`%`",
            "GRANT SHOW_ROUTINE ON *.* TO `backup`@`%`",
            "GRANT SELECT, SHOW VIEW, TRIGGER, EVENT ON `app\\_db`.* TO `backup`@`%`",
        ];
        let lines = |lines: &[&str]| {
            lines
                .iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
        };
        assert!(missing_privileges(&lines(&full), "app_db").is_empty());
        assert!(
            missing_privileges(
                &lines(&["GRANT ALL PRIVILEGES ON *.* TO `root`@`%`"]),
                "app"
            )
            .is_empty()
        );

        // A table-level grant, a wildcard grant and another database do not count.
        let partial = lines(&[
            "GRANT SELECT ON *.* TO `backup`@`%`",
            "GRANT TRIGGER ON `app`.`users` TO `backup`@`%`",
            "GRANT SHOW VIEW ON `ap%`.* TO `backup`@`%`",
            "GRANT EVENT ON `other`.* TO `backup`@`%`",
        ]);
        assert_eq!(
            missing_privileges(&partial, "app"),
            [
                "SHOW VIEW on `app`.*",
                "TRIGGER on `app`.*",
                "EVENT on `app`.*"
            ]
        );

        // A partial revoke removes a global privilege from one database.
        let revoked = lines(&[
            "GRANT SELECT, SHOW VIEW, TRIGGER, EVENT ON *.* TO `backup`@`%`",
            "REVOKE TRIGGER ON `app`.* FROM `backup`@`%`",
        ]);
        assert_eq!(missing_privileges(&revoked, "app"), ["TRIGGER on `app`.*"]);
        assert_eq!(
            missing_privileges(&lines(&["GRANT SHOW VIEW ON `a``b`.* TO `x`@`%`"]), "a`b").len(),
            4
        );
    }

    #[test]
    fn option_values_escape_quotes_backslashes_and_line_breaks() {
        assert_eq!(option_value(" p\"a\\s#s'\nw\r "), r#"" p\"a\\s#s'\nw\r ""#);
    }

    #[test]
    fn client_reads_the_defaults_file_first_and_the_database_last() {
        let mysql = Mysql {
            source: config::Source {
                kind: config::Kind::Mysql,
                host: "db".to_owned(),
                port: 3306,
                database: "-app".to_owned(),
                user: "backup".to_owned(),
                password: config::Password::None,
                tls: Tls::Disable,
            },
            password: Some("s3cr3t".to_owned()),
            ca_file: None,
            control: None,
            definitions: None,
        };
        let safety = Safety {
            lock_timeout: Duration::from_secs(5),
            contention_timeout: Duration::from_secs(2),
            dump_timeout: Duration::from_secs(60),
            max_dump_size: 1 << 30,
            min_free_space: 0,
        };
        let client = mysql.client("abc", Path::new("/run/credentials"), &safety);
        let args: Vec<_> = client
            .args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect();

        assert_eq!(args[0], "--defaults-file=/run/credentials");
        assert_eq!(args[args.len() - 2..], ["--", "-app"]);
        assert!(
            args.iter()
                .any(|arg| arg.contains("lock_wait_timeout = 15"))
        );
        assert!(args.iter().all(|arg| !arg.contains("s3cr3t")));
        assert!(client.credentials.unwrap().contains("password=\"s3cr3t\""));
    }
}
