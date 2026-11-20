//! Finds Board CLI processes left running after their board stopped, and stops them.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use rustix::io::Errno;
use rustix::process::{Pid as RawPid, Signal, getuid, kill_process};
use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind};

/// Upper bound on the processes examined in one look at the process table.
pub const PROCESSES_MAX: usize = 65_536;
/// Upper bound on the orphaned Board CLI processes judged in one look.
pub const CANDIDATES_MAX: usize = 4096;
/// How long a board has to accept a connection before it counts as not answering.
pub const PROBE_MILLIS: u64 = 500;
/// Upper bound on the boards probed at once.
pub const PROBES_AT_ONCE: usize = 32;
/// How long a leaked process has to end after SIGTERM before it is sent SIGKILL.
pub const TERM_GRACE_MILLIS: u64 = 3000;
/// How long a process has to disappear after SIGKILL.
pub const KILL_WAIT_MILLIS: u64 = 2000;
/// Upper bound on the characters of a command line shown.
pub const SHOWN_CHARS_MAX: usize = 120;
const POLL_MILLIS: u64 = 50;
const INIT_PID: usize = 1;
const VB_SUFFIX: &str = "/node_modules/.bin/vb";
const URL_FLAG: &str = "--url";
const SERVE: &str = "serve";

/// A Board CLI process, orphaned, whose board does not answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leak {
    pub pid: u32,
    /// Seconds since the epoch; tells this process from a later one given the same pid.
    pub start_time: u64,
    pub age_seconds: u64,
    pub url: String,
    /// `vb` and its arguments, trimmed to `SHOWN_CHARS_MAX`.
    pub command: String,
}

/// How a leaked process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stopped {
    /// It was gone, or had become another process, before it was signalled.
    Gone,
    Term,
    Kill,
}

/// Why the process table could not be read or a process could not be stopped.
#[derive(Debug)]
pub enum ReapError {
    Table(String),
    TooManyProcesses(usize),
    TooManyCandidates,
    Thread,
    Signal {
        pid: u32,
        message: String,
    },
    /// The process was still running `KILL_WAIT_MILLIS` after SIGKILL.
    Survived(u32),
}

impl fmt::Display for ReapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReapError::Table(message) => write!(f, "could not read the process table: {message}"),
            ReapError::TooManyProcesses(count) => {
                write!(f, "{count} processes are running, over {PROCESSES_MAX}")
            }
            ReapError::TooManyCandidates => {
                write!(
                    f,
                    "over {CANDIDATES_MAX} orphaned Board CLI processes are running"
                )
            }
            ReapError::Thread => write!(f, "a board probe's thread panicked"),
            ReapError::Signal { pid, message } => write!(f, "could not signal {pid}: {message}"),
            ReapError::Survived(pid) => {
                write!(
                    f,
                    "{pid} was still running {KILL_WAIT_MILLIS} ms after SIGKILL"
                )
            }
        }
    }
}

/// The leaked Board CLI processes of the current user, by pid; never one on `configured_url`.
pub fn find_leaks(configured_url: &str) -> Result<Vec<Leak>, ReapError> {
    find_leaks_on(configured_url, &[])
}

