# ADR-001: Core in Rust

- **Status:** Conditional on phase 0
- **Date:** 2026-11-02

## Context
All the code is written by an AI agent and nobody reviews it line by line. The core (SSH,
drivers, MCP, configuration, storage) is the most expensive part to rewrite. A database
client is almost always waiting on the network, so the language's raw performance is not
the main criterion.

## Decision
The core is written in Rust. The deciding criterion is that **the compiler acts as the
reviewer**: exhaustive `match` (adding a variant flags every place that does not handle
it), no implicit nulls, and data races that do not compile.

Rules so the agent does not get stuck:
- Concurrency with actors: tokio tasks that communicate through channels with owned data.
- No lifetimes in public APIs; `Arc`, `String` and owned types.
- `unsafe` forbidden except for justified FFI.
- `cargo clippy -- -D warnings` always clean.
- A workspace split into crates and `cargo check` in the work loop to shorten builds.

## Alternatives
- **Go:** the simplest for the agent and with the best driver ecosystem, but its compiler
  catches far fewer errors. It is plan B if Rust causes too much friction.
- **TypeScript/Node:** a single language, but with no runtime guarantees and memory
  problems with large results.

## Revisit if
- In phase 0 the agent repeatedly gets stuck on async type errors.
- Every build iteration takes minutes and slows the work down.
