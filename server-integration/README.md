# Server integration — AGPL §13 Corresponding Source

This directory holds the **network-integration source** of the deployed public
postflop solver endpoint, provided so the AGPL §13 "Corresponding Source" offer
covers the *whole combined work reached over the network* (ADR-072 §3a), not
just the library:

- `gto_solve.rs` — the exact `POST /api/tools/poker/solve` axum handler as
  deployed (rate-limit / memory-gate / concurrency / cache / §13 source-offer
  logic). It is the endpoint that reaches the AGPL `gto-solver` crate over the
  network.

The four parts of the combined work: (1) the pinned AGPL `postflop-solver`
(via `gto-solver/Cargo.toml` + `Cargo.lock`), (2) the `gto-solver/` wrapper
crate, (3) this `gto_solve.rs` endpoint, (4) the `engine/` crate the wrapper
links.

> This file is a **source offer**, not a workspace member: it references the
> closed BluffKing server's own modules (axum router, rate-limit store, etc.)
> and is not built by this repo's workspace. The rest of the BluffKing server,
> clients, and website remain closed.
