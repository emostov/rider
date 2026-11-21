//! Fake `claude` and `codex` (also as `codex review`) scripts that play a scripted worker, one behaviour per attempt.

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
const HEADS_DIR: &str = "agent-heads";
const RUNS_MAX: usize = 4096;
/// The exit status of an agent told to fail.
pub const FAIL_EXIT_CODE: i32 = 3;
/// How long the one line a `LongLine` agent prints is, in bytes.
pub const LONG_LINE_BYTES: u64 = 40_000;
/// How long a slow agent works before it acts.
pub const SLOW_SECONDS: u64 = 35;
pub const PR_REF: &str = "refs/rider/pr/412";

/// What a fake agent does in one attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behaviour {
    /// Commits `work_file(task, attempt)` and reports success; the default.
    Commit,
    /// Reports success having committed nothing.
    CommitNothing,
    /// Records its worktree's `HEAD` commit and branch, then acts like `CommitNothing`.
    RecordHead,
    /// Writes `uncommitted-<task>.txt` without committing it and reports success.
    LeaveUncommitted,
    /// Blocks its own task as `needs_input` with `vb`, then reports success.
    Block,
    /// Prints nothing and exits `FAIL_EXIT_CODE`.
    ExitNonZero,
    /// As `codex review`, prints no final text and exits 0.
    ReviewNothing,
    /// As `codex review` whose review delegate never started: prints Codex's interruption
    /// message and exits 0.
    ReviewInterrupted,
    /// As `codex review` stopped by the provider's content filter: prints `CONTENT_FILTER_ERROR`
    /// on stderr and exits 1.
    ContentFilterStop,
    /// Waits until the test releases its task, then commits like `Commit`.
    Hold,
    /// Like `Hold`, but ignores SIGTERM.
    HoldIgnoringTerm,
    /// Prints `hold_line(task)` to its output stream, then holds like `Hold`.
    HoldTalking,
    /// As `claude --resume` of a session Claude no longer has: reports the refusal and exits 1
    /// when started with `--resume`; otherwise commits like `Commit`.
    RefuseResume,
    /// Like `RefuseResume`, but only after working for `SLOW_SECONDS`.
    RefuseResumeSlowly,
    /// Resets its branch to the commit below `HEAD`, then commits like `Commit`.
    DropEarlier,
    /// Rebases its branch onto `main`, then commits like `Commit`.
    RebaseOntoMain,
    /// Like `RefuseResume`, but once it has reported the refusal it holds like `Hold` before
    /// it exits 1.
    RefuseResumeThenHold,
    /// Switches to a new branch `elsewhere`, then commits like `Commit`.
    WanderBranch,
    /// Detaches its worktree's `HEAD`, then commits like `Commit`.
    Detach,
    /// Commits like `Commit`, then exits `FAIL_EXIT_CODE`.
    CommitThenFail,
    /// Commits like `Commit`, then starts a `git bisect` it leaves in progress.
    CommitThenBisect,
    /// Commits a `.gitignore` naming `target/`, then writes ignored output under `target/`.
    IgnoredOutput,
    /// Resets its branch to the commit below `HEAD` and commits nothing.
    DropOnly,
    /// Fast-forwards its branch to the branch `sibling` and commits nothing.
    FastForwardSibling,
    /// Prints `line 1` to `line 50`, then exits `FAIL_EXIT_CODE`.
    ManyLines,
    /// Prints one line of `LONG_LINE_BYTES` bytes without a newline, then exits
    /// `FAIL_EXIT_CODE`.
    LongLine,
    /// Prints a line of `LONG_LINE_BYTES` bytes, a line holding the byte 0xff, and `end`,
    /// then exits `FAIL_EXIT_CODE`.
    BadBytes,
    /// Prints 40 lines of 250 four-byte characters each, then exits `FAIL_EXIT_CODE`.
    WideLines,
    /// Commits like `Commit` and reports `dashed_summary(task)`, as JSON-escaped text.
    CommitDashedSummary,
    /// Prints `TALK_LINES` lines of `talk_line(n)` and exits `FAIL_EXIT_CODE`.
    FailTalkingLong,
    /// Kills itself with SIGKILL.
    ExitOnSignal,
    /// Commits like `Commit`, then reports a failed result and exits 0.
    CommitReportingFailure,
    /// Commits like `Commit` and exits 0 without reporting anything.
    CommitSilently,
    /// Commits like `Commit`, then reports a result of only whitespace.
    CommitReportingBlank,
    /// Checks out a new branch `elsewhere-<task>`, then commits like `Commit`.
    LeaveBranch,
    /// Blocks its own task like `Block`, then exits `FAIL_EXIT_CODE`.
    BlockThenExitNonZero,
    /// Commits like `Commit`, then points a new branch `shared-<task>` at that commit.
    CommitShared,
    /// Holds like `Hold`, then reports success having committed nothing.
    HoldThenNothing,
    /// Commits like `Commit`, then reports `USAGE` and `COST_USD` in its result.
    CommitReportingUsage,
    /// Commits like `Commit`, then reports a result that names `usage` twice.
    CommitRepeatingUsage,
    /// Commits nothing and reports `HUGE_CODEX_USAGE` as its last usage snapshot.
    ReportHugeUsage,
    /// Commits like `Commit` when run as `claude`, then prints the harness's recorded fixture.
    ReplayRecorded,
    ReportDecision,
    HoldAttempt,
    MergePr,
}

