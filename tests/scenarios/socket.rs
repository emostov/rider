use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::fake_agent::{self, Behaviour, stop_attempts};
use crate::steps::*;
use crate::world::World;

const CLIENTS_MAX: usize = 8;
const DAEMON_WRITE_SECONDS: u64 = 10;
const FLOOD_LINES: usize = 10_000;
const FLOOD_WRITE_MILLIS: u64 = 500;
const LAG_REQUESTS_MAX: u64 = 1 << 16;
const LAG_STALL_SECONDS: u64 = 3;
const LAG_ATTEMPTS_MAX: usize = 8;
const READ_SECONDS_MAX: u64 = 30;
const READS_MAX: usize = 1 << 20;
const LOG_LINES: usize = 8 * 1024;
const LOG_LINE_BYTES: usize = 1024;
const DAEMON_STARTS: usize = 20;
const WATCH_POLLS_MAX: u64 = u64::MAX;
const SEAT_PROBES_MAX: usize = 1000;
const SEAT_PROBE_MILLIS: u64 = 50;
const DAEMON_IDLE_SECONDS: u64 = 5;
const REQUEST_BYTES_MAX: usize = 64 * 1024;
#[cfg(target_os = "linux")]
const SOCKET_PATH_BYTES_MAX: usize = 107;
#[cfg(not(target_os = "linux"))]
const SOCKET_PATH_BYTES_MAX: usize = 103;
const SOCKET_SUFFIX_BYTES: usize = "/rider.sock".len();
const FOLLOWER_LINES_MAX: usize = 1 << 16;
const SEAT_FREE_SECONDS: u64 = 5;

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

    let (slow, sent) = given_a_client_lagging_with_lines_unsent(&world);
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
                ["rider.sock", ".b"]
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

#[test]
fn an_oversized_request_gets_its_bad_request_reply_before_the_connection_ends() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_running_daemon(&mut world);
    let mut client = given_a_client_on_the_socket(&world);

    client
        .write_all(&vec![b'x'; REQUEST_BYTES_MAX + 1])
        .expect("send the oversized request");
    let data = then_the_client_reads_to_its_end(&client);

    let text = String::from_utf8(data).expect("the daemon sends UTF-8");
    let first: Value = serde_json::from_str(text.lines().next().unwrap_or_default())
        .unwrap_or_else(|_| panic!("no reply line before the end: {text:?}"));
    assert_eq!(first["error"]["code"], json!("bad_request"), "{first}");
}

#[test]
fn idle_connections_in_every_seat_are_closed_so_rider_status_answers() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::HoldTalking]);
    given_a_running_daemon(&mut world);
    when_the_agent_has_started(&world, &task);
    let mut follow = world.command(Path::new(env!("CARGO_BIN_EXE_rider")));
    follow.args(["logs", "-f", &task]).stdout(Stdio::piped());
    let mut follower = follow.spawn().expect("start rider logs -f");
    let lines = crate::lines_of(follower.stdout.take().expect("rider logs -f output"));
    let first = lines.recv_timeout(Duration::from_secs(READ_SECONDS_MAX));
    let opened = Instant::now();
    let idle: Vec<UnixStream> = (1..CLIENTS_MAX)
        .map(|_| given_a_client_on_the_socket(&world))
        .collect();

    let closed: Vec<Vec<u8>> = idle.iter().map(then_the_client_reads_to_its_end).collect();
    let closed_after = opened.elapsed();
    drop(idle);
    let answered = when_rider_status_shows_within(&world, "board up", SEAT_FREE_SECONDS);
    let following = follower.try_wait().expect("poll rider logs -f");
    world.release_agent(&task);
    let deadline = Instant::now() + Duration::from_secs(READ_SECONDS_MAX);
    let rest: Vec<String> = (0..FOLLOWER_LINES_MAX)
        .map_while(|_| {
            lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .ok()
        })
        .collect();
    let followed = then_the_child_exits_within(&mut follower, READ_SECONDS_MAX);

    assert!(
        closed_after >= Duration::from_secs(DAEMON_IDLE_SECONDS),
        "the idle connections were closed after {closed_after:?}"
    );
    assert!(
        answered,
        "rider status stayed busy after the idle clients went"
    );
    assert_eq!(first, Ok(fake_agent::hold_line(&task)));
    assert_eq!(following, None, "rider logs -f ended while it followed");
    assert_eq!(closed, vec![Vec::<u8>::new(); CLIENTS_MAX - 1]);
    assert_eq!(followed, Some(0));
    let summary = fake_agent::summary(&task, 0);
    assert!(rest.iter().any(|line| line.contains(&summary)), "{rest:?}");
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[test]
fn a_log_read_slowly_past_the_idle_cutoff_reaches_rider_logs_whole() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = 7;
    given_a_log_larger_than_every_buffer(&world, task);
    given_a_running_daemon(&mut world);

    let mut logs = world.command(Path::new(env!("CARGO_BIN_EXE_rider")));
    logs.args(["logs", "-f", &task.to_string()])
        .stdout(Stdio::piped());
    let mut reader = logs.spawn().expect("start rider logs -f");
    let mut output = reader.stdout.take().expect("rider logs -f output");
    thread::sleep(Duration::from_secs(DAEMON_IDLE_SECONDS + 1));
    let read = thread::spawn(move || {
        let mut data = Vec::new();
        output.read_to_end(&mut data).map(|_| data.len())
    });
    let exit = then_the_child_exits_within(&mut reader, READ_SECONDS_MAX);

    assert_eq!(exit, Some(0));
    let read = read
        .join()
        .expect("the reader ends")
        .expect("read the output");
    assert_eq!(read, LOG_LINES * LOG_LINE_BYTES);
}

