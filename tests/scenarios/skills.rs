use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

use serde_json::{Value, json};

use crate::steps::{
    given_a_board_and_a_repository, given_a_running_daemon, then_the_daemon_exits, when_rider_runs,
    when_the_daemon_is_sent_sigterm,
};
use crate::world::{Output, World, run_bounded};

const MANIFEST: &str = ".rider-install.json";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const SKILLS: [&str; 2] = ["ride-swarm", "ship-swarm"];
const SOURCE_FILES_MAX: usize = 64;

fn source_files() -> BTreeMap<String, Vec<u8>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills");
    let mut files = BTreeMap::new();
    let mut pending = vec![root.clone()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("read the skills directory") {
            let path = entry.expect("read a skills entry").path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            let relative = path.strip_prefix(&root).expect("under skills/");
            let relative = relative.to_str().expect("UTF-8 path").to_string();
            files.insert(relative, fs::read(&path).expect("read a skill file"));
            assert!(
                files.len() <= SOURCE_FILES_MAX,
                "skills/ holds too many files"
            );
        }
    }
    files
}

fn when_skills_are_installed_into(world: &World, dir: &Path) -> Output {
    let dir = dir.to_str().expect("UTF-8 path");
    when_rider_runs(world, &["skills", "install", "--into", dir])
}

fn manifest(dir: &Path, skill: &str) -> Value {
    let text = fs::read_to_string(dir.join(skill).join(MANIFEST)).expect("read the manifest");
    serde_json::from_str(&text).expect("the manifest is JSON")
}

fn given_an_older_install_without(dir: &Path, skill: &str, file: &str) {
    let mut older = manifest(dir, skill);
    older["version"] = json!("0.0.1");
    older["digest"] = json!("0000000000000000");
    let files = older["files"]
        .as_object_mut()
        .expect("the manifest lists files");
    assert!(files.remove(file).is_some(), "{file} is in the manifest");
    fs::remove_file(dir.join(skill).join(file)).expect("remove a skill file");
    let text = serde_json::to_string(&older).expect("encode the manifest");
    fs::write(dir.join(skill).join(MANIFEST), text).expect("write the manifest");
}

fn given_a_hand_edit(dir: &Path, skill: &str, file: &str) -> String {
    let path = dir.join(skill).join(file);
    let edited = format!(
        "{}\nMy own note.\n",
        fs::read_to_string(&path).expect("read")
    );
    fs::write(&path, &edited).expect("edit a skill file");
    edited
}

fn installed_lines(dir: &Path, word: &str) -> String {
    SKILLS
        .iter()
        .map(|skill| format!("{word} {}\n", dir.join(skill).display()))
        .collect()
}

fn agent_skills(world: &World, name: &str) -> PathBuf {
    world.root.join(name).join("skills")
}

#[test]
fn rider_skills_install_writes_every_skill_file_and_a_manifest_per_skill() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let dir = agent_skills(&world, "claude");

    let output = when_skills_are_installed_into(&world, &dir);

    assert_eq!(output.code, Some(0), "{output:?}");
    assert_eq!(output.stdout, installed_lines(&dir, "installed"));
    for (path, content) in source_files() {
        let installed = fs::read(dir.join(&path)).unwrap_or_else(|e| panic!("{path}: {e}"));
        assert_eq!(installed, content, "{path}");
    }
    for skill in SKILLS {
        let listed = manifest(&dir, skill);
        assert_eq!(listed["version"], json!(VERSION));
        let mut names: Vec<&str> = listed["files"]
            .as_object()
            .expect("the manifest lists files")
            .keys()
            .map(String::as_str)
            .collect();
        names.sort_unstable();
        let prefix = format!("{skill}/");
        let expected: Vec<String> = source_files()
            .into_keys()
            .filter_map(|path| path.strip_prefix(&prefix).map(str::to_string))
            .collect();
        assert_eq!(names, expected);
    }
}

