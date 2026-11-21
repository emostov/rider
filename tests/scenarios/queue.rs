use std::fs;

use serde_json::Value;

use crate::fake_agent::Behaviour;
use crate::steps::*;
use crate::world::World;

const HEAD: &str = "1111111111111111111111111111111111111111";
const MAIN_COMMIT: &str = "2222222222222222222222222222222222222222";
const PR_URL: &str = "https://github.com/acme/widgets/pull/";
const HEADER: &str = " #  KIND     TURN    STATE      AGE  NEXT           PR";
const CARD: &str = "Decide the cache key.\nEither the tenant or the session.";

fn given_a_queue_toml(world: &World, repo: &str) {
    let text = format!("[repo.\"{repo}\"]\nqueue = true\n");
    fs::write(world.home.join("queue.toml"), text).expect("write queue.toml");
}

fn ago(seconds: i64) -> String {
    let now = jiff::Timestamp::now();
    let then = now - jiff::SignedDuration::from_secs(seconds + 30);
    then.to_string()
}

fn when_the_lead_creates_in(
    world: &World,
    channel: &str,
    text: &str,
    labels: &[(&str, &str)],
) -> String {
    let mut args: Vec<String> = ["tasks", "create", channel, "-", "--json"]
        .map(String::from)
        .into();
    for (key, value) in labels {
        args.extend(["--label".to_string(), format!("{key}={value}")]);
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = world.vb_lead(&args, Some(text));
    let created: Value = serde_json::from_str(&output).expect("vb prints JSON");
    let id = created["task"]["id"]
        .as_str()
        .or(created["message"]["id"].as_str());
    id.unwrap_or_else(|| panic!("no id in {output}"))
        .to_string()
}

fn an_owner_item(
    n: u32,
    kind: &str,
    since: &str,
    verdict: &str,
    next: &str,
) -> Vec<(String, String)> {
    [
        ("pr", format!("acme/widgets#{n}")),
        ("kind", kind.to_string()),
        ("turn", "owner".to_string()),
        ("source", "direct".to_string()),
        ("since", since.to_string()),
        ("head", HEAD.to_string()),
        ("main", MAIN_COMMIT.to_string()),
        ("prep", "1".to_string()),
        ("review_task", "2".to_string()),
        ("verdict", verdict.to_string()),
        ("next", next.to_string()),
    ]
    .map(|(key, value)| (key.to_string(), value))
    .into()
}

fn with(mut labels: Vec<(String, String)>, key: &str, value: &str) -> Vec<(String, String)> {
    labels.retain(|(known, _)| known != key);
    labels.push((key.to_string(), value.to_string()));
    labels
}

fn given_an_item(world: &World, labels: &[(String, String)]) -> String {
    let pairs: Vec<(&str, &str)> = (labels.iter())
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    when_the_lead_creates_in(world, "queue", "Review a pull request of acme.", &pairs)
}

// Given an example queue: a decision, a follow-up, a respond, a
// first review, and one item each on the agents' and the author's turn.
fn given_the_example_queue(world: &World) -> String {
    let repo = world.repo_label().to_string();
    given_a_queue_toml(world, &repo);
    let decision = when_the_lead_creates_in(world, "decisions", CARD, &[("since", &ago(2 * 3600))]);
    let follow_up = an_owner_item(412, "review", &ago(3 * 3600), "approve", "approve");
    given_an_item(world, &with(follow_up, "seen", HEAD));
    let respond = an_owner_item(398, "respond", &ago(25 * 3600), "none", "post-drafts");
    given_an_item(
        world,
        &with(with(respond, "threads", "2"), "source", "mine"),
    );
    given_an_item(
        world,
        &an_owner_item(431, "review", &ago(5 * 3600), "concerns", "read-findings"),
    );
    let preparing = with(
        an_owner_item(440, "review", &ago(12 * 60), "none", "read-findings"),
        "turn",
        "agents",
    );
    given_an_item(world, &preparing);
    let waiting = with(
        an_owner_item(377, "review", &ago(48 * 3600), "none", "read-findings"),
        "turn",
        "author",
    );
    given_an_item(world, &waiting);
    decision
}

fn expected_items(decision: &str) -> String {
    format!(
        "{HEADER}
 1  decide   owner   open        2h  decide         decisions #{decision}
 2  review   owner   approve     3h  approve        {PR_URL}412
 3  respond  owner   2 threads   1d  post-drafts    {PR_URL}398
 4  review   owner   concerns    5h  read-findings  {PR_URL}431
 -  review   agents  preparing  12m  -              {PR_URL}440
 -  review   author  waiting     2d  -              {PR_URL}377

{CARD}
"
    )
}

#[test]
fn rider_queue_prints_the_ranked_items_the_first_card_and_that_no_daemon_runs() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let decision = given_the_example_queue(&world);

    let output = when_rider_runs(&world, &["queue"]);

    assert_eq!(output.code, Some(0), "{output:?}");
    let expected = format!("{}\ndaemon not running\n", expected_items(&decision));
    assert_eq!(output.stdout, expected);
    let long: Vec<&str> = (output.stdout.lines())
        .filter(|line| line.chars().count() > 100 && !line.starts_with("      cd "))
        .collect();
    assert_eq!(long, Vec::<&str>::new());
}

