-- Autter platform backend — per-organization schema.
--
-- Each organization has its OWN PostgreSQL database. The backend learns an
-- org's database URL from the verified Better Auth JWT (the `org_db_url` claim)
-- and connects to it, then ensures this schema exists (these migrations run on
-- first connect to each org database).
--
-- Because every database belongs to exactly one org, rows are not org-scoped:
-- the database boundary IS the tenant boundary. Identity (which user uploaded
-- what) comes from the JWT, so there are no user/org/token tables here — those
-- live in the autter.dev control plane.

-- Authorship notes: the serialized note text the CLI also writes to the local
-- git ref refs/notes/ai, keyed by commit. Re-uploads upsert.
CREATE TABLE IF NOT EXISTS authorship_notes (
    commit_sha  TEXT PRIMARY KEY,
    content     TEXT NOT NULL,
    uploaded_by TEXT,        -- Better Auth user id (JWT `sub`)
    distinct_id TEXT,        -- CLI X-Distinct-ID
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Content-addressable store for prompt transcripts. `hash` is the CLI-computed
-- content hash; `content` is the canonicalized transcript JSON. Deduplicated by
-- hash — re-uploading the same content is a no-op.
CREATE TABLE IF NOT EXISTS cas_objects (
    hash        TEXT PRIMARY KEY,
    content     JSONB NOT NULL,
    metadata    JSONB NOT NULL DEFAULT '{}'::jsonb,
    uploaded_by TEXT,        -- Better Auth user id (JWT `sub`)
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
