# Phase 0 — risk experiments

Throwaway code to validate the conditional decisions (ADR-001, 003, 007, 010) before
writing the product. Whatever works is rewritten properly in `crates/`.

| Experiment | Question | If it fails |
|---|---|---|
| 1. Grid | Does a canvas grid with 1 M rows scroll smoothly on WebView2 **and on WebKitGTK**? | Electron (ADR-003) |
| 2. SSH | Does `russh` reach PostgreSQL through 2 hops with a key, Pageant and the Windows OpenSSH agent? | System `ssh` (ADR-007) |
| 3. Streaming | 1 M PG rows → Arrow in the backend → binary windows to the UI, with reasonable memory and latency? | Revisit ADR-010 |

## Setup

```bash
bash testing/docker/gen-keys.sh
docker compose -f testing/docker/compose.yaml up -d --build --wait
```

## Experiment 2: `ssh-hops`

```bash
cd spikes
cargo run -p ssh-hops -- --hops spelunk@127.0.0.1:2221,spelunk@bastion2:22 --auth key --key ../testing/docker/keys/id_ed25519
```

Other methods: `--auth pageant`, `--auth openssh-pipe` (Windows), `--auth agent` (Linux/macOS).
The key with a passphrase is `keys/id_ed25519_pass` with `SPELUNK_KEY_PASSPHRASE=spelunk-test`.

It checks: authentication at each hop, tunnel to a local port, a query, **cancellation**
(PostgreSQL sends it over a second connection that must go through the same tunnel) and
streaming of 1 M rows.

## Experiments 1 and 3: `grid-stream`

A Tauri app with Glide Data Grid. The result lives in the backend as Arrow batches; the UI
requests blocks of 1,000 rows as binary Arrow IPC and only keeps the last 64.

```bash
cd spikes/grid-stream
npm install
npx tauri build --no-bundle
# Automatic mode: synthetic 1 M + Postgres 1 M, 8 s of scrolling on each, saves a report and exits.
SPIKE_AUTORUN=1 SPIKE_PG="host=127.0.0.1 port=15432 user=spelunk password=spelunk dbname=spelunk" SPIKE_RESULTS_DIR=../results ../target/release/grid-stream.exe
```

On Linux/WSL: `bash scripts/setup-wsl.sh` once, and then `bash scripts/run-spike-linux.sh`.

The benchmark does 4 s of very fast continuous scrolling (~70 rows per frame) and 4 s of
random jumps. "Empty cells" are the ones painted as "loading" because their block had not
arrived yet.

## Results

### Windows 11 · WebView2 156 · 2026-11-02

**Experiment 2 — OK**

| Method | Result |
|---|---|
| Key without passphrase | OK, 2 hops (43 ms + 75 ms) |
| Key with passphrase | OK; a wrong passphrase gives a clear error |
| Pageant 0.83 | OK, 2 hops |
| Windows OpenSSH agent | **Not tested**: the `ssh-agent` service is disabled on this machine |

- Cancellation through the tunnel: `pg_sleep(30)` cut after 1.0 s.
- Streaming through 2 hops (debug build): 1 M rows in 1.5–1.7 s (~600,000 rows/s), first row after 45 ms.
- `russh` 0.65 ships Pageant, the named pipe, OpenSSH certificates, keyboard-interactive and GSSAPI.

**Experiments 1 and 3 — OK** (release build)

| | Synthetic 1 M | Postgres 1 M (`select * from big`, all as text) |
|---|---|---|
| First row on screen | 150 ms (generation included) | **33 ms** (first batch in the backend: 25 ms) |
| Full load | 110 ms | 1.5 s |
| Scrolling: mean fps / p95 per frame | 164 fps / 6.2 ms | 164 fps / 6.2 ms |
| Slow frames (> 33 ms) | 0 % | 0 % |
| Empty cells during extreme scrolling | 10.5 % | 10.5 % |
| `fetch_window` (1,000 rows) p95 | 3.2 ms | 3.3 ms |
| Backend memory | 143 MB | 238 MB |
| Total memory (backend + WebView2 processes) | 545 MB | 652 MB |
| JS heap | 18 MB | 22 MB |

- The display runs at ~165 Hz: the grid does not drop a single frame.
- The JS heap stays at ~20 MB with 1 M rows: it confirms the "result in the backend" design.
- WebView2 at rest already adds up to ~280 MB across its processes (Windows working set,
  which counts shared memory several times): the total figure overestimates real usage.

### Linux (WSLg, Ubuntu) · WebKitGTK 2.52.6 · 2026-11-02

**Experiments 1 and 3 — provisionally OK.** WSLg had no graphics acceleration (Mesa found no
GPU and rendered in software), so it is the worst case; it still has to be measured on real
Linux with a GPU.

| | Synthetic 1 M | Postgres 1 M |
|---|---|---|
| First row on screen | 668 ms | **64 ms** (first batch in the backend: 38 ms) |
| Full load | 142 ms | 2.0 s |
| Scrolling: mean fps / p95 per frame | 50 fps / 21 ms | 55 fps / 21 ms |
| Slow frames (> 33 ms) | 0.5 % | 0.7 % |
| Slowest frame | 673 ms (one, at startup) | 43 ms |
| Empty cells during extreme scrolling | 11 % | 11 % |
| `fetch_window` (1,000 rows) p95 | 10 ms | 10 ms |
| Backend memory | 302 MB | 433 MB |

- With the WSLg display at 60 Hz, ~50–55 fps with a p95 of 21 ms is smooth scrolling, even
  with software rendering.
- Backend memory includes ~180 MB of GTK/WebKit loaded into the process at startup; what
  1 M rows add (~120 MB synthetic, ~250 MB of Postgres text) is the same as on Windows.
- The total memory of the process tree is **not valid on WSL** (it counts 170 unrelated
  processes) and the JS heap is not available in WebKit.

## Findings that change the plan

1. **Glide Data Grid is abandoned**: last release in February 2024 and it does not declare
   React 19 support (the spike uses React 18). It performs more than well enough, but the
   product should have its own canvas grid (ADR-010).
2. **The agent's key order matters**: `russh` tried another key loaded in Pageant first.
   With many keys, a server with a low `MaxAuthTries` drops the connection before reaching
   the right one. The product needs to pick the key per hop (`IdentityFile` +
   `IdentitiesOnly`) even when an agent is used.
3. **A new result must mount a new grid**: if the previous one is reused, it keeps the
   scroll position and requests rows that do not exist yet (the first measurement gave
   1.6 s because of that).
4. **Docker does not publish ports of `internal` networks**: Postgres needs an extra network
   just to publish its port.
