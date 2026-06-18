# autter   <a href="https://github.com/orgs/Autter-dev/discussions"><img alt="Discussions" src="https://img.shields.io/github/discussions/autter-dev/autter?style=flat-square" /></a>        

<img src="https://github.com/autter-dev/autter-cli/raw/main/assets/docs/autter.png" align="right"
     alt="Autter Logo" width="200" height="200">

Autter is an open source git extension that tracks the AI-generated code in your repositories. After installing the extension, every line of AI code is linked to the **agent, model, and prompts** that generated it — so you never lose the intent, requirements, and architecture decisions behind your code.

**Just prompt and commit** — no workflow changes:

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

**No per-repo setup or git hooks required.** Commit with the Agent, git, or your favorite git client. Attribution will be linked to commits automatically.

During install you'll be asked whether to run **local-only** or **connect to the Autter platform**. Login is optional: the open source CLI's attribution, blame, and stats features work locally without uploading code, prompts, or agent usage data. You can change modes any time with `autter onboard`.

See the **[complete installation and setup guide](INSTALL.md)** for verification, local-only privacy controls, platform setup, everyday commands, and troubleshooting.

## Connect to the Autter platform (optional)

Local-only mode works without an account. Connecting links this machine to your Autter organization so attribution and prompt history can power cross-repository dashboards, prompt search, team/user analytics, and audit history.

The recommended login is the guided browser flow:

```bash
autter onboard
```

Choose to connect and authorize the device in your browser. If onboarding does not work, create a **Personal Access Token (PAT)** under **Org Settings → Access Tokens**, then add it with:

```bash
autter login --token autter_pat_xxxxxxxx

autter whoami
```

Once connected:

- **Authorship notes** (which lines are AI vs human, per commit) and **prompt transcripts** are written on commit straight to **your organization's own database** — never shared across orgs. The CLI connects to that database directly using the connection URL carried in your signed access token; there is no intermediate Autter server in the data path.
- A token is scoped to your account; if you belong to multiple organizations, each push is automatically routed to the org that owns the repository (resolved from its git remote), falling back to your default org.
- Manage and revoke tokens, and view CLI activity (token created, sign-in, data pushed), under **Settings → Access Tokens** in the dashboard.

To go back to local-only at any time:

```bash
autter logout                 # clear stored credentials
autter onboard --local --force
```

### Configuration

`autter` reads `~/.autter/config.json`. Defaults point at the hosted platform; override per machine (e.g. for self-hosting or CI):

| Field | Default | Purpose |
|-------|---------|---------|
| `api_base_url` | `https://api.autter.dev` | Auth + token exchange |
| `notes_backend.kind` | `git_notes` (local) / `http` (connected) | Where authorship notes go |
| `notes_backend.backend_url` | `https://cli.autter.dev` | Gate that enables cloud sync; the actual notes/prompt writes go straight to your org database (URL from your token), not to this host |
| `prompt_storage` | `local` / `default` (connected) | `default` uploads prompts, `local` keeps them on-device |

Env overrides: `AUTTER_API_BASE_URL`, `AUTTER_WEB_URL`, `AUTTER_NOTES_BACKEND_KIND`, `AUTTER_NOTES_BACKEND_URL`, `AUTTER_API_KEY` (for CI).

