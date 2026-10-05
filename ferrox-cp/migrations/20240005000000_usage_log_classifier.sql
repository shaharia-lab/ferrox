-- Routing decision of a request to a classified model alias (`models[].classifier`):
-- the alias the client asked for (`model` stays the alias that served it), why it
-- was routed there (`classified`, `low_confidence`, `timeout`, `error`,
-- `unknown_choice`, `breaker_open`, `shadow`, `opt_out`), and what the classifier
-- reported.
--
-- Nullable with no default and no backfill on purpose: NULL in `requested_model`
-- and `routing_reason` means "not a request to a classified alias" (or a row
-- written by a gateway that predates classifiers). On a classified row the three
-- answer columns are NULL when the classifier gave no answer (timeout, error,
-- breaker open, opt-out), and `classifier_confidence` also when its backend
-- reports none. `classifier_input_tokens` is 0 for an answer read from the
-- decision cache: that request was billed nothing.
ALTER TABLE usage_log
    ADD COLUMN requested_model TEXT,
    ADD COLUMN routing_reason TEXT,
    ADD COLUMN classifier_confidence DOUBLE PRECISION,
    ADD COLUMN classifier_latency_ms INT,
    ADD COLUMN classifier_input_tokens INT,
    ADD COLUMN classifier_model TEXT;
