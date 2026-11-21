//! A stand-in agent for the harness tests. It prints, sleeps, and exits as its arguments say.
//!
//! Usage: `fake_agent <lines> <bytes_per_line> <sleep_seconds> <exit_code>`
//!
//! It first writes `cwd=<dir>` to stderr, and `echo=<value>` to stdout when the environment
//! holds `FAKE_AGENT_ECHO`. Then it prints `lines` lines of `bytes_per_line` bytes each,
//! newline included, to stdout. Then it sleeps `sleep_seconds` and exits with `exit_code`.

use std::io::Write;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

const LINES_MAX: u64 = 1 << 20;
const LINE_BYTES_MAX: u64 = 1 << 20;
const SLEEP_SECONDS_MAX: u64 = 600;

fn number(args: &[String], index: usize) -> u64 {
    assert!(index < args.len());
    let parsed = args[index].parse::<u64>();
    assert!(parsed.is_ok(), "argument {index} is not a number");
    parsed.unwrap_or(0)
}

fn print_lines(lines: u64, bytes_per_line: u64) {
    assert!(lines <= LINES_MAX && bytes_per_line <= LINE_BYTES_MAX);
    assert!(
        lines == 0 || bytes_per_line >= 1,
        "every line ends in a newline"
    );
    let mut line = vec![b'x'; usize::try_from(bytes_per_line).unwrap_or(0)];
    if let Some(last) = line.last_mut() {
        *last = b'\n';
    }
    let mut out = std::io::stdout().lock();
    for _ in 0..lines {
        if out.write_all(&line).is_err() {
            return;
        }
    }
    let _ = out.flush();
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    assert!(args.len() == 4, "usage: see the file header");
    let (lines, bytes_per_line) = (number(&args, 0), number(&args, 1));
    let seconds = number(&args, 2);
    let code = u8::try_from(number(&args, 3)).unwrap_or(u8::MAX);
    assert!(seconds <= SLEEP_SECONDS_MAX);
    let cwd = std::env::current_dir().map(|d| d.display().to_string());
    eprintln!("cwd={}", cwd.unwrap_or_default());
    if let Ok(value) = std::env::var("FAKE_AGENT_ECHO") {
        println!("echo={value}");
    }
    print_lines(lines, bytes_per_line);
    thread::sleep(Duration::from_secs(seconds));
    ExitCode::from(code)
}
