# Server integration — AGPL §13 Corresponding Source

This directory holds the **network-integration source** of the deployed public
postflop solver endpoint, provided so the AGPL §13 "Corresponding Source" offer
covers the *whole combined work reached over the network* (ADR-072 §3a), not
just the library.

## Files

- `gto_solve.rs` — the exact `POST /api/tools/poker/solve` axum handler as
  deployed (rate-limit / memory-gate / concurrency / cache / §13 source-offer
  logic). It is the endpoint that reaches the AGPL `gto-solver` crate over the
  network.
- `texas-h5-Cargo.lock` (U47) — the **deployed server binary's exact dependency
  resolution**, i.e. the §13 reproduce artifact. It pins every version — including
  the git-pinned `postflop-solver` commit — as actually built and deployed. Use
  *this* lockfile (not the OSS repo's root `Cargo.lock`, which resolves only the
  published crates) to reconstruct the deployed combined work.
- `texas-h5-Cargo.toml` (U47) — the deployed server workspace manifest, provided
  **for reference only**. It lists a `server` workspace member that is the closed
  BluffKing server and is **not published** here; it documents how the published
  crates (`engine`, `mental-poker`, `gto-solver`) sit inside the deployed
  workspace, and is not buildable as-is from this repo.

## The four parts of the combined work

(1) the pinned AGPL `postflop-solver` (declared in `gto-solver/Cargo.toml`, its
exact commit resolved in `server-integration/texas-h5-Cargo.lock`), (2) the
`gto-solver/` wrapper crate, (3) this `gto_solve.rs` endpoint, and (4) the
`engine/` crate the wrapper links.

## Why this file is a source offer, not a build target (U50)

`gto_solve.rs` is a **source offer**, not a workspace member. It references
sibling modules of the closed BluffKing server (its axum router, rate-limit
store, etc.) that are not published, so **this repo's workspace does not compile
it** — nothing here builds `gto_solve.rs` on its own. That is expected for a §13
offer of the *combined* work rather than a standalone crate.

To compile the exact deployed handler, a downstream would drop `gto_solve.rs`
into a server crate that supplies those sibling modules and resolve it against
`texas-h5-Cargo.lock` (which pins `gto-solver` + `postflop-solver` at the deployed
versions). We intentionally do **not** vendor a synthetic wrapper crate around it
here — that would ship a fork of the closed server's private module surface. The
rest of the BluffKing server, clients, and website remain closed.
