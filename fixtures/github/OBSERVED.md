# What `gh` was observed to print

Recorded with gh 2.x against public pull requests, then redacted: owners, repositories, logins,
bodies and ids replaced with `example/repo`, `reviewer`, `commenter` and short numbers.

- `gh pr list --head <branch> --state all --limit 1 --json
  number,url,headRefOid,state,statusCheckRollup,reviews,comments` prints an array of at most one
  pull request. `state` is `OPEN`, `MERGED` or `CLOSED`.
- `statusCheckRollup` holds one `CheckRun` per job: `status` is `IN_PROGRESS` (with
  `conclusion` `""`) until it is `COMPLETED`, then `conclusion` is `SUCCESS` or `FAILURE`.
  `detailsUrl` is the job's page. `pr-list-pending.json` and `pr-list-failed.json` are the same
  head before and after its `test` job failed.
- `reviews` entries have an `id`, `author.login`, `body` and `state` (`COMMENTED`, `APPROVED`,
  `CHANGES_REQUESTED`) but no URL; `comments` entries (PR conversation comments) have `url`.
- Review comments on a line are not in that JSON. `gh api
  repos/{owner}/{repo}/pulls/<n>/comments` prints them with `id` (a number), `user.login`,
  `path`, `line` (null when outdated) and `html_url`; `gh api` fills `{owner}/{repo}` from the
  repository in the current directory.
- `pulls-comments-three.json` is `pulls-comments.json` with its one comment repeated three times
  under new ids, lines and bodies, to pin the cap on fix rounds per head.
- `gh run view --job <id> --log-failed` prints the failed steps' log lines as
  `<job>\t<step>\t<timestamp> <text>`; the job id is the last segment of a check's
  `detailsUrl` after `/job/`. `run-log-failed.txt` is the last lines of a real failed `test`
  job, with its terminal colour codes removed.
