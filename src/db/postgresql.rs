//! The PostgreSQL source. See "PostgreSQL rules" in docs/database-safety.md.

use std::{ffi::OsString, ops::RangeInclusive, path::Path, time::Duration};

use rustls::{
    ClientConfig, RootCertStore,
    pki_types::{CertificateDer, pem::PemObject},
};
use tokio::time;
use tokio_postgres::{NoTls, config::SslMode};
use tokio_postgres_rustls::MakeRustlsConnect;

use super::{Client, Error, Failure, Observation, Source, Wait, listing};
use crate::config::{self, Safety, Tls};

/// The supported major versions of `pg_dump` and of the server.
const VERSIONS: RangeInclusive<i32> = 15..=18;
/// The limit for a connection attempt and for each control statement. The server also ends a
/// backup session within this time after `pg_dump` disconnects.
const TIMEOUT: Duration = Duration::from_secs(10);

const SESSIONS: &str = "SELECT pid FROM pg_stat_activity
WHERE application_name = $1 AND usename = current_user";

/// The lock waits of the backup sessions in $1, and the sessions that wait for them, with the
/// lock that they want. `pg_blocking_pids()` never reports the startup process of a standby, so
/// WAL replay that waits for a table lock of the backup is matched by the relation.
const WAITS: &str = "WITH waiting AS MATERIALIZED (
  SELECT pid, mode, locktype, database, relation FROM pg_locks WHERE NOT granted
)
SELECT pid = ANY($1),
  CASE WHEN pid = ANY($1)
    THEN format('%s on %s', mode, coalesce(relation::regclass::text, locktype))
    ELSE format('session %s (%s on %s)', pid, mode, coalesce(relation::regclass::text, locktype))
  END
FROM waiting
WHERE pid = ANY($1) OR pg_blocking_pids(pid) && $1
  OR mode = 'AccessExclusiveLock' AND locktype = 'relation' AND (database, relation) IN (
    SELECT database, relation FROM pg_locks
    WHERE granted AND pid = ANY($1) AND locktype = 'relation'
  )";

const TERMINATE: &str = "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
WHERE application_name = $1 AND usename = current_user";

/// The tables and sequences that `pg_dump` must read but the role cannot. Row-level security
/// fails the dump unless it does not apply to the role. Of the extension members, `pg_dump`
/// reads only the configuration tables.
const DENIED: &str = "SELECT format('%s %I.%I',
    CASE c.relkind WHEN 'S' THEN 'sequence' ELSE 'table' END, n.nspname, c.relname)
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE c.relkind IN ('r', 'p', 'S')
  AND n.nspname <> 'information_schema' AND n.nspname NOT LIKE 'pg\\_%'
  AND (
    c.oid IN (SELECT unnest(extconfig) FROM pg_extension)
    OR NOT EXISTS (
      SELECT FROM pg_depend d
      WHERE d.classid = 'pg_class'::regclass AND d.objid = c.oid AND d.deptype = 'e'
    )
  )
  AND (
    NOT has_schema_privilege(n.oid, 'USAGE')
    OR NOT has_table_privilege(c.oid, 'SELECT')
    OR c.relrowsecurity
      AND NOT (SELECT rolsuper OR rolbypassrls FROM pg_roles WHERE rolname = current_user)
      AND (c.relforcerowsecurity OR NOT pg_has_role(c.relowner, 'USAGE'))
  )
  AND c.oid <> ALL($1)
ORDER BY 1";

/// The relations that the names in $1 match exactly, whether each is an ordinary table, the OIDs
/// of the table and of the sequences that it owns, and the names of these sequences. A sequence
/// owned by a table is in the same schema.
const EXCLUDED: &str = "SELECT n.nspname || '.' || c.relname, n.nspname, c.relname,
  c.relkind = 'r' AND NOT c.relispartition, array_prepend(c.oid, s.oids), s.names
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
CROSS JOIN LATERAL (
  SELECT coalesce(array_agg(s.oid), '{}'), coalesce(array_agg(s.relname::text), '{}')
  FROM pg_depend d
  JOIN pg_class s ON s.oid = d.objid
  WHERE d.classid = 'pg_class'::regclass AND d.refclassid = 'pg_class'::regclass
    AND d.refobjid = c.oid AND d.deptype IN ('a', 'i') AND s.relkind = 'S'
) s (oids, names)
WHERE n.nspname || '.' || c.relname = ANY($1)
  AND n.nspname <> 'information_schema' AND n.nspname NOT LIKE 'pg\\_%'";

