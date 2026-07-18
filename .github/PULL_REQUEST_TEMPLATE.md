<!-- U57: community-health file — minimal PR template. -->
## Summary

<!-- What does this PR change, and why? -->

## Checklist

- [ ] **DCO sign-off**: every commit carries a `Signed-off-by:` trailer
      (`git commit -s`). Sign-off is required — see
      [`CONTRIBUTING.md`](../CONTRIBUTING.md). PRs missing it will be asked to amend.
- [ ] Local checks relevant to this change pass:
  - [ ] `cargo build`
  - [ ] `cargo test --workspace` (engine/crypto/solver behavior changes; not required for docs/metadata/mechanical-only changes)
  - [ ] `cargo clippy --workspace -- -D warnings`
  - [ ] `cargo fmt --all -- --check`
  - [ ] `cargo deny check`
  - [ ] `bash scripts/prepublish-check.sh` (provenance / secret scan)
- [ ] Commit messages follow Conventional Commits (`feat(scope): …`, `fix(scope): …`, `docs: …`).

## Notes for reviewers

<!-- Anything else the reviewer should know (tradeoffs, follow-ups, related issues). -->
