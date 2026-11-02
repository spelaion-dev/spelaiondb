-- A 1 M row table for the streaming experiment (experiment 3), with varied types.
CREATE TABLE big AS
SELECT
  g                                        AS id,
  md5(g::text)                             AS txt,
  timestamptz '2026-01-01' + g * interval '1 second' AS ts,
  (random() * 100000)::numeric(12, 2)      AS amount,
  (g % 7 = 0)                              AS flag,
  CASE WHEN g % 10 = 0 THEN NULL ELSE g % 1000 END AS maybe_null,
  gen_random_uuid()                        AS uid,
  jsonb_build_object('n', g, 'even', g % 2 = 0) AS doc
FROM generate_series(1, 1000000) AS g;

ALTER TABLE big ADD PRIMARY KEY (id);
ANALYZE big;
