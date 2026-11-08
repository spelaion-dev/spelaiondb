-- Synthetic, isolated MySQL and MariaDB plan fixtures. Run only in an empty test database.
CREATE TABLE customers (
  id INT PRIMARY KEY,
  name VARCHAR(40) NOT NULL,
  country CHAR(2) NOT NULL,
  KEY customers_country (country)
) ENGINE=InnoDB;
CREATE TABLE orders (
  id INT PRIMARY KEY,
  customer_id INT NOT NULL,
  status VARCHAR(12) NOT NULL,
  total DECIMAL(10,2) NOT NULL,
  created DATE NOT NULL,
  note VARCHAR(100) NOT NULL,
  KEY orders_customer (customer_id)
) ENGINE=InnoDB;
INSERT INTO customers
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1000)
SELECT i, CONCAT('customer ', i), ELT(1 + i % 5, 'ES', 'FR', 'DE', 'PT', 'IT') FROM n;
INSERT INTO orders
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1000),
m(i) AS (SELECT a.i + (b.i - 1) * 1000 FROM n a CROSS JOIN n b WHERE b.i <= 120)
SELECT i, 1 + i % 1000, ELT(1 + i % 4, 'new', 'paid', 'shipped', 'cancelled'),
       (i % 9973) / 7, DATE_ADD('2026-01-01', INTERVAL i % 300 DAY), REPEAT('x', 40 + i % 60) FROM m;
ANALYZE TABLE customers, orders;
