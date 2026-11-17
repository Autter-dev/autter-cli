# Run the tests Autter wrote for a PR on your machine

- When a review's generated tests find a bug, the failing test is never committed (it would turn CI red). `autter tests run` now brings it to you: on the PR's branch, it writes the failing tests Autter kept into your checkout and runs each one with its framework's command (vitest, jest, mocha, `node --test`, pytest, `go test`, `cargo test`, rspec, phpunit) from its project directory. Fix the bug and run it again to see it pass.
- `autter tests list` shows what Autter wrote for the PR: which tests failed, where and why, which passed, and what could not run in its sandbox. `autter tests pull` only writes the files, and `autter tests clean` removes exactly what `pull` or `run` wrote (a file you edited stays unless `--force`).
- The PR is the one open from your current branch, or `--pr <n>`. `--all` includes the passing tests and `--only <path>` picks one. If your checkout is not at the commit the tests were checked on, a note says so.
- These tests are model-written from the PR's code and run here with your environment, not in Autter's sandbox, so `run` asks before it runs anything (`--yes` skips the question; `--dry-run` prints what would run). The command is built by the CLI from the test's framework and path; the command string stored on the server is never executed. `pull` never overwrites a file it did not write.
- A test that ran against Autter's temporary database runs only with `--database-url <postgres-url>`, pointing at a scratch database it may write to. It never inherits your `DATABASE_URL`.
- Add `--json` for scripts and coding agents. The command uses your existing `autter login`; in local mode, or for repositories excluded from sharing, it sends nothing.

This command needs the updated Autter API.
