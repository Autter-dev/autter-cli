# Repository memory search

- Ask the Autter cloud what it knows about the repository you're in with `autter memory search "<question>"`. Results are ranked by meaning across the repository's shared memory pages, learnings, production lessons, team rules, past review and scan findings, the wiki and code. Narrow them with `--path` and `--kind`.
- Read the repository's memory with `autter memory show`, or one page with `autter memory show <page>`.
- Add `--json` for scripts and coding agents. The new `repo-memory` skill (installed with `autter install --skills`) tells agents to use these commands when the Autter MCP server isn't connected.
- Both commands use your existing `autter login`. In local mode, or for repositories excluded from sharing, they send nothing and say so.

These commands need the updated Autter API.
