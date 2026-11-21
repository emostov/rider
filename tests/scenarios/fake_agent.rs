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
const STREAMS_DIR: &str = "agent-streams";
const LEFTOVERS_DIR: &str = "agent-leftovers";
const RUNS_MAX: usize = 4096;
/// The exit status of an agent told to fail.
pub const FAIL_EXIT_CODE: i32 = 3;
/// The line a `ComplainThenExitNonZero` agent prints on stderr.
pub const STDERR_LINE: &str = "fake agent complains on stderr";
/// How long the one line a `LongLine` agent prints is, in bytes.
pub const LONG_LINE_BYTES: u64 = 40_000;
/// How long a slow agent works before it acts.
pub const SLOW_SECONDS: u64 = 35;
/// How long the process a `CommitLeavingOrphan` or `CommitLeavingEscapee` agent leaves lives.
pub const LEFTOVER_SECONDS: u64 = 30;
/// How many bytes a `CommitFlooding` agent prints after its report.
pub const FLOOD_BYTES: u64 = 65 * 1024 * 1024;
pub const PR_REF: &str = "refs/rider/pr/412";
pub const ESCAPE_EQUALS: &str = "escape-equals";
pub const ESCAPE_NESTED: &str = "escape-nested";
pub const ESCAPE_PROMISOR: &str = "escape-promisor";
pub const ESCAPE_OPEN: &str = "escape-open";
pub const TRIED_WRITES: &str = "agents.json.new cursor.new board.json.new node/probe .b/probe rider.toml prompts ../tools/probe \
     board wait roles run node.lock ship.toml prs/7-head/patch.diff prs/7-head/probe queue/reviewed/7 \
     queue/reviewed/probe";
pub const REPO_WRITES: &str = ".git/config .git/hooks/pre-commit probe";
pub const OTHER_TASK_WRITES: &str = "worktrees/repo/99/CLAUDE.md worktrees/repo/99/.git \
     worktrees/repo/100 untrusted/99/work/CLAUDE.md untrusted/100";

/// What a fake agent does in one attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behaviour {
    /// Commits `work_file(task, attempt)` and reports success; the default.
    Commit,
    /// Reports success having committed nothing.
    CommitNothing,
    /// As a Codex that fails before its thread starts for a reason other than the session,
    /// such as an expired login: prints an error to stderr and exits 1.
    FailBeforeThread,
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
    /// As Codex stopped by the provider's content filter, exiting 1: `codex exec` prints the
    /// recorded `CONTENT_FILTER_EVENTS`, and `codex review` prints `CONTENT_FILTER_ERROR` on stderr.
    ContentFilterStop,
    /// Like `ContentFilterStop` as `codex exec`, after 24 long commands whose output starts with
    /// an escape sequence, and while `last-command` runs.
    ContentFilterStopLong,
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
    /// Like `RefuseResume`, but started without a session to resume it commits nothing.
    RefuseResumeThenNothing,
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
    /// Runs `$RIDER_BIN kill <task>` and `$RIDER_BIN down --now`, records their output in
    /// `stop_attempts(task)`, then holds like `Hold`.
    TryToStop,
    /// Starts a child in its own process group that, once orphaned, runs
    /// `$RIDER_BIN down --now` and records its output in `stop_attempts(task)`; then holds.
    OrphanStops,
    /// Commits like `Commit`, then reports `USAGE` and `COST_USD` in its result.
    CommitReportingUsage,
    /// Commits like `Commit`, then reports a result that names `usage` twice.
    CommitRepeatingUsage,
    /// Commits nothing and reports `HUGE_CODEX_USAGE` as its last usage snapshot.
    ReportHugeUsage,
    /// Commits like `Commit` when run as `claude`, then prints the harness's recorded fixture.
    ReplayRecorded,
    /// Commits like `Commit`, then prints the text given by `stream` as its whole output
    /// stream and exits 0.
    CommitReplaying,
    /// Commits and reports like `Commit`, then exits leaving a process in its group that
    /// holds its output open for `LEFTOVER_SECONDS`.
    CommitLeavingOrphan,
    /// Like `CommitLeavingOrphan`, but the process it leaves is in a process group of its own.
    CommitLeavingEscapee,
    /// Commits and reports like `Commit`, then prints `FLOOD_BYTES` bytes and exits 0.
    CommitFlooding,
    /// Prints `STDERR_LINE` on stderr and exits `FAIL_EXIT_CODE`.
    ComplainThenExitNonZero,
    /// Runs `rider down --now` in a child of its own process group, waits for that child to
    /// record its exit code and output, then commits like `Commit`.
    StopRiderDetached,
    TryWrites,
    SignalDaemonDetached,
    PlantFsmonitor,
    PlantFilters,
    PlantPromisor,
    OpenApp,
    PlantGitConfig,
    /// Prints a stream in its harness's shape with `STREAM_MESSAGES`, a failed tool call and
    /// `STREAM_ERROR` as the run's error, then exits `FAIL_EXIT_CODE`.
    StreamError,
    /// Prints one Claude message holding ESC, BEL, a bidi override and a newline before a
    /// forged account line, then exits `FAIL_EXIT_CODE`.
    StreamForged,
    /// Prints the first of `STREAM_MESSAGES` as a Claude message, then `STDERR_ERROR` on
    /// standard error, and exits `FAIL_EXIT_CODE`.
    StreamThenStderr,
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

