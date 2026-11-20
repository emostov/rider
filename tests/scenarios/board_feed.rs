use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

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
