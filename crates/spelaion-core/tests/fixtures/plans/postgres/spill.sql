SET search_path = plan_fixture, public;
SET max_parallel_workers_per_gather = 0;
SET work_mem = '64kB';
EXPLAIN (FORMAT JSON, ANALYZE TRUE, BUFFERS TRUE)
SELECT id, note FROM orders ORDER BY note;
