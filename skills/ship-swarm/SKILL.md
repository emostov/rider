---
name: ship-swarm
description: Use when the user asks to ship the stack or a branch the swarm built, open a PR for it, get CI green, babysit CI, or address review comments on a PR, in a session that leads a Rider swarm.
---

# Ship a swarm's branch

Rider stops at reviewed local branches and never pushes. This skill takes one of them to a
green pull request with no open review threads. **This session stays the lead** of
`ride-swarm`: you run the `gh` and `git` steps yourself, and every code change is a board
task done by a worker. `<ride>` is the `ride-swarm` skill's directory; read `<ride>/lead.md`
for how to create tasks (the plan file, `onto`, `role` set last) and follow it here.

## Rules

- Act only when the owner asks, and only on the branch or stack they name. The ask does not
  carry over to another branch or PR.
- Never force-push, never merge (`gh pr merge` included), never push to the base branch.
- Once the branch has an open PR, fix forward only: new commits on top. Never a restack or
  a rebase, since pushing it would need a force-push. A stack ships as one PR from its top
  branch, so every fix goes `onto` the top branch's task, never onto a lower branch.
- Wait in the background, never poll in the foreground: run each watch as a background
  command and take a turn when it returns, so the owner can talk to you meanwhile.
- Posting on GitHub publishes. Say what you posted, with its link.

In the commands below, `<repo>` is the repository, `<branch>` the top branch (the `branch`
label of the stack's top task), `<top>` the id of the last task that moved it, `<n>` the PR.
Every `gh` command takes `-R <owner>/<name>`, left out below to keep them short.

## 1. Open the PR

1. Check the branch is settled: no unfinished task is `onto` it or reviews it
   (`vb tasks work/<project> --state all`), and its reviews report nothing blocking.
2. `git -C <repo> push -u origin <branch>`. If the remote branch exists and the push is
   rejected as non-fast-forward, stop and tell the owner. Never add `--force`.
3. Fill the repository's template (`.github/pull_request_template.md`, or a file in
   `.github/PULL_REQUEST_TEMPLATE/`) and write it to a file outside the repository. Use the
   `writing-pr-descriptions` skill when it is installed. Name the issue it closes
   (`Closes #<issue>`) when there is one.
4. `gh pr create --base <base> --head <branch> --title "<title>" --body-file <file>`.
   Report the PR's URL.

## 2. Watch CI

Run as a background command: `gh pr checks <n> --watch --interval 30`. It exits 0 when
every check passed and non-zero when one failed. Right after a push it may report no checks
yet: run it again in the background. When it returns, list the result with
`gh pr checks <n> --json name,bucket,link`.

All passed: step 4. Any failed: step 3. The board wait from `lead.md` runs alongside it.

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
   `onto=<top>`, `needs` every unfinished task on that branch, and its own `review`. The
   brief carries the check's name, the failing log lines, the local command that reproduces
   the check (from the workflow file under `.github/workflows/`), and: "Fix forward with a
   new commit on this branch. Do not rebase or amend."
5. When the fix and its review complete, read the result, then push: `git -C <repo> push
   origin <branch>` (a fast-forward; never `--force`). `<top>` is now the fix task. Watch
   again (step 2).

If CI fails because the base branch moved (a conflict, or a change on the base breaking
the branch), do not merge or rebase: tell the owner and ask.

## 4. Review comments

1. List the threads, and keep those with `isResolved: false`. Also read
   `gh pr view <n> --comments` for review summaries and top-level comments.

   ```sh
   gh api graphql -f query='query { repository(owner: "<owner>", name: "<name>") {
     pullRequest(number: <n>) { reviewThreads(first: 100) { nodes { id isResolved path line
     comments(first: 20) { nodes { databaseId author { login } body } } } } } } }'
   ```

2. For each thread, decide:
   - **Actionable** (a bug, a missing test, a requested change you agree with): a fix task
     as in step 3.4, its brief quoting the comment, the file and line, and the PR. Fix
     tasks for several threads on one branch run one after another: each `needs` the one
     before, since they share the branch.
   - **A question** the code already answers: reply with the answer, no task.
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

1. Find the linked issue: `gh pr view <n> --json closingIssuesReferences`, or the issue the
   owner named.
2. Comment on it: `gh issue comment <issue> --body-file <file>`, with the PR's link, what
   shipped in a few lines, and the command that verifies it. Do not close the issue:
   merging the PR does.
3. Tell the owner: the PR's link, that CI is green and no thread is open, and the fix task
   ids. Merging is theirs.

## Common mistakes

- Fixing a failure or a comment yourself as the lead. Make it a task.
- Watching CI in the foreground, or looping `gh pr checks` without `--watch`.
- Rerunning a check a second time, or calling a failure a flake because a rerun is cheaper.
- Replying "fixed" or resolving a thread before the fix is pushed and CI is green.
- A restack, rebase, or amend on a branch with an open PR. Those need a force-push.
