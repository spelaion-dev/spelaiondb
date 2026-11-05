# ADR-025: Explorer filters are personal, matched in Rust, and narrowed on the server

- **Status:** Accepted
- **Date:** 2026-11-05
- **Builds on:** ADR-024 (the explorer's folders)

## Context
Big servers have hundreds of schemas (one per tenant) or thousands of tables, and the user
wants to see a few of them. DBeaver lets each connection keep include and exclude masks for
schemas, tables and columns, adds to them from "Hide selected" and "Show only selected", and
puts the masks into its catalog queries so a big catalog is not read whole.

## Decision
1. **Patterns, not regular expressions:** `*` (or `%`) is any run of characters, `?` one
   character, `\` escapes the next one; case is ignored. `_` is literal: it is in most names.
   A name passes when it matches an include pattern (or there is none) and no exclude pattern.
   `NameFilter` in `spelaion-driver-api` is the only implementation of this.
2. **Per connection, three lists:** schemas, objects (every relation and every object of
   `list_schema_objects`) and columns. They apply to the explorer only: autocompletion, the
   table data tab and the MCP keep seeing everything.
3. **Personal, not in the connection's file:** they are kept in the personal `state.db`,
   per workspace and connection like the history preference. A team does not review someone's
   view of the tree, and a change to them does not touch the trust pinned to the repository's
   connections (ADR-022).
4. **Narrowed on the server:** `list_*_matching` in the driver contract filter in Rust by
   default; drivers may put the patterns into their catalog query as `LIKE` (case folded, with
   `!` as the escape character, which every engine writes the same way in a string literal)
   and still apply `NameFilter` to what comes back. What goes to the server must keep every
   name the exact filter keeps: an include pattern turns each non-ASCII character into `%`
   (lowercasing may change how many characters there are), and an exclude pattern is sent only
   when it is plain ASCII without `?`, so case folding on the server can never drop a name that
   should stay. A personal setting
   ("Use server-side filters", on) chooses between the two; the result is the same.

## Alternatives
- **In the connection's TOML, shared with the team:** a team could hide the same tenant
  schemas for everyone, but every change would ask each teammate to trust the connection again,
  or the trust check would need an exception that is itself an attack surface.
- **Filtering in React:** simpler for the first step, but the server-side narrowing needs the
  same patterns in Rust, and two implementations would drift.
- **Regular expressions:** more power, but each engine speaks a different dialect, so they
  could not go to the server, and few users write them for a filter.

## Revisit if
- Teams ask to share filters: then an optional `[explorer]` section reviewed like the rest.
- Someone needs filters by object kind (tables but not views), as DBeaver has per folder.
