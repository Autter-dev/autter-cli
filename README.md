# autter <a href="https://github.com/orgs/Autter-dev/discussions"><img alt="Discussions" src="https://img.shields.io/github/discussions/autter-dev/autter?style=flat-square" /></a>

<img src="https://github.com/autter-dev/autter-cli/raw/main/assets/docs/autter.png" align="right"
     alt="Autter Logo" width="200" height="200">

**Autter is an open source git extension that records which lines of your code were written by AI.** Once installed, every AI-authored line is tied to the **agent, model, and prompt** that produced it — keeping the reasoning, requirements, and design decisions attached to your code instead of lost in a chat window.

There is nothing new to learn. **Write prompts and commit the way you already do.**

`git commit`

```
[hooks-doctor 0afe44b2] wsl compat check
2 files changed, 81 insertions(+), 3 deletions(-)

you  ██░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░ ai
     6%                                  94%
```

`autter blame /src/log_fmt/authorship_log.rs`

```bash
cb832b7 (Sagnik Ghosh      2025-12-13  133) pub fn execute_diff(
cb832b7 (Sagnik Ghosh      2025-12-13  134)     repo: &Repository,
cb832b7 (Sagnik Ghosh      2025-12-13  135)     spec: DiffSpec,
fe2c4c8 (Sagnik Ghosh      2025-12-02  136)     format: DiffFormat,
fe2c4c8 (Sagnik Ghosh      2025-12-02  137) ) -> Result<…> {
fe2c4c8 (claude              2025-12-02  138)     // Resolve commits
fe2c4c8 (claude              2025-12-02  139)     let (from, to) = match spec {
fe2c4c8 (claude              2025-12-02  140)         DiffSpec::TwoCommit(s, e) => {
fe2c4c8 (claude              2025-12-02  141)             let from = resolve(repo, &s)?;
```

## Install

**Mac, Linux, Windows (WSL)**

```bash
curl -sSL https://autter.dev/install.sh | bash
```

**Windows**

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -Command "irm https://autter.dev/install.ps1 | iex"
```

The installer is the only setup step. There are **no per-repo hooks to wire up and no git config to edit** — commit with the Agent, the git CLI, or any git client and attribution lands on the commit automatically.

Installation lets you choose **local-only** mode or **connect to the Autter platform**, and the choice is reversible at any time with `autter onboard`. Local-only attribution, blame, and stats run entirely on your machine — no code, prompts, or usage data ever leave it.

For verification steps, privacy controls, everyday commands, and troubleshooting, read the **[full installation and setup guide](INSTALL.md)**.

## Connect to the Autter platform (optional)

You never need an account to use Autter. Connecting simply links a machine to your Autter organization so attribution and prompt history can drive cross-repository dashboards, prompt search, team and contributor analytics, and audit history.

Start with the guided browser flow:

```bash
autter onboard
```

Pick "connect" and authorize the device in your browser. If that flow is unavailable, mint a **Personal Access Token (PAT)** under **Org Settings → Access Tokens** and register it directly:

```bash
autter login --token autter_pat_xxxxxxxx

autter whoami
```

Once a machine is connected:

- **Authorship notes** (the AI-vs-human breakdown per commit) and **prompt transcripts** are written on commit straight into **your organization's own database**. They are never shared between organizations, and the CLI writes to that database directly using the connection URL embedded in your signed token — no Autter server sits in the data path.
- A token is bound to your account. If you belong to more than one organization, each push is routed to whichever org owns the repository (resolved from its git remote), with your default org as the fallback.
- Tokens can be managed or revoked, and CLI activity (token created, sign-in, data pushed) reviewed, under **Settings → Access Tokens** in the dashboard.

Return to local-only mode whenever you like:

```bash
autter logout                 # clear stored credentials
autter onboard --local --force
```

### Configuration

`autter` reads `~/.autter/config.json`. The defaults target the hosted platform; override them per machine for self-hosting or CI:

| Field | Default | Purpose |
|-------|---------|---------|
| `api_base_url` | `https://api.autter.dev` | Auth + token exchange |
| `notes_backend.kind` | `git_notes` (local) / `http` (connected) | Where authorship notes go |
| `notes_backend.backend_url` | `https://cli.autter.dev` | Gate that enables cloud sync; the actual notes/prompt writes go straight to your org database (URL from your token), not to this host |
| `prompt_storage` | `local` / `default` (connected) | `default` uploads prompts, `local` keeps them on-device |
| `telemetry_oss` | `on` | `off` disables all anonymous usage analytics and error reporting |

