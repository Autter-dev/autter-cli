# autter-server

The open-source Autter platform **data plane**. It receives uploads from the
`autter` CLI and persists them to **each organization's own PostgreSQL
database**. Keeping it in this repo means the data contract — exactly what gets
stored when you connect — is visible and auditable.

## How identity + routing works

```
CLI ──(Authorization: Bearer <Better Auth JWT>)──▶ autter-server
                                                      │
              verify JWT signature against autter.dev's JWKS (stateless)
                                                      │
                 read claims: sub, email, org_id, org_db_url
                                                      │
                  connect to that org's Postgres (org_db_url)
                                                      ▼
                         write authorship_notes / cas_objects
```

- **autter.dev** runs Better Auth and is the identity/control plane. Its JWT
  plugin signs tokens and publishes a JWKS endpoint.
- **autter-server** (this crate) is the data plane. It verifies the JWT against
  the JWKS (no per-request call to autter.dev) and routes storage to the org's
  own database using the `org_db_url` claim. Pools are created lazily per org
  and cached.

### Required JWT claims

Configure Better Auth's JWT `definePayload` so tokens include:

| Claim | Meaning |
|-------|---------|
| `sub` | user id (recorded as `uploaded_by`) |
| `email`, `name` | identity (optional) |
| `org_id` | the caller's organization id |
| `org_db_url` | Postgres URL of that org's database |

## What it stores (per org database)

See [`migrations/0001_init.sql`](migrations/0001_init.sql) — applied to each org
database on first connect:

| Table | Contents |
|-------|----------|
| `authorship_notes` | Serialized authorship note per commit (same text the CLI writes to local `refs/notes/ai`). |
| `cas_objects` | Content-addressed prompt transcripts, deduplicated by hash. |

There are no user/org/token tables here — identity lives in the autter.dev
control plane; the database boundary is the tenant boundary.

## Endpoints (match the CLI wire contract)

| Method & path | Purpose |
|---------------|---------|
| `GET  /health` | Liveness check. |
| `POST /worker/notes/upload` | Upsert a batch of authorship notes. |
| `GET  /worker/notes/?commits=sha1,sha2` | Read notes by commit SHA. |
| `POST /worker/cas/upload` | Store a batch of prompt-transcript objects. |
| `GET  /worker/cas/?hashes=h1,h2` | Read prompt objects by hash. |

Auth: `Authorization: Bearer <jwt>` on every request (plus the CLI's `X-Distinct-ID`).

## Run locally

```bash
# 1. Start a Postgres to act as an org database
docker compose -f server/docker-compose.yml up -d

# 2. Run in dev mode (decodes the JWT without verifying its signature, so you can
#    hand-craft a token whose org_db_url points at the docker Postgres).
export AUTTER_SERVER_DEV_AUTH=1
cargo run -p autter-server
```

In production, set `AUTTER_SERVER_JWKS_URL=https://autter.dev/api/auth/jwks`
instead of dev mode.

## Layout

```
server/
  migrations/0001_init.sql   # per-org storage schema (what we keep)
  src/
    main.rs                  # startup: verifier, pools, serve
    auth.rs                  # Better Auth JWT verification (JWKS) → identity + org_db_url
    state.rs                 # per-org pool manager
    routes/{notes,cas}.rs    # storage endpoints
    models.rs                # wire types (mirror the CLI's src/api/types.rs)
    error.rs
```
