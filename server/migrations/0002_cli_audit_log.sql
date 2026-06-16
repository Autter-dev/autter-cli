-- Dedicated CLI / Personal Access Token activity log for this org. Separate from
-- any general-purpose audit_log. Tracks three event types:
--   'pat.created'  a new PAT was minted          (written by the control plane)
--   'pat.signin'   a PAT was exchanged for a JWT (written by the control plane)
--   'data.push'    a device pushed data with a PAT (written by this worker)
--
-- Also provisioned by the control plane (`bootstrap-org-tables`); created here
-- IF NOT EXISTS so the worker is self-contained against a fresh org database.
CREATE TABLE IF NOT EXISTS cli_audit_log (
    id           TEXT PRIMARY KEY,
    event_type   TEXT NOT NULL,        -- 'pat.created' | 'pat.signin' | 'data.push'
    actor_id     TEXT,                 -- Better Auth user id (JWT `sub`)
    actor_email  TEXT,
    token_id     TEXT,                 -- cli_token id (master DB), when known
    token_name   TEXT,
    resource_id  TEXT,                 -- e.g. the commit SHA for data.push
    detail       JSONB,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS cli_audit_log_eventType_idx ON cli_audit_log (event_type);
CREATE INDEX IF NOT EXISTS cli_audit_log_tokenId_idx ON cli_audit_log (token_id);
CREATE INDEX IF NOT EXISTS cli_audit_log_createdAt_idx ON cli_audit_log (created_at);
