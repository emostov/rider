use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::process::Command;

use crate::fake_agent::{BLOCK_QUESTION, Behaviour};
use crate::steps::*;
use crate::world::{LEAD, World, run_bounded, write_executable};

type EnvCase = (
    Vec<(&'static str, OsString)>,
    &'static [&'static str],
    &'static str,
);

const BAD_ID: &str = "is not a task id: use its number, such as 42 or #42";

fn rider(world: &World) -> Command {
    world.command(Path::new(env!("CARGO_BIN_EXE_rider")))
}

fn given_a_fake_vb_in(home: &Path, record: &Path, code: u8) {
    let bin = home.join("node/node_modules/.bin");
    fs::create_dir_all(&bin).expect("create the fake vb's directory");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexit {code}\n",
        record.display()
    );
    write_executable(&bin.join("vb"), &script);
}

#[test]
fn rider_help_in_each_spelling_prints_the_commands_and_exits_zero() {
    let Some(world) = World::new() else {
        return;
    };

    for help in ["--help", "-h", "help"] {
        let output = when_rider_runs(&world, &[help]);

        assert_eq!(output.code, Some(0), "{help}: {output:?}");
        assert!(output.stdout.contains("Usage: rider"), "{help}: {output:?}");
        assert!(output.stdout.contains("daemon"), "{help}: {output:?}");
    }
}

#[test]
fn rider_with_a_missing_extra_or_removed_argument_exits_two_saying_which() {
    let Some(world) = World::new() else {
        return;
    };
    let cases: [(&[&str], &str); 7] = [
        (&[], "Usage: rider"),
        (
            &["kill"],
            "the following required arguments were not provided",
        ),
        (
            &["logs"],
            "the following required arguments were not provided",
        ),
        (&["serve", "now"], "unexpected argument 'now'"),
        (&["kill", "1", "2"], "unexpected argument '2'"),
        (&["start"], "unrecognized subcommand 'start'"),
        (&["pool", "rider"], "unrecognized subcommand 'pool'"),
    ];

    for (args, says) in cases {
        let output = when_rider_runs(&world, args);

        assert_eq!(output.code, Some(2), "{args:?}: {output:?}");
        assert!(output.stderr.contains(says), "{args:?}: {output:?}");
    }
}

#[test]
fn rider_kill_of_an_id_that_is_not_decimal_exits_two_and_a_number_or_hash_number_is_taken() {
    let Some(world) = World::new() else {
        return;
    };
    let bad = [
        "",
        "#",
        "-1",
        "+1",
        "1.0",
        "0x1",
        "##1",
        "18446744073709551616",
        "12a",
    ];

    for id in bad {
        let output = when_rider_runs(&world, &["kill", "--", id]);

        assert_eq!(output.code, Some(2), "{id:?}: {output:?}");
        assert!(
            output.stderr.contains(&format!("{id:?} {BAD_ID}")),
            "{output:?}"
        );
    }
    for id in ["42", "#7", "18446744073709551615"] {
        let output = when_rider_runs(&world, &["kill", id]);

        assert_eq!(output.code, Some(1), "no daemon answers {id:?}: {output:?}");
        assert!(!output.stderr.contains(BAD_ID), "{id:?}: {output:?}");
    }
}

#[test]
fn rider_refuses_a_missing_relative_empty_non_unicode_or_colon_home_naming_the_variable() {
    let Some(world) = World::new() else {
        return;
    };
    let bad = || OsString::from_vec(vec![0x2f, 0xff]);
    let cases: [EnvCase; 5] = [
        (
            vec![],
            &["RIDER_HOME", "HOME"],
            "neither RIDER_HOME nor HOME is set",
        ),
        (
            vec![("RIDER_HOME", "rider".into())],
            &[],
            r#"RIDER_HOME must be an absolute path, not "rider""#,
        ),
        (
            vec![("BOARD_URL", "".into())],
            &[],
            "BOARD_URL is set but empty",
        ),
        (
            vec![("RIDER_HOME", bad())],
            &[],
            "RIDER_HOME is not valid UTF-8",
        ),
        (
            vec![("RIDER_HOME", "/srv/a:b".into())],
            &[],
            r#"rider's home "/srv/a:b" must not contain ':'"#,
        ),
    ];

    for (set, unset, says) in cases {
        let mut command = rider(&world);
        command.envs(set.iter().map(|(name, value)| (name, value)));
        for name in unset {
            command.env_remove(name);
        }
        command.arg("config");
        let output = run_bounded(command, None);

        assert_eq!(output.code, Some(1), "{says}: {output:?}");
        assert_eq!(output.stderr, format!("rider: {says}\n"));
    }
    let mut command = rider(&world);
    command.env("HOME", bad()).arg("config");
    let output = run_bounded(command, None);
    assert_eq!(
        output.code,
        Some(0),
        "RIDER_HOME wins over a bad HOME: {output:?}"
    );
}