Environment overrides: `AUTTER_API_BASE_URL`, `AUTTER_WEB_URL`, `AUTTER_NOTES_BACKEND_KIND`, `AUTTER_NOTES_BACKEND_URL`, `AUTTER_API_KEY` (for CI).

### Telemetry

Autter reports **anonymous** usage analytics and error data so the tool can improve. Telemetry is **on by default**, is confirmed during `autter onboard`, and is built to be safe and fully auditable:

- **Never any personal data** — no code, prompts, file paths, repo names, usernames, or IP addresses.
- **Only coarse, anonymous signals** — a random install ID, device info (OS, CPU architecture, core count), the Autter version, and error/exception messages.
- **A local mirror of everything sent** lives at `~/.autter/internal/telemetry.log` (one JSON object per event) so you can see exactly what left the machine.

Inspect or change it anytime:

```bash
autter telemetry status      # is it on/off + where the audit log lives
autter telemetry log         # show everything that has been sent (last 50)
autter telemetry log --all   # show the full audit log
autter telemetry off         # disable telemetry
autter telemetry on          # re-enable telemetry
```

You can also disable it by setting `"telemetry_oss": "off"` in `~/.autter/config.json`, or re-run the prompt with `autter onboard --force` (non-interactive installs accept `--telemetry` / `--no-telemetry`).

