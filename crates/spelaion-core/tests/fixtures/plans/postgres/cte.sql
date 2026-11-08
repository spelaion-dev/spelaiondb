SET search_path = plan_fixture, public;
EXPLAIN (FORMAT JSON, ANALYZE TRUE, BUFFERS TRUE)
WITH chosen AS MATERIALIZED (
    SELECT id, customer_id, amount FROM orders WHERE customer_id < 12
)
SELECT c.region, count(*), sum(q.amount)
FROM chosen AS q JOIN customers AS c ON c.id = q.customer_id
GROUP BY c.region;
