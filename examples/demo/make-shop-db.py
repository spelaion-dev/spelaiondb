"""Creates data/shop.db, a small SQLite database to try SpelaionDB without servers."""
import random
import sqlite3
from pathlib import Path

path = Path(__file__).parent / "data" / "shop.db"
path.parent.mkdir(exist_ok=True)
path.unlink(missing_ok=True)
db = sqlite3.connect(path)
db.executescript(
    """
    create table customer (id integer primary key, name text not null, email text unique, country text);
    create table product (id integer primary key, name text not null, price real not null, stock integer default 0);
    create table orders (
        id integer primary key, customer_id integer not null references customer,
        created text not null, total real not null
    );
    create index orders_customer on orders (customer_id);
    create view big_orders as select * from orders where total > 500;
    """
)
random.seed(7)
countries = ["Spain", "Mexico", "Argentina", "Chile", "Colombia", "Peru"]
db.executemany(
    "insert into customer (name, email, country) values (?, ?, ?)",
    [(f"Customer {i}", f"customer{i}@example.com", random.choice(countries)) for i in range(1, 501)],
)
db.executemany(
    "insert into product (name, price, stock) values (?, ?, ?)",
    [(f"Product {i}", round(random.uniform(2, 900), 2), random.randint(0, 300)) for i in range(1, 201)],
)
db.executemany(
    "insert into orders (customer_id, created, total) values (?, ?, ?)",
    [
        (random.randint(1, 500), f"2026-{random.randint(1, 10):02d}-{random.randint(1, 28):02d}", round(random.uniform(5, 2000), 2))
        for _ in range(50_000)
    ],
)
db.commit()
print(path)
