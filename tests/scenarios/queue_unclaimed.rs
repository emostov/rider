use std::collections::BTreeMap;

use serde_json::Value;

use crate::steps::*;
use crate::world::{MAIN, World};

const QUEUE: &str = "queue";

fn when_the_lead_queues_an_implement_task(world: &World, text: &str) -> String {
    let repo = format!("repo={}", world.repo_label());
    let from = format!("from={MAIN}");
    let args = [
        "tasks", "create", QUEUE, "-", "--json", "--label", &repo, "--label", &from,
    ];
    let output = world.vb_lead(&args, Some(text));
    let created: Value = serde_json::from_str(&output).expect("vb prints JSON");
    let id = created["task"]["id"]
        .as_str()
        .or(created["message"]["id"].as_str());
    let id = id
        .unwrap_or_else(|| panic!("no id in {output}"))
        .to_string();
    world.vb_lead(&["label", &id, "role=implement"], None);
    assert!(!id.is_empty());
    id
}

fn queued_labels(world: &World) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("board.task".to_string(), "ready".to_string()),
        ("from".to_string(), MAIN.to_string()),
        ("repo".to_string(), world.repo_label().to_string()),
        ("role".to_string(), "implement".to_string()),
    ])
}

#[test]
fn a_queue_task_with_an_implement_role_is_never_claimed_while_work_tasks_are() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let listed = when_the_lead_queues_an_implement_task(&world, "Review the listed PR.");
    given_a_running_daemon(&mut world);
    let followed = when_the_lead_queues_an_implement_task(&world, "Review the followed PR.");
    let work = when_the_lead_creates(&world, &an_implement_task(&world, "Add one work file."));

    assert_eq!(when_the_tasks_settle(&world, &[&work]), vec!["completed"]);
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));

    for queued in [&listed, &followed] {
        assert_eq!(task_state(&world, queued), "ready");
        assert_eq!(task_labels(&world, queued), queued_labels(&world));
        assert_eq!(task_replies(&world, queued), Vec::<Value>::new());
        assert_eq!(agent_runs_for(&world, queued), vec![]);
    }
    assert_eq!(agent_runs_for(&world, &work).len(), 1);
}
