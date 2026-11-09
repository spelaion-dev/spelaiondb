SELECT customer_id, SUM(CONVERT(bigint, amount)) FROM dbo.plan48_orders
GROUP BY customer_id OPTION (MAXDOP 4, USE HINT('ENABLE_PARALLEL_PLAN_PREFERENCE'));
