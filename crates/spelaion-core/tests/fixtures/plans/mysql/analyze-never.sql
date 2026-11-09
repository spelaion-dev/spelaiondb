EXPLAIN ANALYZE FORMAT=TREE
SELECT o.id, c.name
FROM orders AS o JOIN customers AS c ON c.id = o.customer_id
WHERE o.total < 0;