**The [Autter standard](https://github.com/autter-dev/autter-cli/blob/main/specs/autter_standard_v3.0.0.md) is supported by:**
<table>
<tr>
<td align="center" width="20%"><img src="assets/docs/agents/gray/claude_code.png" alt="Claude Code" width="160" /></td>
<td align="center" width="20%"><img src="assets/docs/agents/gray/codex-black.png" alt="Codex" width="160" /></td>
<td align="center" width="20%"><img src="assets/docs/agents/gray/cursor.png" alt="Cursor" width="160" /></td>
<td align="center" width="20%"><img src="assets/docs/agents/gray/copilot.png" alt="GitHub Copilot" width="160" /></td>
<td align="center" width="20%"><img src="assets/docs/agents/gray/opencode.png" alt="OpenCode" width="160" /></td>
</tr>
<tr>
<td align="center"><img src="assets/docs/agents/gray/pi.png" alt="Pi" width="160" /></td>
<td align="center"><img src="assets/docs/agents/gray/windsurf.png" alt="Windsurf" width="160" /></td>
<td align="center"><img src="assets/docs/agents/gray/droid.png" alt="Droid" width="160" /></td>
<td align="center"><img src="assets/docs/agents/gray/amp.png" alt="Amp" width="160" /></td>
<td align="center"><img src="assets/docs/agents/gray/gemini.png" alt="Gemini" width="160" /></td>
</tr>
<tr>
<td align="center"><img src="assets/docs/agents/gray/continue.png" alt="Continue" width="160" /></td>
<td align="center"><img src="assets/docs/agents/gray/junie_white.png" alt="Junie" width="160" /></td>
<td align="center"><img src="assets/docs/agents/gray/rovodev.png" alt="Rovo Dev" width="160" /></td>
<td align="center"><img src="assets/docs/agents/gray/firebender.png" alt="Firebender" width="160" /></td>
<td align="center"><a href="https://autter.dev/docs/cli/add-your-agent">+ Add an Agent</a></td>
</tr>
</table>


## Our Choices

- 🪄 **Transparent** — Autter requires no workflow changes. Just prompt and commit as you normally would and Autter automatically attaches attribution metadata to every commit. 
- ⚡ **No performance overhead** — Autter does not rely on Git Hooks (slow + difficult to set up in every repo) and it does not wrap the Git binary. Your Git operations are just as fast as they were before Autter was installed. 
- 💻 **Local-first** — Works offline, no login required.
- 🔒 **Secure Prompt Storage** — Autter links each line of AI-code back to the prompt that generated it. These sessions are scanned and redacted, and saved outside of Git -- keeping repos lean, enabling fine-grained access control, and preventing PII or secrets from leaking. 
- 🌐 **Git native and open standard** — Autter built the [open standard](https://github.com/autter-dev/autter-cli/blob/main/specs/autter_standard_v3.0.0.md) for tracking AI-generated code with Git Notes.

Want to learn more? 

<a href="https://calendly.com/d/cxjh-z79-ktm/meeting-with-autter-authors" target="_blank"><img src="assets/docs/buttons/meet-the-maintainers.svg" alt="Meet the maintainers" height="40" /></a>

### Documentation  

- [CI Actions](https://autter.dev/docs/guides/ci-workflows) preserves attribution through Rebase and Merge & Square and Merge.
- [How Autter Works](https://autter.dev/docs/cli/how-autter-works) 
- [Stats command](https://autter.dev/docs/cli/commit-stats) - aggregate % AI stats across commits
- [AI Blame](https://autter.dev/docs/cli/ai-blame) - 
- [Config](https://autter.dev/docs/cli/configuration) - 
- [Add support for an agent in Autter](https://autter.dev/docs/guides/add-your-agent)
- Install Autter in Background Agents: [Claude Web](https://autter.dev/docs/cli/claude-web), [Codex Cloud](https://autter.dev/docs/cli/codex-cloud), [Cursor Agent](https://autter.dev/docs/cli/cursor-agent), and [Devin](https://autter.dev/docs/cli/devin).

## Attribution Stats

Line-level AI-attribution let you track AI-code through the full SDLC. Track how much AI code gets accepted, committed, through code review, and into production — to identify which tools and practices work best.

```bash
autter stats --json
autter stats <start_sha>..<end_sha> --json
```

Calculates % AI-code, AI-lines generated vs committed, accepted rates, human overrides broken down by tool and model. Learn more: [Stats command reference docs](https://autter.dev/docs/cli/reference#stats). 


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

Autter blame is a drop-in replacement for `git blame` that shows AI attribution for each line. It supports [all standard `git blame` flags](https://git-scm.com/docs/git-blame).

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

There are community plugins that display AI-attribution in popular IDEs, color-coded by agent session. Hover over a line to see the raw prompt or summary.

**Supported Editors**

- [VS Code](https://marketplace.visualstudio.com/items?itemName=autter.autter-vscode)
- [Cursor](https://marketplace.visualstudio.com/items?itemName=autter.autter-vscode)
- [Windsurf](https://marketplace.visualstudio.com/items?itemName=autter.autter-vscode)
- [Antigravity](https://marketplace.visualstudio.com/items?itemName=autter.autter-vscode)
- [Emacs magit](https://github.com/jwiegley/maautter)
- *Built support for another editor? [Open a PR](https://github.com/autter-dev/autter-cli/pulls)*

<br clear="all" />

## For teams and enterprises

<a href="https://calendly.com/d/cxjh-z79-ktm/meeting-with-autter-authors" target="_blank"><img src="assets/docs/buttons/get-early-access.svg" alt="Get early access" height="40" /></a>

We built Autter for Teams to make it easy to roll out Autter across your organization. Just connect GitHub, GitLab, Bitbucket, or Azure DevOps and get aggregate insights across all your repositories, plus the full trace of every agent session—from prompt all the way to production.

- See how much AI-code makes it all the way to production
- Measure **% AI** and token cost by Pull Request, Repo, Team, and Contributor
- Measure and improve agent autonomy and token efficiency
- Measure AI-code durability and how much rework AI-code requires before and after deployment
- Tie incidents back to AI-sessions
- Save prompts behind every generated hunk of code for harness engineering and code review

<sub><i>▶ Watch the 2-minute demo</i></sub>

https://github.com/user-attachments/assets/9c0d56a0-d6f6-4189-8d94-32155af33321

## FAQs

#### How does it work?

1. Coding Agents call `autter checkpoint` whenever they write code or modify files with bash scripts. 
1. On commit, Autter stores line-level attribution data in Git Notes, linking each line of AI-generated code to the agent, model, and session that created it. Run `git log --show-notes="ai"` to see them. 
1. Autter moves and merges line-level attributions when you `squash`, `merge`, `reset`, `rebase`, `stash`, `cherry-pick`, etc. so your AI code is always accurately tracked.

*Autter does not use AI or heuristics to "detect" AI code — the Agents report exactly which lines they wrote, providing the most accurate, explicit attribution possible.*

#### Does the agent have to commit for Autter to attribute the code?
No. Autter works no matter how you commit — your Git client, the Git CLI, and your own Git aliases are all supported.

#### Autter notes are attached to commits — how are attributions preserved when I rebase, squash, stash, cherry-pick, etc.?
Autter analyzes the final state of the code after the operation completes and copies/merges the attributions into a Git Note for any completed commits. It's eventually consistent. The note will be written 5-100ms after the operation completes.

#### Can I use this on my own?
Yes. Autter is free and open source, works locally, and requires no login or team setup.

#### Is there a performance impact?
No. Autter does not use Git hooks and it does not wrap Git, so you won't see any overhead on your Git commands.

#### Do I have to set up agent hooks?
Nope — Autter manages the agent hooks and checks/updates them daily. If you want to trigger this yourself (ie just installed a new agent) run `autter install-hooks`.

#### Who uses this?
Hundreds of engineering teams (including many in the Fortune 100) use Autter to understand their AI usage and make agents more effective on their codebase.

#### What's the difference between the open source CLI and the [teams version](https://autter.dev)?
The CLI accurately attributes AI code on every commit. The teams version adds a secure prompt store and joins in data from across the SDLC — tying token spend to individual Pull Requests, calculating % AI by PR, team, and repo, and connecting signals like amount of rework during code review, and even tying incidents back to the AI session that caused them. Self-host it or run it in our cloud: connect your SCM and get aggregate stats across thousands of repos plus full observability into everything your coding agents do.

#### Who built this?
[Sagnik](https://github.com/sagnik11) and [Sasha](https://github.com/svarlamov) — [start a discussion](https://github.com/orgs/Autter-dev/discussions) or email [hi@autter.dev](mailto:hi@autter.dev).

#### What are the capabilities and known limitations?
Autter provides line-level attribution for AI-generated code - whether it is written with an edit tool or a bash command. When a Git rewrite operation is run (`rebase`, `stash`, `squash --merge`, etc) Autter will move and merge attributions so nothing is lost. 

Here is a full breakdown of what is supported today: 

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

Git Rewrite Operations:

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
| Squash and Merge                                                | ✅      | Requires [Autter for Teams](https://calendly.com/d/cxjh-z79-ktm/meeting-with-autter-authors) or [Open Source CI Actions](https://autter.dev/docs/guides/ci-workflows) to preserve attribution. |
| Rebase and Merge                                                | ✅      | Requires [Autter for Teams](https://calendly.com/d/cxjh-z79-ktm/meeting-with-autter-authors) or [Open Source CI Actions](https://autter.dev/docs/guides/ci-workflows) to preserve attribution. |




## License
Apache 2.0
