-- Initial control-plane schema
-- Tenants / clients
CREATE TABLE clients (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name              TEXT NOT NULL UNIQUE,
    description       TEXT,
    -- First 8 characters of the raw API key, stored in plain text.
    -- Used as a fast lookup discriminator before the full bcrypt comparison.
    key_prefix        TEXT NOT NULL,
    -- bcrypt hash of the full raw API key.
    api_key_hash      TEXT NOT NULL,
    allowed_models    TEXT[] NOT NULL DEFAULT '{"*"}',
    rpm               INT NOT NULL DEFAULT 500,
    burst             INT NOT NULL DEFAULT 50,
    token_ttl_seconds INT NOT NULL DEFAULT 900,
    active            BOOLEAN NOT NULL DEFAULT true,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at        TIMESTAMPTZ
);

CREATE INDEX clients_key_prefix_idx ON clients(key_prefix) WHERE active = true;

-- Signing keypairs (supports key rotation)
CREATE TABLE signing_keys (
    kid         TEXT PRIMARY KEY,
    algorithm   TEXT NOT NULL DEFAULT 'RS256',
    private_key BYTEA NOT NULL,
    public_key  BYTEA NOT NULL,
    active      BOOLEAN NOT NULL DEFAULT true,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    retired_at  TIMESTAMPTZ
);

-- Audit log
CREATE TABLE audit_log (
    id         BIGSERIAL PRIMARY KEY,
    client_id  UUID REFERENCES clients(id),
    event      TEXT NOT NULL,
    metadata   JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX audit_log_client_id_idx ON audit_log(client_id);
CREATE INDEX audit_log_created_at_idx ON audit_log(created_at DESC);
