# Security Policy

## Scope

This repository contains **logic-only library crates** — `engine`,
`mental-poker`, and `mp-wasm`. It has no server, database, authentication, or
network surface of its own. Reports about a deployed service that *uses* these
crates should go to that service's operator, not here.

## Reporting a vulnerability

Please report suspected vulnerabilities **privately** — do not open a public
issue for security problems. Use GitHub's **"Report a vulnerability"** (Security
→ Advisories) on this repository, or email the maintainer at the address listed
on the repository profile. We aim to acknowledge within a few business days.

When reporting, please include: affected crate + version/commit, a minimal
reproduction, and the impact you believe it has.

## Prototype cryptography — not for production

`mental-poker`'s server-blind real-cryptography path (`crypto_real`, and its
`mp-wasm` wrapper) is a **prototype pending external audit**. It is fenced off
behind trait seams and is not wired into any production build. **Do not use it
to protect real stakes.** Findings against it are welcome but it is, by its own
disclaimer, unaudited.

## Supply chain

Dependencies are gated in CI with [`cargo-deny`](deny.toml) (RustSec advisories,
a permissive-only license allow-list, and crates.io-only sources).
