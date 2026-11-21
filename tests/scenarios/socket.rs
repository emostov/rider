use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::fake_agent::{Behaviour, stop_attempts};
use crate::steps::*;
use crate::world::World;

const CLIENTS_MAX: usize = 8;
const DAEMON_WRITE_SECONDS: u64 = 10;
const FLOOD_LINES: usize = 10_000;
const FLOOD_WRITE_MILLIS: u64 = 500;
const LAG_REQUESTS_MAX: u64 = 1 << 16;
const LAG_STALL_SECONDS: u64 = 3;
const READ_SECONDS_MAX: u64 = 30;
const READS_MAX: usize = 1 << 20;
const LOG_LINES: usize = 8 * 1024;
const LOG_LINE_BYTES: usize = 1024;
const DAEMON_STARTS: usize = 20;
const WATCH_POLLS_MAX: u64 = u64::MAX;
const SEAT_PROBES_MAX: usize = 1000;
const SEAT_PROBE_MILLIS: u64 = 50;

#[test]
fn a_client_flooding_bad_requests_without_reading_never_holds_up_rider_status() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_running_daemon(&mut world);
    let mut flooder = given_a_client_on_the_socket(&world);

    when_the_client_floods_bad_requests_without_reading(&mut flooder);
    let started = Instant::now();
    let status = when_rider_runs(&world, &["status"]);
    let took = started.elapsed();
    drop(flooder);

    assert_eq!(status.code, Some(0), "{status:?}");
    assert!(status.stdout.contains("board up"), "{status:?}");
    let bound = Duration::from_secs(DAEMON_WRITE_SECONDS / 2);
    assert!(
        took < bound,
        "rider status waited {took:?} on the flooding client"
    );
}

#[test]
fn a_client_that_half_closes_gets_no_more_of_a_log_it_stopped_reading() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = 7;
    given_a_log_larger_than_every_buffer(&world, task);
    given_a_running_daemon(&mut world);
    let mut client = given_a_client_on_the_socket(&world);
    let request = json!({"v": 1, "id": 1, "cmd": "logs", "task": task});
    client
        .write_all(format!("{request}\n").as_bytes())
        .expect("send the logs request");
    let first = then_the_client_reads_a_line(&client);
    let others: Vec<UnixStream> = (1..CLIENTS_MAX)
        .map(|_| given_a_client_on_the_socket(&world))
        .collect();

    client
        .shutdown(Shutdown::Write)
        .expect("half-close the client");
    let freed = when_a_seat_frees_within(&world, DAEMON_WRITE_SECONDS / 2);
    let rest = then_the_client_reads_to_its_end(&client);
    drop(others);

    assert_eq!(first, json!({"id": 1, "ok": true}));
    assert!(freed, "the daemon kept the half-closed client's seat");
    let text = String::from_utf8_lossy(&rest);
    let lines = text.lines().count();
    assert!(
        lines < LOG_LINES,
        "the daemon sent {lines} lines after the half-close"
    );
    assert!(
        !text.contains(r#""end":true"#),
        "the daemon sent the whole log"
    );
}

#[test]
fn a_lagging_client_holds_its_seat_until_it_has_read_whole_lines_ending_in_lagged() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_running_daemon(&mut world);
    let mut slow = given_a_client_on_the_socket(&world);

    let sent = when_the_client_asks_for_status_without_reading(&mut slow);
    let others: Vec<UnixStream> = (1..CLIENTS_MAX)
        .map(|_| given_a_client_on_the_socket(&world))
        .collect();
    let refused = given_a_client_on_the_socket(&world);
    let busy = then_the_client_reads_a_line(&refused);
    let data = then_the_client_reads_to_its_end(&slow);
    drop(others);

    assert_eq!(busy["error"]["code"], json!("busy"), "{busy}");
    let text = String::from_utf8(data).expect("the daemon sends UTF-8");
    assert!(text.ends_with('\n'), "a line was cut short");
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is JSON"))
        .collect();
    let answered = u64::try_from(lines.len()).expect("a line count fits in u64");
    assert!(answered <= sent, "{answered} lines for {sent} requests");
    let last = lines
        .last()
        .expect("the daemon sent the lagging client a line");
    assert_eq!(last["error"]["code"], json!("lagged"), "{last}");
    let after = when_rider_runs(&world, &["status"]);
    assert_eq!(after.code, Some(0), "{after:?}");
}

