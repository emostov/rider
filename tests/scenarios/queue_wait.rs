use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::fake_agent::WORLD_VAR;
use crate::steps::*;
use crate::world::{Output, World};

const OLD_HEAD: &str = "1111111111111111111111111111111111111111";
const PR_HEAD: &str = "38bf936dc1042605ced44bbe6cc794deb1d218fa";
const SLUG: &str = "Pk7QaZ2x";
const REVIEWS_SHOW: &str = include_str!("../../fixtures/reviews/show.json");
const GH_PR_VIEW: &str = include_str!("../../fixtures/gh/pr-view.json");

fn given_fake_reviews_and_gh(world: &World, reviews: &str, gh: &str) {
    fs::create_dir_all(&world.agents).expect("create the agents directory");
    for (name, reply) in [("reviews", reviews), ("gh", gh)] {
        fs::write(world.root.join(format!("{name}.json")), reply).expect("write the reply");
        let script = format!(
            "#!/bin/sh\ndate +%s >> \"${WORLD_VAR}/{name}.calls\"\n\
             printf '%s\\n' \"$*\" >> \"${WORLD_VAR}/{name}.args\"\n\
             cat \"${WORLD_VAR}/{name}.json\"\n"
        );
        let path = world.agents.join(name);
        fs::write(&path, script).expect("write the fake");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod the fake");
    }
}

fn calls(world: &World, name: &str) -> Vec<u64> {
    let text = fs::read_to_string(world.root.join(format!("{name}.calls"))).unwrap_or_default();
    text.lines()
        .map(|line| line.parse().expect("a time"))
        .collect()
}

fn given_a_queue_item(world: &World, n: u32, labels: &[(&str, &str)]) {
    let repo = world.repo_label().to_string();
    let toml = format!("[repo.\"{repo}\"]\nqueue = true\n");
    fs::write(world.home.join("queue.toml"), toml).expect("write queue.toml");
    let pr = format!("acme/widgets#{n}");
    let mut args: Vec<String> = ["tasks", "create", "queue", "-", "--json"]
        .map(String::from)
        .into();
    for (key, value) in [("pr", pr.as_str())].iter().chain(labels) {
        args.extend(["--label".to_string(), format!("{key}={value}")]);
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    world.vb_lead(&args, Some("Review a pull request of acme."));
}

fn when_the_queue_is_waited_on(world: &World) -> (Output, Duration) {
    let started = Instant::now();
    let output = when_rider_runs(world, &["queue", "wait", "--timeout", "1"]);
    (output, started.elapsed())
}

// The one-minute timeout outlives the world's command bound, so this run has its own.
fn when_the_queue_is_waited_on_until_the_timeout(world: &World) -> (Option<i32>, Duration) {
    let started = Instant::now();
    let mut command = world.command(Path::new(env!("CARGO_BIN_EXE_rider")));
    command.args(["queue", "wait", "--timeout", "1"]);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = command.spawn().expect("start rider");
    while started.elapsed() < Duration::from_secs(90) {
        if let Some(status) = child.try_wait().expect("poll rider") {
            return (status.code(), started.elapsed());
        }
        thread::sleep(Duration::from_millis(200));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("rider queue wait outlived 90 seconds");
}

fn without_human_reviews(reply: &str) -> String {
    let mut pull: Value = serde_json::from_str(reply).expect("the fixture is JSON");
    let reviews = pull["reviews"].as_array_mut().expect("reviews is a list");
    reviews.retain(|review| review["author"]["login"] != "maintainer");
    pull.to_string()
}

#[test]
fn rider_queue_wait_ends_on_a_packet_comment_newer_than_packet_at() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    given_fake_reviews_and_gh(&world, REVIEWS_SHOW, GH_PR_VIEW);
    let labels = [
        ("kind", "review"),
        ("turn", "owner"),
        ("packet", SLUG),
        ("packet_at", "2026-11-19T18:50:00Z"),
    ];
    given_a_queue_item(&world, 412, &labels);

    let (output, took) = when_the_queue_is_waited_on(&world);

    assert_eq!(output.code, Some(0), "{output:?}");
    let line = format!("acme/widgets#412: packet {SLUG} has a comment at 2026-11-19T18:55:04Z\n");
    assert_eq!(output.stdout, line);
    assert!(took < Duration::from_secs(10), "{took:?}");
    assert_eq!(calls(&world, "gh"), Vec::<u64>::new());
}

#[test]
fn rider_queue_wait_ends_when_the_head_of_a_pr_on_the_authors_turn_moves() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    given_fake_reviews_and_gh(&world, REVIEWS_SHOW, GH_PR_VIEW);
    let labels = [
        ("kind", "review"),
        ("turn", "author"),
        ("head", OLD_HEAD),
        ("github_at", "2026-11-21T00:00:00Z"),
    ];
    given_a_queue_item(&world, 377, &labels);

    let (output, took) = when_the_queue_is_waited_on(&world);

    assert_eq!(output.code, Some(0), "{output:?}");
    assert_eq!(
        output.stdout,
        format!("acme/widgets#377: head moved to {PR_HEAD}\n")
    );
    assert!(took < Duration::from_secs(10), "{took:?}");
    let args = fs::read_to_string(world.root.join("gh.args")).expect("gh ran");
    assert_eq!(
        args,
        "pr view 377 --repo acme/widgets --json headRefOid,comments,reviews\n"
    );
}

