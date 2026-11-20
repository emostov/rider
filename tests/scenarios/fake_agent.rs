//! Fake `claude` and `codex` scripts that play a scripted worker, one behaviour per attempt.

use std::fs;
use std::path::{Path, PathBuf};

use crate::world::write_executable;

/// The variable that tells a fake agent where its world's files are.
pub const WORLD_VAR: &str = "RIDER_SCENARIO_WORLD";
/// How long a holding agent waits for its release before it gives up and exits 1.
pub const HOLD_SECONDS_MAX: u64 = 60;
const HOLD_STEPS_PER_SECOND: u64 = 10;
const RUNS_FILE: &str = "agent-runs.log";
const BEHAVIOURS_DIR: &str = "agent-behaviours";
const RELEASES_DIR: &str = "agent-releases";
const ARGS_DIR: &str = "agent-args";
const RUNS_MAX: usize = 4096;
/// The exit status of an agent told to fail.
pub const FAIL_EXIT_CODE: i32 = 3;

/// What a fake agent does in one attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behaviour {
    /// Commits `work_file(task, attempt)` and reports success; the default.
    Commit,
    /// Reports success having committed nothing.
    CommitNothing,
    /// Writes `uncommitted-<task>.txt` without committing it and reports success.
    LeaveUncommitted,
    /// Blocks its own task as `needs_input` with `vb`, then reports success.
    Block,
    /// Prints nothing and exits `FAIL_EXIT_CODE`.
    ExitNonZero,
    /// Waits until the test releases its task, then commits like `Commit`.
    Hold,
    /// Like `Hold`, but ignores SIGTERM.
    HoldIgnoringTerm,
    /// As `claude --resume` of a session Claude no longer has: reports the refusal and exits 1
    /// when started with `--resume`; otherwise commits like `Commit`.
    RefuseResume,
}

impl Behaviour {
    fn word(self) -> &'static str {
        match self {
            Behaviour::Commit => "commit",
            Behaviour::CommitNothing => "commit-nothing",
            Behaviour::LeaveUncommitted => "leave-uncommitted",
            Behaviour::Block => "block",
            Behaviour::ExitNonZero => "exit-non-zero",
            Behaviour::Hold => "hold",
            Behaviour::HoldIgnoringTerm => "hold-ignoring-term",
            Behaviour::RefuseResume => "refuse-resume",
        }
    }
}

/// The question a blocking agent asks.
pub const BLOCK_QUESTION: &str = "The fake agent needs an answer.";

/// The file a committing agent writes, relative to its worktree.
pub fn work_file(task: &str, attempt: u32) -> String {
    format!("work-{task}-{attempt}.txt")
}

/// The text of `work_file(task, attempt)`.
pub fn work_text(task: &str, attempt: u32) -> String {
    format!("work for task {task} attempt {attempt}\n")
}

/// The subject of the commit a committing agent makes.
pub fn work_subject(task: &str, attempt: u32) -> String {
    format!("Work for task {task} attempt {attempt}")
}

/// The session id a fake `claude` reports in attempt `attempt` of `task`, unless it continues a
/// session in place: started with `--resume <id>` and no `--fork-session`, it reports `<id>`.
pub fn session(task: &str, attempt: u32) -> String {
    format!("fake-{task}-{attempt}")
}

/// The options attempt `attempt` of `task` was started with: its arguments before `--`.
pub fn options(world: &Path, task: &str, attempt: u32) -> Vec<String> {
    let path = world.join(ARGS_DIR).join(format!("{task}-{attempt}"));
    let text = fs::read_to_string(&path).expect("read an agent's recorded arguments");
    text.lines().map(str::to_string).collect()
}

/// The final message a fake agent reports; it becomes the task's result.
pub fn summary(task: &str, attempt: u32) -> String {
    format!("Fake agent finished task {task} attempt {attempt}.")
}

