# Recorded cases for coderabbit-steward tests

Raw GitHub API responses from real PRs, captured 2026-11-15. They move to `tests/fixtures/` in
the coderabbit-steward repo; `expected.json` holds what the parser and ledger must derive from
each case.

Per case: `pull.json` (PR), `comments.json` (issue comments), `reviews.json`, `inline.json`
(review comments), `check_runs.jsonl` (one CodeRabbit check run per line, per PR commit).

Refresh a case with the `grab` function in the plan's phase 0 notes; add a case for every new
message kind CodeRabbit sends, and for every parser bug.
