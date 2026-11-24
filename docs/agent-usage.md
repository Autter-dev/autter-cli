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
| Codex | `token_count` events, `info.total_token_usage` (running total) | OpenAI: `input_tokens` includes `cached_input_tokens` (and `cache_write_input_tokens`, which only newer Codex versions write); `output_tokens` includes `reasoning_output_tokens` | Nothing |
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

## Cached tokens by agent

What reaches the backend for prompt caching, per agent. "Input includes
cache" means the source's input count already contains the cached tokens, so
the backend subtracts them (OpenAI, Google and Copilot style); otherwise the
counts are disjoint (Anthropic style). Nothing on the way (secret redaction,
batching, the upload queue) changes or drops a number. A key the source does
not write is not added, so "not written" never turns into "0".

| Agent | Cache read | Cache write | 1-hour write | Input includes cache | Checked against |
| --- | --- | --- | --- | --- | --- |
| Claude Code | `cache_read_input_tokens` | `cache_creation_input_tokens` | `cache_creation.ephemeral_1h_input_tokens` (5-minute share: `ephemeral_5m_input_tokens`) | No | Real transcripts (Claude Code 2.1.x) |
| Codex | `cached_input_tokens` | `cache_write_input_tokens`, only from Codex versions that write it; older rollouts (0.133, 0.137-alpha) have no such key | Not recorded (no TTL) | Yes | Real rollouts and codex-rs `TokenUsage` |
| GitHub Copilot (VS Code, OTel) | `span.cached_tokens` | `span.cache_creation_tokens` (from the `gen_ai.usage.cache_creation.input_tokens` attribute; `null` when the span has none) | Not available | Yes | Fixture database |
| GitHub Copilot (VS Code chat sessions) | Not available from this tool | Not available from this tool | Not available | (`promptTokens` only) | Real session file (Copilot Chat 0.37) |
| Copilot CLI | `cacheReadTokens` (per model, session total) | `cacheWriteTokens` | Not available | Yes | Fixture only (no local Copilot CLI) |
| Gemini CLI | `tokens.cached` | Not available from this tool | Not available | Yes | gemini-cli source: writes `cached` as 0 when the API sends none, so 0 there can mean "not reported" |
| OpenCode | `tokens.cache.read` | `tokens.cache.write` | Not available | No | Fixture database |
| Pi | `usage.cacheRead` | `usage.cacheWrite` | `usage.cacheWrite1h` (Anthropic models only; optional) | No | pi-mono `Usage` type |
| Amp | `usage.cacheReadInputTokens` | `usage.cacheCreationInputTokens` | Not available | No | Real thread fixture |
| Continue CLI (per message) | `prompt_tokens_details.cache_read_tokens` (Anthropic adapter) or `cached_tokens` (OpenAI pass-through) | `prompt_tokens_details.cache_write_tokens` (Anthropic adapter) | Not available | No with the Anthropic adapter's keys, yes with `cached_tokens` | Continue CLI source |
| Continue CLI (session total) | Snapshot `cache_read_tokens` (`promptTokensDetails.cachedTokens`) | Snapshot `cache_write_tokens` (`cacheWriteTokens`) | Not available | No | Fixture and Continue source |
| Droid | Snapshot `cache_read_tokens` (`cacheReadTokens`) | Snapshot `cache_write_tokens` (`cacheCreationTokens`) | Not available | No | Fixture |
| Cursor | Not available from this tool | Not available from this tool | Not available | | Cursor's state database keeps only `tokenCount: {inputTokens, outputTokens}` per message, rarely filled; its transcripts have no usage |
| Windsurf, Firebender, Antigravity, Kiro | Not available from this tool | Not available from this tool | Not available | | Transcripts and hook payloads carry no usage |

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
    "cache_read_reported": true,
    "cache_write_reported": true,
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
- `cache_read_reported` and `cache_write_reported` (autter 3.18.0 and later)
  say whether the agent's file had the cache-read and cache-write key at all.
  When one is `false`, the matching count is `0` because the source does not
  report it, not because nothing was cached. Snapshots from older autter
  versions have neither key; read a missing key as unknown.
- `source` is `droid_settings` or `continue_session`. `version` changes only
  for a breaking change; new fields may be added.

## Limits

- Droid writes its settings file after a turn; usage from a session's last turn
  arrives only when the transcript grows again.
- Codex records cache writes without their TTL (30-minute or 12-hour), so they
  are priced at the 30-minute rate.
- A single event larger than the upload endpoint accepts (HTTP 413, about
  5 MB) is dropped with a warning, usage included. Usage-bearing events are
  normally small (Claude's large tool results are on user lines, not on the
  assistant lines that carry usage).
- Amp threads, Continue sessions and older VS Code Copilot chat `.json`
  session files are read by message index: if an agent filled in a message's
  usage after autter had read that message, the update would not be read
  again. Not observed so far (Amp's saved messages carry
  `state: complete` with their usage); Continue's session total covers it.
- autter shows no token counts or cost on your machine; the dashboard's AI
  usage page is where cached tokens and the cache hit rate appear.
- Copilot reports cost in AI units (`copilot_usage_nano_aiu`), not dollars; it
  is forwarded but not used as a dollar cost.