#[test]
fn a_home_too_long_for_a_socket_address_fails_naming_the_limit() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let longest = given_a_home_whose_socket_path_is(&mut world, SOCKET_PATH_BYTES_MAX);
    let fits = when_rider_runs(&world, &["status"]);
    let too_long = given_a_home_whose_socket_path_is(&mut world, SOCKET_PATH_BYTES_MAX + 1);

    let up = when_rider_runs(&world, &["up"]);
    let status = when_rider_runs(&world, &["status"]);

    assert_eq!(fits.code, Some(1), "{fits:?}");
    assert!(
        fits.stderr.contains("the daemon is not running"),
        "{fits:?}"
    );
    assert_eq!(up.code, Some(1), "{up:?}");
    assert!(up.stderr.contains("byte limit for a socket path"), "{up:?}");
    assert!(
        status.stderr.contains("byte limit for a socket path"),
        "{status:?}"
    );
    assert_eq!(entries(&too_long), Vec::<String>::new());
    assert_ne!(longest, too_long);
}

#[test]
fn a_direct_daemon_with_a_home_too_long_for_a_socket_address_creates_nothing() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let home = given_a_home_whose_socket_path_is(&mut world, SOCKET_PATH_BYTES_MAX + 1);

    let daemon = when_rider_runs(&world, &["daemon"]);

    assert_eq!(daemon.code, Some(1), "{daemon:?}");
    assert!(
        daemon.stderr.contains("byte limit for a socket path"),
        "{daemon:?}"
    );
    assert_eq!(entries(&home), Vec::<String>::new());
}

fn given_a_home_whose_socket_path_is(world: &mut World, bytes: usize) -> PathBuf {
    let parent = world.root.clone();
    let home = bytes - SOCKET_SUFFIX_BYTES;
    let used = parent.as_os_str().len() + 1;
    assert!(used < home, "the temporary directory is too long");
    world.home = parent.join("h".repeat(home - used));
    assert_eq!(world.home.join("rider.sock").as_os_str().len(), bytes);
    world.home.clone()
}

fn entries(dir: &Path) -> Vec<String> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    read.map(|entry| {
        let entry = entry.expect("read a directory entry");
        entry.file_name().to_string_lossy().into_owned()
    })
    .collect()
}

fn then_the_child_exits_within(child: &mut Child, seconds: u64) -> Option<i32> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll the child") {
            return status.code();
        }
        thread::sleep(Duration::from_millis(SEAT_PROBE_MILLIS));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("the child did not exit within {seconds} seconds");
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

