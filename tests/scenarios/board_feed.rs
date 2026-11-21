use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use crate::fake_agent::{self, Behaviour};
use crate::steps::*;
use crate::world::{Daemon, World, write_executable};

const FEED_LINE_BYTES_MAX: u64 = 1024 * 1024;
const FEED_RUNS_SECONDS_MAX: u64 = 10;

/// Given every `vb tail` run logs a line in `tails`, then runs `prefix`, a shell fragment in
/// which `$runs` is the number of runs so far, and then the real `vb`.
fn given_vb_tail_starts_with(world: &World, tails: &Path, prefix: &str) {
    let real = fs::read_link(&world.vb).expect("the Board CLI is a link");
    fs::remove_file(&world.vb).expect("remove the Board CLI link");
    let script = format!(
        r#"#!/bin/sh
if [ "$1" = tail ]; then
  echo tail >> '{tails}'
  runs=$(wc -l < '{tails}')
  {prefix}
fi
exec '{real}' "$@"
"#,
        tails = tails.display(),
        real = real.display()
    );
    write_executable(&world.vb, &script);
}

fn feed_runs(tails: &Path) -> usize {
    fs::read_to_string(tails)
        .unwrap_or_default()
        .lines()
        .count()
}

/// The recorded canceled-task event at `cursor`, as one line.
fn event_at(cursor: u64) -> String {
    let recorded = include_str!("../../fixtures/board/tail-task-canceled.json");
    let (before, after) = recorded
        .split_once(r#""cursor":"22""#)
        .expect("the recorded event's cursor");
    format!("{before}\"cursor\":\"{cursor}\"{after}")
}

/// Given the first `vb tail` run prints `events` and runs `then`, logging every run's
/// arguments in `runs`, and every later run is the real one.
fn given_the_first_feed_prints(world: &World, events: &str, then: &str) -> std::path::PathBuf {
    let (tails, runs) = (world.root.join("tails"), world.root.join("tail-runs"));
    let events_file = world.root.join("events");
    fs::write(&events_file, events).expect("write the feed's events");
    given_vb_tail_starts_with(
        world,
        &tails,
        &format!(
            r#"echo "$*" >> '{}'; [ "$runs" -le 1 ] && {{ cat '{}'; {then}; }}"#,
            runs.display(),
            events_file.display()
        ),
    );
    runs
}

/// When the feed is started a second time, within `FEED_RUNS_SECONDS_MAX`; returns the second
/// run's arguments.
fn when_the_feed_restarts(world: &World, runs: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(FEED_RUNS_SECONDS_MAX);
    loop {
        let text = fs::read_to_string(runs).unwrap_or_default();
        if let Some(second) = text.lines().nth(1) {
            return second.to_string();
        }
        assert!(
            Instant::now() < deadline,
            "the feed was not restarted: {text}\n{}",
            world.daemon_log()
        );
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn the_rest_of_a_feed_line_over_the_bound_is_skipped_not_read_as_an_event() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let padding = "x".repeat(usize::try_from(FEED_LINE_BYTES_MAX).expect("fits") + 1);
    let events = format!("{padding}{}{}", event_at(2_000_000), event_at(1_000_001));
    let runs = given_the_first_feed_prints(&world, &events, "exit 3");

    given_a_running_daemon(&mut world);
    let second = when_the_feed_restarts(&world, &runs);

    assert!(second.starts_with("tail --after 1000001 "), "{second}");
    then_the_daemon_stops_cleanly(world);
}

#[test]
fn a_descendant_that_keeps_printing_after_the_feed_vb_exits_is_killed_and_the_feed_restarts() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let events = event_at(1_000_001);
    let runs = given_the_first_feed_prints(
        &world,
        &events,
        &format!(
            "while :; do cat '{}'; done & exit 3",
            world.root.join("events").display()
        ),
    );

    given_a_running_daemon(&mut world);
    let second = when_the_feed_restarts(&world, &runs);

    assert!(second.starts_with("tail --after 1000001 "), "{second}");
    then_the_daemon_stops_cleanly(world);
}

#[test]
fn a_feed_vb_that_exits_while_a_descendant_holds_its_output_is_restarted() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let tails = world.root.join("tails");
    given_vb_tail_starts_with(
        &world,
        &tails,
        r#"[ "$runs" -le 1 ] && { sleep 60 & exit 3; }"#,
    );

    given_a_running_daemon(&mut world);
    let deadline = Instant::now() + Duration::from_secs(FEED_RUNS_SECONDS_MAX);
    while feed_runs(&tails) < 2 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }

    assert_eq!(feed_runs(&tails), 2, "{}", world.daemon_log());
    assert_eq!(daemon_feeds(&world).len(), 1, "{:?}", daemon_feeds(&world));
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

const RESULT_CHARS_MAX: usize = 500;
const TAIL_REPLY_CHARS_MAX: usize = 9_000;

