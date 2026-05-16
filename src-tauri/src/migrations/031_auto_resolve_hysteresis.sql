-- Auto-resolve hysteresis + Undo affordance (#124).
--
-- The profile worker's auto_resolve_missing previously flipped a
-- waiting action to done=1 the moment a single LLM recompute omitted
-- its source_ref_id. A single bad pass — context truncation, an
-- oddly-judged tail, a transient hallucination — silently lost a
-- real outstanding ask. These columns add:
--
--  * `auto_resolve_omissions` — consecutive recomputes the LLM has
--    dropped this id. Reset to 0 when the id reappears in a live
--    recompute, or when the user uses the Undo affordance.
--  * `auto_resolved_ms` — stamped by the worker (not the user) at
--    the moment the threshold-crossing flip happens. Drives the
--    "Margin auto-resolved" pill on the action row.

ALTER TABLE actions ADD COLUMN auto_resolve_omissions INTEGER NOT NULL DEFAULT 0;
ALTER TABLE actions ADD COLUMN auto_resolved_ms INTEGER;

UPDATE meta SET value = '31' WHERE key = 'schema_version';
