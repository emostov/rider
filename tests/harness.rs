//! `harness::run` against `fake_agent`, a stand-in for a real agent CLI.

use rider::harness::{
    CommandLine, Exit, HarnessError, Io, KILL_GRACE_SECONDS, PROMPT_BYTES_MAX, SessionId,
    command_for, last_message, run, run_on_clock, session_id, stream_reports_failure,
};
use rider::roles::{AGENTS_MAX, Harness, role_from_label};
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

    fn file(&self, name: &str, text: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::write(&path, text).unwrap();
        path
    }
}

static READER_BUDGET: RwLock<()> = RwLock::new(());

fn sharing_the_reader_budget() -> RwLockReadGuard<'static, ()> {
    READER_BUDGET.read().unwrap_or_else(PoisonError::into_inner)
}

const PROMPT: &str = "Do the task.";
const LINE_BYTES_MAX: usize = 4 * 1024 * 1024;

fn reports_failure(harness: Harness, text: &str) -> bool {
    let scratch = Scratch::new("stream");
    stream_reports_failure(&harness, &scratch.file("agent.log", text))
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
    let _sharing = sharing_the_reader_budget();
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
    let _sharing = sharing_the_reader_budget();
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
    let _sharing = sharing_the_reader_budget();
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
    let _sharing = sharing_the_reader_budget();
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
    let _sharing = sharing_the_reader_budget();
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
    let _sharing = sharing_the_reader_budget();
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
    let _sharing = sharing_the_reader_budget();
    let scratch = Scratch::new("signal");
    let log = scratch.root.join("agent.log");
    let cmd = fake(&["0", "0", "30", "0", "kill-self"]);
    let exit = run(&cmd, &scratch.root, &log, 20, 4096);
    assert_eq!(exit.unwrap(), Exit::Signal);
}

#[test]
fn a_missing_program_is_a_spawn_error() {
    let _sharing = sharing_the_reader_budget();
    let scratch = Scratch::new("missing");
    let log = scratch.root.join("agent.log");
    let mut cmd = fake(&[]);
    cmd.program = scratch.root.join("no-such-agent").display().to_string();
    let exit = run(&cmd, &scratch.root, &log, 5, 4096);
    assert!(matches!(exit, Err(HarnessError::Spawn(_))));
}

#[test]
fn claude_command_matches_the_recorded_form() {
    let spec = role_from_label("implement").unwrap();
    let c = command_for(spec, PROMPT, Path::new("/w"), None).unwrap();
    assert_eq!(c.program, "claude");
    let expected = [
        "-p",
        "--model",
        "claude-opus-5-5",
        "--output-format",
        "stream-json",
        "--verbose",
        "--dangerously-skip-permissions",
        "--",
        PROMPT,
    ];
    assert_eq!(c.args, expected);
    assert_eq!(c.env, []);
}

#[test]
fn codex_command_matches_the_recorded_form() {
    let spec = role_from_label("review").unwrap();
    let c = command_for(spec, PROMPT, Path::new("/w"), None).unwrap();
    assert_eq!(c.program, "codex");
    let expected = [
        "exec",
        "--model",
        "gpt-6.1-sol",
        "--json",
        "--dangerously-bypass-approvals-and-sandbox",
        "-C",
        "/w",
        "--",
        PROMPT,
    ];
    assert_eq!(c.args, expected);
    assert_eq!(c.env, []);
}

