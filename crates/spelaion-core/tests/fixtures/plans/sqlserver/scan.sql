SELECT SUM(CONVERT(bigint, amount)) FROM dbo.plan48_orders WITH (INDEX(PK_plan48_orders))
WHERE amount > 900 OPTION (MAXDOP 1);