/// The kept objects that need a relation in $1 or its row type, so that their restore fails.
/// A dependent that belongs to a relation in $1 is skipped with it. Constraint triggers
/// depend on the other table automatically, so they are matched separately.
const DEPENDENTS: &str = "WITH excluded (classid, objid) AS (
  SELECT 'pg_class'::regclass, unnest($1::oid[])
  UNION ALL
  SELECT 'pg_type'::regclass, unnest(ARRAY[oid, typarray]) FROM pg_type WHERE typrelid = ANY($1)
)
SELECT CASE WHEN r.rulename = '_RETURN' THEN pg_describe_object('pg_class'::regclass, r.ev_class, 0)
  ELSE pg_describe_object(d.classid, d.objid, 0) END
FROM pg_depend d
JOIN excluded e ON d.refclassid = e.classid AND d.refobjid = e.objid
LEFT JOIN pg_rewrite r ON d.classid = 'pg_rewrite'::regclass AND r.oid = d.objid
WHERE d.deptype = 'n' AND coalesce(CASE d.classid
    WHEN 'pg_class'::regclass THEN d.objid
    WHEN 'pg_rewrite'::regclass THEN r.ev_class
    WHEN 'pg_constraint'::regclass THEN (SELECT conrelid FROM pg_constraint WHERE oid = d.objid)
    WHEN 'pg_trigger'::regclass THEN (SELECT tgrelid FROM pg_trigger WHERE oid = d.objid)
    WHEN 'pg_policy'::regclass THEN (SELECT polrelid FROM pg_policy WHERE oid = d.objid)
    WHEN 'pg_attrdef'::regclass THEN (SELECT adrelid FROM pg_attrdef WHERE oid = d.objid)
    WHEN 'pg_publication_rel'::regclass
      THEN (SELECT prrelid FROM pg_publication_rel WHERE oid = d.objid)
  END, 0::oid) <> ALL($1)
UNION
SELECT format('constraint trigger %I on %s', tgname, tgrelid::regclass)
FROM pg_trigger
WHERE NOT tgisinternal AND tgconstrrelid = ANY($1) AND tgrelid <> ALL($1)
ORDER BY 1";

/// A PostgreSQL database. It holds the password in memory.
pub struct Postgres {
    source: config::Source,
    password: Option<String>,
    /// The TLS connector, or `None` if TLS is disabled.
    tls: Option<MakeRustlsConnect>,
    /// The server accepts `client_connection_check_interval`.
    check_interval: bool,
    /// The `--exclude-table` patterns that the preflight found, for tables and sequences.
    excluded: Vec<String>,
    control: Option<tokio_postgres::Client>,
}

impl Postgres {
    /// Reads the password and the trusted certificates. Makes no connection.
    pub fn new(source: config::Source) -> Result<Postgres, Error> {
        let preflight = |message| Error::new(Failure::Preflight, message);
        let password = source.password.read().map_err(preflight)?;
        // A libpq password file holds one password per line.
        if password
            .as_ref()
            .is_some_and(|password| password.contains(['\n', '\r']))
        {
            return Err(preflight(
                "the source password contains a line break, which pg_dump cannot read.".to_owned(),
            ));
        }
        let tls = match &source.tls {
            Tls::VerifyFull(ca_file) => Some(connector(ca_file.as_deref()).map_err(preflight)?),
            Tls::Disable => None,
        };
        Ok(Postgres {
            source,
            password,
            tls,
            check_interval: false,
            excluded: Vec::new(),
            control: None,
        })
    }

