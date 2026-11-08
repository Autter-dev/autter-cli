# Data Privacy

## OSS Mode

Unless you choose connected mode (`autter onboard --connect`, or "Connected" in `autter onboard`), **no code, prompts, or agent usage data is ever sent to Autter**. Signing in with `autter login` alone does not turn on uploads. Autter runs entirely on your machine and writes attribution data into your local git repository and prompts to a local SQLite. `autter whoami` and `autter sync status` show "Cloud upload: off" exactly when nothing is being uploaded.

Before you finish `autter onboard`, Autter does not read your AI agents' session history, and plain commands such as `autter status` or `autter doctor` do not start the background service.

The only other data Autter can send is anonymous error and usage telemetry. It is off until you opt in during `autter onboard` or with `autter telemetry on`, and you can turn it off at any time by setting `telemetry_oss` to `off`. See [configuration options](https://autter.dev/docs/cli/configuration#configuration-options) for details.

## Data

### Local-only data

- **Prompts** are stored locally on the developer's laptop. They are never shared with teammates or Autter.

### Data written to your git repository

AI attribution data is written to git notes and is readable by anyone with repo access:

- Model, agent, and accepted-rate percentages
- Which lines are AI-generated
- Git profile (name, email) of the person who steered each prompt

### Telemetry

- **Error & exception telemetry** — off until you opt in during `autter onboard` (or with `autter telemetry on`), in local and connected mode alike. It never includes code or prompts. You can turn it off or redirect it to your own endpoint. See [configuration options](https://autter.dev/docs/cli/configuration#configuration-options).

---

## Autter Cloud (Personal Dashboards)

If you opt in to a personal dashboard, the following is uploaded to Autter Cloud:

- **Agent usage telemetry** — cross-agent telemetry for every tool use, MCP call, skill invocation, interruption, error, and token-usage event, along with prompts and agent responses, used for your personal analytics
- **Personal Agent usage data** - % AI, # of Parallel agents and other stats on the dashboard are visible only to you unless you share them with others. 
- **SCM profile metadata** from GitHub, Bitbucket, or GitLab

See our [Cloud Privacy Policy](https://autter.dev/privacy-policy) for details.

---

## Autter for Teams and Enterprise

Teams and Enterprise deployments store additional data in the team instance:

- **Employee identity** — names, emails, and GitHub/Bitbucket/GitLab team membership
- **Agent usage telemetry** — cross-agent telemetry for every tool use, MCP call, skill invocation, interruption, error, and token-usage event
- **Full prompts** uploaded to the Autter prompt store
  - Best-effort stripping of secrets and PII by default
  - You can apply your own filters for enhanced detection
  - Write-only: prompts are saved to your team's instance, but developers cannot read unless explicitly granted permission. 
- **Full agent sessions** can be reviewed, summarized, and made readable to developers
- **SCM PR data** from GitHub, Bitbucket, and GitLab
  - PR metadata: description, opener, reviewer, status
  - PR diffs — processed for computing % AI code, but not stored
- **Error & exception telemetry** — off until you opt in; you can turn it off or redirect it to your own endpoint at any time. See [configuration options](https://autter.dev/docs/cli/configuration#configuration-options).

For more information, see our [Trust Center](https://trust.autter.dev).

In [self-hosted deployments](https://github.com/autter-dev/self-hosted), all data is sent only to your team's Autter instance.