fn task_json(world: &World, id: &str) -> serde_json::Value {
    let text = world.vb_lead(&["read", id, "--json"], None);
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("not JSON ({error}): {text}"))
}

#[test]
fn a_summary_starting_with_a_dash_completes_the_task_as_one_bounded_line() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::CommitDashedSummary]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["completed".to_string()], Some(0)));
    let summary = fake_agent::dashed_summary(&task);
    let words: Vec<&str> = summary.split_whitespace().collect();
    let line: String = words.join(" ").chars().take(RESULT_CHARS_MAX).collect();
    let json = task_json(&world, &task);
    let result = &json["message"]["task"]["result"];
    assert_eq!(result.as_str(), Some(line.trim_end()), "{json}");
}

#[test]
fn a_failed_agents_log_tail_reply_keeps_whole_lines_from_the_end_within_the_limit() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_rider_toml(&world, "[limits]\nattempts = 1\n");
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::FailTalkingLong]);

    when_the_daemon_runs_until_settled(&mut world, &[&task]);

    let replies = task_replies(&world, &task);
    let texts: Vec<&str> = replies
        .iter()
        .filter_map(|reply| reply["text"].as_str())
        .collect();
    let reply = texts
        .iter()
        .find(|text| text.starts_with("The whole log: "));
    let reply = reply.unwrap_or_else(|| panic!("no log tail reply in {replies:?}"));
    assert!(reply.chars().count() <= TAIL_REPLY_CHARS_MAX, "{reply}");
    let lines: Vec<&str> = reply.lines().skip(1).collect();
    let last = fake_agent::TALK_LINES - 1;
    let expected: Vec<String> = (last + 1 - lines.len()..=last)
        .map(fake_agent::talk_line)
        .collect();
    assert!(lines.len() > 20, "{reply}");
    assert_eq!(lines, expected);
}

const CLOSED_URL: &str = "http://127.0.0.1:9/api/board";

#[test]
fn a_claim_by_a_vb_that_cannot_reach_the_board_marks_the_board_down() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let real = fs::read_link(&world.vb).expect("the Board CLI is a link");
    fs::remove_file(&world.vb).expect("remove the Board CLI link");
    let script = format!(
        r#"#!/bin/sh
case " $* " in *" claim "*)
  previous=
  for word; do
    shift
    [ "$previous" = --url ] && word='{CLOSED_URL}'
    set -- "$@" "$word"
    previous=$word
  done;;
esac
exec '{}' "$@"
"#,
        real.display()
    );
    write_executable(&world.vb, &script);
    given_a_running_daemon(&mut world);

    let down = when_rider_status_shows_within(&world, "board down, 0 of", 60);

    assert!(
        down,
        "rider status never said the board was down:\n{}",
        world.daemon_log()
    );
    assert_eq!(agents_running(&world), 0);
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

const HUNG_DAEMON_SECONDS_MAX: u64 = 60;

/// Given the daemon's first identity read starts `/bin/sleep` in the background, writes its pid
/// to `started` and runs `fragment`, and every later one fails at once.
fn given_the_identity_read_hangs_with(world: &World, fragment: &str, started: &Path) {
    fs::remove_file(&world.vb).expect("remove the Board CLI link");
    let script = format!(
        r#"#!/bin/sh
case " $* " in *" rider/identity "*)
  [ -e '{0}' ] && exit 1
  /bin/sleep 120 &
  echo $! > '{0}'
  {fragment};;
esac
exit 1
"#,
        started.display(),
    );
    write_executable(&world.vb, &script);
}

/// When the daemon runs with `path` as its `PATH` until it exits; returns its exit code.
fn when_the_daemon_runs_with_path(world: &mut World, path: &str) -> Option<i32> {
    let mut command = world.command(Path::new(env!("CARGO_BIN_EXE_rider")));
    let child = command
        .env("PATH", path)
        .arg("daemon")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("start rider daemon");
    world.daemon = Some(Daemon { child });
    let daemon = world.daemon.as_mut().expect("the daemon was started");
    let deadline = Instant::now() + Duration::from_secs(HUNG_DAEMON_SECONDS_MAX);
    let status = loop {
        if let Some(status) = daemon.child.try_wait().expect("poll the daemon") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon outlived a hung identity read"
        );
        thread::sleep(Duration::from_millis(100));
    };
    world.daemon = None;
    status.code()
}

