SELECT c.region, COUNT_BIG(*) FROM dbo.plan48_orders AS o
INNER HASH JOIN dbo.plan48_customers AS c ON c.id = o.customer_id
WHERE o.amount > 900 GROUP BY c.region OPTION (MAXDOP 1);
