# Agent token usage and prompt caching

autter's background service streams each agent's session events to the
backend, which turns the token usage in them into the estimated AI spend shown
on the dashboard. Every estimate prices four token classes separately: input,
output, cache reads and cache writes. This page lists what each agent records
and how autter forwards it.

## How usage travels

Most agents put usage inside their own session events, and autter forwards
those events as written (secrets redacted, numbers untouched). The backend
reads each agent's shape and normalizes it there. autter adds data only where
the agent keeps usage outside the events it streams:

| Agent | Where usage comes from | Convention in the source | What autter adds |
| --- | --- | --- | --- |
| Claude Code | `message.usage` on assistant lines | Disjoint: `input_tokens` excludes `cache_read_input_tokens` and `cache_creation_input_tokens`; `cache_creation.ephemeral_5m_input_tokens` / `ephemeral_1h_input_tokens` split writes by TTL | Nothing |
| Codex | `token_count` events, `info.total_token_usage` (running total) | OpenAI: `input_tokens` includes `cached_input_tokens` and `cache_write_input_tokens`; `output_tokens` includes `reasoning_output_tokens` | Nothing |
| GitHub Copilot (VS Code, OTel) | Copilot Chat's OTel traces database | OpenAI: `input_tokens` includes `cached_tokens` (reads) and the cache writes | `span.cache_creation_tokens`, read from the `gen_ai.usage.cache_creation.input_tokens` span attribute, which the database does not copy into a column |
| GitHub Copilot (VS Code chat sessions) | `result.usage` | `promptTokens` / `completionTokens` only, no cache counts | Nothing (cache counts come from OTel) |
| Copilot CLI | `session.shutdown` event, `data.modelMetrics.<model>.usage` (per-model running total) | `inputTokens` includes `cacheReadTokens` and `cacheWriteTokens` | Nothing. Per-request `assistant.usage` events are not written to disk by Copilot CLI |
| Gemini CLI | `tokens` on `gemini` records | Google: `input` (`promptTokenCount`) includes `cached`; `thoughts` and `tool` are counted separately; no cache writes | Nothing |
| OpenCode | `tokens` and `cost` on assistant messages | Disjoint: `input`, `cache.read`, `cache.write`; `reasoning` separate from `output`; `cost` in USD | Nothing |
| Pi | `message.usage` on assistant messages | Disjoint: `input`, `cacheRead`, `cacheWrite` (`cacheWrite1h` is a subset); `output` includes `reasoning`; `cost.total` in USD | Nothing |
| Amp | `usage` on assistant messages | Disjoint (Anthropic style, camelCase): `inputTokens`, `cacheReadInputTokens`, `cacheCreationInputTokens` | Nothing |
| Continue CLI | Newer versions: `message.usage` on assistant history items. All versions: a session-level `usage` total | Per message: `prompt_tokens` with `prompt_tokens_details`; the session total is disjoint with `totalCost` | A usage snapshot of the session-level total (below) |
| Droid | `tokenUsage` in `<session>.settings.json`, next to the transcript (running total) | Disjoint: `inputTokens`, `cacheReadTokens`, `cacheCreationTokens`; `thinkingTokens` | A usage snapshot of the settings total (below) |
| Cursor, Windsurf, Firebender, Antigravity, Kiro | Not recorded in their transcripts or hook payloads | | Nothing; their sessions show as "no usage data" rather than $0 |

## Usage snapshot events

For Droid and Continue CLI, the reader appends one synthetic event to a batch
that read new records (only to the batch that reaches the end of the file, so
one per read-through). It does not count toward the stream's watermark.

```json
{
  "type": "autter_usage_snapshot",
  "timestamp": "2026-01-28T16:57:30.000Z",
  "autter_usage_snapshot": {
    "version": 1,
    "source": "droid_settings",
    "scope": "session_total",
    "model": "claude-sonnet-4-6",
    "input_tokens": 6743,
    "output_tokens": 1450,
    "cache_read_tokens": 42496,
    "cache_write_tokens": 0,
    "reasoning_tokens": 1088,
    "cost_usd": null,
    "input_includes_cache": false
  }
}
```

- The counts are disjoint: `input_tokens` excludes cache reads and writes, and
  `input_includes_cache` is always `false`.
- `output_tokens` includes reasoning; `reasoning_tokens`, when present, is a
  subset of it.
- `scope` is `session_total`: each count is the session's running total when
  the snapshot was taken. Consumers diff successive snapshots of a session and
  never sum them.
- `cost_usd` is the cost the agent reported itself (Continue's `totalCost`),
  or `null`.
- `source` is `droid_settings` or `continue_session`. `version` changes only
  for a breaking change; new fields may be added.

## Limits

- Droid writes its settings file after a turn; usage from a session's last turn
  arrives only when the transcript grows again.
- Codex records cache writes without their TTL (30-minute or 12-hour), so they
  are priced at the 30-minute rate.
- Copilot reports cost in AI units (`copilot_usage_nano_aiu`), not dollars; it
  is forwarded but not used as a dollar cost.
