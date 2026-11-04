# Contributing to SpelaionDB

Thanks for wanting to help. Bug reports, ideas and pull requests are all welcome.

## Before you start
- **Read [`AGENTS.md`](AGENTS.md).** It holds the project's rules: where each piece lives, what
  must never happen (secrets in logs or files, security decided outside the core, a changed SSH
  key accepted with one click…) and what "done" means. It is written for AI coding agents,
  which write most of this code, and it applies to people just the same.
- **Decisions live in [`docs/adr/`](docs/adr/README.md).** If a change contradicts an ADR,
  propose a new ADR that supersedes it instead of working around it.
- **Big changes:** open an issue first, so we can agree on the approach before you spend time on
  it.
- Everything in the repository is in English: code, comments, docs, UI text, tests and commit
  messages.

## Building and testing
With Rust, Node and the [Tauri prerequisites](https://tauri.app/start/prerequisites/), see
"Build from source" in the [README](README.md). The checks a pull request must pass:

```bash
cargo fmt --all --check
```

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

The tests need real databases and SSH bastions, which run in Docker (see
[`testing/docker`](testing/docker/README.md)). Without their variables the tests that need them
are skipped, and a skipped test does not count as tested:

```bash
bash testing/docker/gen-keys.sh
```

```bash
docker compose -f testing/docker/compose.yaml up -d --build --wait
```

```bash
SPELAION_TEST_PG=127.0.0.1:15432 SPELAION_TEST_MYSQL=127.0.0.1:13306 SPELAION_TEST_MARIADB=127.0.0.1:13307 SPELAION_TEST_MSSQL=127.0.0.1:11433 SPELAION_TEST_SSH=127.0.0.1:2221 cargo test --workspace
```

- If dependencies change: `cargo deny check`.
- If the desktop app changes: `npm run build` in `apps/desktop` with no type errors and, if the
  UI changes, the smoke test (`node e2e/smoke.mjs`, after `npx tauri build --no-bundle`).
- If something could not be tested (an OS you do not have, Pageant…), say so in the pull
  request.

## Pull requests
- Open them against **`dev`**. `main` only receives releases.
- One change per pull request, with tests that would fail without it. Changes to the security
  barriers (read-only mode, agents' writes, repository trust) need tests that try to get around
  them.
- Commit subjects go into the release notes as they are, so write them to be read there.
- **Sign off every commit** (`git commit -s`): the `Signed-off-by` line certifies the
  [Developer Certificate of Origin](https://developercertificate.org/), that you have the right
  to submit the change under the project's license. There is no CLA.
- CI runs on every pull request and is the code reviewer: a pull request is merged when it is
  green and agrees with the rules above.

## Reporting problems
- **Bugs and ideas:** [open an issue](https://github.com/spelaion-dev/spelaiondb/issues/new/choose).
- **Security problems:** never in a public issue. See [`SECURITY.md`](SECURITY.md).

By contributing you agree that your contribution is licensed under the
[Apache License 2.0](LICENSE), and to follow the [code of conduct](CODE_OF_CONDUCT.md).
