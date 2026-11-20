---
name: ship-swarm
description: Use when the user asks to ship the stack or a branch the swarm built, open a PR for it, get CI green, babysit CI, or address review comments on a PR, in a session that leads a Rider swarm.
---

# Ship a swarm's branch

Rider stops at reviewed local branches and never pushes. A project usually culminates in
one PR, or a stack of PRs: one PR per branch, each based on its parent branch (the lowest
on the base branch). This skill takes them to green pull requests with no open review
threads. **This session stays the lead** of `ride-swarm`. `<ride>` is the `ride-swarm`
skill's directory; read `<ride>/lead.md` for how to create tasks (the plan file, `onto`,
`role` set last) and follow it here.

## Rules

- Act only when the owner asks, and only on the branch or stack they name. The ask does not
  carry over to another branch or PR.
- Never merge (`gh pr merge` included) and never push to the base branch.
- A feature branch the swarm created may be rewritten, also after its PR is open: restacks
  and rebases are fine. Push a rewrite with `git push --force-with-lease origin <branch>`
  on that branch only. Never force-push the base branch or a branch the swarm did not
  create, and never use a bare `--force`.
- Git work with judgment in it is a board task for a worker: a restack, a rebase with
  conflicts, splitting or squashing commits, and every code change. Quick steps (pushing a
  finished branch, `gh pr create`) you run yourself. Rider has no built-in ship command.
- Never leave a review on a PR: no `gh pr review`, no review comments. Reviewer findings
  stay on the board. Replying to a human's review thread, and resolving it once its fix is
  pushed, is fine.
- Wait in the background, never poll in the foreground: run each watch as a background
  command and take a turn when it returns, so the owner can talk to you meanwhile.
- Posting on GitHub publishes. Say what you posted, with its link.

In the commands below, `<repo>` is the repository, `<branch>` the branch being shipped (its
task's `branch` label; the lowest first in a stack), `<top>` the id of the last task that
moved it, `<n>` its PR, `<base>` its parent branch.
`gh pr`, `gh run` and `gh issue` commands take `-R <owner>/<name>`, left out below to keep
them short. `gh api` has no `-R`: its path and GraphQL variables carry `<owner>` and `<name>`.

## 1. Open the PR

1. Check the branch is settled: no unfinished task is `onto` it or reviews it
   (`vb tasks work/<project> --state all`), and its reviews report nothing blocking.
2. `git -C <repo> push -u origin <branch>`. If the remote branch exists and the push is
   rejected as non-fast-forward, stop and tell the owner: it may not be the swarm's.
3. Fill the repository's template (`.github/pull_request_template.md`, or a file in
   `.github/PULL_REQUEST_TEMPLATE/`) and write it to a file outside the repository. Use the
   `writing-pr-descriptions` skill when it is installed. Name the issue it closes
   (one `Closes #<issue>` line per resolved issue) when there is one.
4. `gh pr create --base <base> --head <branch> --title "<title>" --body-file <file>`.
   Report the PR's URL. For a stack, open the lowest branch first, then each next branch
   with `--base` its parent branch, and say in each body which PR it stacks on.

The lead may open a PR as a draft (`gh pr create --draft`) before the work is finished,
so CI runs while reviews, polish and the security review go on. Push each later fix on the
branch to it. Mark it ready with `gh pr ready <n>` only when the stack's tasks are done and
CI is green.

## 2. Watch CI

Choose `<cap>`, the most minutes to wait for CI (the longest the workflows normally run,
doubled). Run as one background command a bounded poll: every 30 seconds read
`gh pr checks <n> --json name,bucket,link`, and stop when a check has bucket `fail`, or when
there are checks and none is `pending`, or when `<cap>` minutes have passed. No checks yet
(right after a push) counts as still waiting. The cap is tested on every pass, so a check
that stays queued or running cannot hold the poll open. When the poll ends, read the same
output once more:

- Green is `pass`, and `skipping` only for a check that is not required
  (`gh pr checks <n> --required --json name,bucket` lists the required ones).
- `fail`: step 3.
- `cancel`: rerun it once as in step 3, poll again, and report it to the owner if it is
  cancelled again.
- `pending` once `<cap>` is reached: a stuck check. Report it to the owner with its link.
  Do not wait longer or rerun it.

All green: step 4. The board wait from `lead.md` runs alongside it.

## 3. A failed check: flake or real

1. Find the run: its id is in the failed check's `link` (`.../actions/runs/<run>/...`), or
   `gh run list --branch <branch> --commit <sha> --json databaseId,workflowName,conclusion`
   with `<sha>` from `git -C <repo> rev-parse <branch>`.
