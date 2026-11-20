use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use crate::fake_agent::{self, Behaviour};
use crate::steps::*;
use crate::world::{World, write_executable};

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

#[test]
fn a_feed_line_over_the_bound_is_skipped_and_the_feed_keeps_running() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let tails = world.root.join("tails");
    let over = FEED_LINE_BYTES_MAX + 1;
    given_vb_tail_starts_with(
        &world,
        &tails,
        &format!("head -c {over} /dev/zero | tr '\\0' x; echo; echo '{{\"cursor\":\"0\"}}'"),
    );

    given_a_running_daemon(&mut world);
    thread::sleep(Duration::from_secs(3));

    assert_eq!(feed_runs(&tails), 1, "{}", world.daemon_log());
    assert_eq!(daemon_feeds(&world).len(), 1, "{:?}", daemon_feeds(&world));
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
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