fn script() -> String {
    let steps = HOLD_SECONDS_MAX * HOLD_STEPS_PER_SECOND;
    let fail = FAIL_EXIT_CODE;
    format!(
        r#"#!/bin/sh
world="${WORLD_VAR}"
task="$BOARD_TASK"
harness=$(basename "$0")
runs="$world/{RUNS_FILE}"
attempt=$( (grep "^start $task " "$runs" 2>/dev/null || true) | wc -l | tr -d ' ')
plan="$world/{BEHAVIOURS_DIR}/$task"
behaviour=$(sed -n "$((attempt + 1))p" "$plan" 2>/dev/null)
[ -n "$behaviour" ] || behaviour=$(tail -n 1 "$plan" 2>/dev/null)
[ -n "$behaviour" ] || behaviour=commit
echo "start $task $attempt $$ $harness $behaviour $(pwd)" >> "$runs"
printf '%s\n' "$@" > "$world/{ARGS_DIR}/$task-$attempt"
trap 'echo "end $task $attempt $$" >> "$runs"' EXIT
session="fake-$task-$attempt"
resumed=""
fork=""
previous=""
options="$world/{ARGS_DIR}/$task-$attempt"
: > "$options"
for arg in "$@"; do
  [ "$arg" = -- ] && break
  [ "$previous" = --resume ] && resumed="$arg"
  [ "$arg" = --fork-session ] && fork=1
  previous="$arg"
  printf '%s\n' "$arg" >> "$options"
done
[ -n "$resumed" ] && [ -z "$fork" ] && session="$resumed"
summary="Fake agent finished task $task attempt $attempt."
report() {{
  if [ "$harness" = codex ]; then
    echo '{{"type":"thread.started","thread_id":"00000000-0000-0000-0000-000000000000"}}'
    echo '{{"type":"turn.started"}}'
    item='"item":{{"id":"item_0","type":"agent_message","text":"'"$summary"'"}}'
    echo '{{"type":"item.completed",'"$item"'}}'
    usage='"usage":{{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1}}'
    echo '{{"type":"turn.completed",'"$usage"'}}'
  else
    init='"subtype":"init","cwd":"'"$(pwd)"'","session_id":"'"$session"'","model":"fake"'
    echo '{{"type":"system",'"$init"'}}'
    echo '{{"type":"result","subtype":"success","is_error":false,"result":"'"$summary"'"}}'
  fi
}}
commit() {{
  echo "work for task $task attempt $attempt" > "work-$task-$attempt.txt"
  git add "work-$task-$attempt.txt" && git commit -q -m "Work for task $task attempt $attempt"
}}
case "$behaviour" in
  commit) commit || exit 1 ;;
  commit-nothing) ;;
  leave-uncommitted) echo "left behind" > "uncommitted-$task.txt" ;;
  block) vb tasks block "$task" --kind needs_input -- "{BLOCK_QUESTION}" || exit 1 ;;
  exit-non-zero) exit {fail} ;;
  refuse-resume)
    if [ -n "$resumed" ]; then
      echo "No conversation found with session ID: $resumed" >&2
      refused='"subtype":"error_during_execution","is_error":true,"num_turns":0'
      echo '{{"type":"result",'"$refused"',"session_id":"'"$session"'"}}'
      exit 1
    fi
    commit || exit 1 ;;
  hold|hold-ignoring-term)
    [ "$behaviour" = hold ] || trap '' TERM
    step=0
    while [ ! -e "$world/{RELEASES_DIR}/$task" ]; do
      step=$((step + 1))
      [ "$step" -le {steps} ] || exit 1
      sleep 0.1
    done
    commit || exit 1 ;;
  *) echo "unknown behaviour $behaviour" >&2; exit 97 ;;
esac
report
"#
    )
}

/// Writes the fake `claude` and `codex` into `agents` and the world's agent directories.
pub fn install(world: &Path, agents: &Path) {
    let text = script();
    for name in ["claude", "codex"] {
        write_executable(&agents.join(name), &text);
    }
    for dir in [BEHAVIOURS_DIR, RELEASES_DIR, ARGS_DIR] {
        fs::create_dir_all(world.join(dir)).expect("create an agent directory");
    }
    assert!(agents.join("claude").is_file() && agents.join("codex").is_file());
}

