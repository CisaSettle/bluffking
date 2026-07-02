# engine

Pure-logic Texas Hold'em rules engine. No IO, no async, no DB. Given a player list, a dealer position, and an RNG, plays a complete hand and returns a `HandResult`.

<!-- U46 (dual-AI OSS review): self-contained wording — no private-repo paths/ADR links. -->
This crate is the source of truth for poker rules in BluffKing. It drives the game server (one session task per table owns a `GameHand`), the deterministic replay viewer, and the post-hand coach — the same ruleset for live play, tests, and replay.

## Design constraints

- No `rs_poker` types in any public signature — `rs_poker` stays an internal evaluation detail, swappable without breaking consumers
- No `tokio`, `sqlx`, or `axum` dependencies (pure logic — no async, no IO, no DB)
- Public data types (cards, actions, snapshots, results) derive `Debug + Clone` and usually `PartialEq`; stateful handles (`GameHand`, `PokerRng`) do not
- `cargo test -p engine` runs without a live database

## Key modules

| File | What it owns |
|---|---|
| `game.rs` | Hand orchestration — `GameHand`, `HandResult`, blind positions |
| `hand.rs` | `BoardCards`, `HoleCards`, `Street` |
| `round.rs` | Betting rounds + side-pot computation |
| `action.rs` | `PlayerAction` enum, `ActionRecord`, action validation |
| `eval.rs` | Showdown ranking via `rs_poker` (rank wrappers) |
| `rng.rs` | Seedable ChaCha20 CSPRNG (`PokerRng`) |
| `deck.rs` | Deck + shuffle |
| `event.rs` | `EngineEvent` stream for downstream persistence |

## Run tests

```bash
cargo test -p engine             # unit + integration, no DB needed
```

Integration tests live in `engine/tests/`: `full_hand.rs` plays a complete scripted hand end-to-end; `event_emission.rs` asserts the `EngineEvent` stream is well-formed.

## Test-only API surface

The `test-helpers` feature exposes `GameHand::override_hole_cards_for_test`, enabling downstream integration tests (e.g. the game server's) to inject specific hole cards. **Do not enable in production builds.** Consumers should enable it via `dev-dependencies` only.
