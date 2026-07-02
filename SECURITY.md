# Security Policy

## Scope

This repository is the open-source subset of BluffKing: the library crates
`engine`, `mental-poker`, `mp-wasm`, and `gto-solver`, plus — as the AGPL §13
Corresponding Source for the deployed solver — the exact endpoint handler
`server-integration/gto_solve.rs`. Bugs in the **code here** are in scope,
including a DoS / panic / resource-exhaustion / correctness issue in `gto-solver`
or `server-integration/gto_solve.rs` (the source of the live `POST
/api/tools/poker/solve` handler — published as a source offer, not a runnable
server crate). Operational issues of the deployed service (infrastructure,
availability, rate-limit tuning) go to the service operator, not here.

## Reporting a vulnerability

Please report suspected vulnerabilities **privately** — do not open a public
issue for security problems. Use GitHub's **"Report a vulnerability"** button
under this repository's **Security → Advisories** tab (private vulnerability
reporting is enabled), which opens a private advisory visible only to you and the
maintainers. We aim to acknowledge within a few business days.

When reporting, please include: affected crate + version/commit, a minimal
reproduction, and the impact you believe it has.

## Real cryptography — prototype-grade, play-money only

`mental-poker`'s server-blind real-cryptography path (`crypto_real`, and its
`mp-wasm` wrapper) runs in production **only for opt-in, all-human "engine-blind"
tables** (ADR-070). It is **cross-vendor AI-audited (Claude + OpenAI Codex) but
NOT yet audited by a paid external cryptography firm** — treat it as
prototype-grade. Note in particular the interim re-encryption-shuffle argument's
stated **~2⁻²⁶ soundness bound** at N=52 (see
`mental-poker/src/crypto_real/shuffle.rs`), not cryptographic negligibility. It
is **play-money only — do not rely on it to protect real stakes.** Findings
against it are especially welcome.

## Supply chain

Dependencies are gated in CI with [`cargo-deny`](deny.toml) (RustSec advisories,
a permissive-only license allow-list, and crates.io-only sources).