#[test]
fn rider_queue_wait_ends_on_a_bots_comment_on_a_respond_item() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    given_fake_reviews_and_gh(&world, REVIEWS_SHOW, &without_human_reviews(GH_PR_VIEW));
    let labels = [
        ("kind", "respond"),
        ("turn", "owner"),
        ("head", PR_HEAD),
        ("github_at", "2026-11-18T14:10:00Z"),
    ];
    given_a_queue_item(&world, 398, &labels);

    let (output, _) = when_the_queue_is_waited_on(&world);

    assert_eq!(output.code, Some(0), "{output:?}");
    let line = "acme/widgets#398: comment by cli-triage at 2026-11-18T14:21:22Z\n";
    assert_eq!(output.stdout, line);
}

#[test]
fn rider_queue_wait_exits_3_at_the_timeout_after_polls_at_the_stated_intervals() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    given_fake_reviews_and_gh(&world, REVIEWS_SHOW, GH_PR_VIEW);
    let packet = [
        ("kind", "review"),
        ("turn", "owner"),
        ("packet", SLUG),
        ("packet_at", "2026-11-19T18:55:04Z"),
    ];
    given_a_queue_item(&world, 412, &packet);
    let author = [
        ("kind", "review"),
        ("turn", "author"),
        ("head", PR_HEAD),
        ("github_at", "2026-11-20T13:14:08Z"),
    ];
    given_a_queue_item(&world, 377, &author);

    let (code, took) = when_the_queue_is_waited_on_until_the_timeout(&world);

    assert_eq!(code, Some(3));
    assert!(
        took >= Duration::from_secs(59) && took < Duration::from_secs(70),
        "{took:?}"
    );
    let packets = calls(&world, "reviews");
    assert_eq!(
        packets.len(),
        3,
        "packet polls at 0, 10 and 30 seconds: {packets:?}"
    );
    let gaps: Vec<u64> = packets.windows(2).map(|pair| pair[1] - pair[0]).collect();
    assert!(
        (9..=11).contains(&gaps[0]) && (19..=21).contains(&gaps[1]),
        "{gaps:?}"
    );
    assert_eq!(
        calls(&world, "gh").len(),
        1,
        "GitHub is polled again only at 60 seconds"
    );
}

#[test]
fn rider_queue_wait_reports_a_malformed_reply_naming_the_field() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let reviews = r#"{"threads": [{"comments": [{"inserted_at": "yesterday"}]}]}"#;
    let gh = r#"{"comments": [], "reviews": []}"#;
    given_fake_reviews_and_gh(&world, reviews, gh);
    let labels = [
        ("kind", "review"),
        ("turn", "owner"),
        ("packet", SLUG),
        ("packet_at", "2026-11-19T18:50:00Z"),
    ];
    given_a_queue_item(&world, 412, &labels);

    let (output, _) = when_the_queue_is_waited_on(&world);

    assert_eq!(output.code, Some(1), "{output:?}");
    assert!(
        output.stderr.contains("threads[0].comments[0].inserted_at"),
        "{output:?}"
    );

    let labels = [("kind", "respond"), ("turn", "owner"), ("head", PR_HEAD)];
    given_a_queue_item(&world, 398, &labels);
    fs::write(world.root.join("reviews.json"), REVIEWS_SHOW).expect("write the reply");

    let (output, _) = when_the_queue_is_waited_on(&world);

    assert_eq!(output.code, Some(1), "{output:?}");
    assert!(output.stderr.contains("headRefOid"), "{output:?}");
}