/// As `find_leaks`, but only those on one of `only` unless it is empty.
pub fn find_leaks_on(configured_url: &str, only: &[String]) -> Result<Vec<Leak>, ReapError> {
    assert!(!configured_url.is_empty());
    assert!(only.iter().all(|url| !url.is_empty()));
    let mut system = System::new().map_err(|error| ReapError::Table(error.to_string()))?;
    let kind = ProcessRefreshKind::nothing()
        .with_cmd(UpdateKind::Always)
        .with_user(UpdateKind::Always);
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
    let processes = system.processes();
    if processes.len() > PROCESSES_MAX {
        return Err(ReapError::TooManyProcesses(processes.len()));
    }
    let uid = getuid().as_raw();
    let mut candidates: Vec<Leak> = Vec::new();
    for process in processes.values() {
        let ours = process.user_id().is_some_and(|user| **user == uid);
        let orphaned = process.parent() == Some(Pid::from(INIT_PID));
        if !ours || !orphaned || is_gone(process.status()) {
            continue;
        }
        let Some((url, command)) = board_client(process.cmd()) else {
            continue;
        };
        let named = only.is_empty() || only.iter().any(|wanted| same_board(&url, wanted));
        if !named || same_board(&url, configured_url) {
            continue;
        }
        if candidates.len() == CANDIDATES_MAX {
            return Err(ReapError::TooManyCandidates);
        }
        candidates.push(Leak {
            pid: process.pid().as_u32(),
            start_time: process.start_time(),
            age_seconds: process.run_time(),
            url,
            command,
        });
    }
    let urls: BTreeSet<&str> = candidates.iter().map(|leak| leak.url.as_str()).collect();
    let urls: Vec<&str> = urls.into_iter().collect();
    let silent: BTreeSet<String> = silent_boards(&urls)?
        .into_iter()
        .map(str::to_string)
        .collect();
    let mut leaks: Vec<Leak> = candidates
        .into_iter()
        .filter(|leak| silent.contains(leak.url.as_str()))
        .collect();
    leaks.sort_by_key(|leak| leak.pid);
    assert!(leaks.len() <= CANDIDATES_MAX);
    Ok(leaks)
}

// A process whose arguments name `serve` is spared: it may be a Board server, which listens on
// its own port whatever `--url` says.
fn board_client(cmd: &[OsString]) -> Option<(String, String)> {
    let at = cmd
        .iter()
        .take(2)
        .position(|arg| arg.to_str().is_some_and(|arg| arg.ends_with(VB_SUFFIX)))?;
    let args: Vec<&str> = cmd[at + 1..]
        .iter()
        .filter_map(|arg| arg.to_str())
        .collect();
    let options = args
        .iter()
        .position(|arg| *arg == "--")
        .unwrap_or(args.len());
    let mut url = None;
    let mut words = args[..options].iter();
    while let Some(word) = words.next() {
        if *word == SERVE {
            return None;
        }
        if *word == URL_FLAG {
            url = Some(*words.next()?);
        } else if let Some(value) = word.strip_prefix("--url=") {
            url = Some(value);
        }
    }
    let url = url.filter(|url| !url.is_empty())?;
    let mut command = String::from("vb");
    for arg in &args {
        command.push(' ');
        command.push_str(arg);
    }
    let shown = command.chars().take(SHOWN_CHARS_MAX);
    let command: String = shown
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    assert!(command.starts_with("vb") && command.chars().count() <= SHOWN_CHARS_MAX);
    Some((url.to_string(), command))
}

fn same_board(url: &str, configured: &str) -> bool {
    url.trim_end_matches('/') == configured.trim_end_matches('/')
}

fn silent_boards<'a>(urls: &[&'a str]) -> Result<BTreeSet<&'a str>, ReapError> {
    assert!(urls.len() <= CANDIDATES_MAX);
    let mut silent = BTreeSet::new();
    for chunk in urls.chunks(PROBES_AT_ONCE) {
        let answers: Vec<thread::Result<bool>> = thread::scope(|scope| {
            let probes: Vec<_> = chunk
                .iter()
                .map(|url| scope.spawn(move || answers(url)))
                .collect();
            probes.into_iter().map(|probe| probe.join()).collect()
        });
        assert_eq!(answers.len(), chunk.len());
        for (url, answer) in chunk.iter().zip(answers) {
            if !answer.map_err(|_| ReapError::Thread)? {
                silent.insert(*url);
            }
        }
    }
    assert!(silent.len() <= urls.len());
    Ok(silent)
}

