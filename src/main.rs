use std::env;
use std::process::ExitCode;

use rider::cli::{self, ARGS_MAX, Command, USAGE};

fn main() -> ExitCode {
    // Every command takes at most two words, so the bound still keeps an extra one to report.
    let args: Vec<String> = env::args_os()
        .skip(1)
        .take(ARGS_MAX)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    assert!(args.len() <= ARGS_MAX);
    let command = match cli::parse(&args) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("rider: {error}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if command == Command::Help {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let config = match cli::config(cli::read_vars()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("rider: {error}");
            return ExitCode::from(1);
        }
    };
    match cli::execute(&command, &config) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("rider: {error}");
            ExitCode::from(1)
        }
    }
}
