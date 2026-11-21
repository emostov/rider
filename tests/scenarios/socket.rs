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

use crate::fake_agent::{self, Behaviour};
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
const STREAM_LINE_BYTES_MAX: usize = 512 * 1024;
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

#[test]
fn rider_logs_shows_control_characters_in_an_agents_output_escaped() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = "7";
    given_a_log(
        &world,
        task,
        "plain\tline\n\u{1b}[2Jcleared\u{9b}31m\rback\n",
    );
    given_a_running_daemon(&mut world);

    let printed = when_rider_runs(&world, &["logs", task]);
    let followed = when_rider_runs(&world, &["logs", "-f", task]);

    let shown = "plain\tline\n\\u{1b}[2Jcleared\\u{9b}31m\\rback\n";
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

fn given_a_log(world: &World, task: &str, text: &str) {
    let dir = world.home.join("logs").join(task);
    fs::create_dir_all(&dir).expect("create the task's log directory");
    fs::write(dir.join("1.jsonl"), text).expect("write the log");
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
