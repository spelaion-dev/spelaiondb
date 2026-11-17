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

## The grid's sort (2026-11-16)
A header click on a paged query no longer sorts only the rows loaded: the query is read again
from the server, its pages ordered by the columns clicked (several with Shift+click), then by
the query's own `ORDER BY`, then by the key. The rules above hold, with one addition:

- **A column of the grid's sort may hold NULLs.** Engines disagree on where NULLs go
  (PostgreSQL and DuckDB after every value; MySQL, MariaDB, SQL Server and SQLite before), so
  the core writes it: `CASE WHEN c IS NULL THEN 1 ELSE 0 END, c`, NULLs last in both
  directions, as the core sorts a stored result. The comparison that fetches the next page
  follows the same rule: after a value come the greater ones and every NULL; after a NULL,
  nothing in that column; two NULLs are equal. The query's own `ORDER BY` still may only name
  `NOT NULL` columns: its NULLs go where the engine puts them, which the core does not rewrite.
- A sort the pages cannot follow exactly (a column whose type does not round-trip as a
  literal, a masked column) is not read again: the core sorts the rows it holds, and the grid
  says that only the loaded rows were sorted.
- A table data tab keeps its numbered pages (point 4): its header sort is an `ORDER BY` the
  core writes from the table's own columns, followed by the key.
  **Its NULLs stay where the engine puts them** (decided 2026-11-17, SDB-406): the header click
  writes a plain `ORDER BY "c" DESC` into the tab's ORDER BY box, which the user reads and edits,
  and the box is then the same text whether a click or the user wrote it. `CASE WHEN …` there
  would be hard to read and would turn a typed sort into a different one. So the same click puts
  NULLs first on MySQL, MariaDB, SQL Server and SQLite and last on PostgreSQL and DuckDB in a
  table data tab, while a stored result's sort and a paged query's put them last everywhere.
  Revisit if users find that difference surprising enough to trade the box's plain text for it.

## Moving through big results (2026-11-17, SDB-386)
A pagination bar (first, previous, the page's number typed to jump, next, last), "Go to row"
and "Load every row" follow the two paths above, and never claim a page that is not exact:

- **A table data tab** keeps its numbered pages (point 4): page N is `OFFSET (N - 1) × size`
  in the user's order and then the primary key, so it is exact for the rows as they are when
  it is read; a row written meanwhile moves the rows after it, as before. Its last page is
  counted first (`last_table_page`: `COUNT(*)` with the tab's filter, then the page holding the
  last row), so it is exact for the rows as they were counted. A table without a primary key
  has no order that makes a page exact: the bar says "No primary key" and why, instead of
  numbering pages it cannot keep. "Go to row N" reads the page that holds row N and selects
  it. Page N is not reached by keyset there: a keyset can only start after a row it has seen.
- **A paged query** reaches a later page, or a row past the ones loaded, only by keyset pages
  in order: the pages before it are loaded first (`load_query_rows`), so every row that
  existed throughout still appears once. Its last page is only known once every row is
  loaded, so "Last page" is "Load every row" followed by a scroll to its end. Its pages are
  never fetched by an offset or from the end (a reversed keyset would leave a gap in the
  result, which holds its rows in order).
- **"Load every row"** first says how many rows the server counts and roughly how much memory
  they take (the memory the loaded rows take, per row, times the count: `estimate`). A paged
  query then loads keyset pages of `MAX_PAGE_ROWS` until there are no more; a cancel stops the
  page being read (through `Session::cancel`, whose gate keeps a late cancel from stopping a
  later statement of the tab) and every page after it, and keeps the pages loaded before,
  each whole. A table data tab reads its SELECT without its paging, checked as a page is
  (`table_rows_sql_by_key`), as a run of the tab that its Cancel stops. Every page goes through
  the result store, which masks it (ADR-051). A result holds at most `u32::MAX` rows.

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
