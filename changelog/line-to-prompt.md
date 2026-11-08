# From a line to the prompt that wrote it

## Summary

`autter blame` now names the prompt behind each AI line with a short reference, and `autter show-prompt` accepts that reference. One documented path runs from a line to its session, model, transcript and commit.

## Added

- **Prompt legend in `autter blame`**: on a terminal, blame ends with one row per prompt behind the AI lines shown: a short reference (`s_3f9a2c1d`), the agent, the model and the lines, plus the `show-prompt` and `--why` commands to run next. Piped output is unchanged, one line per source line.
- **`--show-prompt`** puts the same reference on every AI line (`claude [s_3f9a2c1d]`) and prints the legend even when piped. It used to print the first 7 characters of the attestation key, which `show-prompt` could not resolve.
- **`autter show-prompt` takes the IDs users see**: a short reference from blame or any unique prefix of 4+ hex characters, a full session ID, a line's attestation key (`s_…::t_…`), an older 16-hex prompt hash, or the agent's own session ID. Brackets copied from `--show-prompt` are ignored. A prefix that matches more than one prompt lists them all.
- `show-prompt` output now has the full `prompt_id` the query resolved to, the `query` when it differed, and a `transcript` object with a `status` and, when there is no transcript, a `reason`.

## Changed

- **`autter blame --why`** prints the prompt reference that `show-prompt` accepts (it printed 12 characters of the attestation key, which nothing accepted), the agent's own session ID, the commit, and the commands for the full record and the change.
- When a transcript can't be shown, `--why` and `show-prompt` say why: none was recorded, it stayed on the machine that made the commit (local mode), you need to `autter login`, Autter has no copy you can read, or the fetch failed.
- **"Prompt not found" errors** say where prompt IDs appear (`blame`, `blame --why`, `show`) and that notes from other machines arrive with `autter fetch-notes`. They no longer start with "Generic error:".
- With the http notes backend, `show-prompt` searches notes cached on this machine (up to the last 5000 commits) instead of only git notes, which that backend does not write.
- `autter show-prompt --help` explains what the ID is, where to find it and what forms it takes. `autter blame --help` lists `--why`, `--open` and `--show-prompt`.
