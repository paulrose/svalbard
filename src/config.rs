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
    pub target: Target,
    pub prefix: String,
    pub upload_timeout: Duration,
}

/// Where the backups go.
#[derive(Debug)]
pub enum Target {
    ObjectStore(ObjectStore),
    Ssh(Ssh),
}

/// A bucket of a service with the S3 API.
#[derive(Debug)]
pub struct ObjectStore {
    pub provider: Provider,
    pub bucket: String,
    /// `auto` for R2.
    pub region: Option<String>,
    /// An S3-compatible endpoint instead of AWS, such as `https://s3.example.com`. Always set for
    /// R2.
    pub endpoint: Option<String>,
}

/// The storage service of an object store. Both use the S3 API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    S3,
    R2,
}

/// A directory on an SSH server.
#[derive(Clone, Debug)]
pub struct Ssh {
    pub mode: SshMode,
    pub host: String,
    pub port: u16,
    pub user: String,
    /// An absolute directory without `\`, `"` or line breaks.
    pub path: String,
    /// An absolute path. Without it, the SSH agent and the default keys.
    pub identity_file: Option<PathBuf>,
    /// An absolute path. Without it, the default known_hosts files of OpenSSH.
    pub known_hosts_file: Option<PathBuf>,
}

/// The transfer protocol of an SSH destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SshMode {
    Sftp,
    /// The legacy SCP protocol, through the remote shell.
    Scp,
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
    #[arg(
        long = "destination-endpoint",
        env = "SVALBARD_DESTINATION_ENDPOINT",
        value_name = "URL"
    )]
    pub endpoint: Option<String>,
    #[arg(
        id = "destination-host",
        long = "destination-host",
        env = "SVALBARD_DESTINATION_HOST",
        value_name = "HOST"
    )]
    pub host: Option<String>,
    #[arg(
        id = "destination-port",
        long = "destination-port",
        env = "SVALBARD_DESTINATION_PORT",
        value_name = "PORT"
    )]
    #[serde(deserialize_with = "port_number")]
    pub port: Option<String>,
    #[arg(
        id = "destination-user",
        long = "destination-user",
        env = "SVALBARD_DESTINATION_USER",
        value_name = "USER"
    )]
    pub user: Option<String>,
    #[arg(
        long = "destination-path",
        env = "SVALBARD_DESTINATION_PATH",
        value_name = "PATH"
    )]
    pub path: Option<String>,
    #[arg(
        long = "destination-identity-file",
        env = "SVALBARD_DESTINATION_IDENTITY_FILE",
        value_name = "PATH"
    )]
    pub identity_file: Option<PathBuf>,
    #[arg(
        long = "destination-known-hosts-file",
        env = "SVALBARD_DESTINATION_KNOWN_HOSTS_FILE",
        value_name = "PATH"
    )]
    pub known_hosts_file: Option<PathBuf>,
    #[arg(
        long = "destination-upload-timeout",
        env = "SVALBARD_DESTINATION_UPLOAD_TIMEOUT",
        value_name = "DURATION"
    )]
    pub upload_timeout: Option<String>,
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

    Ok(Config {
        source: Source {
            kind,
            host: required("source.host", pick(&source.host, &file_source.host))?,
            port: port(
                "source.port",
                pick(&source.port, &file_source.port).unwrap_or(default_port),
            )?,
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
            target: target(destination, file_destination)?,
            prefix: prefix(destination, file_destination)?,
            upload_timeout: Duration::from_secs(limit(
                "destination.upload_timeout",
                pick(
                    &destination.upload_timeout,
                    &file_destination.upload_timeout,
                ),
                ["1h", "1min", "24h"],
                TIME_UNITS,
            )?),
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

fn port(key: &str, value: &str) -> Result<u16, String> {
    match value.parse() {
        Ok(port) if port != 0 => Ok(port),
        _ => Err(format!("{key} is `{value}`. Use a number from 1 to 65535.")),
    }
}

fn target(flags: &DestinationSettings, file: &DestinationSettings) -> Result<Target, String> {
    let kind = required("destination.type", pick(&flags.r#type, &file.r#type))?;
    let ssh_keys = [
        ("host", set(&flags.host, &file.host)),
        ("port", set(&flags.port, &file.port)),
        ("user", set(&flags.user, &file.user)),
        ("path", set(&flags.path, &file.path)),
        (
            "identity_file",
            set(&flags.identity_file, &file.identity_file),
        ),
        (
            "known_hosts_file",
            set(&flags.known_hosts_file, &file.known_hosts_file),
        ),
    ];
    let object_store_keys = [
        ("bucket", set(&flags.bucket, &file.bucket)),
        ("region", set(&flags.region, &file.region)),
        ("endpoint", set(&flags.endpoint, &file.endpoint)),
    ];
    let unused = match kind.as_str() {
        "s3" | "r2" => &ssh_keys[..],
        "sftp" | "scp" => &object_store_keys[..],
        value => {
            return Err(format!(
                "destination.type is `{value}`. Use `s3`, `r2`, `sftp` or `scp`."
            ));
        }
    };
    if let Some((key, _)) = unused.iter().find(|(_, set)| *set) {
        return Err(format!(
            "destination.{key} does not apply to destination.type `{kind}`."
        ));
    }
    Ok(match kind.as_str() {
        "s3" => Target::ObjectStore(object_store(Provider::S3, flags, file)?),
        "r2" => Target::ObjectStore(object_store(Provider::R2, flags, file)?),
        "sftp" => Target::Ssh(ssh(SshMode::Sftp, flags, file)?),
        _ => Target::Ssh(ssh(SshMode::Scp, flags, file)?),
    })
}

fn set<T>(flag: &Option<T>, file: &Option<T>) -> bool {
    flag.is_some() || file.is_some()
}

fn object_store(
    provider: Provider,
    flags: &DestinationSettings,
    file: &DestinationSettings,
) -> Result<ObjectStore, String> {
    let endpoint_value = pick(&flags.endpoint, &file.endpoint);
    Ok(ObjectStore {
        provider,
        bucket: required("destination.bucket", pick(&flags.bucket, &file.bucket))?,
        region: optional("destination.region", pick(&flags.region, &file.region))?
            .or((provider == Provider::R2).then(|| "auto".to_owned())),
        endpoint: endpoint(
            provider,
            match provider {
                Provider::S3 => optional("destination.endpoint", endpoint_value)?,
                Provider::R2 => Some(required("destination.endpoint", endpoint_value)?),
            },
        )?,
    })
}

/// The values go to OpenSSH and, for SCP, to the remote shell, so only plain characters pass.
fn ssh(
    mode: SshMode,
    flags: &DestinationSettings,
    file: &DestinationSettings,
) -> Result<Ssh, String> {
    let plain = |value: &str, extra: &str| {
        !value.starts_with('-')
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
    };
    let host = required("destination.host", pick(&flags.host, &file.host))?;
    if !plain(&host, ".:-") {
        return Err(format!(
            "destination.host is `{host}`. Use a host name or an IP address."
        ));
    }
    let user = required("destination.user", pick(&flags.user, &file.user))?;
    if !plain(&user, "._-") {
        return Err(format!(
            "destination.user is `{user}`. Use letters, digits, `.`, `_` and `-`."
        ));
    }
    let path = required("destination.path", pick(&flags.path, &file.path))?;
    if !path.starts_with('/') || path.contains(['\\', '"', '\n', '\r', '\0']) {
        return Err(format!(
            "destination.path is `{path}`. Use an absolute path without `\\`, `\"` or line breaks."
        ));
    }
    Ok(Ssh {
        mode,
        host,
        port: port(
            "destination.port",
            pick(&flags.port, &file.port).unwrap_or("22"),
        )?,
        user,
        path,
        identity_file: ssh_file(
            "destination.identity_file",
            flags
                .identity_file
                .as_deref()
                .or(file.identity_file.as_deref()),
        )?,
        known_hosts_file: ssh_file(
            "destination.known_hosts_file",
            flags
                .known_hosts_file
                .as_deref()
                .or(file.known_hosts_file.as_deref()),
        )?,
    })
}

/// An existing file for an OpenSSH option, as an absolute path. OpenSSH expands `${…}` in it
/// and splits it at quotes.
fn ssh_file(key: &str, path: Option<&Path>) -> Result<Option<PathBuf>, String> {
    let Some(path) = path else {
        return Ok(None);
    };
    let path = std::path::absolute(existing_file(key, path)?)
        .map_err(|error| format!("{key} {}: {error}", path.display()))?;
    match path.to_str() {
        Some(text) if !text.contains(['"', '\\']) && !text.contains("${") => Ok(Some(path)),
        _ => Err(format!(
            "{key} {} must be UTF-8, without `\"`, `\\` or `${{`.",
            path.display()
        )),
    }
}

/// For SSH, the prefix becomes part of remote file names, so only plain characters pass.
fn prefix(flags: &DestinationSettings, file: &DestinationSettings) -> Result<String, String> {
    let prefix = pick(&flags.prefix, &file.prefix).unwrap_or_default();
    let ssh = matches!(pick(&flags.r#type, &file.r#type), Some("sftp" | "scp"));
    if ssh
        && !prefix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c))
    {
        return Err(format!(
            "destination.prefix is `{prefix}`. For SSH, use letters, digits, `.`, `_`, `-` and `/`."
        ));
    }
    Ok(prefix.to_owned())
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

/// Checks the scheme. A URL with `user:password@` would put a secret in the configuration.
/// An R2 endpoint has no path: the R2 dashboard shows bucket URLs, and the bucket would appear
/// twice in each request.
fn endpoint(provider: Provider, url: Option<String>) -> Result<Option<String>, String> {
    let Some(url) = url else {
        return Ok(None);
    };
    // Checked first, so that the next errors never repeat a secret.
    if url.contains('@') {
        return Err("destination.endpoint must not contain credentials.".to_owned());
    }
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return Err(format!(
            "destination.endpoint is `{url}`. Use an http:// or https:// URL."
        ));
    };
    if provider == Provider::R2 && rest.trim_end_matches('/').contains(['/', '?', '#']) {
        return Err(format!(
            "destination.endpoint is `{url}`. Remove the path: the bucket goes in destination.bucket."
        ));
    }
    // As `s3`, R2 would reject every multipart upload, so only small dumps would succeed.
    let host = rest.split(['/', '?', '#', ':']).next().unwrap_or_default();
    if provider == Provider::S3 && host.ends_with(".r2.cloudflarestorage.com") {
        return Err(format!(
            "destination.endpoint is `{url}`, a Cloudflare R2 endpoint. Set destination.type to `r2`."
        ));
    }
    Ok(Some(url))
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