/// What a `StreamThenStderr` agent prints on standard error before it exits.
pub const STDERR_ERROR: &str = "error: something broke";

/// The assistant messages a `StreamError` agent prints, in order.
pub const STREAM_MESSAGES: [&str; 2] = ["I will add the work.", "The tests failed; I will retry."];
/// The command a `StreamError` agent of Codex runs and sees fail with exit status 1.
pub const STREAM_COMMAND: &str = "cargo test";
/// The error a `StreamError` agent of Claude sees from its tool call.
pub const STREAM_TOOL_ERROR: &str = "Exit code 1";
/// The error a `StreamError` agent's run ends with, as a provider's content filter reports it.
pub const STREAM_ERROR: &str = "API Error: Output blocked by content filtering policy";

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
            Behaviour::FailBeforeThread => "fail-before-thread",
            Behaviour::RecordHead => "record-head",
            Behaviour::LeaveUncommitted => "leave-uncommitted",
            Behaviour::Block => "block",
            Behaviour::ExitNonZero => "exit-non-zero",
            Behaviour::ReviewNothing => "review-nothing",
            Behaviour::ReviewInterrupted => "review-interrupted",
            Behaviour::ContentFilterStop => "content-filter-stop",
            Behaviour::ContentFilterStopLong => "content-filter-stop-long",
            Behaviour::Hold => "hold",
            Behaviour::HoldIgnoringTerm => "hold-ignoring-term",
            Behaviour::HoldTalking => "hold-talking",
            Behaviour::RefuseResume => "refuse-resume",
            Behaviour::RefuseResumeSlowly => "refuse-resume-slowly",
            Behaviour::DropEarlier => "drop-earlier",
            Behaviour::RebaseOntoMain => "rebase-onto-main",
            Behaviour::RefuseResumeThenHold => "refuse-resume-then-hold",
            Behaviour::WanderBranch => "wander-branch",
            Behaviour::RefuseResumeThenNothing => "refuse-resume-then-nothing",
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
            Behaviour::TryToStop => "try-to-stop",
            Behaviour::OrphanStops => "orphan-stops",
            Behaviour::CommitReportingUsage => "commit-reporting-usage",
            Behaviour::CommitRepeatingUsage => "commit-repeating-usage",
            Behaviour::ReportHugeUsage => "report-huge-usage",
            Behaviour::ReplayRecorded => "replay-recorded",
            Behaviour::CommitReplaying => "commit-replaying",
            Behaviour::CommitLeavingOrphan => "commit-leaving-orphan",
            Behaviour::CommitLeavingEscapee => "commit-leaving-escapee",
            Behaviour::CommitFlooding => "commit-flooding",
            Behaviour::ComplainThenExitNonZero => "complain-then-exit-non-zero",
            Behaviour::StopRiderDetached => "stop-rider-detached",
            Behaviour::TryWrites => "try-writes",
            Behaviour::SignalDaemonDetached => "signal-daemon-detached",
            Behaviour::PlantFsmonitor => "plant-fsmonitor",
            Behaviour::PlantFilters => "plant-filters",
            Behaviour::PlantPromisor => "plant-promisor",
            Behaviour::OpenApp => "open-app",
            Behaviour::PlantGitConfig => "plant-git-config",
            Behaviour::StreamError => "stream-error",
            Behaviour::StreamForged => "stream-forged",
            Behaviour::StreamThenStderr => "stream-then-stderr",
            Behaviour::ReportDecision => "report-decision",
            Behaviour::HoldAttempt => "hold-attempt",
            Behaviour::MergePr => "merge-pr",
        }
    }
}

