use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::fake_agent::{self, Behaviour};
use crate::steps::*;
use crate::world::World;

const FAILED: &str = "agent reported failure in its output stream";
const RECONNECTED: &str = "Blocking: `tests/scenarios/world.rs:454` — C-locale Unicode paths \
     cause agent leaks. Blocking: `tests/scenarios/world.rs:509` — discovery errors abandon \
     agent cleanup. 3 non-blocking findings.";
const LINE_BYTES_MAX: usize = 4 * 1024 * 1024;

enum Judged {
    Completed(&'static str),
    Failed,
}

fn recorded(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/harness")
        .join(name);
    fs::read_to_string(&path).expect("read a recorded agent stream")
}

fn when_agents_replay(world: &mut World, streams: &[(String, Judged)]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for (text, _) in streams {
        let id = when_the_lead_creates(world, &an_implement_task(world, "Add the work."));
        given_the_agent_will(world, &id, &[Behaviour::CommitReplaying]);
        fake_agent::stream(&world.root, &id, text.as_bytes());
        ids.push(id);
    }
    let tasks: Vec<&str> = ids.iter().map(String::as_str).collect();
    let (states, exit) = when_the_daemon_runs_until_settled(world, &tasks);
    assert_eq!(exit, Some(0));
    for ((id, state), (_, judged)) in ids.iter().zip(&states).zip(streams) {
        let read: Value = serde_json::from_str(&task_text(world, id)).expect("vb read JSON");
        let task = &read["message"]["task"];
        match judged {
            Judged::Completed(_) if state != "completed" => {
                panic!("{id} {state}: {}", task_text(world, id))
            }
            Judged::Completed(result) => {
                assert_eq!(
                    (state.as_str(), &task["result"]),
                    ("completed", &(*result).into())
                );
            }
            Judged::Failed if state == "completed" => panic!("{id} completed: {task}"),
            Judged::Failed => {
                assert_eq!(
                    (state.as_str(), &task["reason"]),
                    ("failed", &FAILED.into())
                );
            }
        }
    }
    ids
}

#[test]
fn a_claude_stream_is_judged_by_its_last_result_and_hostile_lines_are_skipped() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let huge = "y".repeat(LINE_BYTES_MAX + 10);
    let hostile = format!(
        "{huge}\nnot json\n[1,2]\n\"str\"\n{{\"type\":\"result\",\"result\":7}}\n\
         {{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"  first  \"}}\n{huge}"
    );
    let streams = [
        (recorded("claude.jsonl"), Judged::Completed("DONE")),
        (hostile, Judged::Completed("first")),
        (
            r#"{"type":"result","subtype":"success","is_error":true}"#.to_string(),
            Judged::Failed,
        ),
        (
            r#"{"type":"result","subtype":"error_max_turns","is_error":false}"#.to_string(),
            Judged::Failed,
        ),
        (
            r#"{"type":"result","subtype":"error_during_execution","is_error":true}
{"type":"result","subtype":"success","is_error":false}"#
                .to_string(),
            Judged::Completed("completed"),
        ),
    ];

    when_agents_replay(&mut world, &streams);
}

#[test]
fn a_codex_stream_is_judged_by_how_its_turn_ended() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_rider_toml(
        &world,
        "[roles.implement]\nharness = \"codex\"\nmodel = \"gpt-test-implement\"\n",
    );
    let reconnect = recorded("codex-reconnect.jsonl");
    let cut: Vec<&str> = (reconnect.lines())
        .filter(|line| !line.contains("turn.completed"))
        .collect();
    assert_eq!(cut.len() + 1, reconnect.lines().count());
    assert_eq!(
        cut.iter().filter(|l| l.contains("Reconnecting...")).count(),
        4
    );
    let streams = [
        (recorded("codex.jsonl"), Judged::Completed("DONE")),
        (reconnect.clone(), Judged::Completed(RECONNECTED)),
        (cut.join("\n"), Judged::Failed),
        (
            r#"{"type":"item.completed","item":{"type":"error"}}"#.to_string(),
            Judged::Completed("completed"),
        ),
        (
            r#"{"type":"turn.failed","error":{"message":"x"}}"#.to_string(),
            Judged::Failed,
        ),
        (
            r#"{"type":"error","message":"x"}"#.to_string(),
            Judged::Failed,
        ),
    ];

    when_agents_replay(&mut world, &streams);
}

#[test]
fn only_a_session_id_claude_could_not_misread_is_kept_for_a_later_resume() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let result = r#"{"type":"result","subtype":"success","is_error":false,"result":"DONE"}"#;
    let hostile = format!(
        "{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"--help\"}}\n{result}\n"
    );
    let streams = [
        (recorded("claude.jsonl"), Judged::Completed("DONE")),
        (hostile, Judged::Completed("DONE")),
    ];

    let ids = when_agents_replay(&mut world, &streams);

    let session = world.home.join("logs").join(&ids[0]).join("session");
    let kept = fs::read_to_string(&session).expect("the kept session");
    let repo = world.repo_label();
    assert_eq!(
        kept,
        format!("20de60af-1a54-4519-8f52-8623adc5d76e\n{repo}\n")
    );
    assert_eq!(task_logs(&world, &ids[1]), vec!["1.jsonl", "result.json"]);
}