2. Read the log: `gh run view <run> --log-failed`. Keep the lines that show the failure.
3. Decide:
   - **Flake**: the failure is not in the code: a runner lost, a network or registry
     timeout, a cache download, or a known-intermittent test the diff does not touch. It
     gets **one** rerun: `gh run rerun <run> --failed`, then watch again (step 2). The same
     check failing again is real.
   - **Real**: anything the branch's code causes or could cause: a compile error, a lint, a
     failing assertion, a formatting check.
4. A real failure becomes a fix task, created as `lead.md` describes:
   `role=implement` (or `light` when the log says exactly what to change, as a lint does),
   `onto=<top>`, `needs` every unfinished task on that branch, and its own `review` unless
   the fix is purely mechanical (read its diff). The brief carries the check's name, the failing log lines, and the local command that
   reproduces the check (from the workflow file under `.github/workflows/`).
5. When the fix and its review complete, read the result, then push: `git -C <repo> push
   origin <branch>`. `<top>` is now the fix task. Watch again (step 2).

If CI fails because the base branch moved, or a lower branch in the stack changed, make a
restack task (`role=light`, or `implement` when conflicts need judgment) as in `lead.md`,
once per branch from the bottom up. After each completes, push with
`git -C <repo> push --force-with-lease origin <branch>`, then watch CI again.

## 4. Review comments

1. List the threads, and keep those with `isResolved: false`. Also read
   `gh pr view <n> --comments` for review summaries and top-level comments. `--paginate`
   follows `pageInfo` through every page of threads. A thread with more than 20 comments
   (`hasNextPage` true) is an error: stop and tell the owner.

   ```sh
   gh api graphql --paginate -F owner=<owner> -F name=<name> -F n=<n> -f query='
     query($owner: String!, $name: String!, $n: Int!, $endCursor: String) {
       repository(owner: $owner, name: $name) { pullRequest(number: $n) {
         reviewThreads(first: 50, after: $endCursor) {
           pageInfo { hasNextPage endCursor }
           nodes { id isResolved path line comments(first: 20) {
             pageInfo { hasNextPage }
             nodes { databaseId author { login } body } } } } } } }'
   ```

2. For each thread, decide:
   - **Actionable** (a bug, a missing test, a requested change you agree with): a fix task
     as in step 3.4, its brief quoting the comment, the file and line, and the PR. Fix
     tasks for several threads on one branch run one after another: each `needs` the one
     before, since they share the branch.
   - **A question** the code already answers: reply with the answer, no task, and resolve
     the thread when the answer needs no change. Leave it open only when the owner must
     decide.
   - **A disagreement or a change of scope**: ask the owner. Do not reply on their behalf.
3. Push each fix as in step 3.5 and watch CI. Only after the fix is pushed **and** CI is
   green, reply in the thread naming the commit, then resolve it:

   ```sh
   gh api repos/<owner>/<name>/pulls/<n>/comments/<first comment databaseId>/replies \
     -f body='Fixed in <sha>: <one line>.'
   gh api graphql -f query='mutation { resolveReviewThread(input: {threadId: "<id>"}) {
     thread { isResolved } } }'
   ```
4. New comments arrive while you work: list the threads again after each push.

## 5. Close out

When every check is green and no thread is unresolved:

1. Find the linked issues: `gh pr view <n> --json closingIssuesReferences`, or the issues
   the owner named.
2. The `Closes #<issue>` lines close GitHub issues when the owner merges. Link a Linear issue
   through the Linear MCP. An issue only partly resolved stays open, with a comment saying
   what remains. Once the work ships (the owner has merged it, or told you to close), close
   any GitHub issue that is still open with the PR's link, what shipped in a few lines, and
   the command that verifies it:
   `gh issue close <issue> --comment "<text>"`, or with the text from a file:
   `gh issue close <issue> --comment "$(cat <file>)"`. Close a Linear issue with the Linear
   MCP tools when they are available (comment, then set its status to done); otherwise tell
   the owner it is open.
3. Mark each draft PR ready: `gh pr ready <n>`.
4. Remove the worktree of each task that is cancelled or given up on, as `lead.md` says:
   never with `--force`, and never delete the branch.
5. Tell the owner: each PR's link, that CI is green and no thread is open, the fix task ids,
   and the issues closed. Merging is theirs.

## Common mistakes

- Fixing a failure or a comment yourself as the lead. Make it a task.
- Watching CI in the foreground, or looping `gh pr checks` without `--watch`.
- Rerunning a check a second time, or calling a failure a flake because a rerun is cheaper.
- Replying "fixed" or resolving a thread before the fix is pushed and CI is green.
- Pushing a rewritten branch with a bare `--force` instead of `--force-with-lease`, or
  force-pushing a branch the swarm did not create.
- Leaving a review on a PR. Findings go on the board.