#[test]
fn rider_queue_prints_the_daemons_running_work_as_rider_status_does() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let repo = world.repo_label().to_string();
    given_a_queue_toml(&world, &repo);
    let running = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &running, &[Behaviour::Hold]);
    given_a_running_daemon(&mut world);
    when_the_agent_has_started(&world, &running);

    let output = when_rider_runs(&world, &["queue"]);
    let status = when_rider_runs(&world, &["status"]);

    assert_eq!(output.code, Some(0), "{output:?}");
    let row = (status.stdout.lines())
        .find(|line| line.starts_with(&format!("  #{running} ")))
        .unwrap_or_else(|| panic!("no row for #{running}: {status:?}"));
    assert_eq!(
        output.stdout,
        format!("{HEADER}\n\nrunning 1 of 32\n{row}\n")
    );
    world.release_agent(&running);
    when_the_daemon_is_sent_sigterm(&world);
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[test]
fn rider_queue_json_prints_every_item_with_every_label_in_order() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let repo = world.repo_label().to_string();
    given_a_queue_toml(&world, &repo);
    let first = an_owner_item(431, "review", &ago(5 * 3600), "concerns", "read-findings");
    let second = with(
        an_owner_item(440, "review", &ago(60), "none", "read-findings"),
        "turn",
        "agents",
    );
    let first_id = given_an_item(&world, &first);
    let second_id = given_an_item(&world, &second);

    let output = when_rider_runs(&world, &["queue", "--json"]);

    assert_eq!(output.code, Some(0), "{output:?}");
    let items: Value = serde_json::from_str(&output.stdout).expect("rider queue --json is JSON");
    let item = |id: &str, rank: Value, labels: &[(String, String)]| {
        let labels: serde_json::Map<String, Value> = (labels.iter())
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect();
        serde_json::json!({
            "id": id.parse::<u64>().expect("an id"),
            "channel": "queue",
            "rank": rank,
            "labels": labels,
            "needs": [],
            "invalid": null,
        })
    };
    let expected = serde_json::json!([
        item(&first_id, Value::from(1), &first),
        item(&second_id, Value::Null, &second),
    ]);
    assert_eq!(items, expected);
}

#[test]
fn rider_queue_prints_an_item_with_a_malformed_label_as_invalid_naming_the_key() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let repo = world.repo_label().to_string();
    given_a_queue_toml(&world, &repo);
    let bad = with(
        an_owner_item(431, "review", &ago(60), "concerns", "read-findings"),
        "kind",
        "bogus",
    );
    let bad = given_an_item(&world, &bad);
    let unprepared = an_owner_item(398, "respond", &ago(60), "none", "post-drafts");
    let unprepared = (unprepared.into_iter())
        .filter(|(key, _)| key != "prep")
        .collect();
    let unprepared = given_an_item(&world, &with(unprepared, "source", "mine"));

    let output = when_rider_runs(&world, &["queue"]);

    assert_eq!(output.code, Some(0), "{output:?}");
    let expected = format!(
        "{HEADER}
 !  invalid #{bad}: label kind \"bogus\" is not review, respond or unblock
 !  invalid #{unprepared}: label prep is missing

daemon not running
"
    );
    assert_eq!(output.stdout, expected);
}

// The board refuses a needs cycle itself (`vb tasks needs`: "they would wait for each
// other"), so only the item bound is reachable from outside.
#[test]
fn rider_queue_refuses_a_201st_item() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let repo = world.repo_label().to_string();
    given_a_queue_toml(&world, &repo);
    for n in 0..201 {
        given_an_item(
            &world,
            &an_owner_item(n, "review", &ago(60), "none", "approve"),
        );
    }

    let many = when_rider_runs(&world, &["queue"]);

    assert_eq!(many.code, Some(1), "{many:?}");
    assert_eq!(many.stderr, "rider: the queue holds more than 200 items\n");
}

#[test]
fn rider_queue_without_an_opted_in_repository_exits_1_with_the_toml_to_add() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };

    let output = when_rider_runs(&world, &["queue"]);

    assert_eq!(output.code, Some(1), "{output:?}");
    let expected = format!(
        "no repository is opted in to the queue; add to {}:\n\
         [repo.\"/path/to/checkout\"]\nqueue = true\n",
        world.home.join("queue.toml").display()
    );
    assert_eq!(output.stdout, expected);
}
