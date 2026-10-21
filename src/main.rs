use std::{env, process::ExitCode};

const HELP: &str = "Svalbard — database backups

Usage: svalbard <OPTION>

Options:
  -h, --help       Print help
  -V, --version    Print version
";

fn main() -> ExitCode {
    let mut arguments = env::args_os().skip(1);

    // shortcut: only help/version exist; use a CLI parser when subcommands arrive.
    match (arguments.next(), arguments.next()) {
        (Some(flag), None) if flag == "--help" || flag == "-h" => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        (Some(flag), None) if flag == "--version" || flag == "-V" => {
            println!("svalbard {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("error: expected --help or --version\n\nRun 'svalbard --help' for usage.");
            ExitCode::from(2)
        }
    }
}
