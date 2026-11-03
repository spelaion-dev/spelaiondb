create table product (id integer primary key, name text not null, price real, photo blob);
insert into product values (1, 'Lamp', 19.5, x'00ff'), (2, 'Desk', 120, null), (3, 'it''s a chair', null, null);
