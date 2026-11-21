//! `harness::run` against `fake_agent`, a stand-in for a real agent CLI.

use rider::harness::{
    CommandLine, Exit, HarnessError, PROMPT_BYTES_MAX, command_for, run, run_on_clock,
};
use rider::roles::{AGENTS_MAX, role_from_label};
use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{PoisonError, RwLock, RwLockReadGuard, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use tempfile::{Builder, TempDir};

/// A fresh directory under the system temp dir, removed when dropped.
struct Scratch {
    _dir: TempDir,
    root: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = Builder::new()
            .prefix(&format!("rider-harness-{name}-"))
            .tempdir()
            .unwrap();
        let root = dir.path().canonicalize().unwrap();
        Scratch { _dir: dir, root }
    }
}

static READER_BUDGET: RwLock<()> = RwLock::new(());

fn sharing_the_reader_budget() -> RwLockReadGuard<'static, ()> {
    READER_BUDGET.read().unwrap_or_else(PoisonError::into_inner)
}

fn fake(args: &[&str]) -> CommandLine {
    CommandLine {
        program: env!("CARGO_BIN_EXE_fake_agent").to_string(),
        args: args.iter().map(|a| a.to_string()).collect(),
        env: Vec::new(),
    }
}

#[test]
fn a_prompt_a_cli_could_misread_is_an_error() {
    let claude = role_from_label("implement").unwrap();
    let codex = role_from_label("review").unwrap();
    for prompt in ["", " \n\t ", "-", "mcp", "doctor", "--help"] {
        for spec in [claude, codex] {
            let result = command_for(spec, prompt, Path::new("/w"), None);
            assert!(
                matches!(result, Err(HarnessError::PromptUnsafe)),
                "{prompt:?}"
            );
        }
    }
}

#[test]
fn an_oversized_prompt_is_an_error() {
    let spec = role_from_label("implement").unwrap();
    let prompt = "x ".repeat(PROMPT_BYTES_MAX / 2);
    assert_eq!(prompt.len(), PROMPT_BYTES_MAX);
    let c = command_for(spec, &prompt, Path::new("/w"), None).unwrap();
    assert_eq!(c.args.last(), Some(&prompt));
    let over = format!("{prompt}x");
    let result = command_for(spec, &over, Path::new("/w"), None);
    assert!(matches!(
        result,
        Err(HarnessError::PromptTooLarge { bytes }) if bytes == PROMPT_BYTES_MAX + 1
    ));
}

#[test]
fn reader_slots_are_returned_after_each_run() {
    let _sharing = sharing_the_reader_budget();
    const RUNS: usize = 4 * 2 * AGENTS_MAX as usize / 2 + 1;
    let scratch = Scratch::new("slots");
    let log = scratch.root.join("agent.log");
    for _ in 0..RUNS {
        let exit = run(&fake(&["0", "0", "0", "0"]), &scratch.root, &log, 10, 4096);
        assert_eq!(exit.unwrap(), Exit::Code(0));
    }
}

#[test]
fn a_log_write_that_hangs_does_not_stop_the_time_limit() {
    let _sharing = sharing_the_reader_budget();
    let scratch = Scratch::new("fifo");
    let log = scratch.root.join("agent.log");
    let made = Command::new("mkfifo")
        .arg(&log)
        .stdin(Stdio::null())
        .status();
    assert_eq!(made.map(|status| status.success()).ok(), Some(true));
    let (release, released) = mpsc::sync_channel::<()>(0);
    let fifo = log.clone();
    let unread = thread::spawn(move || {
        let reader = fs::File::open(fifo).unwrap();
        let _ = released.recv();
        drop(reader);
    });
    let (sent, finished) = mpsc::sync_channel(1);
    let cmd = fake(&["1024", "4096", "60", "0"]);
    let root = scratch.root.clone();
    thread::spawn(move || {
        let start = Instant::now();
        let exit = run(&cmd, &root, &log, 1, 1 << 24);
        let _ = sent.send((exit, start.elapsed()));
    });
    let outcome = finished.recv_timeout(Duration::from_secs(20));
    drop(release);
    unread.join().unwrap();
    let (exit, elapsed) = outcome.expect("run returns while its log write hangs");
    assert_eq!(exit.unwrap(), Exit::TimedOut);
    assert!(elapsed < Duration::from_secs(15), "{elapsed:?}");
}

fn run_with_wall_clock(name: &str, shift: fn(SystemTime) -> SystemTime) -> (Exit, Duration) {
    let scratch = Scratch::new(name);
    let log = scratch.root.join("agent.log");
    let read = Cell::new(false);
    let wall_clock = || {
        let now = SystemTime::now();
        if read.replace(true) { shift(now) } else { now }
    };
    let _sharing = sharing_the_reader_budget();
    let start = Instant::now();
    let cmd = fake(&["1", "10", "20", "0"]);
    let exit = run_on_clock(&cmd, &scratch.root, &log, 2, 4096, &wall_clock);
    (exit.unwrap(), start.elapsed())
}

#[test]
fn a_machine_that_slept_past_the_limit_ends_the_attempt_at_once() {
    let (exit, elapsed) = run_with_wall_clock("slept", |now| now + Duration::from_secs(3600));
    assert_eq!(exit, Exit::TimedOut);
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
}

#[test]
fn a_wall_clock_set_back_counts_no_time_and_the_attempt_ends_at_its_limit() {
    let (exit, elapsed) = run_with_wall_clock("back", |now| now - Duration::from_secs(3600));
    assert_eq!(exit, Exit::TimedOut);
    assert!(elapsed >= Duration::from_secs(2), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
}

#[test]
fn a_run_past_the_reader_budget_is_refused_and_the_holders_finish() {
    const HOLDERS_MAX: usize = 4 * 2 * AGENTS_MAX as usize / 2 + 1;
    const POLLS_MAX: u32 = 1000;
    let _alone = READER_BUDGET
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    let scratch = Scratch::new("budget");
    let release = scratch.root.join("release");
    let hold = format!(
        r#"echo started > "$0"; n=0
until [ -e '{}' ] || [ "$n" -gt 600 ]; do n=$((n + 1)); sleep 0.05; done"#,
        release.display()
    );
    let mut holders = Vec::new();
    let mut refused = None;
    for index in 0..HOLDERS_MAX {
        let started = scratch.root.join(format!("started-{index}"));
        let cmd = CommandLine {
            program: "/bin/sh".to_string(),
            args: vec![
                "-c".to_string(),
                hold.clone(),
                started.display().to_string(),
            ],
            env: Vec::new(),
        };
        let (root, log) = (
            scratch.root.clone(),
            scratch.root.join(format!("{index}.log")),
        );
        let holder = thread::spawn(move || run(&cmd, &root, &log, 60, 4096));
        let mut polls = 0;
        while !started.exists() && !holder.is_finished() && polls < POLLS_MAX {
            polls += 1;
            thread::sleep(Duration::from_millis(10));
        }
        if holder.is_finished() {
            refused = Some(holder.join());
            break;
        }
        holders.push(holder);
    }
    fs::write(&release, "").unwrap();
    for holder in holders {
        assert_eq!(holder.join().unwrap().unwrap(), Exit::Code(0));
    }
    let refused = refused.expect("a run past the budget finishes at once");
    let refused = refused.expect("a run past the budget returns, not panics");
    assert!(
        matches!(refused, Err(HarnessError::ReadersExhausted)),
        "{refused:?}"
    );
}
