EXPLAIN (FORMAT JSON, ANALYZE TRUE, BUFFERS TRUE)
SELECT c.region, count(*) AS orders,
       sum(o.amount) AS revenue
FROM plan_fixture.orders AS o
JOIN plan_fixture.customers AS c
  ON c.id = o.customer_id
WHERE o.amount > 900
GROUP BY c.region;
