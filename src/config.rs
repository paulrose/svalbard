use std::{
    env,
    ffi::OsStr,
    fs::{self, File},
    path::{Path, PathBuf},
    time::Duration,
};

use clap::Args;
use serde::{Deserialize, Deserializer};

/// Environment variable that holds the source database password.
pub const PASSWORD_ENV: &str = "SVALBARD_SOURCE_PASSWORD";

const TIME_UNITS: &[(&str, u64)] = &[("s", 1), ("min", 60), ("h", 3600)];
const SIZE_UNITS: &[(&str, u64)] = &[
    ("B", 1),
    ("KiB", 1 << 10),
    ("MiB", 1 << 20),
    ("GiB", 1 << 30),
    ("TiB", 1 << 40),
];

/// Validated configuration. It holds no secret values.
#[derive(Debug)]
pub struct Config {
    pub source: Source,
    pub destination: Destination,
    pub safety: Safety,
}

#[derive(Debug)]
pub struct Source {
    pub kind: Kind,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: Password,
    pub tls: Tls,
}

/// The database system of the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Postgresql,
    Mysql,
}

/// Where the source password comes from. Validation never reads the password.
#[derive(Debug)]
pub enum Password {
    None,
    Env,
    File(PathBuf),
}

impl Password {
    /// Reads the password. A line break at the end of the file is not part of it.
    pub fn read(&self) -> Result<Option<String>, String> {
        match self {
            Password::None => Ok(None),
            // The error omits the value, which can hold the secret.
            Password::Env => env::var(PASSWORD_ENV)
                .map(Some)
                .map_err(|_| format!("{PASSWORD_ENV} is not set or is not valid UTF-8.")),
            Password::File(path) => {
                let text = fs::read_to_string(path).map_err(|error| {
                    format!(
                        "cannot read source.password_file {}: {error}",
                        path.display()
                    )
                })?;
                let line = text.strip_suffix('\n').unwrap_or(&text);
                Ok(Some(line.strip_suffix('\r').unwrap_or(line).to_owned()))
            }
        }
    }
}

/// TLS for the database connections.
#[derive(Debug)]
pub enum Tls {
    /// Requires TLS, and verifies the server certificate and host name against the CA file,
    /// or against the system trust store if there is no file.
    VerifyFull(Option<PathBuf>),
    Disable,
}

#[derive(Debug)]
pub struct Destination {
    pub bucket: String,
    pub prefix: String,
    pub region: Option<String>,
}

#[derive(Debug)]
pub struct Safety {
    pub lock_timeout: Duration,
    pub contention_timeout: Duration,
    pub dump_timeout: Duration,
    pub max_dump_size: u64,
    pub min_free_space: u64,
}

/// Unvalidated, non-secret values from one layer: flags and environment variables, or the file.
#[derive(Args, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    #[command(flatten)]
    pub source: SourceSettings,
    #[command(flatten)]
    pub destination: DestinationSettings,
    #[command(flatten)]
    pub safety: SafetySettings,
}

#[derive(Args, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[command(next_help_heading = "Source")]
pub struct SourceSettings {
    #[arg(
        id = "source-type",
        long = "source-type",
        env = "SVALBARD_SOURCE_TYPE",
        value_name = "TYPE"
    )]
    pub r#type: Option<String>,
    #[arg(
        long = "source-host",
        env = "SVALBARD_SOURCE_HOST",
        value_name = "HOST"
    )]
    pub host: Option<String>,
    #[arg(
        long = "source-port",
        env = "SVALBARD_SOURCE_PORT",
        value_name = "PORT"
    )]
    #[serde(deserialize_with = "port_number")]
    pub port: Option<String>,
    #[arg(
        long = "source-database",
        env = "SVALBARD_SOURCE_DATABASE",
        value_name = "NAME"
    )]
    pub database: Option<String>,
    #[arg(
        long = "source-user",
        env = "SVALBARD_SOURCE_USER",
        value_name = "USER"
    )]
    pub user: Option<String>,
    #[arg(
        long = "source-password-file",
        env = "SVALBARD_SOURCE_PASSWORD_FILE",
        value_name = "PATH"
    )]
    pub password_file: Option<PathBuf>,
    #[arg(long = "source-tls", env = "SVALBARD_SOURCE_TLS", value_name = "MODE")]
    pub tls: Option<String>,
    #[arg(
        long = "source-tls-ca-file",
        env = "SVALBARD_SOURCE_TLS_CA_FILE",
        value_name = "PATH"
    )]
    pub tls_ca_file: Option<PathBuf>,
}