/// What Codex's interruption message for a review that never started begins with.
pub const REVIEW_INTERRUPTED: &str =
    "Review was interrupted. Please re-run /review and wait for it to complete.";
pub const CONTENT_FILTER_PLAN: &str = "Plan: check the sandbox profile first.";
pub const CONTENT_FILTER_ERROR: &str = "ERROR: This content was flagged for possible cybersecurity risk. If this seems wrong, try \
     rephrasing your request.";
const CONTENT_FILTER_EVENTS: &str =
    include_str!("../../fixtures/harness/codex-content-filter.jsonl");

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

/// The session id a fake agent reports in attempt `attempt` of `task`, unless it continues a
/// session in place: `claude --resume <id>` without `--fork-session`, or `codex exec resume`
/// with `<id>` first after `--`, reports `<id>`.
pub fn session(task: &str, attempt: u32) -> String {
    format!("fake-{task}-{attempt}")
}

/// The options attempt `attempt` of `task` was started with: its arguments before `--`.
pub fn options(world: &Path, task: &str, attempt: u32) -> Vec<String> {
    let path = world.join(ARGS_DIR).join(format!("{task}-{attempt}"));
    let text = fs::read_to_string(&path).expect("read an agent's recorded arguments");
    text.lines().map(str::to_string).collect()
}

