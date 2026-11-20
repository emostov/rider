use std::env;
use std::ffi::OsString;
use std::process::ExitCode;

use clap::Parser;
use rider::cli::{self, ARGS_MAX, Cli};

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().take(ARGS_MAX.saturating_add(2)).collect();
    if args.len() > ARGS_MAX.saturating_add(1) {
        eprintln!("rider: more than {ARGS_MAX} arguments");
        return ExitCode::from(2);
    }
    let cli = Cli::parse_from(args);
    let config = match cli::config(cli::read_vars()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("rider: {error}");
            return ExitCode::from(1);
        }
    };
    match cli::execute(&cli.command, &config) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("rider: {error}");
            ExitCode::from(1)
        }
    }
}
