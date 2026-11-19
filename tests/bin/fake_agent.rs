//! A stand-in agent for the harness tests. It prints, sleeps, and exits as its arguments say.
//!
//! Usage: `fake_agent <lines> <bytes_per_line> <sleep_seconds> <exit_code> [mode]`
//!
//! It first writes `cwd=<dir>` to stderr, and `echo=<value>` to stdout when the environment
//! holds `FAKE_AGENT_ECHO`. Then it prints `lines` lines of `bytes_per_line` bytes each,
//! newline included, to stdout. Then, by `mode`:
//! - none: sleeps `sleep_seconds`, then exits with `exit_code`;
//! - `ignore-term`: becomes `sh`, which ignores SIGTERM and sleeps `sleep_seconds`;
//! - `orphan`: starts `sleep <sleep_seconds>` holding stdout open, then exits at once;
//! - `kill-self`: ends itself with SIGKILL;
//! - `escape`: starts `sleep <sleep_seconds>` in a process group of its own, holding stdout
//!   open, then exits at once.

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};
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
    assert!(
        args.len() == 4 || args.len() == 5,
        "usage: see the file header"
    );
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
    match args.get(4).map(String::as_str) {
        None => thread::sleep(Duration::from_secs(seconds)),
        Some("ignore-term") => {
            let script = format!("trap '' TERM; sleep {seconds}");
            let error = Command::new("sh").args(["-c", &script]).exec();
            panic!("exec sh: {error}");
        }
        Some("orphan") => {
            let spawned = Command::new("sleep").arg(seconds.to_string()).spawn();
            assert!(spawned.is_ok(), "sleep did not start");
        }
        Some("escape") => {
            let script = format!("set -m; sleep {seconds} &");
            let status = Command::new("sh").args(["-c", &script]).status();
            assert!(status.is_ok_and(|s| s.success()), "sh did not start sleep");
        }
        Some("kill-self") => {
            let pid = std::process::id().to_string();
            let _ = Command::new("kill").args(["-s", "KILL", &pid]).status();
            thread::sleep(Duration::from_secs(seconds));
        }
        Some(other) => panic!("unknown mode {other}"),
    }
    ExitCode::from(code)
}
