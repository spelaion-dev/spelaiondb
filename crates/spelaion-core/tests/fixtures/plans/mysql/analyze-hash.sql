EXPLAIN ANALYZE FORMAT=TREE
SELECT o.id, c.country
FROM orders AS o JOIN customers AS c ON c.name = CONCAT('customer ', o.customer_id)
WHERE o.created >= '2026-10-01'
ORDER BY o.total DESC
LIMIT 20;
