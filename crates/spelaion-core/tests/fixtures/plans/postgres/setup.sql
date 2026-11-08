-- Synthetic, isolated PostgreSQL plan fixtures. Run only in an empty test database.
CREATE SCHEMA plan_fixture;
CREATE TABLE plan_fixture.customers (
    id integer PRIMARY KEY,
    region text NOT NULL
);
INSERT INTO plan_fixture.customers
SELECT g, 'region_' || (g % 8)::text FROM generate_series(1, 1000) AS g;
CREATE TABLE plan_fixture.orders (
    id integer PRIMARY KEY,
    customer_id integer NOT NULL,
    amount numeric(12,2) NOT NULL,
    state text NOT NULL,
    note text NOT NULL
);
INSERT INTO plan_fixture.orders
SELECT g, 1 + g % 1000, (g % 100000)::numeric / 100,
       CASE WHEN g % 4 = 0 THEN 'pending' ELSE 'complete' END,
       repeat(md5(g::text), 4)
FROM generate_series(1, 120000) AS g;
CREATE INDEX orders_customer_id_idx ON plan_fixture.orders(customer_id);
ANALYZE plan_fixture.customers;
ANALYZE plan_fixture.orders;