#[test]
fn rider_skills_install_without_into_installs_into_the_rider_home() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };

    let output = when_rider_runs(&world, &["skills", "install"]);

    assert_eq!(output.code, Some(0), "{output:?}");
    let dir = world.home.join("skills");
    assert_eq!(output.stdout, installed_lines(&dir, "installed"));
    assert_eq!(manifest(&dir, "ride-swarm")["version"], json!(VERSION));
}

#[test]
fn a_second_install_of_the_same_version_changes_nothing() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let dir = agent_skills(&world, "codex");
    let first = when_skills_are_installed_into(&world, &dir);
    assert_eq!(first.code, Some(0), "{first:?}");
    let before = fs::metadata(dir.join("ride-swarm").join(MANIFEST)).expect("stat");

    let second = when_skills_are_installed_into(&world, &dir);

    assert_eq!(second.code, Some(0), "{second:?}");
    assert_eq!(second.stdout, installed_lines(&dir, "up to date"));
    let after = fs::metadata(dir.join("ride-swarm").join(MANIFEST)).expect("stat");
    assert_eq!(after.modified().ok(), before.modified().ok());
}

#[test]
fn an_older_unmodified_install_is_replaced() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let dir = agent_skills(&world, "claude");
    assert_eq!(when_skills_are_installed_into(&world, &dir).code, Some(0));
    given_an_older_install_without(&dir, "ride-swarm", "lead.md");

    let output = when_skills_are_installed_into(&world, &dir);

    assert_eq!(output.code, Some(0), "{output:?}");
    let expected = format!(
        "replaced {} (was 0.0.1)\nup to date {}\n",
        dir.join("ride-swarm").display(),
        dir.join("ship-swarm").display()
    );
    assert_eq!(output.stdout, expected);
    let lead = fs::read(dir.join("ride-swarm/lead.md")).expect("lead.md is back");
    assert_eq!(Some(&lead), source_files().get("ride-swarm/lead.md"));
    assert_eq!(manifest(&dir, "ride-swarm")["version"], json!(VERSION));
}

#[test]
fn a_hand_edited_install_is_refused_and_left_untouched() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let dir = agent_skills(&world, "claude");
    assert_eq!(when_skills_are_installed_into(&world, &dir).code, Some(0));
    given_an_older_install_without(&dir, "ride-swarm", "lead.md");
    let edited = given_a_hand_edit(&dir, "ride-swarm", "SKILL.md");
    let older = manifest(&dir, "ride-swarm");

    let output = when_skills_are_installed_into(&world, &dir);

    assert_eq!(output.code, Some(1), "{output:?}");
    let skipped = format!(
        "skipped {}: edited by hand: SKILL.md\n",
        dir.join("ride-swarm").display()
    );
    assert!(output.stdout.contains(&skipped), "{output:?}");
    let skill = fs::read_to_string(dir.join("ride-swarm/SKILL.md")).expect("read SKILL.md");
    assert_eq!(skill, edited);
    assert!(!dir.join("ride-swarm/lead.md").exists());
    assert_eq!(manifest(&dir, "ride-swarm"), older);
}

#[test]
fn installing_anywhere_updates_every_recorded_older_install() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let claude = agent_skills(&world, "claude");
    let codex = agent_skills(&world, "codex");
    assert_eq!(
        when_skills_are_installed_into(&world, &claude).code,
        Some(0)
    );
    assert_eq!(when_skills_are_installed_into(&world, &codex).code, Some(0));
    given_an_older_install_without(&claude, "ship-swarm", "settings.md");

    let output = when_skills_are_installed_into(&world, &codex);

    assert_eq!(output.code, Some(0), "{output:?}");
    let replaced = format!(
        "replaced {} (was 0.0.1)\n",
        claude.join("ship-swarm").display()
    );
    assert!(output.stdout.contains(&replaced), "{output:?}");
    assert_eq!(manifest(&claude, "ship-swarm")["version"], json!(VERSION));
}

