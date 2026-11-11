# The background service

Autter runs one small background process per user, called the background service (`autter bg`). This page says what it does, what it listens to, and how to look at it when something is wrong.

## What it does

- **Writes authorship notes.** When you commit, git reports the command to the service, which matches it with the checkpoints your agents sent and writes the authorship note for the commit to `refs/notes/ai`. It does the same bookkeeping for rebase, cherry-pick, merge, stash and reset, so attribution follows the code.
- **Receives checkpoints.** Agent hooks call `autter checkpoint`, which hands the edit to the service and returns at once, so the agent is not slowed down.
- **Uploads, in connected mode only.** If this machine is connected to the Autter platform, the service uploads queued authorship and usage data. In local mode it uploads nothing.
- **Checks for updates.** Release builds check for a newer version about once an hour and install it in the background. It does not do this in CI, when `AUTTER_RELEASE_TAG` pins a release, or when updates are turned off (`AUTTER_DISABLE_AUTO_UPDATES=1`, or `autter config set disable_auto_updates true`). `autter upgrade` upgrades on request.

It does not open a network port, does not read files outside the repositories you commit in and the agent transcripts named in checkpoints, and is not started until you agree to it during `autter onboard` (or run `autter install --system`).

## What it listens to

Two local endpoints that only your user account can open:

- a **trace socket**: git writes one event per git command to it. This is what the `trace2.eventTarget` setting in your global git config points at (`git config --global --get-regexp '^trace2\.'`).
- a **control socket**: the `autter` command uses it to send checkpoints and ask for status.

On macOS and Linux these are Unix sockets in a per-user folder under the system temp directory (`autter-d-<id>/`). On Windows they are named pipes (`\\.\pipe\autter-<id>-trace2` and `-control`).

## Commands

```bash
autter bg status     # is it running, which version, queue and upload health
autter bg restart    # start it, or stop and start it if it is running
autter bg shutdown   # stop it
autter bg tail -f    # follow its log
```

Its log files are in `~/.autter/internal/daemon/logs/`, one per run.

## When something is wrong

Start with `autter doctor`. It checks that the service is running, that a git command reaches it through the trace socket, and that a checkpoint makes the round trip, and prints a `fix:` line for each failure.

| What you see | Usual cause | What to do |
| --- | --- | --- |
| Commits get no authorship note | git is not reporting to the service: the global `trace2` settings are missing or point at an old socket | `autter install --system`, then `autter doctor` |
| `failed to connect to autter background service` | the service is not running | `autter bg restart`, then `autter doctor` |
| "Access is denied" or "lock held" | the service was started as root or Administrator, so your own account cannot reach it | stop the elevated one (from the elevated terminal, `autter bg shutdown`), then `autter bg restart` from a normal terminal |
| The service and the CLI report different versions | an upgrade replaced the binary while the old service was still running | `autter bg restart` |
| Doctor times out on a slow machine | time limits too short for this machine | `AUTTER_DOCTOR_TIMEOUT_SCALE=5 autter doctor` |

To remove it along with the rest of Autter's setup, run `autter uninstall`.
