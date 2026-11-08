EXPLAIN ANALYZE FORMAT=TREE
SELECT c.country, t.n,
       (SELECT max(o2.total) FROM orders AS o2 WHERE o2.customer_id = c.id) AS top
FROM customers AS c
JOIN (SELECT customer_id, count(*) AS n FROM orders WHERE status = 'new' GROUP BY customer_id) AS t
  ON t.customer_id = c.id
WHERE c.country IN ('ES', 'FR')
UNION ALL
SELECT 'XX', 0, 0;
