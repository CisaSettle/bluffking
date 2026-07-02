<!-- U57: community-health file — Keep a Changelog. -->
# Changelog

All notable changes to the BluffKing open-source subset (`engine`,
`mental-poker`, `mp-wasm`, `gto-solver`) are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project aims to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> **Deployed-source snapshots.** The exact source of the deployed public postflop
> solver (the AGPL §13 Corresponding Source) is not tracked by the version below;
> it is keyed to immutable `solver-src-*` git tags. The current tag is whatever
> `GET /api/tools/poker/solve/source` returns — see
> [`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md).

## [Unreleased]

## [0.1.0] — 2026-07-01

Initial public release of the open-source subset.

### Added

- `engine/` — pure-Rust No-Limit Texas Hold'em rules engine, Monte-Carlo equity
  estimator, and local post-hand solver/coach (no IO, no async, no DB).
- `mental-poker/` — verifiable commit–reveal dealing with a signed, append-only
  hash-chain transcript and offline verifiers (`pf_verify`, `mp-verify`), plus
  the real server-blind `crypto_real` path (re-encryption-mixnet shuffle +
  threshold decryption).
- `mp-wasm/` — `wasm-bindgen` surface over `mental_poker::crypto_real` so a
  browser can run the verifiable dealing locally (detached workspace).
- `gto-solver/` — wrapper over the AGPL-3.0 `postflop-solver` (Discounted-CFR)
  behind BluffKing engine types; powers the free public
  `POST /api/tools/poker/solve` study tool.
- `server-integration/` — the deployed solver endpoint source and the deployed
  binary's lockfile, published as the AGPL §13 Corresponding Source of the
  combined network work.
- Cross-vendor AI audit (`audits/`), supply-chain gate (`cargo deny`), and a
  prepublish provenance/secret CI gate (`scripts/prepublish-check.sh`).

### License

- AGPL-3.0-only (driven by the AGPL `postflop-solver` dependency); the BluffKing
  brand is not licensed with the code — see [`TRADEMARKS.md`](TRADEMARKS.md).

[Unreleased]: https://github.com/CisaSettle/bluffking/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/CisaSettle/bluffking/releases/tag/v0.1.0