enum Flood {
    Stalled(u64),
    HungUp,
}

// A client whose lines all fit in the socket buffers is hung up on once they are queued, so
// its seat frees before it reads; only a flood that stalls leaves the daemon a line unsent.
fn given_a_client_lagging_with_lines_unsent(world: &World) -> (UnixStream, u64) {
    for _ in 0..LAG_ATTEMPTS_MAX {
        let mut client = given_a_client_on_the_socket(world);
        match when_the_client_asks_for_status_without_reading(&mut client) {
            Flood::Stalled(sent) => return (client, sent),
            Flood::HungUp => {}
        }
    }
    panic!("the daemon hung up on all {LAG_ATTEMPTS_MAX} lagging clients before they read");
}

fn when_the_client_asks_for_status_without_reading(client: &mut UnixStream) -> Flood {
    client
        .set_write_timeout(Some(Duration::from_secs(LAG_STALL_SECONDS)))
        .expect("set a write timeout");
    for id in 1..=LAG_REQUESTS_MAX {
        let request = format!("{}\n", json!({"v": 1, "id": id, "cmd": "status"}));
        match client.write_all(request.as_bytes()) {
            Ok(()) => {}
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Flood::Stalled(id - 1);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::ConnectionReset | ErrorKind::BrokenPipe
                ) =>
            {
                return Flood::HungUp;
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

    let refused = if cfg!(target_os = "macos") {
        "talking to the daemon: Operation not permitted"
    } else {
        "forbidden: down and kill are refused from the daemon's own agents"
    };
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

#[cfg(not(target_os = "macos"))]
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

const STREAM_LINE_BYTES_MAX: usize = 512 * 1024;

#[test]
fn rider_logs_shows_control_characters_in_an_agents_output_escaped() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = "7";
    given_a_log(
        &world,
        task,
        "plain\tline\n\u{1b}[2Jcleared\u{9b}31m\rback\nbidi \u{202e}gnp.exe\u{200b}\n",
    );
    given_a_running_daemon(&mut world);

    let printed = when_rider_runs(&world, &["logs", task]);
    let followed = when_rider_runs(&world, &["logs", "-f", task]);

    let shown = "plain\\tline\n\\u{1b}[2Jcleared\\u{9b}31m\\rback\n\
                 bidi \\u{202e}gnp.exe\\u{200b}\n";
    assert_eq!(
        (printed.code, printed.stdout.as_str()),
        (Some(0), shown),
        "{printed:?}"
    );
    assert_eq!(
        (followed.code, followed.stdout.as_str()),
        (Some(0), shown),
        "{followed:?}"
    );
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[test]
fn rider_logs_follow_splits_a_line_too_long_for_one_stream_line_between_characters() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = "7";
    let line = format!("a{}", "\u{e9}".repeat(STREAM_LINE_BYTES_MAX));
    given_a_log(&world, task, &format!("{line}\n"));
    given_a_running_daemon(&mut world);

    let followed = when_rider_runs(&world, &["logs", "-f", task]);

    assert_eq!(followed.code, Some(0), "{}", followed.stderr);
    assert!(
        followed.stdout.lines().count() > 1,
        "the line was not split"
    );
    assert!(
        followed.stdout.replace('\n', "") == line,
        "the split line changed"
    );
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[cfg(target_os = "macos")]
#[test]
fn a_detached_child_of_an_agent_is_refused_the_socket_and_cannot_stop_rider() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::StopRiderDetached]);
    given_a_running_daemon(&mut world);

    let states = when_the_tasks_settle(&world, &[&task]);

    let (code, output) = fake_agent::detached_stop(&world.root, &task, 0);
    assert_eq!(states, ["completed"], "{}", world.daemon_log());
    assert_ne!(code, "0", "{output}");
    assert!(output.contains("Operation not permitted"), "{output}");
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[cfg(target_os = "macos")]
#[test]
fn an_agent_cannot_write_the_daemons_staging_files_or_its_board_cli_install() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::TryWrites]);
    given_pinned_pr_files_and_a_reviewed_head(&world);
    given_a_running_daemon(&mut world);

    let states = when_the_tasks_settle(&world, &[&task]);

    assert_eq!(states, ["completed"], "{}", world.daemon_log());
    let expected: String = (fake_agent::TRIED_WRITES.split(' '))
        .map(|name| format!("{name} refused\n"))
        .collect();
    assert_eq!(fake_agent::tried_writes(&world.root, &task, 0), expected);
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[cfg(target_os = "macos")]
#[test]
fn a_daemon_started_inside_a_sandbox_runs_its_agents_unconfined_and_says_so() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_a_running_daemon_in_a_sandbox(&mut world);

    let states = when_the_tasks_settle(&world, &[&task]);

    assert_eq!(states, ["completed"], "{}", world.daemon_log());
    let note = "agents run unconfined: the daemon runs in a sandbox";
    let log = fs::read_to_string(world.home.join("daemon.log")).expect("read daemon.log");
    assert!(log.contains(note), "{log}");
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[cfg(target_os = "macos")]
#[test]
fn a_detached_child_of_an_agent_cannot_signal_the_daemon() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::SignalDaemonDetached]);
    given_a_running_daemon(&mut world);

    let states = when_the_tasks_settle(&world, &[&task]);

    let (code, output) = fake_agent::detached_signal(&world.root, &task, 0);
    assert_eq!(states, ["completed"], "{}", world.daemon_log());
    assert_ne!(code, "0", "{output}");
    assert!(output.contains("Operation not permitted"), "{output}");
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[test]
fn a_file_left_in_the_sockets_staging_directory_does_not_stop_the_daemon_starting() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let staging = world.home.join(".b");
    fs::create_dir_all(&staging).expect("create the staging directory");
    fs::write(staging.join("left"), "").expect("leave a file there");

    given_a_running_daemon(&mut world);

    assert_eq!(when_rider_runs(&world, &["status"]).code, Some(0));
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