// A URL that cannot be read, or names a host rather than an IP address, counts as answering, so
// its process is left alone; resolving a name has no deadline.
fn answers(url: &str) -> bool {
    let (rest, default_port) = if let Some(rest) = url.strip_prefix("http://") {
        (rest, 80)
    } else if let Some(rest) = url.strip_prefix("https://") {
        (rest, 443)
    } else {
        return true;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let socket = match authority.parse::<SocketAddr>() {
        Ok(socket) => socket,
        Err(_) => {
            let host = authority.trim_start_matches('[').trim_end_matches(']');
            let Ok(ip) = host.parse::<IpAddr>() else {
                return true;
            };
            SocketAddr::new(ip, default_port)
        }
    };
    TcpStream::connect_timeout(&socket, Duration::from_millis(PROBE_MILLIS)).is_ok()
}

fn is_gone(status: ProcessStatus) -> bool {
    matches!(status, ProcessStatus::Zombie | ProcessStatus::Dead)
}

/// Sends each leak SIGTERM, then SIGKILL to those still running after `TERM_GRACE_MILLIS`.
pub fn stop(leaks: &[Leak]) -> Vec<Result<Stopped, ReapError>> {
    assert!(leaks.len() <= CANDIDATES_MAX);
    let mut system = match System::new() {
        Ok(system) => system,
        Err(error) => {
            let message = error.to_string();
            return leaks
                .iter()
                .map(|_| Err(ReapError::Table(message.clone())))
                .collect();
        }
    };
    let mut ends: Vec<Option<Result<Stopped, ReapError>>> = leaks
        .iter()
        .map(|leak| match signal(&mut system, leak, Signal::TERM) {
            Ok(true) => None,
            Ok(false) => Some(Ok(Stopped::Gone)),
            Err(error) => Some(Err(error)),
        })
        .collect();
    wait_for(
        &mut system,
        leaks,
        &mut ends,
        TERM_GRACE_MILLIS,
        Stopped::Term,
    );
    for (leak, end) in leaks.iter().zip(ends.iter_mut()) {
        if end.is_none() {
            *end = match signal(&mut system, leak, Signal::KILL) {
                Ok(true) => None,
                Ok(false) => Some(Ok(Stopped::Term)),
                Err(error) => Some(Err(error)),
            };
        }
    }
    wait_for(
        &mut system,
        leaks,
        &mut ends,
        KILL_WAIT_MILLIS,
        Stopped::Kill,
    );
    let ends: Vec<Result<Stopped, ReapError>> = leaks
        .iter()
        .zip(ends)
        .map(|(leak, end)| end.unwrap_or(Err(ReapError::Survived(leak.pid))))
        .collect();
    assert_eq!(ends.len(), leaks.len());
    ends
}

fn signal(system: &mut System, leak: &Leak, signal: Signal) -> Result<bool, ReapError> {
    if !running(system, leak) {
        return Ok(false);
    }
    let raw = i32::try_from(leak.pid).ok().and_then(RawPid::from_raw);
    let Some(pid) = raw else {
        return Ok(false);
    };
    assert!(leak.pid > 1, "init is never a leak");
    match kill_process(pid, signal) {
        Ok(()) => Ok(true),
        Err(Errno::SRCH) => Ok(false),
        Err(error) => Err(ReapError::Signal {
            pid: leak.pid,
            message: error.to_string(),
        }),
    }
}

fn running(system: &mut System, leak: &Leak) -> bool {
    let pid = Pid::from_u32(leak.pid);
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    let process = system.process(pid);
    process.is_some_and(|p| p.start_time() == leak.start_time && !is_gone(p.status()))
}

fn wait_for(
    system: &mut System,
    leaks: &[Leak],
    ends: &mut [Option<Result<Stopped, ReapError>>],
    millis: u64,
    how: Stopped,
) {
    assert_eq!(leaks.len(), ends.len());
    let deadline = Instant::now() + Duration::from_millis(millis);
    let polls = millis / POLL_MILLIS + 1;
    for _ in 0..polls {
        for (leak, end) in leaks.iter().zip(ends.iter_mut()) {
            if end.is_none() && !running(system, leak) {
                *end = Some(Ok(how.clone()));
            }
        }
        if ends.iter().all(Option::is_some) || Instant::now() >= deadline {
            return;
        }
        thread::sleep(Duration::from_millis(POLL_MILLIS));
    }
}

/// One line describing `leak`: its pid, age, and command.
pub fn leak_line(leak: &Leak) -> String {
    let age = leak.age_seconds;
    let age = if age >= 86_400 {
        format!("{}d", age / 86_400)
    } else if age >= 3600 {
        format!("{}h", age / 3600)
    } else if age >= 60 {
        format!("{}m", age / 60)
    } else {
        format!("{age}s")
    };
    let line = format!("pid {} age {age} {}", leak.pid, leak.command);
    assert!(line.starts_with("pid ") && !line.contains('\n'));
    line
}
