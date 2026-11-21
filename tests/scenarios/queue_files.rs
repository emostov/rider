use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use crate::steps::*;
use crate::world::World;

const BASE: &str = "1111111111111111111111111111111111111111";
const HEAD: &str = "2222222222222222222222222222222222222222";

fn given_a_fake_gh_serving(world: &World, files: &[(&str, &str)]) {
    let store = world.root.join("gh-files");
    fs::create_dir_all(&store).expect("create the fake gh store");
    let list: Vec<String> = (files.iter())
        .map(|(name, _)| format!(r#"{{"filename":"{name}","status":"added"}}"#))
        .collect();
    fs::write(store.join("list.json"), format!("[[{}]]", list.join(","))).expect("list");
    for (index, (_, content)) in files.iter().enumerate() {
        fs::write(store.join(index.to_string()), content).expect("write a file");
    }
    let mut cases = String::new();
    for (index, (name, _)) in files.iter().enumerate() {
        let url = format!("repos/acme/widgets/contents/{name}?ref={HEAD}");
        cases.push_str(&format!(
            "    \"{url}\") cat \"{}/{index}\" ;;\n",
            store.display()
        ));
    }
    let script = format!(
        "#!/bin/sh\nfor arg in \"$@\"; do last=\"$arg\"; case \"$arg\" in\n\
         repos/*) url=\"$arg\" ;;\nesac; done\ncase \"$url\" in\n\
         \"repos/acme/widgets/pulls/412/files?per_page=100\") cat \"{store}/list.json\" ;;\n\
         \"repos/acme/widgets/compare/{BASE}...{HEAD}\") printf 'diff --git a/x b/x\\n' ;;\n\
         {cases}    *) echo \"unknown $url\" >&2; exit 1 ;;\nesac\n",
        store = store.display()
    );
    fs::create_dir_all(&world.agents).expect("create the agents directory");
    let path = world.agents.join("gh");
    fs::write(&path, script).expect("write the fake gh");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod the fake gh");
}

fn when_the_files_are_fetched(world: &World) -> crate::world::Output {
    when_rider_runs(
        world,
        &["queue", "files", "7", "acme/widgets#412", BASE, HEAD],
    )
}

fn git_in(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", dir)
        .output()
        .expect("run git")
}

#[test]
fn rider_queue_files_writes_a_prs_files_where_they_cannot_form_a_git_repository() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let marker = world.root.join("external-diff-ran");
    let config = format!(
        "[diff]\n\texternal = sh -c 'echo ran >> {}' x\n",
        marker.display()
    );
    given_a_fake_gh_serving(
        &world,
        &[
            ("HEAD", "ref: refs/heads/main\n"),
            ("config", &config),
            ("objects/keep", ""),
            ("refs/keep", ""),
            ("src/a", "one\n"),
            ("src/b", "two\n"),
        ],
    );

    let output = when_the_files_are_fetched(&world);

    assert_eq!(output.code, Some(0), "{output:?}");
    let dir = world.home.join("prs").join(format!("7-{HEAD}"));
    assert_eq!(output.stdout, format!("{}\n", dir.display()));
    assert_eq!(
        fs::read_to_string(dir.join("patch.diff")).ok().as_deref(),
        Some("diff --git a/x b/x\n")
    );
    let files = dir.join("files");
    assert_eq!(
        fs::read_to_string(files.join("config.pr")).ok(),
        Some(config.clone())
    );
    assert!(!files.join("HEAD").exists() && !files.join("config").exists());
    let src = files.join("src");
    let found = git_in(&src, &["rev-parse", "--git-dir"]);
    assert!(!found.status.success(), "{found:?}");
    let _ = git_in(&src, &["diff", "--no-index", "a.pr", "b.pr"]);
    assert!(!marker.exists());
}

#[test]
fn rider_queue_files_refuses_a_path_it_cannot_store_safely() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_fake_gh_serving(&world, &[("a';touch x;'.txt", "x\n")]);

    let output = when_the_files_are_fetched(&world);

    assert_eq!(output.code, Some(1), "{output:?}");
    let line = "rider: the pull request has a path rider will not store: \"a';touch x;'.txt\"\n";
    assert_eq!(output.stderr, line);
    assert!(!world.home.join("prs").join(format!("7-{HEAD}")).exists());
}