/// What a `TryToStop` agent's stop commands printed, once both have ended.
pub fn stop_attempts(world: &Path, task: &str) -> Option<String> {
    fs::read_to_string(world.join(HEADS_DIR).join(format!("{task}-stops"))).ok()
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
    let (leftover, flood) = (LEFTOVER_SECONDS, FLOOD_BYTES);
    let rider = env!("CARGO_BIN_EXE_rider");
    assert!(
        !rider.contains('\''),
        "the rider binary's path is quoted in the script"
    );
    let ([said0, said1], error) = (STREAM_MESSAGES, STREAM_ERROR);
    let (command, tool_error, stderr_error) = (STREAM_COMMAND, STREAM_TOOL_ERROR, STDERR_ERROR);
    format!(
        r#"#!/bin/sh
world="${{{WORLD_VAR}:-$(dirname "$(dirname "$(command -v "$0")")")}}"
task="$BOARD_TASK"
harness=$(basename "$0")
[ "$harness" = codex ] && [ "$1" = review ] && harness=codex-review
runs="$world/{RUNS_FILE}"
attempt=$( (grep "^start $task " "$runs" 2>/dev/null || true) | wc -l | tr -d ' ')
plan="$world/{BEHAVIOURS_DIR}/$task"
[ -f "$plan" ] || plan="$world/{BEHAVIOURS_DIR}/default"
behaviour=$(sed -n "$((attempt + 1))p" "$plan" 2>/dev/null)
[ -n "$behaviour" ] || behaviour=$(tail -n 1 "$plan" 2>/dev/null)
[ -n "$behaviour" ] || behaviour=commit
echo "start $task $attempt $$ $harness $behaviour $(pwd)" >> "$runs"
printf '%s\n' "$@" > "$world/{ARGS_DIR}/$task-$attempt.all"
echo "$#" > "$world/{ARGS_DIR}/$task-$attempt.argc"
[ "$harness" = codex-review ] && cat > "$world/{ARGS_DIR}/$task-$attempt.stdin"
env | sed -n 's/^\(GIT_[A-Z_]*\)=.*/\1/p' | sort > "$world/{ARGS_DIR}/$task-$attempt.git-env"
env | grep -E '^(GH|GITHUB)_' | sort > "$world/{ARGS_DIR}/$task-$attempt.gh-env"
# bash and dash each export a different set of their own variables.
env | sed -n 's/^\([A-Za-z_][A-Za-z0-9_]*\)=.*/\1/p' | grep -v -x -e PWD -e OLDPWD -e SHLVL -e _ \
  | sort > "$world/{ARGS_DIR}/$task-$attempt.env"
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
if [ "$harness" = codex ] && [ "$2" = resume ]; then
  resumed=$(printf '%s\n' "$@" | sed -n '/^--$/{{n;p;q;}}')
fi
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
    echo '{{"type":"thread.started","thread_id":"'"$session"'"}}'
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
  fail-before-thread)
    echo "Error: not logged in" >&2
    exit 1 ;;
  record-head)
    head="$world/{HEADS_DIR}/$task-$attempt"
    git rev-parse HEAD > "$head" && (git symbolic-ref -q HEAD || true) >> "$head" || exit 1 ;;
  drop-earlier) git reset -q --hard HEAD~1 && commit || exit 1 ;;
  rebase-onto-main) git rebase -q main && commit || exit 1 ;;
  leave-uncommitted) echo "left behind" > "uncommitted-$task.txt" ;;
  block) vb tasks block "$task" --kind needs_input -- "{BLOCK_QUESTION}" || exit 1 ;;
  exit-non-zero) exit {fail} ;;
  complain-then-exit-non-zero) echo "{STDERR_LINE}" >&2; exit {fail} ;;
  review-nothing) exit 0 ;;
  review-interrupted) echo "{REVIEW_INTERRUPTED}"; exit 0 ;;
  content-filter-stop)
    vb reply "$task" -- "{CONTENT_FILTER_PLAN}" > /dev/null || exit 2
    if [ "$harness" = codex ]; then
      cat <<'EVENTS'
{CONTENT_FILTER_EVENTS}EVENTS
    else
      printf 'exec\n/bin/zsh -lc ls\n succeeded in 5ms:\nsrc\n' >&2
      printf '%s\ntokens used\n4,080\n' "{CONTENT_FILTER_ERROR}" >&2
    fi
    exit 1 ;;
  content-filter-stop-long)
    pad=$(printf '%160s' '' | tr ' ' c)
    out=$(printf '%160s' '' | tr ' ' o)
    i=0
    while [ $i -lt 24 ]; do
      item='"id":"item_'$i'","type":"command_execution","command":"cmd'$i' '$pad'"'
      item="$item"',"aggregated_output":"\u001b[2J'$out'","exit_code":0,"status":"completed"'
      echo '{{"type":"item.completed","item":{{'"$item"'}}}}'
      i=$((i + 1))
    done
    item='"id":"item_99","type":"command_execution","command":"last-command","status":"in_progress"'
    echo '{{"type":"item.started","item":{{'"$item"'}}}}'
    cat <<'EVENTS' | tail -n 2
{CONTENT_FILTER_EVENTS}EVENTS
    exit 1 ;;
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
  refuse-resume|refuse-resume-slowly|refuse-resume-then-hold|refuse-resume-then-nothing)
    [ "$behaviour" = refuse-resume-slowly ] && sleep {slow}
    if [ -n "$resumed" ] && [ "$harness" = codex ]; then
      echo "Error: thread/resume: thread/resume failed: no rollout found for thread id $resumed" >&2
      exit 1
    fi
    if [ -n "$resumed" ]; then
      echo "No conversation found with session ID: $resumed" >&2
      refused='"subtype":"error_during_execution","is_error":true,"num_turns":0'
      echo '{{"type":"result",'"$refused"',"session_id":"'"$session"'"}}'
      [ "$behaviour" = refuse-resume-then-hold ] && hold
      exit 1
    fi
    [ "$behaviour" = refuse-resume-then-nothing ] || commit || exit 1 ;;
  hold-then-nothing) hold ;;
  orphan-stops)
    stops="$world/{HEADS_DIR}/$task-stops"
    # dash, Linux's sh, ignores `set -m` without a terminal, so perl makes the new group.
    ( perl -e 'setpgrp(0, 0); exec @ARGV' /bin/sh -c 'sleep 1; "$RIDER_BIN" down --now > "$1.new" 2>&1; echo "down exit $?" >> "$1.new"; mv "$1.new" "$1"' sh "$stops" & )
    hold
    commit || exit 1 ;;
  try-to-stop)
    stops="$world/{HEADS_DIR}/$task-stops"
    "$RIDER_BIN" kill "$task" > "$stops.new" 2>&1; echo "kill exit $?" >> "$stops.new"
    "$RIDER_BIN" down --now >> "$stops.new" 2>&1; echo "down exit $?" >> "$stops.new"
    mv "$stops.new" "$stops"
    hold
    commit || exit 1 ;;
  commit-replaying) commit || exit 1; cat "$world/{STREAMS_DIR}/$task"; exit 0 ;;
  commit-leaving-orphan)
    commit || exit 1; report
    sleep {leftover} & echo $! > "$world/{LEFTOVERS_DIR}/$task"; exit 0 ;;
  commit-leaving-escapee)
    commit || exit 1; report
    pidfile="$world/{LEFTOVERS_DIR}/$task"
    perl -e 'setpgrp(0, 0); open(my $f, ">", $ARGV[1]) or exit 1; print $f "$$\n";
      close($f); exec "sleep", $ARGV[0]' {leftover} "$pidfile" &
    step=0
    until [ -s "$pidfile" ]; do
      step=$((step + 1)); [ "$step" -le {steps} ] || exit 1; sleep 0.1
    done
    exit 0 ;;
  commit-flooding) commit || exit 1; report; yes | head -c {flood}; exit 0 ;;
  stop-rider-detached)
    stopped="$world/{ARGS_DIR}/$task-$attempt.stop"
    set -m
    ( '{rider}' down --now > "$stopped.out" 2>&1; echo "$?" > "$stopped.code" ) &
    set +m
    step=0
    while [ ! -s "$stopped.code" ]; do
      step=$((step + 1))
      [ "$step" -le {steps} ] || exit 1
      sleep 0.1
    done
    commit || exit 1 ;;
  plant-filters)
    escaped="$world/rider-home/agents"
    printf 'hi\n' > f.txt
    printf '* filter=a=b\n' > .gitattributes
    git init -q sub && printf 'hi\n' > sub/g.txt && printf '* filter=evil\n' > sub/.git/info/attributes \
      && git -C sub add g.txt && git -C sub commit -q -m Sub || exit 1
    git add f.txt .gitattributes sub 2>/dev/null && git commit -q -m Filtered || exit 1
    git config 'filter.a=b.clean' "sh -c ': > $escaped/{ESCAPE_EQUALS}; cat'" \
      && git -C sub config filter.evil.clean "sh -c ': > $escaped/{ESCAPE_NESTED}; cat'" || exit 1
    sleep 1
    printf 'hi\n' > f.txt
    printf 'hi\n' > sub/g.txt ;;
  plant-promisor)
    escaped="$world/rider-home/agents"
    commit || exit 1
    git config extensions.partialClone evil && git config remote.evil.promisor true \
      && git config remote.evil.url "$(pwd)" \
      && git config remote.evil.uploadpack \
        "sh -c ': > $escaped/{ESCAPE_PROMISOR}; git-upload-pack \"\$@\"' sh" || exit 1
    absent=$( {{
      printf 'tree 1234567890123456789012345678901234567890\nparent %s\n' "$(git rev-parse HEAD)"
      printf 'author t <t@e> 1 +0000\ncommitter t <t@e> 1 +0000\n\nx\n'
    }} | git hash-object -t commit -w --stdin --literally) && git update-ref HEAD "$absent" || exit 1 ;;
  plant-git-config)
    common=$(git rev-parse --path-format=absolute --git-common-dir)
    own=$(git rev-parse --absolute-git-dir)
    tried="$world/{ARGS_DIR}/$task-$attempt.git-config"
    for target in "$common/config" "$common/config.worktree" "$common/hooks/post-checkout" \
      "$common/info/attributes" "$common/commondir" "$own/config.worktree" "$own/commondir" \
      "$own/gitdir" "$(pwd)/.git"; do
      name=${{target#"$common"/}}
      name=${{name#"$(pwd)"/}}
      if ( : >> "$target" ) 2>/dev/null; then echo "$name written"; else echo "$name refused"; fi
    done > "$tried"
    git config core.sshCommand planted 2>/dev/null && echo "git config written" >> "$tried"
    commit || exit 1 ;;
  open-app)
    app="$world/{ARGS_DIR}/$task-$attempt.app"
    mkdir -p "$app/Contents/MacOS"
    {{
      printf '<?xml version="1.0" encoding="UTF-8"?>\n<plist version="1.0"><dict>\n'
      printf '<key>CFBundleExecutable</key><string>esc</string>\n'
      printf '<key>LSBackgroundOnly</key><true/></dict></plist>\n'
    }} > "$app/Contents/Info.plist"
    printf '#!/bin/sh\n: > "%s"\n' "$world/rider-home/agents/{ESCAPE_OPEN}" > "$app/Contents/MacOS/esc"
    chmod +x "$app/Contents/MacOS/esc"
    /usr/bin/open -g -j -n "$app" > "$app.out" 2>&1; echo "$?" > "$app.code"
    sleep 2
    commit || exit 1 ;;
  plant-fsmonitor)
    planted="$world/{ARGS_DIR}/$task-$attempt.fsmonitor"
    printf '#!/bin/sh\n: > "%s.ran"\n' "$planted" > "$planted"
    chmod +x "$planted"
    commit || exit 1
    git config core.fsmonitor "$planted" 2>/dev/null || : ;;
  signal-daemon-detached)
    signalled="$world/{ARGS_DIR}/$task-$attempt.signal"
    daemon=$(awk '{{print $2}}' "$world/rider-home/daemon.lock")
    set -m
    ( kill -TERM "$daemon" > "$signalled.out" 2>&1; echo "$?" > "$signalled.code" ) &
    set +m
    step=0
    while [ ! -s "$signalled.code" ]; do
      step=$((step + 1))
      [ "$step" -le {steps} ] || exit 1
      sleep 0.1
    done
    commit || exit 1 ;;
  try-writes)
    for name in {TRIED_WRITES}; do
      if ( : > "$world/rider-home/$name" ) 2>/dev/null; then
        rm -f "$world/rider-home/$name"; echo "$name written"
      else
        echo "$name refused"
      fi
    done > "$world/{ARGS_DIR}/$task-$attempt.writes"
    for name in {REPO_WRITES}; do
      if ( : >> "$world/repo/$name" ) 2>/dev/null; then echo "$name written"; else echo "$name refused"; fi
    done > "$world/{ARGS_DIR}/$task-$attempt.repo-writes"
    for name in {OTHER_TASK_WRITES} untrusted/$task/work/probe; do
      if ( : >> "$world/rider-home/$name" ) 2>/dev/null; then echo "$name written"; else echo "$name refused"; fi
    done > "$world/{ARGS_DIR}/$task-$attempt.other-writes"
    commit || exit 1 ;;
  stream-error)
    if [ "$harness" = codex ]; then
      echo '{{"type":"thread.started","thread_id":"00000000-0000-0000-0000-000000000000"}}'
      echo '{{"type":"item.completed","item":{{"type":"error","message":"a warning"}}}}'
      echo '{{"type":"item.completed","item":{{"type":"agent_message","text":"{said0}"}}}}'
      echo '{{"type":"item.completed","item":{{"type":"command_execution","command":"{command}","exit_code":1}}}}'
      echo '{{"type":"item.completed","item":{{"type":"agent_message","text":"{said1}"}}}}'
      echo '{{"type":"turn.failed","error":{{"message":"{error}"}}}}'
    else
      echo '{{"type":"system","subtype":"init","session_id":"'"$session"'","tools":["Bash"]}}'
      echo '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{said0}"}}]}}}}'
      echo '{{"type":"assistant","message":{{"content":[{{"type":"tool_use","name":"Bash"}}]}}}}'
      tool='{{"type":"tool_result","is_error":true,"content":"{tool_error}"}}'
      echo '{{"type":"user","message":{{"content":['"$tool"']}}}}'
      echo '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{said1}"}}]}}}}'
      echo '{{"type":"result","subtype":"success","is_error":true,"result":"{error}"}}'
    fi
    exit {fail} ;;
  stream-forged)
    forged='ok\u001b]0;x\u0007\u202e\nIts run ended in error: fake'
    printf '%s\n' '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"'"$forged"'"}}]}}}}'
    exit {fail} ;;
  stream-then-stderr)
    echo '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{said0}"}}]}}}}'
    echo "{stderr_error}" >&2
    exit {fail} ;;
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
    for dir in [
        BEHAVIOURS_DIR,
        RELEASES_DIR,
        ARGS_DIR,
        HEADS_DIR,
        STREAMS_DIR,
        LEFTOVERS_DIR,
    ] {
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

/// Makes a `CommitReplaying` agent of `task` print `text` as its output stream.
pub fn stream(world: &Path, task: &str, text: &[u8]) {
    assert!(!task.is_empty());
    let path = world.join(STREAMS_DIR).join(task);
    fs::write(&path, text).expect("write an agent's stream");
    assert!(path.is_file());
}

/// The pid of the process a `CommitLeavingOrphan` or `CommitLeavingEscapee` agent of `task`
/// left running.
pub fn leftover(world: &Path, task: &str) -> u32 {
    let path = world.join(LEFTOVERS_DIR).join(task);
    let text = fs::read_to_string(&path).expect("read a leftover's pid");
    let pid = text.trim().parse().expect("a leftover's pid");
    assert!(pid > 1);
    pid
}

/// What attempt `attempt` of `task`, started as `codex review`, read from its standard input.
pub fn stdin(world: &Path, task: &str, attempt: u32) -> String {
    let path = world.join(ARGS_DIR).join(format!("{task}-{attempt}.stdin"));
    fs::read_to_string(&path).expect("read an agent's standard input")
}

/// How many arguments attempt `attempt` of `task` was started with.
pub fn arg_count(world: &Path, task: &str, attempt: u32) -> usize {
    let path = world.join(ARGS_DIR).join(format!("{task}-{attempt}.argc"));
    let text = fs::read_to_string(&path).expect("read an agent's argument count");
    text.trim().parse().expect("an argument count")
}

/// The arguments attempt `attempt` of `task` was started with, one per line.
pub fn args(world: &Path, task: &str, attempt: u32) -> String {
    let path = world.join(ARGS_DIR).join(format!("{task}-{attempt}.all"));
    fs::read_to_string(&path).expect("read an agent's arguments")
}

pub fn detached_signal(world: &Path, task: &str, attempt: u32) -> (String, String) {
    let read = |end: &str| {
        let path = world
            .join(ARGS_DIR)
            .join(format!("{task}-{attempt}.signal.{end}"));
        fs::read_to_string(&path).expect("read what the detached signal recorded")
    };
    let (code, output) = (read("code"), read("out"));
    (code.trim().to_string(), output)
}

pub fn fsmonitor_ran(world: &Path, task: &str, attempt: u32) -> bool {
    let planted = world
        .join(ARGS_DIR)
        .join(format!("{task}-{attempt}.fsmonitor"));
    assert!(planted.exists(), "the agent planted its fsmonitor");
    planted.with_extension("fsmonitor.ran").exists()
}

pub fn escaped(world: &Path, marker: &str) -> bool {
    world.join("rider-home/agents").join(marker).exists()
}

pub fn opened(world: &Path, task: &str, attempt: u32) -> (String, String) {
    let read = |end: &str| {
        let path = world
            .join(ARGS_DIR)
            .join(format!("{task}-{attempt}.app.{end}"));
        fs::read_to_string(&path).expect("read what open recorded")
    };
    let (code, output) = (read("code"), read("out"));
    (code.trim().to_string(), output)
}

pub fn git_config_writes(world: &Path, task: &str, attempt: u32) -> String {
    let path = world
        .join(ARGS_DIR)
        .join(format!("{task}-{attempt}.git-config"));
    fs::read_to_string(&path).expect("read what the agent wrote to git's directory")
}

pub fn tried_other_task_writes(world: &Path, task: &str, attempt: u32) -> String {
    let path = world
        .join(ARGS_DIR)
        .join(format!("{task}-{attempt}.other-writes"));
    fs::read_to_string(&path).expect("read what the agent tried to write in other tasks' dirs")
}

pub fn tried_repo_writes(world: &Path, task: &str, attempt: u32) -> String {
    let path = world
        .join(ARGS_DIR)
        .join(format!("{task}-{attempt}.repo-writes"));
    fs::read_to_string(&path).expect("read what the agent tried to write in the repository")
}

pub fn tried_writes(world: &Path, task: &str, attempt: u32) -> String {
    let path = world
        .join(ARGS_DIR)
        .join(format!("{task}-{attempt}.writes"));
    fs::read_to_string(&path).expect("read what the agent tried to write")
}

/// The exit code and output of the `rider down --now` that attempt `attempt` of `task` started
/// in a detached child, once the child has recorded them.
pub fn detached_stop(world: &Path, task: &str, attempt: u32) -> (String, String) {
    let read = |end: &str| {
        let path = world
            .join(ARGS_DIR)
            .join(format!("{task}-{attempt}.stop.{end}"));
        fs::read_to_string(&path).expect("read what the detached stop recorded")
    };
    let (code, output) = (read("code"), read("out"));
    (code.trim().to_string(), output)
}

/// The names of the `GIT_*` variables attempt `attempt` of `task` was started with.
pub fn git_env(world: &Path, task: &str, attempt: u32) -> Vec<String> {
    let path = world
        .join(ARGS_DIR)
        .join(format!("{task}-{attempt}.git-env"));
    let text = fs::read_to_string(&path).expect("read an agent's git environment");
    text.lines().map(str::to_string).collect()
}

/// The names of every variable attempt `attempt` of `task` was started with.
pub fn env_names(world: &Path, task: &str, attempt: u32) -> Vec<String> {
    let path = world.join(ARGS_DIR).join(format!("{task}-{attempt}.env"));
    let text = fs::read_to_string(&path).expect("read an agent's environment");
    text.lines().map(str::to_string).collect()
}

/// The `GH_*` and `GITHUB_*` variables, as `NAME=value`, attempt `attempt` of `task` saw.
pub fn gh_env(world: &Path, task: &str, attempt: u32) -> Vec<String> {
    let path = world
        .join(ARGS_DIR)
        .join(format!("{task}-{attempt}.gh-env"));
    let text = fs::read_to_string(&path).expect("read an agent's GitHub environment");
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
