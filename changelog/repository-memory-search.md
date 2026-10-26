# Repository memory search

- Ask the Autter cloud what it knows about the repository you're in with `autter memory search "<question>"`. Results are ranked by meaning across the repository's shared memory pages, learnings, production lessons, team rules, past review and scan findings, the wiki and code. Narrow them with `--path` and `--kind`.
- Read the memory for the files you're about to edit with `autter memory for <file>...` (or `autter memory for` for the files you've changed): the detailed page for each area of the codebase they're in (how it works, conventions, what breaks, what works), most specific first, and what the team's what-works ledger says about those areas.
- Read the repository's memory with `autter memory show`, one page with `autter memory show <page>`, the areas with `autter memory areas`, and the ledger with `autter memory ledger`.
- Add `--json` for scripts and coding agents. The new `repo-memory` skill (installed with `autter install --skills`) tells agents to use these commands when the Autter MCP server isn't connected.
- They use your existing `autter login`. In local mode, or for repositories excluded from sharing, they send nothing and say so.

These commands need the updated Autter API.
