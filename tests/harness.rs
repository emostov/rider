//! `harness::run` against `fake_agent`, a stand-in for a real agent CLI.

use rider::harness::{
    CommandLine, Exit, HarnessError, Io, KILL_GRACE_SECONDS, last_message, run,
    stream_reports_failure,
};
use rider::roles::Harness;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::{Duration, Instant};
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

fn fake(args: &[&str]) -> CommandLine {
    CommandLine {
        program: env!("CARGO_BIN_EXE_fake_agent").to_string(),
        args: args.iter().map(|a| a.to_string()).collect(),
        env: Vec::new(),
        io: Io::Stream,
    }
}

#[test]
fn the_recorded_codex_review_output_is_its_final_text() {
    let log = Path::new("fixtures/harness/codex-review.txt");
    let recorded = fs::read_to_string(log).unwrap();
    let text = last_message(&Harness::CodexReview, log).unwrap();
    assert_eq!(text, recorded.trim());
    assert!(text.starts_with("The new function returns the wrong number of items"));
    assert!(!stream_reports_failure(&Harness::CodexReview, log));
}

#[test]
fn exit_code_is_reported() {
    let scratch = Scratch::new("exit");
    let log = scratch.root.join("logs").join("deep").join("agent.log");
    let mut cmd = fake(&["2", "5", "0", "3"]);
    cmd.env
        .push(("FAKE_AGENT_ECHO".to_string(), "hello".to_string()));
    let exit = run(&cmd, &scratch.root, &log, 10, 4096).unwrap();
    assert_eq!(exit, Exit::Code(3));
    let text = fs::read_to_string(&log).unwrap();
    assert!(
        text.contains("echo=hello"),
        "stdout and the env reach the log"
    );
    let cwd = format!("cwd={}", scratch.root.display());
    assert!(
        text.contains(&cwd),
        "stderr reaches the log and cwd is the dir"
    );
    assert!(text.contains("xxxx\n"));
}

#[test]
fn log_stops_growing_at_the_cap() {
    let scratch = Scratch::new("cap");
    let log = scratch.root.join("agent.log");
    let exit = run(
        &fake(&["256", "4096", "0", "0"]),
        &scratch.root,
        &log,
        10,
        4096,
    );
    // The agent exits on its own, so rider kept draining past the cap.
    assert_eq!(exit.unwrap(), Exit::Code(0));
    assert_eq!(fs::metadata(&log).unwrap().len(), 4096);
}

#[test]
fn a_hung_agent_is_killed_at_the_limit() {
    let scratch = Scratch::new("hung");
    let log = scratch.root.join("agent.log");
    let start = Instant::now();
    let exit = run(&fake(&["1", "10", "60", "0"]), &scratch.root, &log, 1, 4096);
    assert_eq!(exit.unwrap(), Exit::TimedOut);
    // Only SIGTERM, not the SIGKILL after the grace, can end it this soon.
    assert!(start.elapsed() < Duration::from_secs(4));
    assert!(start.elapsed() >= Duration::from_secs(1));
}

#[test]
fn an_agent_that_ignores_sigterm_is_killed_after_the_grace() {
    let scratch = Scratch::new("stubborn");
    let log = scratch.root.join("agent.log");
    let start = Instant::now();
    let cmd = fake(&["1", "10", "60", "0", "ignore-term"]);
    let exit = run(&cmd, &scratch.root, &log, 1, 4096);
    assert_eq!(exit.unwrap(), Exit::TimedOut);
    assert!(start.elapsed() >= Duration::from_secs(1 + KILL_GRACE_SECONDS));
    assert!(start.elapsed() < Duration::from_secs(15));
}

#[test]
fn a_leftover_process_holding_the_pipe_does_not_stall_rider() {
    let scratch = Scratch::new("orphan");
    let log = scratch.root.join("agent.log");
    let start = Instant::now();
    let cmd = fake(&["1", "10", "30", "0", "orphan"]);
    let exit = run(&cmd, &scratch.root, &log, 20, 4096);
    assert_eq!(exit.unwrap(), Exit::Code(0));
    // Under the readers' 5 s drain limit: the group kill closed the leftover's pipe.
    assert!(start.elapsed() < Duration::from_secs(4));
}

#[test]
fn a_process_that_left_the_group_does_not_stall_rider() {
    let scratch = Scratch::new("escape");
    let log = scratch.root.join("agent.log");
    let start = Instant::now();
    let cmd = fake(&["1", "10", "7", "0", "escape"]);
    let exit = run(&cmd, &scratch.root, &log, 20, 4096);
    assert_eq!(exit.unwrap(), Exit::Code(0));
    assert!(start.elapsed() < Duration::from_secs(9));
    assert_eq!(
        fs::read_to_string(&log).unwrap().matches("xxxx\n").count(),
        1
    );
}

#[test]
fn a_signal_rider_did_not_send_is_reported() {
    let scratch = Scratch::new("signal");
    let log = scratch.root.join("agent.log");
    let cmd = fake(&["0", "0", "30", "0", "kill-self"]);
    let exit = run(&cmd, &scratch.root, &log, 20, 4096);
    assert_eq!(exit.unwrap(), Exit::Signal);
}

#[test]
fn a_missing_program_is_a_spawn_error() {
    let scratch = Scratch::new("missing");
    let log = scratch.root.join("agent.log");
    let mut cmd = fake(&[]);
    cmd.program = scratch.root.join("no-such-agent").display().to_string();
    let exit = run(&cmd, &scratch.root, &log, 5, 4096);
    assert!(matches!(exit, Err(HarnessError::Spawn(_))));
}