#[test]
fn the_socket_is_never_seen_at_a_mode_other_than_0600() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let path = world.home.join("rider.sock");
    let done = AtomicBool::new(false);

    let (seen, left) = thread::scope(|scope| {
        let watcher = scope.spawn(|| first_mode_other_than_0600(&path, &done));
        let stop = StopOnDrop(&done);
        let mut left = Vec::new();
        for _ in 0..DAEMON_STARTS {
            given_a_running_daemon(&mut world);
            let down = when_rider_runs(&world, &["down"]);
            assert_eq!(down.code, Some(0), "{down:?}");
            assert_eq!(then_the_daemon_exits(&mut world), Some(0));
            left.extend(
                ["rider.sock", ".bind"]
                    .into_iter()
                    .filter(|name| fs::symlink_metadata(world.home.join(name)).is_ok()),
            );
        }
        drop(stop);
        (watcher.join().expect("the watcher ends"), left)
    });

    assert_eq!(seen, None, "the socket was seen at another mode");
    assert_eq!(
        left,
        Vec::<&str>::new(),
        "left behind by a daemon that stopped"
    );
}

struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn given_a_client_on_the_socket(world: &World) -> UnixStream {
    UnixStream::connect(world.home.join("rider.sock")).expect("connect to the daemon socket")
}

fn given_a_log_larger_than_every_buffer(world: &World, task: u64) {
    let dir = world.home.join("logs").join(task.to_string());
    fs::create_dir_all(&dir).expect("create the task's log directory");
    let line = format!("{}\n", "x".repeat(LOG_LINE_BYTES - 1));
    fs::write(dir.join("1.jsonl"), line.repeat(LOG_LINES)).expect("write the log");
}

fn when_a_seat_frees_within(world: &World, seconds: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let request = format!("{}\n", json!({"v": 1, "id": 1, "cmd": "status"}));
    for _ in 0..SEAT_PROBES_MAX {
        if Instant::now() >= deadline {
            return false;
        }
        let mut probe = given_a_client_on_the_socket(world);
        // A refused client may find the daemon's end closed before its request goes.
        let _ = probe.write_all(request.as_bytes());
        let reply = then_the_client_reads_a_line(&probe);
        if reply["ok"] == json!(true) {
            return true;
        }
        assert_eq!(reply["error"]["code"], json!("busy"), "{reply}");
        thread::sleep(Duration::from_millis(SEAT_PROBE_MILLIS));
    }
    false
}

fn when_the_client_floods_bad_requests_without_reading(client: &mut UnixStream) {
    client
        .set_write_timeout(Some(Duration::from_millis(FLOOD_WRITE_MILLIS)))
        .expect("set a write timeout");
    let _ = client.write_all(&b"{}\n".repeat(FLOOD_LINES));
}

fn when_the_client_asks_for_status_without_reading(client: &mut UnixStream) -> u64 {
    client
        .set_write_timeout(Some(Duration::from_secs(LAG_STALL_SECONDS)))
        .expect("set a write timeout");
    for id in 1..=LAG_REQUESTS_MAX {
        let request = format!("{}\n", json!({"v": 1, "id": id, "cmd": "status"}));
        match client.write_all(request.as_bytes()) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock
                        | ErrorKind::TimedOut
                        | ErrorKind::ConnectionReset
                        | ErrorKind::BrokenPipe
                ) =>
            {
                return id - 1;
            }
            Err(error) => panic!("sending request {id} failed: {error}"),
        }
    }
    panic!("the daemon read all {LAG_REQUESTS_MAX} requests from a client that never read");
}

fn then_the_client_reads_a_line(client: &UnixStream) -> Value {
    client
        .set_read_timeout(Some(Duration::from_secs(READ_SECONDS_MAX)))
        .expect("set a read timeout");
    let mut line = String::new();
    BufReader::new(client)
        .read_line(&mut line)
        .expect("read a line from the daemon");
    serde_json::from_str(&line).expect("the line is JSON")
}

fn then_the_client_reads_to_its_end(mut client: &UnixStream) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(READ_SECONDS_MAX);
    let mut data = Vec::new();
    let mut buffer = vec![0; 64 * 1024];
    for _ in 0..READS_MAX {
        let left = deadline.saturating_duration_since(Instant::now());
        let bytes = data.len();
        assert!(!left.is_zero(), "no end of stream in time: {bytes} bytes");
        // macOS refuses the timeout with EINVAL once the daemon has closed its end; a read
        // then returns at once, so the error is safe to ignore.
        let _ = client.set_read_timeout(Some(left));
        match client.read(&mut buffer) {
            Ok(0) => return data,
            Ok(read) => data.extend_from_slice(&buffer[..read]),
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            // Linux resets a connection the daemon closes with the client's requests unread.
            Err(error) if error.kind() == ErrorKind::ConnectionReset => return data,
            Err(error) => panic!("reading to the end failed after {bytes} bytes: {error}"),
        }
    }
    panic!("no end of stream within {READS_MAX} reads");
}