fn then_the_hung_identity_read_left_nothing(mut world: World, fragment: &str, path: &str) {
    let started = world.root.join("sleep-started");
    given_the_identity_read_hangs_with(&world, fragment, &started);
    let path = path.replace("<world>", &world.child_path());

    let exit = when_the_daemon_runs_with_path(&mut world, &path);

    assert_eq!(exit, Some(1));
    let pid = fs::read_to_string(&started).expect("the identity read started its descendant");
    let pid: u32 = pid.trim().parse().expect("the descendant's pid");
    // macOS hides a platform binary's environment from `ps -E`, so the world's mark misses
    // `/bin/sleep` there and its own pid is checked as well.
    assert!(
        when_the_process_ends_within(pid, 5),
        "the descendant {pid} outlived rider"
    );
    world.then_no_process_of_the_world_is_left();
}

#[test]
fn a_descendant_holding_the_output_of_an_exited_vb_is_killed_at_the_call_limit() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    then_the_hung_identity_read_left_nothing(world, "exit 0", "<world>");
}

#[test]
fn a_descendant_of_a_waiting_vb_is_killed_at_the_call_limit() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    then_the_hung_identity_read_left_nothing(world, "wait", "<world>");
}

#[test]
fn a_descendant_of_a_waiting_vb_is_killed_with_no_kill_command_on_path() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let vb_dir = world
        .vb
        .parent()
        .expect("vb has a directory")
        .display()
        .to_string();
    assert!(!Path::new(&vb_dir).join("kill").exists());
    then_the_hung_identity_read_left_nothing(world, "wait", &vb_dir);
}

const BURST_EVENTS: u64 = 400;
const BURST_FIRST_CURSOR: u64 = 1_000_001;

#[test]
fn every_event_a_feed_vb_prints_before_it_exits_reaches_the_daemon() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let highest = BURST_FIRST_CURSOR + BURST_EVENTS;
    let middle = BURST_EVENTS / 2;
    let burst: String = (0..BURST_EVENTS)
        .map(|n| {
            if n == middle {
                highest
            } else {
                BURST_FIRST_CURSOR + n
            }
        })
        .map(event_at)
        .collect();
    let runs = given_the_first_feed_prints(&world, &burst, "exit 3");

    given_a_running_daemon(&mut world);
    let second = when_the_feed_restarts(&world, &runs);

    assert!(
        second.starts_with(&format!("tail --after {highest} ")),
        "{second}\n{}",
        world.daemon_log()
    );
    then_the_daemon_stops_cleanly(world);
}

/// Given every `vb` run first runs `fragment`, a shell fragment seeing the run's arguments, and
/// then the real `vb`.
fn given_vb_first_runs(world: &World, fragment: &str) {
    let real = fs::read_link(&world.vb).expect("the Board CLI is a link");
    fs::remove_file(&world.vb).expect("remove the Board CLI link");
    let script = format!("#!/bin/sh\n{fragment}\nexec '{}' \"$@\"\n", real.display());
    write_executable(&world.vb, &script);
}

/// Waits up to `seconds` for the daemon's log to contain `text`; returns whether it did.
fn when_the_daemon_log_shows_within(world: &World, text: &str, seconds: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !world.daemon_log().contains(text) {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
    true
}

fn then_the_daemon_stops_cleanly(mut world: World) {
    assert!(
        !world.daemon_log().contains("panicked"),
        "{}",
        world.daemon_log()
    );
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

const COMPLETE_SECONDS_MAX: u64 = 20;

#[test]
fn a_complete_refused_as_claim_lost_is_not_retried() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::Commit]);
    let completes = world.root.join("completes");
    let stderr = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/board/claim-lost.stderr.txt"
    );
    given_vb_first_runs(
        &world,
        &format!(
            r#"if [ "$1 $2" = "tasks complete" ]; then
  echo complete >> '{}'; cat '{stderr}' >&2; exit 1
fi"#,
            completes.display()
        ),
    );
    given_a_running_daemon(&mut world);

    when_the_file_exists(&world, &completes);
    let settled = when_rider_status_shows_within(&world, "board up, 0 of", COMPLETE_SECONDS_MAX);

    assert!(settled, "{}", world.daemon_log());
    let tries = fs::read_to_string(&completes).expect("read the complete log");
    assert_eq!(tries.lines().count(), 1, "{}", world.daemon_log());
    then_the_daemon_stops_cleanly(world);
}

#[test]
fn a_complete_refused_with_an_unknown_error_envelope_is_retried() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::Commit]);
    let completes = world.root.join("completes");
    given_vb_first_runs(
        &world,
        &format!(
            r#"if [ "$1 $2" = "tasks complete" ]; then
  echo complete >> '{completes}'
  if [ "$(wc -l < '{completes}')" -eq 1 ]; then
    echo '{{"status":"unexpected","reason":"claim_lost"}}' >&2; exit 1
  fi
fi"#,
            completes = completes.display()
        ),
    );
    given_a_running_daemon(&mut world);

    let states = when_the_tasks_settle(&world, &[&task]);

    assert_eq!(states, ["completed"], "{}", world.daemon_log());
    let tries = fs::read_to_string(&completes).expect("read the complete log");
    assert_eq!(tries.lines().count(), 2, "{}", world.daemon_log());
    then_the_daemon_stops_cleanly(world);
}