    /// The control connection. Opens a new one if there is none or if it failed.
    async fn control(&mut self) -> Result<&tokio_postgres::Client, String> {
        if self
            .control
            .as_ref()
            .is_none_or(tokio_postgres::Client::is_closed)
        {
            let source = &self.source;
            let mut config = tokio_postgres::Config::new();
            config
                .host(&source.host)
                .port(source.port)
                .dbname(&source.database)
                .user(&source.user)
                .application_name("svalbard")
                .options(format!("-c statement_timeout={}s", TIMEOUT.as_secs()))
                .connect_timeout(TIMEOUT);
            if let Some(password) = &self.password {
                config.password(password);
            }
            let failed = |error| describe("cannot connect to PostgreSQL", error);
            let connect = async {
                match &self.tls {
                    Some(tls) => {
                        let (client, connection) = config
                            .ssl_mode(SslMode::Require)
                            .connect(tls.clone())
                            .await
                            .map_err(failed)?;
                        tokio::spawn(connection);
                        Ok::<_, String>(client)
                    }
                    None => {
                        let (client, connection) = config
                            .ssl_mode(SslMode::Disable)
                            .connect(NoTls)
                            .await
                            .map_err(failed)?;
                        tokio::spawn(connection);
                        Ok(client)
                    }
                }
            };
            // `connect_timeout` covers only the TCP connection, not a server that stops answering.
            let client = time::timeout(TIMEOUT, connect).await.map_err(|_| {
                "cannot connect to PostgreSQL: the connection timed out".to_owned()
            })??;
            self.control = Some(client);
        }
        Ok(self
            .control
            .as_ref()
            .expect("the control connection is open"))
    }
}

impl Source for Postgres {
    const CLIENT: &'static str = "pg_dump";
    const FORMAT: &'static str = "pg_dump-custom";
    const EXTENSION: &'static str = "dump";

    async fn preflight(&mut self, client_version: &str) -> Result<String, String> {
        let client = client_major(client_version)
            .filter(|major| VERSIONS.contains(major))
            .ok_or_else(|| {
                format!("`{client_version}` is not a supported pg_dump. Use pg_dump 15 to 18.")
            })?;
        // The control connection borrows `self`.
        let names = self.source.exclude_tables.clone();
        let control = self.control().await?;
        let row = control
            .query_one(
                "SELECT current_setting('server_version_num')::int / 10000,
                  current_setting('server_version')",
                &[],
            )
            .await
            .map_err(|error| describe("PostgreSQL query failed", error))?;
        let (server, version): (i32, String) = (row.get(0), row.get(1));
        if !VERSIONS.contains(&server) {
            return Err(format!(
                "PostgreSQL {version} is not supported. Use PostgreSQL 15 to 18."
            ));
        }
        if client < server {
            return Err(format!(
                "pg_dump {client} cannot dump PostgreSQL {server}. Use pg_dump {server} or newer."
            ));
        }

        let (skipped, excluded) = exclusions(control, &names).await?;
        let denied: Vec<String> = control
            .query(DENIED, &[&skipped])
            .await
            .map_err(|error| describe("PostgreSQL query failed", error))?
            .iter()
            .map(|row| row.get(0))
            .collect();
        if !denied.is_empty() {
            return Err(format!(
                "the role {} cannot read {}. It needs pg_read_all_data, and BYPASSRLS for tables with row-level security.",
                self.source.user,
                listing(&denied)
            ));
        }

        // Servers on some platforms reject the setting. See the Timeouts section of the contract.
        let interval = format!(
            "SET client_connection_check_interval = '{}s'",
            TIMEOUT.as_secs()
        );
        self.check_interval = control.batch_execute(&interval).await.is_ok();
        self.excluded = excluded;
        Ok(version)
    }

