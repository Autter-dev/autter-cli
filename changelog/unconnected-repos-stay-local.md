# Repositories no org has connected stay on your computer

Before 3.1.0, connected mode sent every repository's data to an org. When autter could not tell which org a repository belonged to, it used the org you signed in to. That included repositories nobody had connected: a personal project or a client's repository worked on from a work login ended up in the workplace's dashboard, with its prompts, transcripts and file paths.

## What changed

- Before uploading, autter asks the server which of your orgs has connected the repository (by its default remote, `owner/repo`). The answer is cached for an hour.
  - An org connected it: the data goes to that org. With a personal access token autter uses a token for that org; a device-code login uploads only when it is that login's org.
  - No org connected it, or the commit has no remote: the data stays in the local queues and is not uploaded anywhere. It is offered again once a day, so connecting the repository on the web later uploads it.
  - The server could not be reached: nothing is decided; the upload is retried on the normal schedule.
- Prompt transcripts now record their repository (`repo_url` in the transcript's metadata) and are routed like everything else. Transcripts queued by older versions have no repository, so they stay on this computer.
- The server checks the repository again on every upload and refuses data for a repository the token's org hasn't connected (`repo_not_connected`). autter keeps refused records locally instead of retrying them.
- Waiting for a repository to be connected is not counted as a failed upload, so it never makes autter give up on a record, and `autter sync status` doesn't report it as a stuck queue.

## Checking it

`autter sync status` lists records kept on this computer because their repository isn't connected:

```
Kept on this computer (repository not connected to any of your orgs): 4 authorship notes, 4 commit summaries, 3 transcripts, 12 file-change records
```

`autter sync status --json` reports the same numbers under `not_connected`.

## Requires

The updated Autter API: the repository lookup for device-code logins and the upload check ship with it.
