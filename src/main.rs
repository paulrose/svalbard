use std::{env, path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use svalbard::config::{self, PASSWORD_ENV, Settings};

/// Exit code for an invalid configuration. Usage errors exit with 2.
const INVALID_CONFIG: u8 = 3;

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
    Check {
        /// Configuration file in TOML format
        #[arg(long, env = "SVALBARD_CONFIG", value_name = "PATH")]
        config: Option<PathBuf>,
        /// Print the result as JSON on stdout
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        settings: Settings,
    },
}

fn main() -> ExitCode {
    let Command::Config(ConfigCommand::Check {
        config,
        json,
        settings,
    }) = Cli::parse().command;
    let result = config::load(
        config.as_deref(),
        &settings,
        env::var_os(PASSWORD_ENV).as_deref(),
    );

    if json {
        let output = match &result {
            Ok(_) => serde_json::json!({ "valid": true }),
            Err(error) => serde_json::json!({ "valid": false, "error": error }),
        };
        println!("{output}");
    }
    match result {
        Ok(_) => {
            if !json {
                println!("Configuration is valid.");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(INVALID_CONFIG)
        }
    }
}
