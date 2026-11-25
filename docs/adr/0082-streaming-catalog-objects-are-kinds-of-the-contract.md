# ADR-082: Materialize's sinks, connections, secrets and clusters are kinds of the contract (protocol 1.3)

- **Status:** Accepted (implemented in the core; the PostgreSQL plugin reads them from its 0.1.6)
- **Date:** 2026-11-25
- **Builds on:** ADR-024 (schema and server objects), ADR-041 and ADR-042 (plugins, protocol
  versions), ADR-056 (protocol 1.1 added object kinds)

## Context
Materialize is reached through the PostgreSQL plugin (SDB-303). Its tables, views, materialized
views and indexes show in the explorer; its sources show among the tables, because `pg_class`
lists them as tables. Its other objects show nowhere: sinks, connections and secrets (in
schemas), and clusters with their replicas (outside every schema). `spelaion-driver-api`'s
`SchemaObjectKind` and `ServerObjectKind` have no kind for any of them, and nothing outside a
schema has a definition: ADR-024's server objects only have a name and a short detail.

## Decision
1. **New kinds, not existing ones reused.** `SchemaObjectKind` gains `sink`, `connection` and
   `secret`; `ServerObjectKind` gains `cluster` and `cluster_replica`. Showing a sink as a
   view or a cluster as a tablespace would put them in the wrong folder, give them actions that
   fail (Drop writes `DROP VIEW`), and make every reader of the contract guess. The kinds are
   named for what they are, not for Materialize: another streaming engine's sinks would use
   them too.
2. **A definition for server objects.** `Connection::server_object_definition` returns an
   object's DDL ending in `;`, like `object_definition`, and by default says the engine has
   none. The core says which server kinds can have one (clusters and their replicas); the
   explorer offers "Open definition" only for those, so a role never shows a button that
   always fails.
3. **Protocol 1.3, additive (ADR-042 point 3).** The kinds and the `server_object_definition`
   request are version 1.3 of `docs/plugin-protocol.md`:
   - the core never asks a plugin that chose 1.2 or older for a server object's definition: the
     engine then has none, as a compiled-in driver without it would;
   - the plugin SDK never sends the new kinds to a core that chose an older version: it leaves
     those objects out of the lists, because such a core would fail to read the whole list for
     one kind it does not know;
   - an older plugin never sends them, so nothing changes for it.
4. **What the core does with them.** They are listed, filtered and opened like the other kinds
   of their place. Drop writes Materialize's `DROP SINK`, `DROP CONNECTION` and `DROP SECRET`
   for a schema object of those kinds on the `postgres` dialect (the only one whose driver
   lists them); clusters and replicas have no drop in the explorer yet. Schema compare (ADR-070)
   does not compare them. Agents see them as they see the other schema objects.
5. **A secret's value is never read.** The plugin lists secrets by name from the catalog and
   never asks for their value; a secret has no definition, and opening it says why.
6. **Sources stay tables for now.** A source told apart from a table needs an `ObjectKind`,
   which every SQL writer of the core reads (table data tabs, edits, structure forms, ER). That
   is a change of its own; until then a source keeps showing among the tables with its right
   definition (`SHOW CREATE SOURCE`).

## Alternatives
- **Under existing kinds** (sinks as views, clusters as tablespaces): no protocol change, but
  wrong folders, failing actions, and readers that must know the engine to tell them apart.
- **A free-form "other" kind with a label:** one change for every future kind, but the core
  could not decide anything about such an object (its drop, its definition, whether agents see
  it), so every decision would move to the plugin.
- **A major version:** nothing breaks; 1.1 and 1.2 were added the same way.

## What would change this
- Another engine with objects of its own outside these kinds is a new kind each time, not a
  label; if that becomes frequent, a typed "engine object" with core-known actions could be
  considered.