fn first_mode_other_than_0600(path: &Path, done: &AtomicBool) -> Option<u32> {
    for _ in 0..WATCH_POLLS_MAX {
        if done.load(Ordering::SeqCst) {
            return None;
        }
        let Ok(metadata) = fs::symlink_metadata(path) else {
            continue;
        };
        let mode = metadata.permissions().mode() & 0o777;
        if metadata.file_type().is_socket() && mode != 0o600 {
            return Some(mode);
        }
    }
    None
}

const UP_REFUSAL_SECONDS_MAX: u64 = 5;

#[test]
fn rider_up_refuses_a_home_other_users_can_write_to_and_makes_no_socket() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    fs::set_permissions(&world.home, fs::Permissions::from_mode(0o777)).expect("open the home");

    let up = when_rider_runs(&world, &["up"]);

    assert_eq!(up.code, Some(1), "{up:?}");
    let home = world.home.display().to_string();
    assert!(up.stderr.contains(&home), "{up:?}");
    assert!(!world.home.join("rider.sock").exists());
    assert!(!world.home.join("daemon.log").exists());
}

#[test]
fn rider_up_fails_fast_on_a_fifo_at_daemon_log() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let log = world.home.join("daemon.log");
    let made = std::process::Command::new("mkfifo")
        .arg(&log)
        .stdin(std::process::Stdio::null())
        .status()
        .expect("run mkfifo");
    assert!(made.success());

    let started = Instant::now();
    let up = when_rider_runs(&world, &["up"]);
    let took = started.elapsed();

    assert_eq!(up.code, Some(1), "{up:?}");
    assert!(up.stderr.contains(&log.display().to_string()), "{up:?}");
    assert!(
        took < Duration::from_secs(UP_REFUSAL_SECONDS_MAX),
        "took {took:?}"
    );
    assert!(!world.home.join("rider.sock").exists());
}

#[test]
fn the_home_and_every_log_are_private_to_the_owner_after_a_run() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let legacy = world.home.join("daemon.log");
    fs::write(&legacy, "old\n").expect("write a legacy log");
    fs::set_permissions(&legacy, fs::Permissions::from_mode(0o644)).expect("make it public");
    let mut reader = fs::File::open(&legacy).expect("open the legacy log");
    given_a_running_daemon(&mut world);
    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["completed"]);
    let mut seen = String::new();
    reader
        .read_to_string(&mut seen)
        .expect("read the legacy log");
    assert_eq!(seen, "old\n");
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));

    let mode = |path: &Path| fs::metadata(path).expect("stat").permissions().mode() & 0o777;
    let attempt = world.home.join("logs").join(&task).join("1.jsonl");
    let modes: Vec<u32> = [world.home.clone(), world.home.join("daemon.log"), attempt]
        .iter()
        .map(|path| mode(path))
        .collect();
    assert_eq!(modes, vec![0o700, 0o600, 0o600]);
}

#[test]
fn an_agent_running_rider_kill_or_down_is_refused_and_keeps_running() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::TryToStop]);
    given_a_running_daemon_with(&mut world, &[("RIDER_BIN", env!("CARGO_BIN_EXE_rider"))]);
    let run = when_the_agent_has_started(&world, &task);

    let stops = when_the_stop_attempts_end(&world, &task);

    let refused = "forbidden: down and kill are refused from the daemon's own agents";
    assert_eq!(
        stops.lines().filter(|line| line.contains(refused)).count(),
        2,
        "{stops}"
    );
    assert!(
        stops.contains("kill exit 1") && stops.contains("down exit 1"),
        "{stops}"
    );
    assert!(is_alive(run.pid), "the agent {} was stopped", run.pid);
    let kill = when_rider_runs(&world, &["kill", &task]);
    assert_eq!(kill.code, Some(0), "{kill:?}");
    assert!(
        when_the_process_ends_within(run.pid, 10),
        "the agent still runs"
    );
    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["failed"]);
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

fn when_the_stop_attempts_end(world: &World, task: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(READ_SECONDS_MAX);
    loop {
        if let Some(stops) = stop_attempts(&world.root, task) {
            return stops;
        }
        assert!(
            Instant::now() < deadline,
            "the agent never finished trying to stop"
        );
        thread::sleep(Duration::from_millis(SEAT_PROBE_MILLIS));
    }
}

#[test]
fn rider_up_refuses_a_home_under_a_directory_other_users_can_write_to() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    fs::set_permissions(&world.root, fs::Permissions::from_mode(0o777)).expect("open the parent");

    let up = when_rider_runs(&world, &["up"]);
    fs::set_permissions(&world.root, fs::Permissions::from_mode(0o700)).expect("close it");

    assert_eq!(up.code, Some(1), "{up:?}");
    let parent = format!("another user can change {}", world.root.display());
    assert!(up.stderr.contains(&parent), "{up:?}");
    assert!(!world.home.join("rider.sock").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn rider_up_refuses_a_home_with_an_acl() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let added = std::process::Command::new("/bin/chmod")
        .args(["+a", "everyone allow list,search,add_file"])
        .arg(&world.home)
        .stdin(std::process::Stdio::null())
        .status()
        .expect("run chmod");
    assert!(added.success());

    let up = when_rider_runs(&world, &["up"]);

    assert_eq!(up.code, Some(1), "{up:?}");
    assert!(up.stderr.contains("it carries an ACL"), "{up:?}");
    assert!(!world.home.join("rider.sock").exists());
}

