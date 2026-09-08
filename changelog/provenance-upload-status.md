# CLI 1.7.2: Capture and upload status

Release date: September 8, 2026.

## Added

- `autter status` shows local capture and cloud upload status together.
- Interactive commits show upload status and the next action for blocked uploads.
- `autter sync status --json` includes queue availability, the last confirmed metrics batch, and the dashboard organization.
- `autter sync open` opens the provenance dashboard for the reported organization.

## Fixed

- An empty queue no longer implies that all local changes reached the dashboard.
- Missing sign-in credentials, a stopped service, and queue read failures have clear states.
- Rejected metrics stay queued for retry. Accepted records are removed only after confirmation.
- Retried records move behind newer records so repeated failures do not block the queue.
- A successful upload from another queue does not hide a metrics upload failure.
- Running `autter sync` without a subcommand shows status instead of failing.

## Update

Install this release with the normal CLI installer or npm package. Run `autter status` in a repository, then use the stated action if upload is blocked. Earlier successful uploads do not have a local receipt; a new successful metrics batch records one.