const LABELS_OVER_MAX: usize = 33;

#[test]
fn a_task_with_more_labels_than_rider_tracks_is_refused_without_a_panic() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let mut crowded = an_implement_task(&world, "Add the crowded work.");
    for n in 0..LABELS_OVER_MAX {
        crowded = crowded.label(&format!("extra{n}"), "x");
    }
    when_the_lead_creates(&world, &crowded);

    when_the_daemon_is_started(&mut world, &[]);

    assert!(
        when_the_daemon_log_shows_within(&world, "the task has more than 32 labels", 20),
        "{}",
        world.daemon_log()
    );
    then_the_daemon_stops_cleanly(world);
}

const LARGE_TASKS: usize = 180;
const LARGE_TASK_WORDS: usize = 1_980;
const CREATORS: usize = 12;

#[test]
fn a_listing_over_the_output_bound_is_refused_without_a_panic() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let text = "日日日日 ".repeat(LARGE_TASK_WORDS);
    thread::scope(|scope| {
        for _ in 0..CREATORS {
            scope.spawn(|| {
                for _ in 0..LARGE_TASKS / CREATORS {
                    when_the_lead_creates(&world, &NewTask::new(&text));
                }
            });
        }
    });

    when_the_daemon_is_started(&mut world, &[]);

    assert!(
        when_the_daemon_log_shows_within(&world, "bytes on one stream", 30),
        "{}",
        world.daemon_log()
    );
    then_the_daemon_stops_cleanly(world);
}

const PAGE_TASKS: u64 = 200;
const PAGES_MAX: u64 = 16;

#[test]
fn a_listing_longer_than_its_page_limit_is_refused() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    when_the_lead_creates(&world, &NewTask::new("Open the work channel."));
    let pages = world.root.join("pages");
    fs::create_dir(&pages).expect("create the pages directory");
    for page in 0..=PAGES_MAX {
        let first = page * PAGE_TASKS + 1;
        let tasks: Vec<String> = (first..first + PAGE_TASKS)
            .map(|id| {
                format!(
                    r#"{{"message":{{"id":"{id}","channel":"work/t","text":"t","labels":{{}},"task":{{"state":"ready","epoch":0,"needs":[],"claim":null}},"version":{id}}}}}"#
                )
            })
            .collect();
        let last = first + PAGE_TASKS - 1;
        let json = format!(r#"{{"tasks":[{}],"cursor":"{last}"}}"#, tasks.join(","));
        let name = pages.join((page * PAGE_TASKS).to_string());
        fs::write(name, json).expect("write a listing page");
    }
    given_vb_first_runs(
        &world,
        &format!(
            r#"if [ "$1" = tasks ] && [ "$3" = --state ]; then
  after=0
  for word; do [ "$previous" = --after ] && after=$word; previous=$word; done
  exec cat '{}'/"$after"
fi"#,
            pages.display()
        ),
    );

    when_the_daemon_is_started(&mut world, &[]);

    assert!(
        when_the_daemon_log_shows_within(&world, "tasks in one listing", 30),
        "{}",
        world.daemon_log()
    );
    then_the_daemon_stops_cleanly(world);
}

const HELD_READ_SECONDS_MAX: u64 = 30;

#[test]
fn rider_kill_while_the_worker_reads_the_thread_before_launch_starts_no_agent() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let held = world.root.join("read-held");
    let released = world.root.join("read-released");
    given_vb_first_runs(
        &world,
        &format!(
            r#"case " $* " in " read {task} "*" --as implement-"*)
  [ -d '{worktree}' ] || exec '{vb}' "$@"
  : > '{held}'
  n=0
  while [ ! -e '{released}' ] && [ "$n" -lt 600 ]; do n=$((n + 1)); sleep 0.1; done ;;
esac"#,
            worktree = world.home.join("worktrees/repo").join(&task).display(),
            vb = fs::read_link(&world.vb)
                .expect("the Board CLI is a link")
                .display(),
            held = held.display(),
            released = released.display(),
        ),
    );
    given_a_running_daemon(&mut world);
    let deadline = Instant::now() + Duration::from_secs(HELD_READ_SECONDS_MAX);
    while !held.exists() {
        assert!(Instant::now() < deadline, "{}", world.daemon_log());
        thread::sleep(Duration::from_millis(100));
    }

    let kill = when_rider_runs(&world, &["kill", &task]);
    fs::write(&released, "").expect("release the held read");

    assert_eq!(kill.code, Some(0), "{kill:?}");
    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["failed"]);
    let runs = agent_runs_for(&world, &task);
    assert!(runs.is_empty(), "an agent started after the kill: {runs:?}");
    then_the_daemon_stops_cleanly(world);
}
