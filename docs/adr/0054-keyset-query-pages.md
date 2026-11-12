# ADR-054: Query results are paged only when each page can be fetched exactly: keyset pages over the table's primary key

- **Status:** Accepted
- **Date:** 2026-11-12
- **Builds on:** ADR-010 (results live in the core), ADR-014 (the primary key comes from the
  database), ADR-051 (masking)

## Context
The editor fetched a plain `SELECT` a page at a time by running it again with `LIMIT n OFFSET
m` (SQL Server: `ORDER BY (SELECT NULL) OFFSET … FETCH …`). That is not stable. Without an
`ORDER BY`, or with one that has ties, the server may return rows in a different order on each
run (a new plan, a parallel scan). A row inserted or deleted between two pages shifts every row
after it. The grid could show a row twice and never show another, while saying it had loaded
them all. Table data tabs had the same problem: they had no order unless the user sorted.

## Decision
1. **A query is paged only when every page can be fetched exactly.** It must be a single plain
   `SELECT` of one table (the shapes the core already pages, without joins), the table must
   have a primary key the catalog reports (read again on every page, as edits do), and the
   result must hold every key column. Its `ORDER BY`, if any, may only name columns of that
   table that are in the result and `NOT NULL`, with `ASC` or `DESC`. Every column the pages are
   ordered by must have a type whose values the core writes back exactly as literals (integers,
   decimals, text, UUIDs, dates).
2. **Keyset pages.** The pages are ordered by the user's `ORDER BY` and then the key. The next
   page asks for the rows after the last one shown, by those columns' values (read from the
   stored result in the core, masked or not), never with `OFFSET`. Every row that exists
   throughout appears once, whatever is written between pages: rows inserted after the last
   page shown appear later, and deleted rows are simply not fetched.
3. **Everything else runs once, as written**, without a `LIMIT` added: its rows stream into the
   grid as they did before paging existed. The history keeps the user's own text, agents are
   unaffected, and the grid's "loaded rows only" notes keep applying to paged results.
4. **Table data tabs are ordered by the primary key** when the table has one, after the order
   the user chose, so their numbered pages no longer depend on the server's order.

## Alternatives
- **`OFFSET` with the key appended to the order.** Stable against plan changes and ties, but a
  row inserted or deleted between pages still shifts the rest.
- **A server-side cursor kept open between pages.** Exact, but it holds a transaction (and on
  some engines locks or a snapshot) open while the user reads, which ADR-031's transaction rules
  and the health checks would have to treat as the user's transaction.
- **Paging any query by wrapping it** (`ROW_NUMBER()` over a derived table). Without a unique
  key in the result, nothing makes the order total.

## What would change this
- If users need pages of joins or views, a unique key in the result (several tables' keys)
  could be accepted the same way, with a test that writes between pages.
- If a type's literals are proven to round-trip on every engine (timestamps), it joins the list.
