SET search_path = plan_fixture, public;
EXPLAIN (FORMAT JSON, COSTS TRUE, ANALYZE FALSE)
SELECT id, region FROM customers WHERE id = 42;