/// Makes attempt n of `task` behave as `plan[n]`, the last entry repeating.
pub fn plan(world: &Path, task: &str, plan: &[Behaviour]) {
    assert!(!plan.is_empty() && !task.is_empty());
    let words: Vec<&str> = plan.iter().map(|b| b.word()).collect();
    let path = world.join(BEHAVIOURS_DIR).join(task);
    fs::write(&path, format!("{}\n", words.join("\n"))).expect("write an agent's plan");
    assert!(path.is_file());
}

/// The arguments attempt `attempt` of `task` was started with, one per line.
pub fn args(world: &Path, task: &str, attempt: u32) -> String {
    let path = world.join(ARGS_DIR).join(format!("{task}-{attempt}"));
    fs::read_to_string(&path).expect("read an agent's arguments")
}

/// Lets a holding agent of `task` go on.
pub fn release(world: &Path, task: &str) {
    assert!(!task.is_empty());
    let path = world.join(RELEASES_DIR).join(task);
    fs::write(&path, "").expect("release an agent");
    assert!(path.exists());
}

/// One start of a fake agent, as it recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub task: String,
    pub attempt: u32,
    pub pid: u32,
    /// `claude` or `codex`.
    pub harness: String,
    pub behaviour: String,
    pub dir: PathBuf,
    pub ended: bool,
}

/// Every fake agent start in the world, in order; `ended` says whether it has exited.
pub fn runs(world: &Path) -> Vec<Run> {
    let text = fs::read_to_string(world.join(RUNS_FILE)).unwrap_or_default();
    let mut runs: Vec<Run> = Vec::new();
    for line in text.lines().take(RUNS_MAX * 2) {
        let words: Vec<&str> = line.splitn(7, ' ').collect();
        match words.as_slice() {
            ["start", task, attempt, pid, harness, behaviour, dir] => runs.push(Run {
                task: task.to_string(),
                attempt: attempt.parse().expect("an attempt number"),
                pid: pid.parse().expect("a pid"),
                harness: harness.to_string(),
                behaviour: behaviour.to_string(),
                dir: PathBuf::from(dir),
                ended: false,
            }),
            ["end", _, _, pid] => {
                let pid: u32 = pid.parse().expect("a pid");
                let run = runs
                    .iter_mut()
                    .rev()
                    .find(|run| run.pid == pid && !run.ended);
                run.expect("an end follows its start").ended = true;
            }
            _ => panic!("unrecognised agent run line: {line:?}"),
        }
    }
    assert!(runs.len() <= RUNS_MAX, "more agent runs than RUNS_MAX");
    runs
}

/// The most fake agents that were running at the same moment.
pub fn most_at_once(world: &Path) -> usize {
    let text = fs::read_to_string(world.join(RUNS_FILE)).unwrap_or_default();
    let (mut now, mut most) = (0usize, 0usize);
    for line in text.lines().take(RUNS_MAX * 2) {
        if line.starts_with("start ") {
            now += 1;
            most = most.max(now);
        } else if line.starts_with("end ") {
            now = now.checked_sub(1).expect("an end follows its start");
        }
    }
    assert!(now <= most);
    most
}

/// The pids of fake agents that have not recorded their end.
// It runs inside `Drop`, so it skips what it cannot parse rather than panic.
pub fn running_pids(world: &Path) -> Vec<u32> {
    let text = fs::read_to_string(world.join(RUNS_FILE)).unwrap_or_default();
    let mut pids: Vec<u32> = Vec::new();
    for line in text.lines().take(RUNS_MAX * 2) {
        let words: Vec<&str> = line.split(' ').collect();
        let pid = words.get(3).and_then(|pid| pid.parse::<u32>().ok());
        match (words.first(), pid) {
            (Some(&"start"), Some(pid)) if pid > 1 => pids.push(pid),
            (Some(&"end"), Some(pid)) => pids.retain(|running| *running != pid),
            _ => {}
        }
    }
    assert!(pids.len() <= RUNS_MAX * 2);
    pids
}