#[derive(Args, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[command(next_help_heading = "Destination")]
pub struct DestinationSettings {
    #[arg(
        id = "destination-type",
        long = "destination-type",
        env = "SVALBARD_DESTINATION_TYPE",
        value_name = "TYPE"
    )]
    pub r#type: Option<String>,
    #[arg(
        long = "destination-bucket",
        env = "SVALBARD_DESTINATION_BUCKET",
        value_name = "NAME"
    )]
    pub bucket: Option<String>,
    #[arg(
        long = "destination-prefix",
        env = "SVALBARD_DESTINATION_PREFIX",
        value_name = "PREFIX"
    )]
    pub prefix: Option<String>,
    #[arg(
        long = "destination-region",
        env = "SVALBARD_DESTINATION_REGION",
        value_name = "REGION"
    )]
    pub region: Option<String>,
}

#[derive(Args, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[command(next_help_heading = "Safety")]
pub struct SafetySettings {
    #[arg(
        long = "safety-lock-timeout",
        env = "SVALBARD_SAFETY_LOCK_TIMEOUT",
        value_name = "DURATION"
    )]
    pub lock_timeout: Option<String>,
    #[arg(
        long = "safety-contention-timeout",
        env = "SVALBARD_SAFETY_CONTENTION_TIMEOUT",
        value_name = "DURATION"
    )]
    pub contention_timeout: Option<String>,
    #[arg(
        long = "safety-dump-timeout",
        env = "SVALBARD_SAFETY_DUMP_TIMEOUT",
        value_name = "DURATION"
    )]
    pub dump_timeout: Option<String>,
    #[arg(
        long = "safety-max-dump-size",
        env = "SVALBARD_SAFETY_MAX_DUMP_SIZE",
        value_name = "SIZE"
    )]
    pub max_dump_size: Option<String>,
    #[arg(
        long = "safety-min-free-space",
        env = "SVALBARD_SAFETY_MIN_FREE_SPACE",
        value_name = "SIZE"
    )]
    pub min_free_space: Option<String>,
}