#[test]
fn rider_serve_runs_vb_on_the_homes_board_in_the_foreground_and_exits_with_its_code() {
    let Some(world) = World::new() else {
        return;
    };
    let user_home = world.root.join("serving-user");
    let home = user_home.join(".rider");
    let record = world.root.join("vb-args");
    given_a_fake_vb_in(&home, &record, 3);

    let mut command = rider(&world);
    command
        .env_remove("RIDER_HOME")
        .env("HOME", &user_home)
        .arg("serve");
    let output = run_bounded(command, None);

    assert_eq!(output.code, Some(3), "{output:?}");
    let board = home.join("board");
    assert!(board.is_dir());
    let args = fs::read_to_string(&record).expect("read the fake vb's arguments");
    assert_eq!(
        args,
        format!("serve\n--dir\n{}\n--no-open\n", board.display())
    );
    assert!(!home.join("prompts").exists(), "serve copies no prompts");
}

#[test]
fn rider_without_board_url_reaches_the_local_board_on_port_4100() {
    let Some(world) = World::new() else {
        return;
    };
    let home = world.root.join("default-url-home");
    let record = world.root.join("vb-args");
    given_a_fake_vb_in(&home, &record, 1);

    let mut command = rider(&world);
    command
        .env_remove("BOARD_URL")
        .env("RIDER_HOME", &home)
        .args(["wait", "rider", "--timeout", "1"]);
    let output = run_bounded(command, None);

    assert_eq!(
        output.code,
        Some(1),
        "the fake vb fails the read: {output:?}"
    );
    let args = fs::read_to_string(&record).expect("read the fake vb's arguments");
    let url = args.lines().skip_while(|arg| *arg != "--url").nth(1);
    assert_eq!(url, Some("http://127.0.0.1:4100/api/board"), "{args}");
}

#[test]
fn an_agent_whose_inherited_path_lacks_the_board_cli_still_runs_rider_homes_vb() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let vb_dir = world
        .vb
        .parent()
        .expect("vb has a directory")
        .display()
        .to_string();
    let path = world.child_path().replace(&format!(":{vb_dir}:"), ":");
    assert!(!path.contains(&vb_dir), "{path}");
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::Block]);

    given_a_running_daemon_with(&mut world, &[("PATH", &path)]);
    let states = when_the_tasks_settle(&world, &[&task]);

    assert_eq!(states, vec!["blocked"]);
    let block = task_block(&world, &task);
    assert_eq!(
        block,
        Some(("needs_input".to_string(), BLOCK_QUESTION.to_string()))
    );
}

const PINNED_BOARD: &str = "@vercel/board@0.52.0";

fn given_a_fake_npm_in(dir: &Path, record: &Path) {
    fs::create_dir_all(dir).expect("create the fake npm's directory");
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$@" >> '{}'
mkdir -p "$3/node_modules/.bin"
printf '#!/bin/sh\nexit 3\n' > "$3/node_modules/.bin/vb"
chmod +x "$3/node_modules/.bin/vb"
"#,
        record.display()
    );
    write_executable(&dir.join("npm"), &script);
}

fn listed_ids(json: &str) -> Vec<String> {
    let value: serde_json::Value = serde_json::from_str(json)
        .unwrap_or_else(|error| panic!("not a task listing ({error}): {json}"));
    let tasks = value["tasks"].as_array().expect("a listing holds tasks");
    tasks
        .iter()
        .map(|task| match &task["message"]["id"] {
            serde_json::Value::String(id) => id.clone(),
            id => id.to_string(),
        })
        .collect()
}

