-- Synthetic data in the caller's disposable database, SQL Server 2022.
CREATE TABLE dbo.plan48_customers (
    id int NOT NULL CONSTRAINT PK_plan48_customers PRIMARY KEY,
    region int NOT NULL
);
CREATE TABLE dbo.plan48_orders (
    id int NOT NULL CONSTRAINT PK_plan48_orders PRIMARY KEY,
    customer_id int NOT NULL,
    amount int NOT NULL,
    label char(200) NOT NULL
);
WITH numbers AS (
    SELECT TOP (1000) CONVERT(int, ROW_NUMBER() OVER (ORDER BY (SELECT NULL))) AS n
    FROM sys.all_objects
)
INSERT dbo.plan48_customers(id, region) SELECT n, n % 8 FROM numbers;
WITH numbers AS (
    SELECT TOP (300000) CONVERT(int, ROW_NUMBER() OVER (ORDER BY (SELECT NULL))) AS n
    FROM sys.all_objects a CROSS JOIN sys.all_objects b
)
INSERT dbo.plan48_orders(id, customer_id, amount, label)
SELECT n, (n % 1000) + 1, n % 1000,
       CONVERT(char(200), CONCAT(REPLICATE('x', 180), RIGHT(REPLICATE('0', 10) + CONVERT(varchar(10), n), 10)))
FROM numbers;
CREATE INDEX IX_plan48_orders_customer ON dbo.plan48_orders(customer_id);
UPDATE STATISTICS dbo.plan48_orders WITH FULLSCAN;
UPDATE STATISTICS dbo.plan48_customers WITH FULLSCAN;
