SET search_path = plan_fixture, public;
EXPLAIN (FORMAT JSON, ANALYZE TRUE, BUFFERS TRUE)
SELECT c.region, count(*), sum(o.amount)
FROM orders AS o JOIN customers AS c ON c.id = o.customer_id
WHERE o.amount > 900
GROUP BY c.region;
