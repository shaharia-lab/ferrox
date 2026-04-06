-- Per-request token usage records reported by the gateway.
CREATE TABLE usage_log (
    id                BIGSERIAL PRIMARY KEY,
    client_id         UUID NOT NULL REFERENCES clients(id),
    request_id        TEXT NOT NULL,
    model             TEXT NOT NULL,
    provider          TEXT NOT NULL,
    prompt_tokens     INT NOT NULL,
    completion_tokens INT NOT NULL,
    total_tokens      INT NOT NULL,
    latency_ms        INT,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX usage_log_client_created_idx ON usage_log(client_id, created_at DESC);
CREATE INDEX usage_log_created_at_idx ON usage_log(created_at DESC);