/// The input, cache write, cache read and output tokens a `CommitReportingUsage` agent reports.
pub const USAGE: [u64; 4] = [4, 20, 300, 50];
/// The input, cached, cache write and output tokens of each cumulative snapshot a fake `codex`
/// reports, the last holding the run's totals.
pub const CODEX_USAGE: [[u64; 4]; 2] = [[400, 200, 100, 20], [1000, 600, 300, 50]];
/// The input, cached, cache write and output tokens a `ReportHugeUsage` agent reports.
pub const HUGE_CODEX_USAGE: [u64; 4] = [3_298_534_883_328, 1 << 40, 1 << 40, 1 << 40];
/// The cost a `CommitReportingUsage` agent reports, in US dollars.
pub const COST_USD: f64 = 0.25;

pub const TALK_LINES: usize = 300;
pub const TALK_LINE_PAD: usize = 250;
pub const SUMMARY_WORDS: usize = 200;

pub fn talk_line(n: usize) -> String {
    format!("-é{n} {}", "x".repeat(TALK_LINE_PAD))
}

pub fn dashed_summary(task: &str) -> String {
    let words: String = (0..SUMMARY_WORDS).map(|n| format!(" é{n}")).collect();
    format!("-first  line\n\t-task {task}{words}")
}

impl Behaviour {
    fn word(self) -> &'static str {
        match self {
            Behaviour::Commit => "commit",
            Behaviour::CommitNothing => "commit-nothing",
            Behaviour::RecordHead => "record-head",
            Behaviour::LeaveUncommitted => "leave-uncommitted",
            Behaviour::Block => "block",
            Behaviour::ExitNonZero => "exit-non-zero",
            Behaviour::ReviewNothing => "review-nothing",
            Behaviour::ReviewInterrupted => "review-interrupted",
            Behaviour::ContentFilterStop => "content-filter-stop",
            Behaviour::Hold => "hold",
            Behaviour::HoldIgnoringTerm => "hold-ignoring-term",
            Behaviour::HoldTalking => "hold-talking",
            Behaviour::RefuseResume => "refuse-resume",
            Behaviour::RefuseResumeSlowly => "refuse-resume-slowly",
            Behaviour::DropEarlier => "drop-earlier",
            Behaviour::RebaseOntoMain => "rebase-onto-main",
            Behaviour::RefuseResumeThenHold => "refuse-resume-then-hold",
            Behaviour::WanderBranch => "wander-branch",
            Behaviour::Detach => "detach",
            Behaviour::CommitThenFail => "commit-then-fail",
            Behaviour::CommitThenBisect => "commit-then-bisect",
            Behaviour::IgnoredOutput => "ignored-output",
            Behaviour::DropOnly => "drop-only",
            Behaviour::FastForwardSibling => "fast-forward-sibling",
            Behaviour::ManyLines => "many-lines",
            Behaviour::LongLine => "long-line",
            Behaviour::BadBytes => "bad-bytes",
            Behaviour::WideLines => "wide-lines",
            Behaviour::CommitDashedSummary => "commit-dashed-summary",
            Behaviour::FailTalkingLong => "fail-talking-long",
            Behaviour::ExitOnSignal => "exit-on-signal",
            Behaviour::CommitReportingFailure => "commit-reporting-failure",
            Behaviour::CommitSilently => "commit-silently",
            Behaviour::CommitReportingBlank => "commit-reporting-blank",
            Behaviour::LeaveBranch => "leave-branch",
            Behaviour::BlockThenExitNonZero => "block-then-exit-non-zero",
            Behaviour::CommitShared => "commit-shared",
            Behaviour::HoldThenNothing => "hold-then-nothing",
            Behaviour::CommitReportingUsage => "commit-reporting-usage",
            Behaviour::CommitRepeatingUsage => "commit-repeating-usage",
            Behaviour::ReportHugeUsage => "report-huge-usage",
            Behaviour::ReplayRecorded => "replay-recorded",
            Behaviour::ReportDecision => "report-decision",
            Behaviour::HoldAttempt => "hold-attempt",
            Behaviour::MergePr => "merge-pr",
        }
    }
}