    fn client(&self, run_id: &str, credentials: &Path, safety: &Safety) -> Client {
        let source = &self.source;
        let mut env: Vec<(&'static str, OsString)> = vec![
            ("PGHOST", source.host.clone().into()),
            ("PGPORT", source.port.to_string().into()),
            ("PGDATABASE", source.database.clone().into()),
            ("PGUSER", source.user.clone().into()),
            ("PGAPPNAME", label(run_id).into()),
            ("PGPASSFILE", credentials.into()),
            // libpq prefers GSS encryption to TLS. The control connection uses TLS only.
            ("PGGSSENCMODE", "disable".into()),
        ];
        match &source.tls {
            Tls::VerifyFull(ca_file) => {
                env.push(("PGSSLMODE", "verify-full".into()));
                // libpq 16 and newer read `system` as the system trust store.
                let root = ca_file.as_ref().map_or("system".into(), |path| path.into());
                env.push(("PGSSLROOTCERT", root));
            }
            Tls::Disable => env.push(("PGSSLMODE", "disable".into())),
        }
        if self.check_interval {
            let options = format!("-c client_connection_check_interval={}s", TIMEOUT.as_secs());
            env.push(("PGOPTIONS", options.into()));
        }
        let mut args: Vec<OsString> = vec![
            "--format=custom".into(),
            "--no-password".into(),
            // A backstop if Svalbard stops. The watchdog ends a lock wait first, as lock_timeout.
            format!(
                "--lock-wait-timeout={}s",
                (safety.lock_timeout + TIMEOUT).as_secs()
            )
            .into(),
        ];
        args.extend(
            self.excluded
                .iter()
                .map(|pattern| format!("--exclude-table={pattern}").into()),
        );
        Client {
            args,
            env,
            credentials: self.password.as_deref().map(password_file),
        }
    }

    async fn observe(&mut self, run_id: &str) -> Result<Observation, String> {
        let control = self.control().await?;
        let sessions: Vec<i32> = control
            .query(SESSIONS, &[&label(run_id)])
            .await
            .map_err(|error| describe("PostgreSQL query failed", error))?
            .iter()
            .map(|row| row.get(0))
            .collect();
        if sessions.is_empty() {
            return Ok(Observation::default());
        }
        let waits = control
            .query(WAITS, &[&sessions])
            .await
            .map_err(|error| describe("PostgreSQL query failed", error))?
            .iter()
            .map(|row| match row.get(0) {
                true => Wait::Backup(row.get(1)),
                false => Wait::Blocked(row.get(1)),
            })
            .collect();
        Ok(Observation {
            sessions: sessions.iter().map(i32::to_string).collect(),
            waits,
        })
    }

    async fn terminate(&mut self, run_id: &str) -> Result<(), String> {
        let control = self.control().await?;
        control
            .execute(TERMINATE, &[&label(run_id)])
            .await
            .map_err(|error| describe("PostgreSQL query failed", error))?;
        Ok(())
    }

    fn excluded_tables(&self) -> &[String] {
        &self.source.exclude_tables
    }
}

/// Checks `names` from `source.exclude_tables`. Returns the OIDs that the dump skips, and the
/// `--exclude-table` patterns: the tables and the sequences that they own.
async fn exclusions(
    control: &tokio_postgres::Client,
    names: &[String],
) -> Result<(Vec<u32>, Vec<String>), String> {
    if names.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let failed = |error| describe("PostgreSQL query failed", error);
    // pg_dump sends the patterns in the database encoding.
    if names.iter().any(|name| !name.is_ascii()) {
        let row = control
            .query_one("SELECT current_setting('server_encoding')", &[])
            .await
            .map_err(failed)?;
        let encoding: String = row.get(0);
        if encoding != "UTF8" {
            return Err(format!(
                "the database encoding is {encoding}, so pg_dump cannot match the names in source.exclude_tables that are not ASCII."
            ));
        }
    }

    let rows = control.query(EXCLUDED, &[&names]).await.map_err(failed)?;
    let (mut skipped, mut patterns) = (Vec::new(), Vec::new());
    let (mut missing, mut other, mut ambiguous) = (Vec::new(), Vec::new(), Vec::new());
    for name in names {
        let matches: Vec<_> = rows
            .iter()
            .filter(|row| row.get::<_, &str>(0) == name)
            .collect();
        match matches[..] {
            [] => missing.push(name.clone()),
            [row] if row.get(3) => {
                skipped.extend(row.get::<_, Vec<u32>>(4));
                let schema = row.get(1);
                patterns.push(pattern(schema, row.get(2)));
                // Otherwise pg_dump dumps a serial sequence, and waits for the lock of its table.
                for sequence in row.get::<_, Vec<String>>(5) {
                    patterns.push(pattern(schema, &sequence));
                }
            }
            [_] => other.push(name.clone()),
            _ => ambiguous.push(name.clone()),
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "source.exclude_tables names no table: {}",
            listing(&missing)
        ));
    }
    if !other.is_empty() {
        return Err(format!(
            "source.exclude_tables can name only ordinary tables, not partitioned tables, partitions, views or sequences: {}",
            listing(&other)
        ));
    }
    if !ambiguous.is_empty() {
        return Err(format!(
            "source.exclude_tables names more than one table with {}",
            listing(&ambiguous)
        ));
    }

    let dependents: Vec<String> = control
        .query(DEPENDENTS, &[&skipped])
        .await
        .map_err(failed)?
        .iter()
        .map(|row| row.get(0))
        .collect();
    if !dependents.is_empty() {
        return Err(format!(
            "these objects need a table in source.exclude_tables, so a restore would fail: {}",
            listing(&dependents)
        ));
    }
    Ok((skipped, patterns))
}