fn given_a_log(world: &World, task: &str, text: &str) {
    let dir = world.home.join("logs").join(task);
    fs::create_dir_all(&dir).expect("create the task's log directory");
    fs::write(dir.join("1.jsonl"), text).expect("write the log");
}

#[cfg(target_os = "macos")]
#[test]
fn filters_an_agent_plants_in_its_repository_and_a_nested_one_never_run_outside_its_sandbox() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::PlantFilters]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!(exit, Some(0), "{}", world.daemon_log());
    assert_eq!(states.len(), 1);
    let escaped = [fake_agent::ESCAPE_EQUALS, fake_agent::ESCAPE_NESTED]
        .map(|marker| fake_agent::escaped(&world.root, marker));
    assert_eq!(escaped, [false, false]);
}

#[cfg(target_os = "macos")]
#[test]
fn a_promisor_remote_an_agent_plants_never_runs_its_uploadpack_outside_its_sandbox() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_rider_toml(&world, "[limits]\nattempts = 1\n");
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::PlantPromisor]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!(exit, Some(0), "{}", world.daemon_log());
    assert_eq!(states.len(), 1);
    assert!(!fake_agent::escaped(
        &world.root,
        fake_agent::ESCAPE_PROMISOR
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn an_agent_cannot_open_an_app_it_wrote_to_leave_its_sandbox() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::OpenApp]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["completed".to_string()], Some(0)));
    let (code, output) = fake_agent::opened(&world.root, &task, 0);
    assert_ne!(code, "0", "{output}");
    assert!(!fake_agent::escaped(&world.root, fake_agent::ESCAPE_OPEN));
}

#[cfg(target_os = "macos")]
#[test]
fn an_agent_cannot_write_its_repositorys_git_config_hooks_or_remotes_but_still_commits() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::PlantGitConfig]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["completed".to_string()], Some(0)));
    let worktree = format!("worktrees/{task}");
    let refused = format!(
        "config refused\nconfig.worktree refused\nhooks/post-checkout refused\n\
         info/attributes refused\ncommondir refused\n{worktree}/config.worktree refused\n\
         {worktree}/commondir refused\n{worktree}/gitdir refused\n.git refused\n"
    );
    assert_eq!(
        fake_agent::git_config_writes(&world.root, &task, 0),
        refused
    );
}