/// Validates the configuration without network access.
///
/// Flags and environment variables take precedence over the file, and the file over defaults.
/// `password_env` is the value of [`PASSWORD_ENV`]. Only its presence matters.
pub fn load(
    file: Option<&Path>,
    flags: &Settings,
    password_env: Option<&OsStr>,
) -> Result<Config, String> {
    let file = match file {
        Some(path) => read_file(path)?,
        None => Settings::default(),
    };
    let (source, file_source) = (&flags.source, &file.source);
    let (destination, file_destination) = (&flags.destination, &file.destination);
    let (safety, file_safety) = (&flags.safety, &file.safety);

    let kind = match required("source.type", pick(&source.r#type, &file_source.r#type))?.as_str() {
        "postgresql" => Kind::Postgresql,
        "mysql" => Kind::Mysql,
        value => {
            return Err(format!(
                "source.type is `{value}`. Use `postgresql` or `mysql`."
            ));
        }
    };
    let default_port = match kind {
        Kind::Postgresql => "5432",
        Kind::Mysql => "3306",
    };
    expect_type(
        "destination.type",
        pick(&destination.r#type, &file_destination.r#type),
        "s3",
    )?;

    Ok(Config {
        source: Source {
            kind,
            host: required("source.host", pick(&source.host, &file_source.host))?,
            port: port(pick(&source.port, &file_source.port).unwrap_or(default_port))?,
            database: required(
                "source.database",
                pick(&source.database, &file_source.database),
            )?,
            user: required("source.user", pick(&source.user, &file_source.user))?,
            password: password(
                password_env,
                source
                    .password_file
                    .as_deref()
                    .or(file_source.password_file.as_deref()),
            )?,
            tls: tls(
                optional("source.tls", pick(&source.tls, &file_source.tls))?.as_deref(),
                source
                    .tls_ca_file
                    .as_deref()
                    .or(file_source.tls_ca_file.as_deref()),
            )?,
        },
        destination: Destination {
            bucket: required(
                "destination.bucket",
                pick(&destination.bucket, &file_destination.bucket),
            )?,
            prefix: pick(&destination.prefix, &file_destination.prefix)
                .unwrap_or_default()
                .to_owned(),
            region: optional(
                "destination.region",
                pick(&destination.region, &file_destination.region),
            )?,
        },
        safety: Safety {
            lock_timeout: Duration::from_secs(limit(
                "safety.lock_timeout",
                pick(&safety.lock_timeout, &file_safety.lock_timeout),
                ["10s", "1s", "5min"],
                TIME_UNITS,
            )?),
            contention_timeout: Duration::from_secs(limit(
                "safety.contention_timeout",
                pick(&safety.contention_timeout, &file_safety.contention_timeout),
                ["2s", "1s", "60s"],
                TIME_UNITS,
            )?),
            dump_timeout: Duration::from_secs(limit(
                "safety.dump_timeout",
                pick(&safety.dump_timeout, &file_safety.dump_timeout),
                ["1h", "1min", "24h"],
                TIME_UNITS,
            )?),
            max_dump_size: limit(
                "safety.max_dump_size",
                pick(&safety.max_dump_size, &file_safety.max_dump_size),
                ["20GiB", "1MiB", "16TiB"],
                SIZE_UNITS,
            )?,
            min_free_space: limit(
                "safety.min_free_space",
                pick(&safety.min_free_space, &file_safety.min_free_space),
                ["2GiB", "0B", "1TiB"],
                SIZE_UNITS,
            )?,
        },
    })
}

fn read_file(path: &Path) -> Result<Settings, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    toml::from_str(&text).map_err(|error| {
        // Use the message only: the full error quotes the source line, which can hold a secret.
        let start = error.span().map_or(0, |span| span.start);
        let line = text[..start].matches('\n').count() + 1;
        format!("{}:{line}: {}", path.display(), error.message().trim_end())
    })
}

fn port_number<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Ok(Some(u16::deserialize(deserializer)?.to_string()))
}

fn pick<'a>(flag: &'a Option<String>, file: &'a Option<String>) -> Option<&'a str> {
    flag.as_deref().or(file.as_deref())
}

fn required(key: &str, value: Option<&str>) -> Result<String, String> {
    optional(key, value)?.ok_or_else(|| {
        format!(
            "{key} is required. Set it in the configuration file, with --{} or with SVALBARD_{}.",
            key.replace(['.', '_'], "-"),
            key.replace('.', "_").to_uppercase()
        )
    })
}

fn optional(key: &str, value: Option<&str>) -> Result<Option<String>, String> {
    match value {
        Some("") => Err(format!("{key} is empty.")),
        value => Ok(value.map(str::to_owned)),
    }
}

fn expect_type(key: &str, value: Option<&str>, expected: &str) -> Result<(), String> {
    match required(key, value)? {
        value if value == expected => Ok(()),
        value => Err(format!("{key} is `{value}`. Use `{expected}`.")),
    }
}

fn port(value: &str) -> Result<u16, String> {
    match value.parse() {
        Ok(port) if port != 0 => Ok(port),
        _ => Err(format!(
            "source.port is `{value}`. Use a number from 1 to 65535."
        )),
    }
}