/// An exact `pg_dump` pattern for a table. The quotes keep `.`, `*`, `?` and capitals literal.
fn pattern(schema: &str, table: &str) -> String {
    let quote = |name: &str| format!("\"{}\"", name.replace('"', "\"\""));
    format!("{}.{}", quote(schema), quote(table))
}

/// The trusted certificates from `ca_file`, or from the system trust store.
fn connector(ca_file: Option<&Path>) -> Result<MakeRustlsConnect, String> {
    let (certificates, origin) = match ca_file {
        Some(path) => (
            CertificateDer::pem_file_iter(path)
                .and_then(|certificates| certificates.collect())
                .map_err(|error| {
                    format!("cannot read source.tls_ca_file {}: {error}", path.display())
                })?,
            format!("source.tls_ca_file {}", path.display()),
        ),
        None => (
            rustls_native_certs::load_native_certs().certs,
            "the system trust store".to_owned(),
        ),
    };
    let mut roots = RootCertStore::empty();
    if roots.add_parsable_certificates(certificates).0 == 0 {
        return Err(format!("{origin} holds no usable CA certificate."));
    }
    Ok(MakeRustlsConnect::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

/// The major version in a first line such as `pg_dump (PostgreSQL) 17.2`.
fn client_major(version: &str) -> Option<i32> {
    let number = version.strip_prefix("pg_dump (PostgreSQL) ")?;
    let digits = number
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(number.len());
    number[..digits].parse().ok()
}

/// The application name of the backup sessions. All users can see it.
fn label(run_id: &str) -> String {
    format!("svalbard-{run_id}")
}

/// A libpq password file that gives the password for every connection.
fn password_file(password: &str) -> String {
    format!(
        "*:*:*:*:{}\n",
        password.replace('\\', "\\\\").replace(':', "\\:")
    )
}

/// The error with its cause. Only the cause holds the server's message.
fn describe(context: &str, error: tokio_postgres::Error) -> String {
    match std::error::Error::source(&error) {
        Some(cause) => format!("{context}: {error}: {cause}"),
        None => format!("{context}: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_major_reads_release_and_distribution_versions() {
        assert_eq!(client_major("pg_dump (PostgreSQL) 17.2"), Some(17));
        assert_eq!(
            client_major("pg_dump (PostgreSQL) 16.4 (Debian 16.4-1.pgdg120+1)"),
            Some(16)
        );
        assert_eq!(client_major("pg_dump (PostgreSQL) 18beta1"), Some(18));
        assert_eq!(client_major("pg_dump (EnterpriseDB) 17.2"), None);
    }

    #[test]
    fn patterns_quote_each_name() {
        assert_eq!(pattern("public", "Cache.*"), r#""public"."Cache.*""#);
        assert_eq!(pattern("a\"b", "c"), r#""a""b"."c""#);
    }
}