/// What Codex's interruption message for a review that never started begins with.
pub const REVIEW_INTERRUPTED: &str =
    "Review was interrupted. Please re-run /review and wait for it to complete.";
/// The error a `ContentFilterStop` agent prints on stderr.
pub const CONTENT_FILTER_ERROR: &str =
    "ERROR: This content was flagged for possible cybersecurity risk.";

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

/// The line a `HoldTalking` agent prints before it holds.
pub fn hold_line(task: &str) -> String {
    format!(r#"{{"type":"fake","text":"holding task {task}"}}"#)
}

pub fn heard(decisions: &[&str]) -> String {
    let heard: Vec<String> = decisions
        .iter()
        .map(|word| format!("Decision {word}"))
        .collect();
    format!("Fake agent heard {}.", heard.join(", "))
}

/// The final message a fake agent reports; it becomes the task's result.
pub fn summary(task: &str, attempt: u32) -> String {
    format!("Fake agent finished task {task} attempt {attempt}.")
}

fn script() -> String {
    let steps = HOLD_SECONDS_MAX * HOLD_STEPS_PER_SECOND;
    let fail = FAIL_EXIT_CODE;
    let slow = SLOW_SECONDS;
    let long = LONG_LINE_BYTES;
    let (words, pad, lines) = (SUMMARY_WORDS, TALK_LINE_PAD, TALK_LINES);
    let [input, write, read, output] = USAGE;
    let usage = format!(
        ",\"usage\":{{\"input_tokens\":{input},\"cache_creation_input_tokens\":{write},\
         \"cache_read_input_tokens\":{read},\"output_tokens\":{output}}},\
         \"total_cost_usd\":{COST_USD}"
    );
    let [early, last, huge] =
        [CODEX_USAGE[0], CODEX_USAGE[1], HUGE_CODEX_USAGE].map(|[input, cached, write, output]| {
            format!(
                "\"usage\":{{\"input_tokens\":{input},\"cached_input_tokens\":{cached},\
             \"cache_write_input_tokens\":{write},\"output_tokens\":{output}}}"
            )
        });
    let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/harness");
    format!(
        r#"#!/bin/sh
world="${WORLD_VAR}"
task="$BOARD_TASK"
harness=$(basename "$0")
[ "$harness" = codex ] && [ "$1" = review ] && harness=codex-review
runs="$world/{RUNS_FILE}"
attempt=$( (grep "^start $task " "$runs" 2>/dev/null || true) | wc -l | tr -d ' ')
plan="$world/{BEHAVIOURS_DIR}/$task"
behaviour=$(sed -n "$((attempt + 1))p" "$plan" 2>/dev/null)
[ -n "$behaviour" ] || behaviour=$(tail -n 1 "$plan" 2>/dev/null)
[ -n "$behaviour" ] || behaviour=commit
echo "start $task $attempt $$ $harness $behaviour $(pwd)" >> "$runs"
printf '%s\n' "$@" > "$world/{ARGS_DIR}/$task-$attempt.all"
[ "$harness" = codex-review ] && cat > "$world/{ARGS_DIR}/$task-$attempt.stdin"
env | sed -n 's/^\(GIT_[A-Z_]*\)=.*/\1/p' | sort > "$world/{ARGS_DIR}/$task-$attempt.git-env"
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
extra=""
codex_last='{last}'
fixtures="{fixtures}"
report() {{
  if [ "$harness" = codex-review ]; then
    printf 'OpenAI Codex v0.0.0\n--------\ncodex\n%s\n' "$summary" >&2
    echo "$summary"
  elif [ "$harness" = codex ]; then
    echo '{{"type":"thread.started","thread_id":"00000000-0000-0000-0000-000000000000"}}'
    echo '{{"type":"turn.started"}}'
    item='"item":{{"id":"item_0","type":"agent_message","text":"'"$summary"'"}}'
    echo '{{"type":"item.completed",'"$item"'}}'
    echo '{{"type":"turn.completed",{early}}}'
    echo '{{"type":"turn.completed",'"$codex_last"'}}'
  else
    init='"subtype":"init","cwd":"'"$(pwd)"'","session_id":"'"$session"'","model":"fake"'
    echo '{{"type":"system",'"$init"'}}'
    echo '{{"type":"result","subtype":"success","is_error":false,"result":"'"$summary"'"'"$extra"'}}'
  fi
}}
hold() {{
  step=0
  while [ ! -e "$world/{RELEASES_DIR}/$task" ]; do
    step=$((step + 1))
    [ "$step" -le {steps} ] || exit 1
    sleep 0.1
  done
}}
commit() {{
  echo "work for task $task attempt $attempt" > "work-$task-$attempt.txt"
  git add "work-$task-$attempt.txt" && git commit -q -m "Work for task $task attempt $attempt"
}}
case "$behaviour" in
  commit) commit || exit 1 ;;
  commit-nothing) ;;
  record-head)
    head="$world/{HEADS_DIR}/$task-$attempt"
    git rev-parse HEAD > "$head" && (git symbolic-ref -q HEAD || true) >> "$head" || exit 1 ;;
  drop-earlier) git reset -q --hard HEAD~1 && commit || exit 1 ;;
  rebase-onto-main) git rebase -q main && commit || exit 1 ;;
  leave-uncommitted) echo "left behind" > "uncommitted-$task.txt" ;;
  block) vb tasks block "$task" --kind needs_input -- "{BLOCK_QUESTION}" || exit 1 ;;
  exit-non-zero) exit {fail} ;;
  review-nothing) exit 0 ;;
  review-interrupted) echo "{REVIEW_INTERRUPTED}"; exit 0 ;;
  content-filter-stop) echo "{CONTENT_FILTER_ERROR}" >&2; exit 1 ;;
  wander-branch) git switch -q -c elsewhere && commit || exit 1 ;;
  detach) git checkout -q --detach && commit || exit 1 ;;
  commit-then-fail) commit; exit {fail} ;;
  commit-then-bisect) commit && git bisect start || exit 1 ;;
  ignored-output)
    printf 'target/\n' > .gitignore && git add .gitignore && git commit -q -m "Ignore target" \
      && mkdir -p target && echo built > target/out.o || exit 1 ;;
  drop-only) git reset -q --hard HEAD~1 || exit 1 ;;
  fast-forward-sibling) git merge -q --ff-only sibling || exit 1 ;;
  many-lines) i=1; while [ $i -le 50 ]; do echo "line $i"; i=$((i + 1)); done; exit {fail} ;;
  long-line) head -c {long} /dev/zero | tr '\0' z; exit {fail} ;;
  bad-bytes)
    head -c {long} /dev/zero | tr '\0' y; printf '\nbad \377 byte\nend\n'; exit {fail} ;;
  wide-lines)
    l=$(i=0; while [ $i -lt 250 ]; do printf '\360\237\230\200'; i=$((i + 1)); done)
    i=0; while [ $i -lt 40 ]; do printf '%s\n' "$l"; i=$((i + 1)); done; exit {fail} ;;
  commit-dashed-summary)
    commit || exit 1
    summary='-first  line\\n\\t-task '"$task"
    n=0
    while [ $n -lt {words} ]; do summary="$summary é$n"; n=$((n + 1)); done ;;
  fail-talking-long)
    pad=$(printf '%{pad}s' '' | tr " " x)
    n=0
    while [ $n -lt {lines} ]; do printf -- '-é%s %s\n' $n "$pad"; n=$((n + 1)); done
    exit {fail} ;;
  refuse-resume|refuse-resume-slowly|refuse-resume-then-hold)
    [ "$behaviour" = refuse-resume-slowly ] && sleep {slow}
    if [ -n "$resumed" ]; then
      echo "No conversation found with session ID: $resumed" >&2
      refused='"subtype":"error_during_execution","is_error":true,"num_turns":0'
      echo '{{"type":"result",'"$refused"',"session_id":"'"$session"'"}}'
      [ "$behaviour" = refuse-resume-then-hold ] && hold
      exit 1
    fi
    commit || exit 1 ;;
  hold-then-nothing) hold ;;
  hold-attempt)
    step=0
    while [ ! -e "$world/{RELEASES_DIR}/$task-$attempt" ]; do
      step=$((step + 1))
      [ "$step" -le {steps} ] || exit 1
      sleep 0.1
    done
    commit || exit 1 ;;
  report-decision)
    for arg in "$@"; do prompt="$arg"; done
    heard=$(printf '%s\n' "$prompt" | grep -o 'Decision [a-z0-9]*' | paste -s -d '#' - \
      | sed 's/#/, /g')
    [ -n "$heard" ] || heard="Decision none"
    summary="Fake agent heard $heard." ;;
  merge-pr) git merge -q --no-edit {PR_REF} || exit 1 ;;
  hold|hold-ignoring-term|hold-talking)
    [ "$behaviour" = hold-ignoring-term ] && trap '' TERM
    [ "$behaviour" = hold-talking ] && echo '{{"type":"fake","text":"holding task '"$task"'"}}'
    hold
    commit || exit 1 ;;
  exit-on-signal) kill -KILL $$ ;;
  commit-reporting-failure)
    commit || exit 1
    echo '{{"type":"result","subtype":"error_during_execution","is_error":true}}'
    exit 0 ;;
  commit-silently) commit || exit 1; exit 0 ;;
  commit-reporting-usage) commit || exit 1; extra='{usage}' ;;
  commit-repeating-usage) commit || exit 1; extra=',"usage":1,"usage":2' ;;
  report-huge-usage) codex_last='{huge}' ;;
  replay-recorded)
    [ "$harness" = claude ] && {{ commit || exit 1; }}
    cat "$fixtures/$harness.jsonl"
    exit 0 ;;
  commit-reporting-blank) commit || exit 1; summary="  " ;;
  leave-branch) git checkout -q -b "elsewhere-$task" && commit || exit 1 ;;
  commit-shared) commit && git branch "shared-$task" || exit 1 ;;
  block-then-exit-non-zero)
    vb tasks block "$task" --kind needs_input -- "{BLOCK_QUESTION}" || exit 1
    exit {fail} ;;
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
    for dir in [BEHAVIOURS_DIR, RELEASES_DIR, ARGS_DIR, HEADS_DIR] {
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

/// What attempt `attempt` of `task`, started as `codex review`, read from its standard input.
pub fn stdin(world: &Path, task: &str, attempt: u32) -> String {
    let path = world.join(ARGS_DIR).join(format!("{task}-{attempt}.stdin"));
    fs::read_to_string(&path).expect("read an agent's standard input")
}

/// The arguments attempt `attempt` of `task` was started with, one per line.
pub fn args(world: &Path, task: &str, attempt: u32) -> String {
    let path = world.join(ARGS_DIR).join(format!("{task}-{attempt}.all"));
    fs::read_to_string(&path).expect("read an agent's arguments")
}

/// The names of the `GIT_*` variables attempt `attempt` of `task` was started with.
pub fn git_env(world: &Path, task: &str, attempt: u32) -> Vec<String> {
    let path = world
        .join(ARGS_DIR)
        .join(format!("{task}-{attempt}.git-env"));
    let text = fs::read_to_string(&path).expect("read an agent's git environment");
    text.lines().map(str::to_string).collect()
}

/// The commit and the branch ref, `None` when detached, that attempt `attempt` of `task`
/// found its worktree's `HEAD` at.
pub fn head(world: &Path, task: &str, attempt: u32) -> (String, Option<String>) {
    let path = world.join(HEADS_DIR).join(format!("{task}-{attempt}"));
    let text = fs::read_to_string(&path).expect("read an agent's recorded HEAD");
    let mut lines = text.lines().map(str::to_string);
    let commit = lines.next().expect("the recorded HEAD names a commit");
    let branch = lines.next();
    assert_eq!(
        lines.next(),
        None,
        "the recorded HEAD is at most two lines: {text:?}"
    );
    (commit, branch)
}

/// Lets a holding agent of `task` go on.
pub fn release(world: &Path, task: &str) {
    assert!(!task.is_empty());
    let path = world.join(RELEASES_DIR).join(task);
    fs::write(&path, "").expect("release an agent");
    assert!(path.exists());
}

pub fn release_attempt(world: &Path, task: &str, attempt: u32) {
    let path = world.join(RELEASES_DIR).join(format!("{task}-{attempt}"));
    fs::write(&path, "").expect("release an agent's attempt");
    assert!(path.exists());
}

/// One start of a fake agent, as it recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub task: String,
    pub attempt: u32,
    pub pid: u32,
    /// `claude`, `codex`, or `codex-review`.
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