fn password(env: Option<&OsStr>, file: Option<&Path>) -> Result<Password, String> {
    match (env, file) {
        (Some(_), Some(_)) => Err(format!(
            "Set {PASSWORD_ENV} or source.password_file, not both."
        )),
        (Some(value), None) if value.is_empty() => Err(format!("{PASSWORD_ENV} is empty.")),
        (Some(_), None) => Ok(Password::Env),
        (None, Some(path)) => existing_file("source.password_file", path).map(Password::File),
        (None, None) => Ok(Password::None),
    }
}

fn tls(mode: Option<&str>, ca_file: Option<&Path>) -> Result<Tls, String> {
    match (mode.unwrap_or("verify-full"), ca_file) {
        ("verify-full", None) => Ok(Tls::VerifyFull(None)),
        ("verify-full", Some(path)) => Ok(Tls::VerifyFull(Some(existing_file(
            "source.tls_ca_file",
            path,
        )?))),
        ("disable", None) => Ok(Tls::Disable),
        ("disable", Some(_)) => {
            Err("source.tls_ca_file needs source.tls `verify-full`.".to_owned())
        }
        (mode, _) => Err(format!(
            "source.tls is `{mode}`. Use `verify-full` or `disable`."
        )),
    }
}

/// Checks that `path` opens and is a file, without reading it.
fn existing_file(key: &str, path: &Path) -> Result<PathBuf, String> {
    match File::open(path).and_then(|file| file.metadata()) {
        Ok(metadata) if metadata.is_file() => Ok(path.to_owned()),
        Ok(_) => Err(format!("{key} {} is not a file.", path.display())),
        Err(error) => Err(format!(
            "{key} {} cannot be opened: {error}",
            path.display()
        )),
    }
}

/// Parses `value`, or the default if it is missing, and checks it against the range.
fn limit(
    key: &str,
    value: Option<&str>,
    [default, min, max]: [&str; 3],
    units: &[(&str, u64)],
) -> Result<u64, String> {
    let amount = quantity(key, value.unwrap_or(default), units)?;
    if amount < quantity(key, min, units)? || amount > quantity(key, max, units)? {
        return Err(format!("{key} must be from {min} to {max}."));
    }
    Ok(amount)
}

/// Parses a whole number and a unit, such as `10s` or `20 GiB`, into the base unit.
fn quantity(key: &str, value: &str, units: &[(&str, u64)]) -> Result<u64, String> {
    let digits = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(digits);
    let factor = units
        .iter()
        .find(|(name, _)| *name == unit.trim_start())
        .map(|&(_, factor)| factor);
    number
        .parse::<u64>()
        .ok()
        .zip(factor)
        .and_then(|(number, factor)| number.checked_mul(factor))
        .ok_or_else(|| {
            let names: Vec<_> = units.iter().map(|(name, _)| *name).collect();
            format!(
                "{key} is `{value}`. Use a whole number and one of these units: {}.",
                names.join(", ")
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantities_accept_whole_numbers_with_known_units() {
        assert_eq!(quantity("k", "10s", TIME_UNITS), Ok(10));
        assert_eq!(quantity("k", "2 min", TIME_UNITS), Ok(120));
        assert_eq!(quantity("k", "20GiB", SIZE_UNITS), Ok(20 << 30));
        assert_eq!(quantity("k", "0B", SIZE_UNITS), Ok(0));
    }

    #[test]
    fn quantities_reject_missing_units_fractions_and_overflow() {
        for value in ["10", "s", "", "1.5h", "-1s", "10 m", "10s ", "99999999TiB"] {
            let units = if value.ends_with('B') {
                SIZE_UNITS
            } else {
                TIME_UNITS
            };
            assert!(quantity("k", value, units).is_err(), "value: {value:?}");
        }
    }
}