#[test]
fn a_prompt_is_one_argument_after_the_options() {
    let prompt = "--help me\nplease";
    let security = role_from_label("security").unwrap();
    let c = command_for(security, prompt, Path::new("/w"), None).unwrap();
    assert_eq!(c.args[c.args.len() - 2..], ["--", prompt]);
    assert_eq!(c.args[1..3], ["--model", "claude-mythos-5-2"]);
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
fn the_session_id_is_read_from_the_recorded_claude_stream() {
    let id = session_id(Path::new("fixtures/harness/claude.jsonl"));
    let expected = SessionId::parse("20de60af-1a54-4519-8f52-8623adc5d76e");
    assert_eq!(id, expected);
    assert_eq!(session_id(Path::new("fixtures/harness/codex.jsonl")), None);
    let scratch = Scratch::new("session");
    let log = scratch.file("hostile", "{\"session_id\":\"--help\"}\n");
    assert_eq!(session_id(&log), None);
}

#[test]
fn last_message_is_found_in_each_recorded_stream() {
    let cases = [(Harness::Claude, "claude"), (Harness::Codex, "codex")];
    for (h, f) in cases {
        let path = format!("fixtures/harness/{f}.jsonl");
        let m = last_message(&h, Path::new(&path));
        assert_eq!(m.as_deref(), Some("DONE"), "{f}");
    }
}

#[test]
fn last_message_survives_hostile_logs() {
    let missing = Path::new("/nonexistent/rider/agent.log");
    assert_eq!(last_message(&Harness::Claude, missing), None);
    let huge = "y".repeat(LINE_BYTES_MAX + 10);
    let events = r#"not json
[1,2]
"str"
{"type":"result","result":7}
{"type":"result","result":"  first  "}
"#;
    let scratch = Scratch::new("hostile");
    let log = scratch.file("hostile", &format!("{huge}\n{events}{huge}"));
    assert_eq!(
        last_message(&Harness::Claude, &log).as_deref(),
        Some("first")
    );
    assert_eq!(last_message(&Harness::Codex, &log), None);
    let blank = scratch.file("blank", r#"{"type":"result","result":" \n "}"#);
    assert_eq!(last_message(&Harness::Claude, &blank), None);
}

#[test]
fn recorded_streams_report_no_failure() {
    let cases = [(Harness::Claude, "claude"), (Harness::Codex, "codex")];
    for (h, f) in cases {
        let path = format!("fixtures/harness/{f}.jsonl");
        assert!(!stream_reports_failure(&h, Path::new(&path)), "{f}");
    }
    let missing = Path::new("/nonexistent/rider/agent.log");
    assert!(!stream_reports_failure(&Harness::Codex, missing));
}

#[test]
fn failures_in_the_stream_are_reported() {
    let claude = r#"{"type":"result","subtype":"success","is_error":true}"#;
    assert!(reports_failure(Harness::Claude, claude));
    let claude = r#"{"type":"result","subtype":"error_max_turns","is_error":false}"#;
    assert!(reports_failure(Harness::Claude, claude));
    let codex = r#"{"type":"item.completed","item":{"type":"error"}}"#;
    assert!(!reports_failure(Harness::Codex, codex));
    let codex = r#"{"type":"turn.failed","error":{"message":"x"}}"#;
    assert!(reports_failure(Harness::Codex, codex));
    let codex = r#"{"type":"error","message":"x"}"#;
    assert!(reports_failure(Harness::Codex, codex));
}

#[test]
fn a_claude_stream_is_judged_by_its_last_result() {
    let recovered = r#"{"type":"result","subtype":"error_during_execution","is_error":true}
{"type":"result","subtype":"success","is_error":false}"#;
    assert!(!reports_failure(Harness::Claude, recovered));
}

#[test]
fn a_codex_turn_is_judged_by_how_it_ended() {
    let path = Path::new("fixtures/harness/codex-reconnect.jsonl");
    assert!(!stream_reports_failure(&Harness::Codex, path));
    let recorded = fs::read_to_string(path).unwrap();
    let cut: Vec<&str> = recorded
        .lines()
        .filter(|l| !l.contains("turn.completed"))
        .collect();
    assert_eq!(cut.len() + 1, recorded.lines().count());
    assert_eq!(
        cut.iter().filter(|l| l.contains("Reconnecting...")).count(),
        4
    );
    assert!(reports_failure(Harness::Codex, &cut.join("\n")));
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
fn a_log_that_is_a_fifo_with_a_reader_is_refused_before_the_agent_starts() {
    let _sharing = sharing_the_reader_budget();
    let scratch = Scratch::new("fifo");
    let log = scratch.root.join("agent.log");
    let made = Command::new("mkfifo")
        .arg(&log)
        .stdin(Stdio::null())
        .status();
    assert_eq!(made.map(|status| status.success()).ok(), Some(true));
    let (release, released) = mpsc::sync_channel::<()>(0);
    let (opened, reading) = mpsc::sync_channel::<()>(0);
    let fifo = log.clone();
    let unread = thread::spawn(move || {
        let reader = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(fifo)
            .unwrap();
        let _ = opened.send(());
        let _ = released.recv();
        drop(reader);
    });
    reading.recv().unwrap();
    let cmd = fake(&["1024", "4096", "60", "0"]);
    let start = Instant::now();

    let exit = run(&cmd, &scratch.root, &log, 1, 1 << 24);

    let elapsed = start.elapsed();
    drop(release);
    unread.join().unwrap();
    let expected = format!(
        "{}: it is not a regular file of the current user",
        log.display()
    );
    assert!(
        matches!(&exit, Err(HarnessError::Log(message)) if *message == expected),
        "{exit:?}"
    );
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
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
            io: Io::Stream,
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
