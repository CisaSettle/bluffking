# engine

Pure-logic Texas Hold'em rules engine. No IO, no async, no DB. Given a player list, a dealer position, and an RNG, plays a complete hand and returns a `HandResult`.

This crate is the source of truth for poker rules in the project. It is consumed by:
- `server/` — the per-session task that drives live play
- `client-game/` — Vue 3 client; hosts **both** the live-play table and the deterministic replay viewer (history / replay / coach / drills all live here). The client re-implements a subset of engine types client-side for animations / preview, mirroring `server/src/protocol.rs` via `client-game/src/protocol.ts`.

## Design constraints

- No `rs_poker` types in any public signature (see `docs/architecture/adr/`)
- No `tokio`, `sqlx`, or `axum` dependencies
- All public types are `Debug + Clone + PartialEq`
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

The `test-helpers` feature exposes `GameHand::override_hole_cards_for_test`, enabling integration tests in `server/` to inject specific hole cards. **Do not enable in production builds.** The server's `dev-dependencies` enables it for tests only.
