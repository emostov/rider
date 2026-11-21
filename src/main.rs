use std::env;
use std::ffi::OsString;
use std::process::ExitCode;

use clap::Parser;
use rider::cli::{self, ARGS_MAX, Cli, Command};

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().take(ARGS_MAX.saturating_add(2)).collect();
    if args.len() > ARGS_MAX.saturating_add(1) {
        eprintln!("rider: more than {ARGS_MAX} arguments");
        return ExitCode::from(2);
    }
    let mut cli = Cli::parse_from(&args);
    // Clap drops a `--` that comes first after `vb`, so vb gets rider's own arguments.
    if let Command::Vb { args: passed } = &mut cli.command {
        assert_eq!(args.get(1).and_then(|arg| arg.to_str()), Some("vb"));
        *passed = args.get(2..).map(<[OsString]>::to_vec).unwrap_or_default();
    }
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
