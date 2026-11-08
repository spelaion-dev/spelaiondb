# SDB-46 — PostgreSQL visual execution plans

All images are screenshots of the real Windows Tauri/WebView2 application, using isolated
temporary profiles and a privately owned disposable PostgreSQL database. There are no
mockups or generated images. The data is synthetic: 1,000 customers and 120,000 orders from
[`setup.sql`](../../../crates/spelaion-core/tests/fixtures/plans/postgres/setup.sql).
The query is [`query.sql`](query.sql). Before and after use the same SQL and deterministic
dataset; planner estimates, timing and buffers naturally vary between captures.

## Before

The immutable pre-feature executable has SHA-256
`A2EBCAE59F0E9579D68035A918B6A160AB3AA553753996B18F43E25C4AE72FD3`.
Its runtime corresponds to merged `77a2fb9`: SDB-17 source `982f1d2` on base `6b9b74a`,
before a later test-fixture-only change. These images show an actual
`EXPLAIN (FORMAT JSON, ANALYZE TRUE, BUFFERS TRUE)` result as one `QUERY PLAN` cell.
They were captured before this feature was built; the baseline executable was never rebuilt.

| Theme | 1400×900 | 900×600 |
| --- | --- | --- |
| Light | [Before](before-plan-light-1400.png) | [Before](before-plan-light-900.png) |
| Dark | [Before](before-plan-dark-1400.png) | [Before](before-plan-dark-900.png) |

[`before-provenance.json`](before-provenance.json) records timestamps, dimensions and hashes;
[`before-query.sql`](before-query.sql) is the exact EXPLAIN statement.

## After

| View | Light 1400×900 | Dark 1400×900 | Light 900×600 | Dark 900×600 |
| --- | --- | --- | --- | --- |
| Overview | [Image](after-plan-overview-light-1400.png) | [Image](after-plan-overview-dark-1400.png) | [Image](after-plan-overview-light-900.png) | [Image](after-plan-overview-dark-900.png) |
| Tree/table | [Image](after-plan-tree-light-1400.png) | [Image](after-plan-tree-dark-1400.png) | [Image](after-plan-tree-light-900.png) | [Image](after-plan-tree-dark-900.png) |
| Graph | [Image](after-plan-graph-light-1400.png) | [Image](after-plan-graph-dark-1400.png) | [Image](after-plan-graph-light-900.png) | [Image](after-plan-graph-dark-900.png) |
| Raw | [Image](after-plan-raw-light-1400.png) | [Image](after-plan-raw-dark-1400.png) | [Image](after-plan-raw-light-900.png) | [Image](after-plan-raw-dark-900.png) |

Also captured: the separate [ANALYZE execution warning](after-analyze-warning-light-1400.png),
[per-step findings](after-findings-dark-900.png), and [read-only mode](after-readonly-dark-1400.png).
The graph and minimum-size views use the existing Maximize results command so the plan has
room; their scrollable graph/table/inspector remain real interactive regions.

[`capture-provenance.json`](capture-provenance.json) records the after executable's SHA-256,
capture time, image hashes and successful database/profile cleanup. Private profile paths,
password values and personal connection files are excluded from published provenance.
After captures use integrated `dev` base `87b6ae0`, including ADR-037's schema picker
and ADR-038's data import workflow.
The harness also checks the configured search path and skipped-setup warning at 900×600
before dismissing that warning for the main plan captures.

## Reproduce

Build the native debug application and CLI, then run from the repository root:

```sh
node apps/desktop/e2e/plans.mjs /absolute/path/to/screenshots
```

Set `SPELAION_E2E_EXE` and `SPELAION_E2E_CLI` when the shared target directory differs from
the checkout. The harness uses the compose PostgreSQL service at `127.0.0.1:15432`, creates
and drops its own database, and starts only its own app with a private profile and free CDP
port. It verifies warning cancel/accept, real core tree/graph/raw/findings, save/open, CSV,
CLI pages, desktop `--plan`, and a read-only UPDATE refusal independently of disabled UI.
The full `apps/desktop/e2e/smoke.mjs` also opens and exercises a query's actual visual plan.

Opening a plan file never executes its contents. The screenshot query is a SELECT; ANALYZE
is explicitly confirmed by the harness. Native screenshots document behavior and layout,
not a performance benchmark or tuning recommendation.
