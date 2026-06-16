# autter-server

The open-source Autter platform backend. It receives uploads from the `autter`
CLI (when a developer connects to the platform) and persists them to
PostgreSQL. Keeping it in this repo means the data contract — exactly what gets
stored when you connect — is visible and auditable.

> Status: **storage-first cut.** The authorship-note and prompt (CAS) storage
> endpoints the CLI already calls are implemented. Full OAuth token issuance is
> a separate batch; until then, run with `AUTTER_SERVER_DEV_AUTH=1` to exercise
> the pipeline.

## What it stores

All data is scoped to an organization (see [`migrations/0001_init.sql`](migrations/0001_init.sql)):

| Table | Contents |
|-------|----------|
| `orgs` | Organizations (teams/companies). |
| `users` | Developer identities. |
| `access_tokens` | Maps a presented credential (OAuth bearer / `X-API-Key`) → org. |
| `authorship_notes` | The serialized authorship note per `(org, commit_sha)` — the same text the CLI writes to the local `refs/notes/ai`. |
| `cas_objects` | Content-addressed prompt transcripts, deduplicated per `(org, hash)`. |

## Endpoints (match the CLI wire contract)

| Method & path | Purpose |
|---------------|---------|
| `GET  /health` | Liveness check. |
| `POST /worker/notes/upload` | Upsert a batch of authorship notes. |
| `GET  /worker/notes/?commits=sha1,sha2` | Read notes by commit SHA. |
| `POST /worker/cas/upload` | Store a batch of prompt-transcript objects. |
| `GET  /worker/cas/?hashes=h1,h2` | Read prompt objects by hash. |

Auth: send `X-API-Key: <key>` or `Authorization: Bearer <token>` (plus the
CLI's `X-Distinct-ID`).

## Run locally

```bash
# 1. Start PostgreSQL
docker compose -f server/docker-compose.yml up -d

# 2. Configure + run (migrations run automatically on startup)
cp server/.env.example server/.env          # or export the vars
export DATABASE_URL=postgres://autter:autter@localhost:5432/autter
export AUTTER_SERVER_DEV_AUTH=1
cargo run -p autter-server
```

Point the CLI at it by setting the API base URL:

```bash
autter config set api_base_url http://localhost:8787
# then connect:
autter onboard --connect      # (once the auth batch lands)
# or, today, drive uploads with an API key in dev-auth mode:
autter config set api_key dev-token-123
```

## Layout

```
server/
  migrations/0001_init.sql   # the storage schema (what we keep)
  src/
    main.rs                  # startup: pool, migrate, serve
    routes/{notes,cas}.rs    # storage endpoints
    auth.rs                  # credential → org resolution
    models.rs                # wire types (mirror the CLI's src/api/types.rs)
    error.rs, state.rs
```