#[test]
fn rider_vb_runs_the_homes_board_cli_when_vb_is_not_on_path() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "List me."));
    let vb_dir = world.vb.parent().expect("vb has a directory");
    let path = world
        .child_path()
        .replace(&format!(":{}:", vb_dir.display()), ":");
    assert!(!path.contains(&vb_dir.display().to_string()), "{path}");

    let mut command = rider(&world);
    command.env("PATH", &path).args([
        "vb",
        "tasks",
        "work/demo",
        "--state",
        "all",
        "--json",
        "--as",
        LEAD,
    ]);
    let output = run_bounded(command, None);

    assert_eq!(output.code, Some(0), "{output:?}");
    assert_eq!(listed_ids(&output.stdout), vec![task]);
}

#[test]
fn rider_vb_passes_every_argument_and_the_local_board_url_and_exits_with_vbs_code() {
    let Some(world) = World::new() else {
        return;
    };
    let home = world.root.join("vb-home");
    let record = world.root.join("vb-args");
    let bin = home.join("node/node_modules/.bin");
    fs::create_dir_all(&bin).expect("create the fake vb's directory");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$BOARD_URL\" \"$@\" > '{}'\nexit 7\n",
        record.display()
    );
    write_executable(&bin.join("vb"), &script);
    let args = ["reply", "42", "--help", "--", "-starts with a dash"];

    let mut command = rider(&world);
    command
        .env_remove("BOARD_URL")
        .env("RIDER_HOME", &home)
        .arg("vb")
        .args(args);
    let output = run_bounded(command, None);

    assert_eq!(output.code, Some(7), "{output:?}");
    let recorded = fs::read_to_string(&record).expect("read the fake vb's arguments");
    let expected = format!("http://127.0.0.1:4100/api/board\n{}\n", args.join("\n"));
    assert_eq!(recorded, expected);
    let mut command = rider(&world);
    command
        .env("BOARD_URL", "http://127.0.0.1:9/api/board")
        .env("RIDER_HOME", &home)
        .args(["vb", "whoami"]);
    let output = run_bounded(command, None);
    assert_eq!(output.code, Some(7), "{output:?}");
    let recorded = fs::read_to_string(&record).expect("read the fake vb's arguments");
    assert_eq!(recorded, "http://127.0.0.1:9/api/board\nwhoami\n");
}

#[test]
fn rider_serve_installs_the_pinned_board_cli_once_when_it_is_missing() {
    let Some(world) = World::new() else {
        return;
    };
    let home = world.root.join("empty-home");
    let npm_dir = world.root.join("npm");
    let record = world.root.join("npm-args");
    given_a_fake_npm_in(&npm_dir, &record);
    let path = format!("{}:{}", npm_dir.display(), world.child_path());

    for _ in 0..2 {
        let mut command = rider(&world);
        command
            .env("PATH", &path)
            .env("RIDER_HOME", &home)
            .arg("serve");
        let output = run_bounded(command, None);

        assert_eq!(output.code, Some(3), "the stand-in vb ran: {output:?}");
    }
    let recorded = fs::read_to_string(&record).expect("read the fake npm's arguments");
    let prefix = home.join("node");
    assert_eq!(
        recorded,
        format!("install\n--prefix\n{}\n{PINNED_BOARD}\n", prefix.display())
    );
}

#[test]
fn rider_serve_and_vb_without_the_board_cli_or_npm_exit_one_naming_node_22() {
    let Some(world) = World::new() else {
        return;
    };
    let home = world.root.join("no-npm-home");
    let empty = world.root.join("no-tools");
    fs::create_dir_all(&empty).expect("create an empty PATH directory");

    for args in [&["serve"][..], &["vb", "whoami"], &["up"]] {
        let mut command = rider(&world);
        command
            .env("PATH", &empty)
            .env("RIDER_HOME", &home)
            .args(args);
        let output = run_bounded(command, None);

        assert_eq!(output.code, Some(1), "{args:?}: {output:?}");
        assert!(output.stderr.contains("Node 22"), "{args:?}: {output:?}");
    }
}