**The [Autter standard](https://github.com/autter-dev/autter-cli/blob/main/specs/autter_standard_v3.0.0.md) is supported by:**

![Claude Code](https://img.shields.io/badge/Claude_Code-555?style=for-the-badge)
![Codex](https://img.shields.io/badge/Codex-555?style=for-the-badge)
![Cursor](https://img.shields.io/badge/Cursor-555?style=for-the-badge)
![GitHub Copilot](https://img.shields.io/badge/GitHub_Copilot-555?style=for-the-badge)
![OpenCode](https://img.shields.io/badge/OpenCode-555?style=for-the-badge)
![Pi](https://img.shields.io/badge/Pi-555?style=for-the-badge)
![Windsurf](https://img.shields.io/badge/Windsurf-555?style=for-the-badge)
![Droid](https://img.shields.io/badge/Droid-555?style=for-the-badge)
![Amp](https://img.shields.io/badge/Amp-555?style=for-the-badge)
![Gemini](https://img.shields.io/badge/Gemini-555?style=for-the-badge)
![Continue](https://img.shields.io/badge/Continue-555?style=for-the-badge)
![Junie](https://img.shields.io/badge/Junie-555?style=for-the-badge)
![Rovo Dev](https://img.shields.io/badge/Rovo_Dev-555?style=for-the-badge)
![Firebender](https://img.shields.io/badge/Firebender-555?style=for-the-badge)
[![+ Add an Agent](https://img.shields.io/badge/+_Add_an_Agent-1f6feb?style=for-the-badge)](https://autter.dev/docs/cli/add-your-agent)

## Why Autter

- 🪄 **Zero workflow change** — prompt and commit exactly as you do today; attribution metadata is attached to every commit for you.
- ⚡ **No overhead** — Autter avoids Git hooks (slow and painful to set up per repo) and never wraps the git binary, so your git commands run at full speed.
- 💻 **Local-first** — works offline and needs no login.
- 🔒 **Safe prompt storage** — each AI line traces back to the prompt that produced it. Sessions are scanned, redacted, and stored outside git, keeping repos lean, enabling access control, and stopping PII or secrets from leaking.
- 🌐 **Git-native and open** — Autter authored the [open standard](https://github.com/autter-dev/autter-cli/blob/main/specs/autter_standard_v3.0.0.md) for tracking AI-generated code in Git Notes.

Want a closer look?

<a href="https://cal.com/sagnik-autter/15min" target="_blank"><img src="assets/docs/buttons/meet-the-maintainers.svg" alt="Meet the maintainers" height="40" /></a>

### Documentation

- [CI Actions](https://autter.dev/docs/guides/ci-workflows) — preserve attribution through Rebase and Merge and Squash and Merge.
- [How Autter Works](https://autter.dev/docs/cli/how-autter-works)
- [Stats command](https://autter.dev/docs/cli/commit-stats) — aggregate % AI stats across commits.
- [AI Blame](https://autter.dev/docs/cli/ai-blame)
- [Config](https://autter.dev/docs/cli/configuration)
- [Add support for an agent in Autter](https://autter.dev/docs/guides/add-your-agent)
- Install Autter in background agents: [Claude Web](https://autter.dev/docs/cli/claude-web), [Codex Cloud](https://autter.dev/docs/cli/codex-cloud), [Cursor Agent](https://autter.dev/docs/cli/cursor-agent), and [Devin](https://autter.dev/docs/cli/devin).

## Attribution Stats

Line-level attribution lets you follow AI code across the whole SDLC. Measure how much AI code is accepted, committed, survives review, and reaches production — so you can tell which tools and practices actually work.

```bash
autter stats --json
autter stats <start_sha>..<end_sha> --json
```

It computes % AI code, AI lines generated vs committed, acceptance rates, and human overrides, broken down by tool and model. More in the [stats command reference](https://autter.dev/docs/cli/reference#stats).

<details>
<summary>Example JSON output</summary>

```json
{
  "human_additions": 28,
  "ai_additions": 76,
  "ai_accepted": 47,
  "git_diff_deleted_lines": 34,
  "git_diff_added_lines": 104,
  "tool_model_breakdown": {
    "claude_code/claude-sonnet-4-5-20250929": {
      "ai_additions": 76,
      "ai_accepted": 47
    }
  }
}
```

</details>

## AI Blame

`autter blame` is a drop-in replacement for `git blame` that adds AI attribution to every line. It accepts [all standard `git blame` flags](https://git-scm.com/docs/git-blame).

```bash
autter blame /src/log_fmt/authorship_log.rs
```

```bash
cb832b7 (Sagnik Ghosh 2025-12-13 08:16:29 -0500  136)     format: DiffFormat,
cb832b7 (Sagnik Ghosh 2025-12-13 08:16:29 -0500  137) ) -> Result<String, AutterError> {
fe2c4c8 (claude         2025-12-02 19:25:13 -0500  138)     // Resolve commits to get from/to SHAs
fe2c4c8 (claude         2025-12-02 19:25:13 -0500  139)     let (from_commit, to_commit) = match spec {
fe2c4c8 (claude         2025-12-02 19:25:13 -0500  140)         DiffSpec::TwoCommit(start, end) => {

```

<img align="right" width="350" alt="Autter VS Code extension showing color-coded AI blame in the gutter" src="https://github.com/user-attachments/assets/94e332e7-5d96-4e5c-8757-63ac0e2f88e0" />

Community plugins surface this attribution directly in popular IDEs, color-coded by agent session. Hover a line to read the underlying prompt or its summary.

**Supported Editors**

- [VS Code](https://marketplace.visualstudio.com/items?itemName=autter.autter-vscode)
- [Cursor](https://marketplace.visualstudio.com/items?itemName=autter.autter-vscode)
- [Windsurf](https://marketplace.visualstudio.com/items?itemName=autter.autter-vscode)
- [Antigravity](https://marketplace.visualstudio.com/items?itemName=autter.autter-vscode)
- [Emacs magit](https://github.com/jwiegley/maautter)
- *Added support for another editor? [Open a PR](https://github.com/autter-dev/autter-cli/pulls)*

<br clear="all" />

## For teams and enterprises

<a href="https://cal.com/sagnik-autter/15min" target="_blank"><img src="assets/docs/buttons/get-early-access.svg" alt="Get early access" height="40" /></a>

**Autter for Teams** rolls Autter out across your whole organization. Connect GitHub, GitLab, Bitbucket, or Azure DevOps to get aggregate insight across every repository, plus the full trace of each agent session — from the first prompt to production.

- **Provenance** — line-level AI vs. human authorship across every repo, with the share of AI-written lines rolled up by pull request, repo, team, and contributor.
- **Analytics** — measure **% AI** and token cost, and track agent autonomy, token efficiency, and how much rework AI code needs before and after deploy.
- **Lifecycle** — follow AI code through your release pipelines and environments, see how much actually reaches production, and trace incidents back to the AI session that caused them.
- **Codebase scans** — agent-driven security, dependency, secret-detection, and code-quality scans across your codebase.
- **PR review & automation** — automatic AI pull-request reviews, generated PR descriptions, and after-review fix agents you can tune or build yourself.
- **Repo wikis** — auto-generated, always-current documentation for every repository.
- **Prompt store** — keep the prompt and full session behind every generated hunk for harness engineering and review.

**Autter for Enterprise** runs entirely in your own cloud. Deploy into a customer-owned AWS account from versioned container images and Terraform modules — your VPC, data stores, object storage, KMS keys, and IAM roles — with audit logs and scoped access tokens. Nothing leaves your boundary.

## FAQs

#### How does it work?

1. Coding agents call `autter checkpoint` whenever they write code or change files via bash scripts.
1. On commit, Autter records line-level attribution in Git Notes, tying every AI line to the agent, model, and session behind it. Run `git log --show-notes="ai"` to view it.
1. When you `squash`, `merge`, `reset`, `rebase`, `stash`, or `cherry-pick`, Autter moves and merges those attributions so tracking stays accurate.

*Autter never uses AI or heuristics to "guess" which code is AI — the agents report exactly which lines they wrote, giving the most accurate, explicit attribution possible.*

#### Does the agent have to commit for attribution to work?
No. Autter works no matter how a commit is created — your git client, the git CLI, and your own git aliases are all supported.

#### Notes attach to commits — how do attributions survive a rebase, squash, stash, or cherry-pick?
Autter inspects the final state of the code once the operation finishes and copies or merges attributions into a Git Note for every resulting commit. It is eventually consistent: the note is written 5–100 ms after the operation completes.

#### Can I use this solo?
Yes. Autter is free and open source, runs locally, and needs no login or team setup.

#### Is there a performance impact?
No. Autter uses no Git hooks and does not wrap git, so your git commands carry no added overhead.

#### Do I need to set up agent hooks?
No — Autter manages the agent hooks and checks them daily. To trigger that yourself (for example after installing a new agent), run `autter install-hooks`.

#### Who uses this?
Hundreds of engineering teams, including many in the Fortune 100, use Autter to understand their AI usage and make agents more effective in their codebases.

#### What's the difference between the open source CLI and the [teams version](https://autter.dev)?
The CLI accurately attributes AI code on every commit. The teams version adds a secure prompt store and joins data from across the SDLC — tying token spend to individual pull requests, computing % AI by PR, team, and repo, surfacing signals like rework during review, and tracing incidents back to the AI session that caused them. It also layers on agent-driven workflows: codebase security scans, automatic PR reviews and descriptions, after-review fix agents, and auto-generated repo wikis. Connect your SCM for aggregate stats across thousands of repos plus full observability into everything your agents do. Run it in our cloud, or deploy Autter for Enterprise entirely inside your own AWS account so nothing leaves your boundary.

#### What's supported, and what isn't?
Autter provides line-level attribution for AI-generated code whether it was written through an edit tool or a bash command. During a history rewrite (`rebase`, `stash`, `squash --merge`, and so on) Autter moves and merges attributions so nothing is lost.

Here is the full breakdown of what is supported today:

| Capability                                                      | Status | Notes                                                                        |
| --------------------------------------------------------------- | ------ | ---------------------------------------------------------------------------- |
| Edit / Write / Patch tools                                      | ✅      | Line-level attribution recorded automatically.                               |
| Files created via Bash                                          | ✅      | May not work if the agent is not operating from the repository root.         |
| Git worktrees                                                   | ✅      | Attribution maintained across linked worktrees.                              |
| Background Agents                                               | ✅      | See docs for [Claude Web](https://autter.dev/docs/cli/claude-web), [Codex Cloud](https://autter.dev/docs/cli/codex-cloud), [Cursor Agent](https://autter.dev/docs/cli/cursor-agent), and [Devin](https://autter.dev/docs/cli/devin). |
| Attribute lines from multiple Agent Sessions in the same commit | ✅      |                                                                              |
| Record which lines a human overrode                             | ✅      |                                                                              |
| Attribute sessions that produced no code                        | ✅      | Records token usage and session activity even when no code is accepted.      |
| Accepted rate per session                                       | ✅      |                                                                              |
| Added and deleted lines per session                             | ✅      |                                                                              |
| Tool-call level attribution                                     | ✅      | Resolves attributed lines to the tool call that generated them.              |
| Tokens and cost per commit and PR                               | ✅      | Aggregates token usage and cost across the sessions behind each commit/PR.   |
| Formatters                                                      | ✅      | Formatting will not change attribution to human.                             |
| Multi-repo root                                                 | ⚠️     | If you run an agent that edits multiple repos, Bash attributions only work when the agent runs each command with its cwd inside that repo. |

Git rewrite operations:

| Operation                                                       | Status | Notes                                                                        |
| --------------------------------------------------------------- | ------ | ---------------------------------------------------------------------------- |
| `git rebase`                                                    | ✅      | Attribution preserved. [View Code](https://github.com/autter-dev/autter-cli/blob/f3da782e93c492303e44d14805179123d1740e7f/src/daemon.rs#L6578-L6664) |
| `git cherry-pick`                                               | ✅      | Attribution preserved. [View Code](https://github.com/autter-dev/autter-cli/blob/f3da782e93c492303e44d14805179123d1740e7f/src/daemon.rs#L6675-L6718) |
| `git stash` / `git stash pop`                                  | ✅      | Attribution preserved. [View Code](https://github.com/autter-dev/autter-cli/blob/f3da782e93c492303e44d14805179123d1740e7f/src/daemon.rs#L6758-L6824) |
| `git merge --squash`                                            | ✅      | Attribution preserved. [View Code](https://github.com/autter-dev/autter-cli/blob/f3da782e93c492303e44d14805179123d1740e7f/src/daemon.rs#L6729-L6757) |
| `git reset --soft`                                              | ✅      | Attribution preserved. [View Code](https://github.com/autter-dev/autter-cli/blob/f3da782e93c492303e44d14805179123d1740e7f/src/daemon.rs#L6504-L6577) |
| `git reset --mixed`                                            | ✅      | Attribution preserved. [View Code](https://github.com/autter-dev/autter-cli/blob/f3da782e93c492303e44d14805179123d1740e7f/src/daemon.rs#L6504-L6577) |
| `git reset --hard`                                              | ✅      | Attribution preserved for commits that remain in history. [View Code](https://github.com/autter-dev/autter-cli/blob/f3da782e93c492303e44d14805179123d1740e7f/src/daemon.rs#L6504-L6577) |
| `git merge` (merge commit)                                      | ✅      | Attribution preserved. [View Code](https://github.com/autter-dev/autter-cli/blob/f3da782e93c492303e44d14805179123d1740e7f/src/daemon.rs#L6475-L6485) |
| `git commit --amend`                                            | ✅      | Attribution preserved, including unstaged and partially staged changes. [View Code](https://github.com/autter-dev/autter-cli/blob/f3da782e93c492303e44d14805179123d1740e7f/src/daemon.rs#L6486-L6503) |
| `git checkout` / `git switch` (branches)                       | ✅      | Attribution follows the working tree across branch changes. [View Code](https://github.com/autter-dev/autter-cli/blob/f3da782e93c492303e44d14805179123d1740e7f/src/daemon.rs#L966-L978) |
| `git pull` (fast-forward / `--rebase`)                          | ✅      | Attribution preserved, including autostashed changes. [View Code](https://github.com/autter-dev/autter-cli/blob/f3da782e93c492303e44d14805179123d1740e7f/src/daemon.rs#L6825-L6874) |
| `git push` / `git fetch`                                       | ✅      | Attribution notes synced to/from the remote. [View Code](https://github.com/autter-dev/autter-cli/blob/f3da782e93c492303e44d14805179123d1740e7f/src/commands/hooks/push_hooks.rs#L7-L30) |
| `git mv`                                                        | ❌      | Renames are not yet tracked; attribution does not follow the moved file.     |
| `git filter-branch` / `git filter-repo`                        | ❌      | Bulk history rewrites are not tracked.                                        |
| `git replace`                                                  | ❌      | Object replacements are not tracked.                                         |

GitHub, GitLab, Bitbucket, Azure DevOps:

| Capability                                                      | Status | Notes                                                                        |
| --------------------------------------------------------------- | ------ | ---------------------------------------------------------------------------- |
| Squash and Merge                                                | ✅      | Requires [Autter for Teams](https://cal.com/sagnik-autter/15min) or [Open Source CI Actions](https://autter.dev/docs/guides/ci-workflows) to preserve attribution. |
| Rebase and Merge                                                | ✅      | Requires [Autter for Teams](https://cal.com/sagnik-autter/15min) or [Open Source CI Actions](https://autter.dev/docs/guides/ci-workflows) to preserve attribution. |

## Acknowledgments

Autter builds on the foundation laid by the original authors of **[git-ai](https://github.com/git-ai-project/git-ai)**, who first showed that AI-code attribution could live natively inside a repository rather than in an external service. Their work is the starting point everything here grew from, and we're grateful for it.

The key idea we carried forward and built on is **Git Notes**. Git Notes let you attach metadata to a commit *after* it already exists, without rewriting the commit or touching its tree — so attribution can be recorded, moved, and merged independently of the code itself. Autter stores its line-level authorship data in a dedicated `refs/notes/ai` namespace, which is what makes attribution survive `rebase`, `squash`, `stash`, `cherry-pick`, and the other history rewrites, and what lets the same data sync across machines and the platform. We took that primitive and extended it into the full [Autter standard](https://github.com/autter-dev/autter-cli/blob/main/specs/autter_standard_v3.0.0.md) for tracking AI-generated code.

## License
Apache 2.0
