-- Autter platform backend — initial schema.
--
-- This is the full, transparent record of what the Autter platform stores when
-- a developer connects the CLI to the platform. Everything the CLI uploads
-- lands in one of these tables, scoped to an organization.
--
-- All org-scoped data cascades on org deletion so a customer can be fully
-- removed.

CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- An organization (team/company). All stored data is scoped to an org.
CREATE TABLE IF NOT EXISTS orgs (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    slug        TEXT NOT NULL UNIQUE,
    name        TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- A developer/user. Identity is established by the platform auth flow; the
-- storage endpoints only reference users via the credential they present.
CREATE TABLE IF NOT EXISTS users (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    email       TEXT UNIQUE,
    name        TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Maps a presented credential (OAuth bearer token or X-API-Key) to an org and
-- optional user. The storage endpoints resolve the caller's org via this table.
-- (Full OAuth issuance lands with the auth batch; this table is the seam the
-- storage endpoints authenticate against today.)
CREATE TABLE IF NOT EXISTS access_tokens (
    token       TEXT PRIMARY KEY,
    org_id      UUID NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    user_id     UUID REFERENCES users(id) ON DELETE SET NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Authorship notes: the exact serialized note text the CLI also writes to the
-- local git ref refs/notes/ai, one row per (org, commit). Re-uploads upsert.
CREATE TABLE IF NOT EXISTS authorship_notes (
    id          BIGSERIAL PRIMARY KEY,
    org_id      UUID NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    commit_sha  TEXT NOT NULL,
    content     TEXT NOT NULL,
    uploaded_by UUID REFERENCES users(id) ON DELETE SET NULL,
    distinct_id TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (org_id, commit_sha)
);

CREATE INDEX IF NOT EXISTS authorship_notes_org_commit_idx
    ON authorship_notes (org_id, commit_sha);

-- Content-addressable store for prompt transcripts. `hash` is the CLI-computed
-- content hash; `content` is the canonicalized transcript JSON. Deduplicated
-- per (org, hash) — re-uploading the same hash is a no-op.
CREATE TABLE IF NOT EXISTS cas_objects (
    id          BIGSERIAL PRIMARY KEY,
    org_id      UUID NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    hash        TEXT NOT NULL,
    content     JSONB NOT NULL,
    metadata    JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (org_id, hash)
);

CREATE INDEX IF NOT EXISTS cas_objects_org_hash_idx
    ON cas_objects (org_id, hash);
