# Contributing

Thanks for your interest. This repo is the open core of a larger product: the
logic-only crates (`engine` + `mental-poker` + `mp-wasm`), the `gto-solver`
crate, and the published source of the deployed solver endpoint
(`server-integration/gto_solve.rs`).

## Build &amp; verify

```bash
cargo build
cargo test --workspace                    # engine + mental-poker + gto-solver, no DB needed
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
cargo deny check                          # advisories + licenses + sources
```

All four must pass. CI runs the same gates (`.github/workflows/ci.yml`).

## Style

- `cargo fmt` (rustfmt defaults) and **zero** clippy warnings (`-D warnings`).
- Keep crates pure: no `tokio` / `sqlx` / `axum` in `engine`; no `rs_poker`
  types in `engine`'s public signatures.
- Prefer real tests over mocks for cross-module behaviour.
- Conventional-commit messages: `feat(scope): …`, `fix(scope): …`, `docs: …`.

## Contributor inbound terms (please read)

The maintainer ships these crates under **AGPL-3.0** *and* uses them, as the sole
copyright holder, inside a separate **closed-source** service. To preserve the
ability to do both, contributions are accepted under a **Developer Certificate of
Origin** sign-off (`git commit -s`), and the maintainer may ask for a short
contributor agreement before merging non-trivial code that could flow into the
closed service.

If you are not comfortable with those terms, you are still free under AGPL to
fork, modify, and self-host — you just need to offer your own corresponding
source (including over a network, per AGPL §13) and to rebrand (see
[`TRADEMARKS.md`](TRADEMARKS.md)).

## Reporting bugs vs. security issues

Functional bugs → GitHub issues. Security problems → see [`SECURITY.md`](SECURITY.md)
(private disclosure).
