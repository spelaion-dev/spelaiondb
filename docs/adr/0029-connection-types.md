# ADR-029: Connection types: a badge and safeguards per environment, decided in the core

- **Status:** Accepted (implemented)
- **Date:** 2026-11-07
- **Builds on:** ADR-009 (configuration), ADR-022 (repository trust), ADR-028 (idle sessions)

## Context
A connection's `env` is a free-text label (`dev`, `staging`, `prod`…). The core treats only
`prod` and `production` as production: the editor then asks before any statement that is not a
read (`guard.rs`), table edits need a confirmation, and `spelaiondb check` warns when agents may
write there. The UI decided "production" on its own (`env` starting with `prod`), so `prodlike`
showed a production badge the core did not honour.

DBeaver has **connection types** (Development, Test, Production and custom ones), each with a
color and per-type defaults: auto-commit, confirm SQL execution, confirm data changes, smart
commit, close idle transactions and close idle connections. Users pick one per connection and
can also give a connection its own color. Teams want to agree on them: what production looks
like and which safeguards it carries.

## Decision
1. **`env` names a connection type by id.** It is matched case-insensitively and `production`
   is an alias of `prod`, so every existing file keeps its meaning. A label that names no type
   stays a plain label with no badge and no behaviour, as before. `schema_version` stays 1: the
   connection files did not change shape.
2. **Three built-in types:** Development (`dev`), Test (`test`) and Production (`prod`).
   Production has *confirm data changes* on, which is exactly what production did before;
   Development and Test only have a badge.
3. **A type has** an id (lowercase letters, digits, `-` and `_`), a name, a badge **tone**, a
   description, and three safeguards:
   - `confirm_execution`: the editor asks before running any script on the connection.
   - `confirm_data_changes`: the editor asks before any statement that is not a read, and table
     edits need a confirmation (what production did).
   - `close_idle_after = <seconds>`: the session is closed after that long without being used
     (below).
4. **It shows as a badge, as Production's did:** a small badge with the type's name next to the
   connection in the explorer and in its editor's toolbar (Production keeps its `PRODUCTION`
   badge there). The tone only tints the badge and comes from a fixed palette the app draws for
   the light and the dark theme: `neutral`, `green`, `blue`, `amber` and `red`. Development is
   green, Test amber, Production red and a custom type neutral unless it says otherwise. There
   is no free color: a palette keeps every badge legible in both themes and the app consistent.
   No stripes or other marks on the sidebar, the tabs or the toolbar.
5. **Where they are defined:** `connection-types.toml` in the repository's `.spelaion/` (the
   team's, shared through git) and in the personal directory (the user's), with the same
   format: `schema_version` and one `[types.<id>]` table per type. Both layers may define the
   same id (also a built-in one, to change its badge).
   - **Looks** (name, tone, description) come field by field from the personal layer, then the
     team's, then the built-in definition: a user may change a type's badge for themselves.
   - **Safeguards only add up:** a safeguard is on if any layer that defines the type turns it
     on, and the shortest `close_idle_after` wins. No file can switch one off, so Production
     always confirms data changes, and a user cannot loosen a team type for themselves either.
   - A layer whose file does not load does not silently drop its types: every connection with
     a type label is refused until the file is fixed (a missing type would loosen it).
6. **Team types follow repository trust (ADR-022).** They are used only from a trusted
   repository. The file is part of the trust snapshots, as `connection_types`, with one field
   per type and setting: its name and every safeguard, *without* the tone and the description.
   If a type changed, is new, or disappeared since the user trusted the repository, the
   connections that use that type are refused until the change is reviewed (the same review as
   for a changed connection); the other connections keep working. A change of tone or
   description needs no review.
7. **Every decision is made in the core.** The resolver gives each connection its type and
   safeguards; the app only shows them. `guard.rs` lists what to confirm (`Risk::Execution`,
   `Risk::DataChange`) and refuses a run that skipped the question (`check_script`); table edits
   go through `check_edit_policy` with the same safeguards. The connection list carries the type
   (name, tone, whether it is Production, its safeguards) to the UI, which no longer reads `env`
   itself.
8. **Closing idle connections** follows ADR-028: a session idle (no user use; keep-alives do not
   count) for `close_idle_after` seconds is closed and its tab says so ("closed after N minutes
   idle"), with the usual Reconnect button. Nothing reconnects or re-runs by itself. It is never
   closed while a transaction is open (the driver's keep-alive check says so), and engines that
   cannot tell (SQLite and DuckDB files) are not closed at all, so no uncommitted work is ever
   rolled back behind the user's back. Agents' sessions do not use it.
9. **DBeaver import** maps DBeaver's connection type to the closest built-in type: its own
   `dev`, `test` and `prod` are ours; a custom type that confirms execution or data changes
   becomes Production (never looser than it was); otherwise its id or name decides (`prod`,
   `live` → Production; `test`, `qa`, `uat`, `stag` → Test) and Development when nothing does.
   DBeaver's colors, of types and connections, are not imported.

## Left out
- **A per-connection color** (DBeaver's `connectionColor`) was considered and left out for
  visual consistency: the type's badge already tells environments apart, and a free color per
  connection would bring back the legibility problems the palette avoids. If it ever comes back,
  it would be a small dot next to the connection, not a stripe.
- **Future work:** DBeaver's **auto-commit**, **smart commit** and **close idle transactions**
  need a manual-commit mode the app does not have: every statement commits as it runs. They are
  not in the file format or the UI; when a manual-commit mode exists, they become type settings
  with the same "only add up" rule.

## Alternatives
- **Personal types only.** Simpler, but a team cannot agree on what production looks like or
  which safeguards a type carries, which is the point of sharing connections.
- **Free colors (a light and a dark one per type), shown as stripes.** What a first version
  did; arbitrary colors are hard to keep legible in both themes and the stripes added noise
  everywhere. A palette and the existing badge style are enough to tell environments apart.
- **Team types override personal ones completely.** The team's badges would win over a user who
  needs other tones (color blindness); with safeguards that only add up, letting the user choose
  the looks is safe.
- **Pinning the tone too.** Every change of tone would ask each teammate to review a change
  that cannot hurt anyone; it is still visible in the diff in git.
- **A `production = true` flag on custom types.** "Production" only ever meant confirming data
  changes; the safeguard says that directly, and the built-in Production type keeps the id
  `prod` everything else already uses.

## What would change this
- If users really need to switch a safeguard off for one connection of a type, a per-connection
  override in the personal layer only (never in the repository) would be the place.
- A manual-commit mode brings the three settings above back.
