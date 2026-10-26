use std::{env, path::PathBuf, process::ExitCode};

use clap::{Args, Parser, Subcommand};
use svalbard::{
    backup::{self, Failed, Stage},
    config::{self, Config, PASSWORD_ENV, Settings},
};
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::watch,
};

/// Exit code for an invalid configuration. Usage errors exit with 2.
const INVALID_CONFIG: u8 = 3;
/// Exit code for a failed backup after a complete cleanup.
const FAILED: u8 = 4;
/// Exit code for a failed backup whose cleanup failed too.
const CLEANUP_FAILED: u8 = 5;

/// Database backups
#[derive(Parser)]
#[command(version, arg_required_else_help = false)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Work with the configuration
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Back up the source to the destination
    ///
    /// Svalbard checks the destination, dumps the source into a private run directory, closes
    /// the snapshot, uploads the dump and then its completion record, and removes the run
    /// directory. SIGINT and SIGTERM cancel the backup and clean up.
    ///
    /// Exit codes: 0 the backup is complete, 2 usage error, 3 invalid configuration, 4 the
    /// backup failed and Svalbard cleaned up, 5 the cleanup failed too, so a backup session,
    /// the run directory or an unfinished upload may remain.
    Backup(Input),
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Validate the configuration without network access
    ///
    /// A flag takes precedence over its environment variable, the variable over the
    /// configuration file, and the file over the default. The source password comes from
    /// SVALBARD_SOURCE_PASSWORD or source.password_file.
    ///
    /// A DURATION is a whole number with s, min or h, such as 10s. A SIZE is a whole
    /// number with B, KiB, MiB, GiB or TiB, such as 20GiB.
    ///
    /// The check does not connect to the database or to storage. It cannot check
    /// connectivity, credentials or permissions.
    Check(Input),
}

#[derive(Args)]
struct Input {
    /// Configuration file in TOML format
    #[arg(long, env = "SVALBARD_CONFIG", value_name = "PATH")]
    config: Option<PathBuf>,
    /// Print the result as JSON on stdout
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    settings: Settings,
}

impl Input {
    fn load(&self) -> Result<Config, String> {
        config::load(
            self.config.as_deref(),
            &self.settings,
            env::var_os(PASSWORD_ENV).as_deref(),
        )
    }
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Config(ConfigCommand::Check(input)) => check(&input),
        Command::Backup(input) => run_backup(&input),
    }
}

fn check(input: &Input) -> ExitCode {
    let result = input.load();
    if input.json {
        let output = match &result {
            Ok(config) => serde_json::json!({
                "valid": true,
                "excluded_tables": config.source.exclude_tables,
            }),
            Err(error) => serde_json::json!({ "valid": false, "error": error }),
        };
        println!("{output}");
    }
    match result {
        Ok(config) => {
            if !input.json {
                println!("Configuration is valid.");
                if let Some(tables) = excluded(&config.source.exclude_tables) {
                    println!("Backups leave out these tables: {tables}.");
                }
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(INVALID_CONFIG)
        }
    }
}

fn run_backup(input: &Input) -> ExitCode {
    let result = match input.load() {
        Ok(config) => backup(config),
        Err(error) => Err(Failed::new(
            Stage::Configuration,
            "invalid_configuration",
            error,
            None,
        )),
    };
    match result {
        Ok(report) => {
            if input.json {
                let mut output = serde_json::to_value(&report).expect("the report serializes");
                output["result"] = "complete".into();
                println!("{output}");
            } else {
                println!("Backup complete: {} and {}.", report.data, report.record);
                if let Some(tables) = excluded(&report.excluded_tables) {
                    println!("Excluded tables: {tables}.");
                }
            }
            ExitCode::SUCCESS
        }
        Err(failed) => {
            if input.json {
                let mut output = serde_json::to_value(&failed).expect("the failure serializes");
                output["result"] = "failed".into();
                println!("{output}");
            }
            eprintln!("error: {}: {}", failed.stage, failed.message);
            ExitCode::from(match (failed.stage, failed.error.as_str()) {
                (Stage::Configuration, _) => INVALID_CONFIG,
                (_, "cleanup_failed") => CLEANUP_FAILED,
                _ => FAILED,
            })
        }
    }
}

/// The excluded tables as a list for people, or `None` if there are none.
fn excluded(tables: &[String]) -> Option<String> {
    (!tables.is_empty()).then(|| tables.join(", "))
}

/// Runs the backup on one thread: the dump futures are not `Send`, and the client's
/// parent-death signal needs a thread that lives as long as the run.
fn backup(config: Config) -> Result<backup::Report, Failed> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("the runtime starts");
    runtime.block_on(async {
        let (sender, cancel) = watch::channel(false);
        // Registered before any work. The handlers stay, so a second signal changes nothing.
        let mut interrupt = signal(SignalKind::interrupt()).expect("SIGINT can be handled");
        let mut terminate = signal(SignalKind::terminate()).expect("SIGTERM can be handled");
        tokio::spawn(async move {
            tokio::select! {
                _ = interrupt.recv() => {}
                _ = terminate.recv() => {}
            }
            eprintln!("svalbard: cancelling after a signal");
            let _ = sender.send(true);
        });
        backup::backup(config, cancel).await
    })
}
