# ADR-023: Rename SpelunkDB to SpelaionDB, under the Spelaion brand

- **Status:** Proposed. It is accepted once the trademark search is done (EU, Spain and the
  US, class 9). The domain `spelaion.dev` is the project's; the crates.io and npm names were
  free on 2026-11-04 (see "The name").
- **Date:** 2026-11-04

## Context
The project is about to go public: open source, with a website and its own GitHub
organization. Being public also gives back free CI, since GitHub Actions minutes for public
repositories are not counted. Two reasons not to keep the name:

- **"SpelunkDB" is close to "Splunk"**, a well-known trademark in the same field (data
  tools). Being public makes that a real legal risk.
- **A brand, not one product.** A second product is planned: SpelaionSSH, to manage SSH
  connections. It would share the tunnels, the team's server keys and the keychain entries
  with the database client.

Renaming is cheap now, with no users but the team, and expensive later, when it breaks
installs, links, configuration files and MCP setups.

## Decision
The brand is **Spelaion**; this product is **SpelaionDB**. What both products could share
takes the brand's name; what belongs to this one takes the product's.

| What | Now | New |
|---|---|---|
| Product name (app, installers, docs) | SpelunkDB | SpelaionDB |
| GitHub | `petilolz/spelunkdb` | `spelaion-dev/spelaiondb`, in a `spelaion-dev` organization (named after the domain: `Spelaion` is taken, see "The name") |
| Rust crates | `spelunk-*` | `spelaion-*` (the SSH and configuration crates may serve SpelaionSSH later) |
| CLI binary and MCP server | `spelunk`, `spelunk mcp`, server `spelunkdb` | `spelaiondb`, `spelaiondb mcp`, server `spelaiondb` (`spelaion` stays free for a future command shared by the products) |
| Repository folder | `.spelunk/` | `.spelaion/` (brand: SpelaionSSH would read the same `tunnels/` and `known_hosts`) |
| Personal folder | `%APPDATA%\spelunkdb`, `~/Library/Application Support/spelunkdb`, `~/.config/spelunkdb` | the same places, named `spelaion` (brand). The product's own state goes in `db/` (`db/state.db`); `user.toml`, `connections/`, `tunnels/`, `known_hosts` and `trusted-workspaces.toml` stay at its root |
| Keychain service | `SpelunkDB` | `Spelaion` (brand: a tunnel password saved once serves both products; the entry names already tell connections and tunnels apart) |
| Environment variables | `SPELUNK_USER_DIR`, `SPELUNK_TEST_*` | `SPELAION_USER_DIR`, `SPELAION_TEST_*` |
| Website | none | `https://spelaion.dev`, this product at `https://spelaion.dev/db` |
| App identifier (Tauri) | `dev.spelunkdb.app` | `dev.spelaion.db` (the domain, reversed) |
| Schema URL in new files | the GitHub repository's raw file | `https://spelaion.dev/schema/db/connection.schema.json`, published by the website (the new repository's raw file until then) |
| Test environment | Docker project and kind cluster `spelunk-test` | `spelaion-test` |

The update signing key (ADR-021) has no name in it and stays. Earlier ADRs keep their text
as written: their names read with this table.

### The name
Checked on 2026-11-04:

- **Meaning:** σπήλαιον, Greek for "cave", in keeping with the old name (to spelunk is to
  explore caves).
- **Domain:** `spelaion.dev`, bought by the project.
- **Package names:** `spelaion`, `spelaiondb` and `spelaion-core` on crates.io, and
  `spelaion`, `spelaiondb` and `@spelaion/*` on npm, were free.
- **GitHub:** the `Spelaion` organization exists (created in August 2025, someone else's,
  with no public repositories). The project uses `spelaion-dev`, which matches the domain.
  If the brand is registered, GitHub's name-squatting policy is the way to ask for
  `spelaion` later.
- **Others using the name:** a Brazilian company, Spelaion, sells work-at-height equipment
  (it represents Petzl in Brazil). It is another market, but protective equipment is in
  class 9, like software, so the trademark search must look at Brazil (INPI) too, not only
  the EU, Spain and the US. No software product by that name turned up.

### Migration
Only the team has installs, but the migration is done properly, so nobody loses
connections, passwords or history.

- **Personal folder:** on its first start, if the `spelaion` folder does not exist and the
  `spelunkdb` one does, the app copies it, putting `state.db` under `db/`. It never moves or
  deletes the old folder: that stays as a backup.
- **Keychain:** a missing entry under `Spelaion` is read from `SpelunkDB` and copied to
  `Spelaion`, leaving the old one in place. This copy-on-read is kept for the 0.4 versions
  and removed afterwards.
- **Repositories:** `.spelunk/` is still read in 0.4, with a notice to rename it
  (`git mv .spelunk .spelaion`). Having both folders is an error. Trust (ADR-022) is keyed by
  the repository's path and the items' identifiers, so renaming the folder needs no review.
- **The app:** with a new identifier it installs alongside SpelunkDB, which is uninstalled
  by hand. Automatic updates are off (ADR-021), so none breaks.
- **MCP clients:** set up again with the new command (`claude mcp add spelaiondb --
  spelaiondb mcp`). The release notes say so.
- **GitHub:** transferring the repository keeps its issues, PRs and releases, and redirects
  the old URLs. Local clones only update their `origin`.

### How it is done
In one change on `dev`, verified as usual (every test against the services, smoke test)
and released as 0.4.0, the first public version. It goes before making the repository
public and before the website, so nothing public carries the old name.

## Alternatives
- **Keep SpelunkDB:** no work, but the closeness to Splunk is a risk that grows with use.
- **Everything named after the product** (`.spelaiondb/`, keychain `SpelaionDB`): simpler
  now, but SpelaionSSH would duplicate tunnels, server keys and passwords, or read another
  product's folder.
- **A `spelaion` command with subcommands now** (`spelaion db mcp`): it fits a family of
  products, but there is only one. The name stays free for when there is a second.

## Revisit if
The name is not available (then this ADR is superseded with another one), or the design of
SpelaionSSH needs a different split of what is shared.
