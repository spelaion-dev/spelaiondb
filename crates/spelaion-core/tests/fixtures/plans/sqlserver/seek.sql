SELECT id, customer_id FROM dbo.plan48_orders WITH (INDEX(IX_plan48_orders_customer))
WHERE customer_id = 42 OPTION (MAXDOP 1);
