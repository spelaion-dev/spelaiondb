# ADR-042: Our plugins live in their own repositories, pinned by this one

- **Status:** Accepted (not implemented: it changes how ADR-041's stages are carried out)
- **Date:** 2026-11-10
- **Supersedes:** in ADR-041, "built from this repository" and "always the same build as the
  core" (point 5, "Ours") and the check of downloaded engines against the update key; in
  ADR-013, the monorepo, for plugins only
- **Builds on:** ADR-041 (engines as plugins), ADR-012 (CI is the reviewer), ADR-021 (signed
  updates), ADR-022 (trust pinned to content)

## Context
ADR-041 makes every engine a plugin and kept ours in this repository, built together with the
core, so that a plugin and the core were always the same build and nothing had to be said about
compatibility. That has a price the owner does not want to pay: with every plugin in the
workspace, each CI run and each release builds all of them again, whether or not anyone touched
them (the Rust cache keeps dependencies, not our own crates), and they all share one cache of
10 GB that is already full. The point of taking engines out of the core was that work nobody
changed is not done again.

With a repository per plugin, "the same build" is gone, and three things ADR-041 got for free
need an answer: how the core and a plugin know they understand each other, where the two are
tested together, and how the app knows a plugin's file is the one we built.

## Decision
**Each plugin of ours has its own repository. This repository keeps the contract and pins, by
hash, the exact plugin builds it ships and tests.**

1. **One repository per plugin**, under `spelaion-dev`, named `spelaiondb-plugin-<engine>`
   (`spelaiondb-plugin-duckdb`), with this repository's licence, sign-off and rules: protected
   branches, CI as the only reviewer, owners for what is sensitive, an `AGENTS.md` that points
   here. They start from one template, and their CI comes from workflows kept in one place, so
   a change to how plugins are built is made once. A patched library in `vendor/` goes with the
   plugin that uses it. A repository is created when its stage of ADR-041 starts, not before.
2. **What stays here:** the contract (`spelaion-driver-api`), the protocol's specification,
   the plugin-side library, a conformance test set that any plugin can run against itself, the
   dialects and barriers, and the core's tests that run on every engine. Plugins take the
   library as a git dependency on a tag of this repository (`plugin-sdk-vX.Y.Z`); it is not
   published on crates.io.
3. **A protocol version replaces "the same build".** The protocol has a version
   (`major.minor`), exchanged in `initialize`. Within a version, changes only add: new optional
   messages and capabilities. A change that breaks is a new version: the core keeps speaking
   the previous one until every pinned plugin has moved, and drops it in a release of its own.
   The core refuses a plugin whose version it does not speak, and says which it expected.
4. **This repository pins what it ships.** A file here (`plugins.toml`, under
   `.github/CODEOWNERS`) names, for each plugin, its repository, its release, its protocol
   version and the SHA-256 of its archive for each platform. Moving a pin is a pull request
   here.
   - **CI here never compiles a plugin:** it downloads the pinned builds, checks their hashes
     and runs the core's every-engine tests through them.
   - **The release** puts the pinned builds of the five current engines inside the installers
     and next to the CLI.
   - **The app carries the pins.** Before it starts a plugin, shipped or downloaded, the core
     checks the file's hash against its pin and refuses a file that does not match. An engine
     downloaded on first use (ADR-041, stage 3) comes from its repository's release and is
     accepted only with the pinned hash. So the update key (ADR-021) stays in this repository
     only: a plugin's repository holds no secret.
   - A plugin fix reaches users with the app release that moves its pin, never by itself.
5. **The gate of ADR-041 is the pull request that moves the pin.** Migrating an engine is one
   pull request here: it pins the plugin and removes the built-in driver. Its CI is the gate's
   part (a), the tests passing through the plugin; its description carries (b), the measured
   times, and (c), the connection test. In the plugin's own repository, CI builds it for every
   platform and runs its type zoo against its engine and the conformance set.
6. **A plugin's release** builds the four platforms and publishes each archive with its
   SHA-256. Nothing is signed there: what makes a build ours is its hash in a pin that a
   reviewed pull request put here.
7. **Working on both at once.** A development build of the core can be pointed at a plugin
   built on the same machine, with no pin; a release build cannot, by any setting, variable or
   repository file.
8. **One tracker.** The work stays in this project's tracker; an item says which repository it
   is in.

Other people's plugins are as ADR-041 left them (point 5, stage 4).

## Consequences
- This repository's CI and builds stop compiling engines altogether, and a plugin is built
  only when it changes, with a cache of its own.
- **A protocol change is no longer one pull request.** It is one here (the new version, with
  the old one still spoken), one in each plugin, then the pins, then removing the old version.
  Until 1.0 the protocol changes often, so this is the main cost of the decision.
- A change that needs the core and a plugin together cannot be tested in one pull request: the
  pin is where they meet. A pin pull request can fail for a plugin that passed its own CI.
- More repositories to keep in order: branch protection, owners, CI and rules, once per plugin.
  The template and the shared workflows reduce it; they do not remove it.
- The hash pin guarantees the app ships exactly the bytes a reviewed pull request named. It
  does not prove those bytes came from the plugin's source: that rests on each plugin
  repository's protected branches and CI.
- Test servers are needed in two places: here for the core's tests, and in each plugin's
  repository for its type zoo.
- Agents and people working on a plugin and the core need both checkouts.

## Alternatives
- **Plugins in this repository, with CI that builds and tests only the ones that changed:**
  keeps "the same build" and one pull request per protocol change, and was the first
  recommendation. Not chosen: untouched plugins still share this repository's builds, releases
  and cache.
- **One repository for all our plugins:** one place to administer, but one cache again, and
  releases and tags of different plugins in the same list.
- **Signing each plugin's release with the update key:** the key in every repository; leaking
  it once means shipping code to every user (ADR-021).
- **Publishing the plugin-side library on crates.io:** a public promise about an interface
  that is experimental until 1.0.

## Revisit if
- Before 1.0, keeping two protocol versions alive across the repositories holds the work back:
  the plugins of ours come back here until the protocol settles.
- The number of repositories becomes more work than the builds it saves: one repository for
  all of them.
- A way to prove that a release's bytes come from its source (build attestations) is simple
  enough to require of every pin.