#[test]
fn rider_daemon_under_umask_000_creates_its_home_private_before_writing_to_it() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let home = world.root.join("fresh-home");
    let mut command = world.command(Path::new("/bin/sh"));
    command.env("RIDER_HOME", &home).args([
        "-c",
        r#"umask 000; exec "$0" daemon"#,
        env!("CARGO_BIN_EXE_rider"),
    ]);

    let _ = crate::world::run_bounded(command, None);

    let metadata = fs::metadata(&home).expect("the home exists");
    assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
    assert!(home.join("prompts").is_dir());
}

#[test]
fn rider_up_refuses_a_symlink_at_daemon_log_and_leaves_its_target_alone() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let target = world.root.join("elsewhere.txt");
    fs::write(&target, "kept\n").expect("write the target");
    std::os::unix::fs::symlink(&target, world.home.join("daemon.log")).expect("link the log");

    let up = when_rider_runs(&world, &["up"]);

    assert_eq!(up.code, Some(1), "{up:?}");
    assert!(up.stderr.contains("daemon.log"), "{up:?}");
    assert_eq!(
        fs::read_to_string(&target).expect("read the target"),
        "kept\n"
    );
    assert!(!world.home.join("rider.sock").exists());
}

#[test]
fn a_known_gap_an_orphaned_agent_child_in_its_own_process_group_can_still_stop_the_daemon() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::OrphanStops]);
    given_a_running_daemon_with(&mut world, &[("RIDER_BIN", env!("CARGO_BIN_EXE_rider"))]);
    when_the_agent_has_started(&world, &task);

    let stops = when_the_stop_attempts_end(&world, &task);

    assert!(stops.ends_with("down exit 0\n"), "{stops}");
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

fn when_rider_runs_on_home_under_umask_000(world: &World, home: &Path, args: &str) {
    let mut command = world.command(Path::new("/bin/sh"));
    let script = format!(r#"umask 000; exec "$0" {args}"#);
    command
        .env("RIDER_HOME", home)
        .args(["-c", &script, env!("CARGO_BIN_EXE_rider")]);
    let _ = crate::world::run_bounded(command, None);
}

#[test]
fn rider_serve_and_rider_wait_under_umask_000_create_their_home_private() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let served = world.root.join("served-home");
    let waited = world.root.join("waited-home");

    when_rider_runs_on_home_under_umask_000(&world, &served, "serve");
    when_rider_runs_on_home_under_umask_000(&world, &waited, "wait rider --timeout 0");

    let mode =
        |path: &Path| fs::metadata(path).map(|metadata| metadata.permissions().mode() & 0o777);
    assert_eq!(
        (mode(&served).ok(), mode(&waited).ok()),
        (Some(0o700), Some(0o700))
    );
}

#[cfg(target_os = "macos")]
fn given_an_acl(path: &Path, entry: &str) {
    let added = std::process::Command::new("/bin/chmod")
        .args(["+a", entry])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .status()
        .expect("run chmod");
    assert!(added.success());
}

#[cfg(target_os = "macos")]
#[test]
fn rider_up_refuses_a_home_with_an_acl_whose_path_holds_a_newline() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let home = world.root.join("odd\nhome");
    fs::create_dir(&home).expect("create the home");
    given_an_acl(&home, "everyone allow list,search,add_file");
    let mut command = world.command(Path::new(env!("CARGO_BIN_EXE_rider")));
    command.env("RIDER_HOME", &home).arg("up");

    let up = crate::world::run_bounded(command, None);

    assert_eq!(up.code, Some(1), "{up:?}");
    assert!(up.stderr.contains("it carries an ACL"), "{up:?}");
    assert!(!home.join("rider.sock").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn rider_up_refuses_a_home_under_a_directory_whose_acl_lets_others_change_it() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    given_an_acl(&world.root, "everyone allow add_subdirectory,delete_child");

    let up = when_rider_runs(&world, &["up"]);
    let removed = std::process::Command::new("/bin/chmod")
        .arg("-N")
        .arg(&world.root)
        .status()
        .expect("run chmod");

    assert!(removed.success());
    assert_eq!(up.code, Some(1), "{up:?}");
    let parent = format!("another user can change {}", world.root.display());
    assert!(up.stderr.contains(&parent), "{up:?}");
    assert!(!world.home.join("rider.sock").exists());
}
