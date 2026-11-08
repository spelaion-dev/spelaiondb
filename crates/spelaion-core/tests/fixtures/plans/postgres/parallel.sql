SET search_path = plan_fixture, public;
SET max_parallel_workers_per_gather = 2;
SET min_parallel_table_scan_size = 0;
SET min_parallel_index_scan_size = 0;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
EXPLAIN (FORMAT JSON, ANALYZE TRUE, BUFFERS TRUE, VERBOSE TRUE)
SELECT sum(amount), count(*) FROM orders;
