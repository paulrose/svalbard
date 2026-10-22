//! The PostgreSQL source. See "PostgreSQL rules" in docs/database-safety.md.

use std::{ffi::OsString, ops::RangeInclusive, path::Path, time::Duration};

use rustls::{
    ClientConfig, RootCertStore,
    pki_types::{CertificateDer, pem::PemObject},
};
use tokio_postgres::{NoTls, config::SslMode};
use tokio_postgres_rustls::MakeRustlsConnect;

use super::{Client, Error, Failure, Observation, Source, Wait};
use crate::config::{self, Safety, Tls};

/// The supported major versions of `pg_dump` and of the server.
const VERSIONS: RangeInclusive<i32> = 15..=18;
/// The limit for a connection attempt and for each control statement. The server also ends a
/// backup session within this time after `pg_dump` disconnects.
const TIMEOUT: Duration = Duration::from_secs(10);

const SESSIONS: &str = "SELECT pid FROM pg_stat_activity
WHERE application_name = $1 AND usename = current_user";

/// The lock waits of the backup sessions in $1, and the sessions that wait for them.
const WAITS: &str = "WITH waiting AS MATERIALIZED (
  SELECT pid, mode, locktype, relation FROM pg_locks WHERE NOT granted
)
SELECT pid = ANY($1),
  CASE WHEN pid = ANY($1)
    THEN format('%s on %s', mode, coalesce(relation::regclass::text, locktype))
    ELSE format('session %s', pid)
  END
FROM waiting
WHERE pid = ANY($1) OR pg_blocking_pids(pid) && $1";

const TERMINATE: &str = "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
WHERE application_name = $1 AND usename = current_user";

/// The tables and sequences that `pg_dump` must read but the role cannot. Row-level security
/// fails the dump unless it does not apply to the role.
const DENIED: &str = "SELECT format('%s %I.%I',
    CASE c.relkind WHEN 'S' THEN 'sequence' ELSE 'table' END, n.nspname, c.relname)
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE c.relkind IN ('r', 'p', 'S')
  AND n.nspname <> 'information_schema' AND n.nspname NOT LIKE 'pg\\_%'
  AND NOT EXISTS (
    SELECT FROM pg_depend d
    WHERE d.classid = 'pg_class'::regclass AND d.objid = c.oid AND d.deptype = 'e'
  )
  AND (
    NOT has_schema_privilege(n.oid, 'USAGE')
    OR NOT has_table_privilege(c.oid, 'SELECT')
    OR c.relrowsecurity
      AND NOT (SELECT rolsuper OR rolbypassrls FROM pg_roles WHERE rolname = current_user)
      AND (c.relforcerowsecurity OR NOT pg_has_role(c.relowner, 'USAGE'))
  )
ORDER BY 1";

/// A PostgreSQL database. It holds the password in memory.
pub struct Postgres {
    source: config::Source,
    password: Option<String>,
    /// The TLS connector, or `None` if TLS is disabled.
    tls: Option<MakeRustlsConnect>,
    /// The server accepts `client_connection_check_interval`.
    check_interval: bool,
    control: Option<tokio_postgres::Client>,
}

impl Postgres {
    /// Reads the password and the trusted certificates. Makes no connection.
    pub fn new(source: config::Source) -> Result<Postgres, Error> {
        let preflight = |message| Error::new(Failure::Preflight, message);
        let password = source.password.read().map_err(preflight)?;
        let tls = match &source.tls {
            Tls::VerifyFull(ca_file) => Some(connector(ca_file.as_deref()).map_err(preflight)?),
            Tls::Disable => None,
        };
        Ok(Postgres {
            source,
            password,
            tls,
            check_interval: false,
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
            let client = match &self.tls {
                Some(tls) => {
                    let (client, connection) = config
                        .ssl_mode(SslMode::Require)
                        .connect(tls.clone())
                        .await
                        .map_err(failed)?;
                    tokio::spawn(connection);
                    client
                }
                None => {
                    let (client, connection) = config
                        .ssl_mode(SslMode::Disable)
                        .connect(NoTls)
                        .await
                        .map_err(failed)?;
                    tokio::spawn(connection);
                    client
                }
            };
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

    async fn preflight(&mut self, client_version: &str) -> Result<String, String> {
        let client = client_major(client_version)
            .filter(|major| VERSIONS.contains(major))
            .ok_or_else(|| {
                format!("`{client_version}` is not a supported pg_dump. Use pg_dump 15 to 18.")
            })?;
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

        let denied: Vec<String> = control
            .query(DENIED, &[])
            .await
            .map_err(|error| describe("PostgreSQL query failed", error))?
            .iter()
            .map(|row| row.get(0))
            .collect();
        if !denied.is_empty() {
            let mut objects = denied[..denied.len().min(5)].join(", ");
            if denied.len() > 5 {
                objects += &format!(" and {} more", denied.len() - 5);
            }
            return Err(format!(
                "the role {} cannot read {objects}. It needs pg_read_all_data, and BYPASSRLS for tables with row-level security.",
                self.source.user
            ));
        }

        // Servers on some platforms reject the setting. See the Timeouts section of the contract.
        let interval = format!(
            "SET client_connection_check_interval = '{}s'",
            TIMEOUT.as_secs()
        );
        self.check_interval = control.batch_execute(&interval).await.is_ok();
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
        Client {
            args: vec![
                "--format=custom".into(),
                "--no-password".into(),
                format!("--lock-wait-timeout={}s", safety.lock_timeout.as_secs()).into(),
            ],
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
}
