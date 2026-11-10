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
- **Where work is tracked:** the maintainers plan and track their work in Plane; GitHub issues
  are where anyone else reports bugs and proposes ideas. An issue we accept is linked from
  Plane, so it is not done twice.
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
  UI changes, the smoke test (`node e2e/smoke.mjs` on Windows, `node e2e/smoke-linux.mjs` on
  Linux, after building the app and the CLI as [below](#release-build-the-app-and-the-cli); see
  [The smoke test](#the-smoke-test)).
- If something could not be tested (an OS you do not have, Pageant…), say so in the pull
  request.

### Memory and jobs
DuckDB is compiled into the app from its C++ sources (`libduckdb-sys`, bundled), and that is
what decides how much memory a build needs:

- **Debug builds** (`cargo clippy`, `cargo test`, `npm run tauri dev`) peak near 8 GB with 8
  jobs (`-j 8`).
- **Release builds** compile DuckDB with optimizations, and each C++ compiler job then holds
  about 2 GB. With cargo's default of one job per thread, a 16 GB machine runs out of memory
  (8 jobs went past 9 GB). On 16 GB use **3 jobs** (`CARGO_BUILD_JOBS=3`): the build below
  built everything, DuckDB included, with a peak of 4.8 GB in 23 minutes on a 16-thread laptop.

`CARGO_BUILD_JOBS` (or `-j`) caps cargo's jobs and the C++ compiler jobs together, because the
build script shares cargo's job slots. On Linux, cap the build's memory too, so that the kernel
stops the compiler and not your desktop:

```bash
systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=0 cargo clippy --workspace --all-targets -- -D warnings
```

### Release build: the app and the CLI
The smoke test needs both the app and the `spelaiondb` CLI. Build them **in one command**, from
`apps/desktop`:

```bash
CARGO_BUILD_JOBS=3 npx tauri build --no-bundle -- -p spelaion-desktop -p spelaion-cli
```

The app ends up in `target/release/spelaion-desktop` and the CLI in `target/release/spelaiondb`.
Building them separately (`npx tauri build`, then `cargo build --release -p spelaion-cli`)
compiles DuckDB twice, about 20 minutes each with 3 jobs: DuckDB's build script has build
dependencies of its own (`serde`, `serde_json`, `flate2`…), cargo turns on their features from
all the packages of the same command only, and the app's build dependencies (Tauri's) turn on
more of them than the CLI's. Each set of packages therefore gets its own DuckDB, and so does
`cargo build --release -p spelaion-driver-duckdb` alone. Build both again with the same command
after a change and only what changed is rebuilt.

### The smoke test
Both drive the release build in `target/release` with `examples/demo` and a temporary personal
folder, so they never touch your configuration, AI clients, kubeconfig or keychain. Both need
`python examples/demo/make-shop-db.py` run once and the Docker services up
(`docker compose -f testing/docker/compose.yaml up -d --wait`).

- **Windows**: `node e2e/smoke.mjs` (from `apps/desktop`). The whole test: Playwright over
  WebView2's debugging port.
- **Linux**: `node e2e/smoke-linux.mjs` (from `apps/desktop`). WebKitGTK has no debugging port,
  so this pass speaks WebDriver to `tauri-driver` (which it starts itself), in front of
  WebKitWebDriver. It is shorter: start, repository trust, a query on SQLite, DuckDB, PostgreSQL,
  MySQL, MariaDB and SQL Server, a grid copy read back from the system clipboard, the import
  dialog taking the system clipboard's text, PostgreSQL behind the two SSH bastions, and quitting
  with a transaction open. It needs:
  - `tauri-driver` (`cargo install tauri-driver --locked`, or set `TAURI_DRIVER`) and
    WebKitWebDriver (`webkitgtk-6.0` on Arch, `webkit2gtk-driver` on Debian and Ubuntu; set
    `WEBKIT_WEBDRIVER` if it is not on the `PATH`);
  - a desktop session: Wayland with `wl-clipboard`, or X11 with `xclip`;
  - a way to ask the window to close like its close button: Hyprland (`hyprctl`), Sway
    (`swaymsg`) or an X11 window manager with `wmctrl`.

  It sets the app's environment itself (`SPELAION_USER_DIR`, `SPELAION_NO_UPDATES=1`,
  `SPELAION_MCP_CLIENTS_HOME`, an empty `KUBECONFIG`, no SSH agent and no session bus, so the
  Secret Service cannot be reached). It prints each step with its time; a failing step prints its
  name and the path of a screenshot (`e2e/screenshots/linux/failure.png`, or the folder given as
  its argument). It replaces your clipboard while it runs.

## Pull requests
- Work on a branch of your own, from an up-to-date `dev`, and open the pull request against
  **`dev`**. Nobody pushes to `dev` or `main`: they are protected, and `main` only receives
  releases.
- Before starting on an issue, say on it that you are taking it, so two people do not do the
  same work.
- Rebase on `dev` if it moved while you worked (`git fetch` and `git rebase origin/dev`).
- One change per pull request, with tests that would fail without it. Changes to the security
  barriers (read-only mode, agents' writes, repository trust) need tests that try to get around
  them.
- Commit subjects go into the release notes as they are, so write them to be read there.
- **Sign off every commit** (`git commit -s`): the `Signed-off-by` line certifies the
  [Developer Certificate of Origin](https://developercertificate.org/), that you have the right
  to submit the change under the project's license. There is no CLA.
- CI runs on every pull request and is the code reviewer: a pull request is merged when it is
  green and agrees with the rules above. One that changes a security file (listed in
  [`.github/CODEOWNERS`](.github/CODEOWNERS)) also needs a maintainer's approval.

## Reporting problems
- **Bugs and ideas:** [open an issue](https://github.com/spelaion-dev/spelaiondb/issues/new/choose).
- **Security problems:** never in a public issue. See [`SECURITY.md`](SECURITY.md).

By contributing you agree that your contribution is licensed under the
[Apache License 2.0](LICENSE), and to follow the [code of conduct](CODE_OF_CONDUCT.md).
