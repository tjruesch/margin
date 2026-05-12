-- Embeddings + sqlite-vec virtual table (#104).
--
-- `embeddings` is the source-of-truth metadata row per
-- (ref_kind, ref_id, model). `source_hash` (sha256 of the embedded
-- text) lets the worker skip rows whose source hasn't changed since
-- last index. `indexed_ms` is the last successful embed time.
--
-- `embeddings_vec` is the vec0 virtual table that holds the actual
-- 1024-dim Voyage embedding, keyed by the rowid of the matching
-- `embeddings` row. JOINs are free; ANN search returns the rowid +
-- cosine distance via `MATCH ?` + `k = ?`.

PRAGMA foreign_keys = ON;

CREATE TABLE embeddings (
  rowid       INTEGER PRIMARY KEY AUTOINCREMENT,
  ref_kind    TEXT NOT NULL,
  ref_id      TEXT NOT NULL,
  model       TEXT NOT NULL,
  source_hash TEXT NOT NULL,
  indexed_ms  INTEGER NOT NULL,
  UNIQUE (ref_kind, ref_id, model)
);
CREATE INDEX idx_embeddings_ref ON embeddings(ref_kind, ref_id);

CREATE VIRTUAL TABLE embeddings_vec USING vec0(
  embedding float[1024]
);

UPDATE meta SET value = '23' WHERE key = 'schema_version';
