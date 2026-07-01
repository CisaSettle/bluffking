# PUBLISH BLOCKERS — read before making this repository public

This repo is **prepared but NOT yet cleared for public release.** It builds and
its tests pass, but the items below must be resolved first. The gate
[`scripts/prepublish-check.sh`](scripts/prepublish-check.sh) must exit `0` before
you run `gh repo create … --public`.

---

## 1. Data provenance — `engine/data/preflop_v2.json` — ✅ RESOLVED (2026-07-01)

**Resolved via Option B (clean-room regeneration).** The ranges are now generated
by `engine/examples/gen_preflop_ranges.rs` from BluffKing's OWN equity engine +
a CFR variant — reproducible byte-identically, with NO third-party charts/solver
outputs. The file's `_source` reads "clean-room" and `engine/tests/
preflop_chart_contract.rs` guards the provenance. The former problem (below) no
longer applies; kept for history.

> _Former problem:_ the `_source` once claimed derivation from third-party
> commercial poker-range products — a redistribution risk. That data was
> replaced by the clean-room generator above.

**Resolve by ONE of:**
- **Option B (recommended): regenerate** the ranges from a clean source — an
  open-source solver (e.g. OpenSpiel, **Apache-2.0** — preserve its NOTICE) or
  self-computed equity — with a **checked-in, reproducible generator** and a
  clean-room record (the implementer must not target-match the vendor charts).
  Re-run your poker-range sanity check. *Equity thresholds alone are not a
  credible GTO range model — verify product quality.*
- **Option C: exclude + generate** — keep the finished JSON out of the repo and
  produce it at build time from a clean-source generator, or make solver preflop
  advice an optional feature that degrades gracefully when the data is absent.

**Then:** remove the third-party source claims from `engine/data/README.md` and
confirm `scripts/prepublish-check.sh` passes.

**Get counsel sign-off** on the chosen option and the replacement data's
provenance. This is the one blocker an engineer cannot decide alone.

---

## 2. Credential rotation (prerequisite) 🟠

None of the credentials referenced anywhere in the upstream project's history are
present in this subset. As standard hygiene before drawing public attention to a
project that was extracted from a private repo, rotate any credentials that
appear in that private history, per your own internal runbook.

---

## 3. Contributor inbound terms (decide before the first external PR) 🟡

If you intend to keep a separate closed-source service while publishing this code
under AGPL, that dual-use depends on the maintainer holding all copyright. The
instant an external contribution could flow into the closed service, that breaks
unless contributions carry a DCO/CLA. `CONTRIBUTING.md` already states DCO
sign-off + a possible contributor agreement — confirm this is the policy you
want before accepting PRs, and confirm there is no unassigned
employee/contractor/prior-contributor copyright.

---

## 4. AGPL §13 "Source code" link — AFTER publishing (do not ship early) 🟡

If you run a modified version as a network service, AGPL §13 requires offering
corresponding source. Add a visible **"Source code"** link in your service's UI,
pinned to the **exact deployed commit** (inject the version at build time).

**Do not deploy that link until this public repo exists and is tagged** — a link
to a non-existent repo is a 404, i.e. a *failed* §13 offer. Your deployed tree
will diverge from the public tree over time, so pin to the deployed commit, not
"latest".

---

## Pre-flight checklist

```bash
cargo test --workspace                 # green
cargo clippy --workspace -- -D warnings
cargo deny check
bash scripts/prepublish-check.sh       # MUST print PASS

# only then, and only after items 1–3 above:
gh repo create <name> --public --source=. --remote=origin --push
```
