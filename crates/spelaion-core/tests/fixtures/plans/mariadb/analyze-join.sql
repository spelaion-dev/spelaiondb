ANALYZE FORMAT=JSON
SELECT c.country, count(*) AS orders, sum(o.total) AS revenue
FROM orders AS o JOIN customers AS c ON c.id = o.customer_id
WHERE o.status = 'paid'
GROUP BY c.country
ORDER BY revenue DESC;
