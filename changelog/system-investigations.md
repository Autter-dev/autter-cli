# 2.2.0 — System investigations

- Ask plain-language questions with `autter ask`. Answers use captured Runtime signals and repository evidence to explain causes, observed impact, recommended fixes, and verification steps.
- Search captured logs with `autter logs`, filtering by repository, severity, service, environment, time range, message, and custom fields. Results appear in a readable table; `--json` preserves complete fields.
- Find and reopen saved debugging sessions with `autter threads search` and `autter threads show`. Continue a conversation with `autter ask --thread`; private history is shared with Dashboard → Ask.

These commands use your existing CLI login and require the updated Autter API. Run `autter update` to upgrade an existing installation.
