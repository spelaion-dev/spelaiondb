# ADR-017: SQL Server driver

- **Status:** Accepted
- **Date:** 2026-11-03

## Context
SQL Server is the database most asked for in companies after PostgreSQL and MySQL. It speaks
TDS, its SQL (T-SQL) needs no `;` between statements and wraps bodies in `BEGIN … END`
without quoting, and a batch can return several result sets.

## Decision
- **`tiberius` (pure Rust TDS) over rustls**, with ring installed as the process's crypto
  provider like the other drivers. SQL Server authentication (user and password). Windows
  authentication is left for later.
- **TLS:** the libpq-style modes map to TDS encryption: `disable` still encrypts the login
  (as every SQL Server client does, so the password never travels in the clear) without
  checking the certificate; `prefer` encrypts if the server can; `require` always encrypts
  without checking the certificate; `verify-ca` and `verify-full` check it (the name too: the
  TLS library cannot check only the authority), with the system's roots or the given CA.
- **Batches:** the dialect ends a batch at a `GO` line and splits at `;` outside quotes,
  `[…]`, comments and `BEGIN … END` / `CASE … END` blocks; after `CREATE`/`ALTER` of a
  procedure, function, trigger or view the rest of the batch is one statement (they must be
  alone in it anyway). Statements without `;` between them go as one batch.
- **Several result sets:** the contract allows a statement to repeat `Columns` → `Batch`*
  (ADR-005); the session closes each result when the next one starts.
- **Row counts:** TDS reports them, tiberius does not expose them for batches: after a
  statement without result sets that writes (`INSERT`, `UPDATE`, `DELETE`, `MERGE`), the
  driver asks `ROWCOUNT_BIG()` in the same session. Editing (ADR-014) relies on it, with
  `BEGIN TRANSACTION` instead of `BEGIN`.
- **Cancel with a TDS Attention**, which stops the running batch and keeps the session
  (transactions, temporary tables). `KILL` would end the session and needs permissions few
  users have. tiberius 0.13 read only the first of the two messages SQL Server answers an
  Attention with, and left every later response one behind; SpelunkDB uses a patched copy
  (`vendor/tiberius`, see its `PATCHED.md`) until upstream fixes it.
- **Paging and generated SQL:** table data pages use `ORDER BY … OFFSET m ROWS FETCH NEXT n
  ROWS ONLY` (with `ORDER BY (SELECT NULL)` when the user gives no order), checked like the
  other engines' pages; previews use `SELECT TOP n`; literals are `N'…'`, `0x…` and `1`/`0`.
- **Types:** `bit`, integers, floats, binary and the date and time types keep their own
  column (`datetimeoffset` as an instant in UTC); decimals and money as exact text (money with
  its four decimals), GUIDs in upper case, `xml` as text, `sql_variant` as its value's text.
  A nullable `smallmoney`/`smalldatetime` arrives as the same TDS type as `money`/`datetime`,
  so its native type shows as that.
- **Not in `spelunk mcp` yet**, like MySQL: its read-only mode would need T-SQL's own
  barriers (read-only transactions do not exist; `SET TRANSACTION ISOLATION LEVEL` is not
  enough).

## Alternatives
- **ODBC (Microsoft's driver):** complete, but a system install on every machine.
- **`KILL` to cancel:** loses the session and needs `ALTER ANY CONNECTION`.
- **Waiting for upstream to fix the Attention:** cancelling is essential and the bug mixes up
  results, which is worse than not cancelling.

## Revisit if
tiberius fixes the Attention handling (drop the vendored copy), or users need Windows
authentication, named instances (SQL Browser) or Azure AD.
