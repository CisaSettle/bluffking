# Third-Party Notices

BluffKing bundles or depends on the following third-party software. Each is
listed with its license and (where the license requires it) the source-code
availability information for the exact version we deploy.

This file satisfies the **attribution** obligation for the dependencies below
(per ADR-072 §D6). It is **not** the AGPL §13 Corresponding-Source offer for our
own crates — that offer (a login-free "Source code (AGPL-3.0)" link pinned to the
deployed engine commit) is specified in ADR-072 §3 and, per ADR-072 §D4, must
not ship before the public engine repository exists. See "Compliance status"
below.

---

## postflop-solver

- **Used by:** the `gto-solver` workspace crate, which powers the public study
  endpoint `POST /api/tools/poker/solve` (the "GTO Solver (CFR)" tool). The CFR
  equilibrium computation is performed by this dependency.
- **Upstream project:** b-inary/postflop-solver — an open-source Discounted-CFR
  postflop equilibrium solver.
- **License:** **AGPL-3.0-or-later** (GNU Affero General Public License,
  version 3 or later).
- **Repository:** https://github.com/b-inary/postflop-solver
- **Exact deployed version (pinned commit / Corresponding Source):**
  `9d1509fe5077d019825f833eed04b16d342dfda1`
  (https://github.com/b-inary/postflop-solver/tree/9d1509fe5077d019825f833eed04b16d342dfda1)
- **Modifications:** none — consumed unmodified as a pinned-commit git
  dependency (`default-features = false`, `features = ["rayon"]`). See
  `gto-solver/Cargo.toml`.

### AGPL §13 (network use) — disclosure for this dependency

Because `postflop-solver` is AGPL-3.0 and is reached over the network by the
public `POST /api/tools/poker/solve` endpoint, AGPL §13 requires that users
interacting with it over the network be offered the Corresponding Source of the
**exact version running**. The unmodified upstream source for the deployed
commit is available, free and without login, at the pinned-commit URL above —
this entry is its **attribution** record.

F1 (source completeness): the §13 Corresponding Source offered to network users
(`POSTFLOP_SOLVER_SOURCE_URL` / `GET /api/tools/poker/solve/source`) is the source
of the **combined deployed work**, not the upstream library alone. That combined
work is upstream `postflop-solver` (pinned, above) **plus** our `gto-solver/`
wrapper crate, our `server/src/handlers/gto_solve.rs` network integration, and the
`Cargo.lock` / `gto-solver/Cargo.toml` versions needed to rebuild it. Operators
must therefore set `POSTFLOP_SOLVER_SOURCE_URL` to the published source of the
whole deployed work at its deployed SHA (which pins the upstream commit in its
lockfile), not to the upstream tree alone. See ADR-072 §3a.

---

## Compliance status (ADR-072 / ADR-071)

The AGPL-backed solver endpoint is **gated OFF by default** in the server. It
mounts only when an operator sets **BOTH** `ENABLE_POSTFLOP_SOLVER=true` **and**
`POSTFLOP_SOLVER_SOURCE_URL` to a login-free `http(s)://` link offering the
Corresponding Source of the exact deployed AGPL solver version (F4 — structural
AGPL §13 enforcement; see `server/src/handlers/poker_tools.rs`). If the source
URL is unset/blank the route does **not** mount even with the enable flag set, so
the server can never expose a §13-non-compliant public endpoint with nothing in
the API/UI offering the source. When both are set, the server also serves a
login-free network-user offer at `GET /api/tools/poker/solve/source` and every
`/api/tools/poker/solve` response echoes the URL so the web UI renders a visible
"Source code (AGPL-3.0)" link. (This file remains the *attribution* record; the
runtime source link is the *network-user* offer.) The remaining free poker-math
tools (equity / outs / pot-odds / scare-card / Spot Analyzer) are backed only by
permissive-licensed code (MIT / Apache-2.0 / BSD) and stay public
unconditionally.

**Update 2026-07-01 — the public repo now exists and the solver is enabled.**
The owner authorized publishing the AGPL public subset early (ahead of the
ADR-071 10k-user gate) specifically to enable the free postflop solver; the one
hard blocker (preflop-data provenance) was resolved by the clean-room
regeneration (`engine/examples/gen_preflop_ranges.rs`; `_source` = clean-room,
no third-party charts). The Corresponding Source of the deployed combined work
is published, login-free, covering ALL FOUR parts of the §3a definition: (1) the
pinned AGPL `postflop-solver` (via the published `gto-solver/Cargo.toml` +
`Cargo.lock`), (2) the `gto-solver/` wrapper crate, (3) the exact deployed
endpoint `server/src/handlers/gto_solve.rs` (published at
`server-integration/gto_solve.rs`), and (4) the `engine/` crate the wrapper
links. It is pinned to an **immutable tag** so the offer matches the exact
deployed version:
**https://github.com/CisaSettle/bluffking/tree/solver-src-2026-07-01**.
`POSTFLOP_SOLVER_SOURCE_URL` points there and `ENABLE_POSTFLOP_SOLVER=true` in
production, so `/api/tools/poker/solve` mounts and every response + the
`GET /solve/source` endpoint offer that link (the web UI renders a visible
"Source code (AGPL-3.0)" link on the solver page). This satisfies the AGPL §13
network-user offer for the solver endpoint per ADR-072 §3a. The broader ADR-072
§3 engine-wide footer/About "Source code" link (build-SHA-pinned, covering the
whole deployed engine) remains a follow-up and is tracked in ADR-072.
