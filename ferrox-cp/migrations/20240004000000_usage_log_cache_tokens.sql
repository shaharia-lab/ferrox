-- Prompt-cache token counters, reported by providers that support prompt caching
-- (Anthropic `cache_read_input_tokens` / `cache_creation_input_tokens`, Bedrock
-- `cacheReadInputTokens` / `cacheWriteInputTokens`).
--
-- Nullable with no default and no backfill on purpose: NULL means "written by a
-- gateway that predates cache accounting", which is a different fact from a
-- recorded 0 ("this provider reported no cache usage for this request").
ALTER TABLE usage_log
    ADD COLUMN cache_read_tokens INT,
    ADD COLUMN cache_write_tokens INT;