#[test]
fn a_recorded_older_install_is_updated_when_the_daemon_starts_and_an_edited_one_skipped() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let claude = agent_skills(&world, "claude");
    let codex = agent_skills(&world, "codex");
    assert_eq!(
        when_skills_are_installed_into(&world, &claude).code,
        Some(0)
    );
    assert_eq!(when_skills_are_installed_into(&world, &codex).code, Some(0));
    given_an_older_install_without(&claude, "ride-swarm", "lead.md");
    given_an_older_install_without(&codex, "ride-swarm", "lead.md");
    let edited = given_a_hand_edit(&codex, "ride-swarm", "SKILL.md");

    given_a_running_daemon(&mut world);
    when_the_daemon_is_sent_sigterm(&world);
    then_the_daemon_exits(&mut world);

    let lead = fs::read(claude.join("ride-swarm/lead.md")).expect("lead.md is back");
    assert_eq!(Some(&lead), source_files().get("ride-swarm/lead.md"));
    assert_eq!(manifest(&claude, "ride-swarm")["version"], json!(VERSION));
    let skill = fs::read_to_string(codex.join("ride-swarm/SKILL.md")).expect("read SKILL.md");
    assert_eq!(skill, edited);
    let skipped = format!(
        "skipped {}: edited by hand: SKILL.md",
        codex.join("ride-swarm").display()
    );
    assert!(
        world.daemon_log().contains(&skipped),
        "{}",
        world.daemon_log()
    );
}

#[test]
fn rider_skills_list_and_show_print_the_embedded_skills() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };

    let list = when_rider_runs(&world, &["skills", "list"]);
    let show = when_rider_runs(&world, &["skills", "show", "ship-swarm"]);
    let unknown = when_rider_runs(&world, &["skills", "show", "no-such-skill"]);

    assert_eq!(list.code, Some(0), "{list:?}");
    let names: Vec<&str> = list
        .stdout
        .lines()
        .map(|line| line.split_once("  ").map_or(line, |(name, _)| name))
        .collect();
    assert_eq!(names, SKILLS);
    assert!(
        list.stdout
            .contains("ride-swarm  Use when the user asks to ride a swarm")
    );
    assert_eq!(show.code, Some(0), "{show:?}");
    let source = source_files();
    let expected = source
        .get("ship-swarm/SKILL.md")
        .expect("ship-swarm has a SKILL.md");
    assert_eq!(show.stdout.as_bytes(), expected.as_slice());
    assert_eq!(unknown.code, Some(1), "{unknown:?}");
    assert!(unknown.stderr.contains("no-such-skill"), "{unknown:?}");
}

const INSTALLERS: usize = 6;
const INSTALLS_MAX: usize = 32;

fn rider(world: &World) -> Command {
    world.command(Path::new(env!("CARGO_BIN_EXE_rider")))
}

#[test]
fn several_installs_into_one_directory_at_once_leave_every_skill_whole() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let dir = agent_skills(&world, "claude");

    let outputs: Vec<Output> = thread::scope(|scope| {
        let installers: Vec<_> = (0..INSTALLERS)
            .map(|_| scope.spawn(|| when_skills_are_installed_into(&world, &dir)))
            .collect();
        installers
            .into_iter()
            .map(|installer| installer.join().expect("an installer thread"))
            .collect()
    });

    for output in &outputs {
        assert_eq!(output.code, Some(0), "{output:?}");
    }
    let again = when_skills_are_installed_into(&world, &dir);
    assert_eq!(again.stdout, installed_lines(&dir, "up to date"), "{again:?}");
    for (path, content) in source_files() {
        assert_eq!(fs::read(dir.join(&path)).ok(), Some(content), "{path}");
    }
}

#[test]
fn a_replace_killed_between_its_renames_is_finished_by_the_next_run() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let claude = agent_skills(&world, "claude");
    let codex = agent_skills(&world, "codex");
    assert_eq!(when_skills_are_installed_into(&world, &claude).code, Some(0));
    given_an_older_install_without(&claude, "ship-swarm", "settings.md");
    let aside = claude.join(".ship-swarm.rider-old");
    fs::rename(claude.join("ship-swarm"), &aside).expect("move the skill aside");

    let output = when_skills_are_installed_into(&world, &codex);

    assert_eq!(output.code, Some(0), "{output:?}");
    let replaced = format!("replaced {} (was 0.0.1)\n", claude.join("ship-swarm").display());
    assert!(output.stdout.contains(&replaced), "{output:?}");
    let settings = fs::read(claude.join("ship-swarm/settings.md")).ok();
    assert_eq!(settings.as_ref(), source_files().get("ship-swarm/settings.md"));
    assert!(!aside.exists());
}

#[test]
fn an_into_path_that_is_not_utf8_or_holds_a_newline_is_refused_before_any_write() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let mut bytes = world.root.join("bad").into_os_string().into_vec();
    bytes.push(0xff);
    let not_utf8 = PathBuf::from(OsString::from_vec(bytes));
    let newline = world.root.join("first\n").join("second");

    for dir in [&not_utf8, &newline] {
        let mut command = rider(&world);
        command.args(["skills", "install", "--into"]).arg(dir);
        let output = run_bounded(command, None);

        assert_eq!(output.code, Some(1), "{output:?}");
        assert!(output.stderr.contains("--into"), "{output:?}");
        assert!(!dir.exists(), "{}", dir.display());
    }
    assert!(!world.home.join("skill-installs").exists());
}

#[test]
fn a_fifo_in_place_of_a_manifest_is_skipped_without_waiting() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let claude = agent_skills(&world, "claude");
    let codex = agent_skills(&world, "codex");
    assert_eq!(when_skills_are_installed_into(&world, &claude).code, Some(0));
    let manifest = claude.join("ride-swarm").join(MANIFEST);
    fs::remove_file(&manifest).expect("remove the manifest");
    let mut mkfifo = Command::new("mkfifo");
    mkfifo.arg(&manifest);
    assert_eq!(run_bounded(mkfifo, None).code, Some(0));

    let output = when_skills_are_installed_into(&world, &codex);

    assert_eq!(output.code, Some(0), "{output:?}");
    let skipped = format!("skipped {}: ", claude.join("ride-swarm").display());
    let line = output.stdout.lines().find(|line| line.starts_with(&skipped));
    assert!(
        line.is_some_and(|line| line.ends_with("is not a regular file")),
        "{output:?}"
    );
}

#[test]
fn a_directory_where_nothing_was_installed_is_not_recorded() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let file = world.root.join("a-file");
    fs::write(&file, "not a directory\n").expect("write a file");
    let failed = when_skills_are_installed_into(&world, &file);
    assert_eq!(failed.code, Some(1), "{failed:?}");

    let output = when_skills_are_installed_into(&world, &agent_skills(&world, "claude"));

    assert_eq!(output.code, Some(0), "{output:?}");
    let file_text = file.display().to_string();
    assert!(!output.stdout.contains(&file_text), "{output:?}");
}

#[test]
fn a_full_record_refuses_a_new_directory_before_writing_and_forgets_removed_ones() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let dirs: Vec<PathBuf> = (0..INSTALLS_MAX)
        .map(|index| agent_skills(&world, &format!("agent{index}")))
        .collect();
    for dir in &dirs {
        assert_eq!(when_skills_are_installed_into(&world, dir).code, Some(0));
    }
    let extra = agent_skills(&world, "extra");

    let full = when_skills_are_installed_into(&world, &extra);

    assert_eq!(full.code, Some(1), "{full:?}");
    assert!(full.stderr.contains("too many install directories"), "{full:?}");
    assert!(!extra.exists());
    fs::remove_dir_all(dirs[0].parent().expect("an agent directory")).expect("remove");
    let freed = when_skills_are_installed_into(&world, &extra);
    assert_eq!(freed.code, Some(0), "{freed:?}");
    assert_eq!(freed.stdout, installed_lines(&extra, "installed"));
}
