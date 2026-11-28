# Server integration — AGPL §13 Corresponding Source

This directory holds the **network-integration source** of the deployed public
postflop solver endpoint, provided so the AGPL §13 "Corresponding Source" offer
covers the *whole combined work reached over the network* (ADR-072 §3a), not
just the library.

## Game rules

`game-rules/` contains the exact source for board moves, card ranking,
hand state machines, and variant-specific scoring. Shared banker rules and
mahjong claim resolution are included. Some files reference private server
types and are published for inspection. The room runtime, bots and wallet
storage are outside this source directory. The independently buildable
[draw verifier](../draw-games/verifier) and [card proof library](../mental-poker)
cover random draws and supported dealing transcripts respectively.

## Files

- `gto_solve.rs` — the exact `POST /api/tools/poker/solve` axum handler as
  deployed (rate-limit / memory-gate / concurrency / cache / §13 source-offer
  logic). It is the endpoint that reaches the AGPL `gto-solver` crate over the
  network.
- `sealed_dealer.rs` — the **sealed dealer** as deployed: the separate process
  that deals every default (non-mental-poker) Hold'em hand. It is published for
  transparency so anyone can read exactly what it does: it locks an encrypted,
  verifiably shuffled standard deck (`mental-poker` `pf2`, `pf-shuffle-v2`) and
  commits to it before the hand, lets the players' seeds decide the final
  order, never publishes its seed, sends each
  human player's hole cards straight to that player's browser over its own
  WebSocket (they never pass through the game-server process), releases board
  and contested showdown cards only when the game reaches those stages, and
  hands the completed deck to the game server after settlement for the owner's
  private hand review. It is a single trusted dealer, **not** mental poker: the
  dealer process itself holds the deck. For dealing that no server can read, see
  `mental-poker/` (`crypto_real`). Like `gto_solve.rs` it is a source offer, not a
  build target: it references closed server modules (`protocol`,
  `session_registry`, `runtime_checkpoint`, `outbound`). Its one submodule under
  `sealed_dealer/` is published with it: `authorization.rs` (the game-process signature
  every internal game-server-to-dealer request must carry).
- `zjh/` — the **Zha Jin Hua (炸金花) dealer** as deployed, published for the
  same transparency reason as `sealed_dealer.rs`:
  - `zjh/fairness.rs` — per-hand deal state: draws a fresh master seed, locks an
    encrypted, verifiably shuffled standard deck (`mental-poker` `pf2_zjh`,
    scheme `zjh-pf-shuffle-v2`; the game, hand id and frozen seat roster are
    bound into the deck context) and publishes only its commitment before any
    player seed exists; runs the two-round player-seed commit → reveal exchange
    (seats that commit but do not reveal are recorded as `withheld_seats`);
    derives the final card order from the commitment and the revealed seeds;
    and after the hand builds the proof bundle (shuffle proof plus decryption
    proofs only for cards that were already shown). The master seed is never
    published.
  - `zjh/hand.rs` + `zjh/rules.rs` — the pure hand state machine and hand
    ranking. `Hand::new` gives the i-th dealt seat (ascending seat order, the
    same roster bound into the commitment) cards `3i, 3i+1, 3i+2` of the final
    deck order. For natural-deck rooms the closed room module converts each
    `mental-poker` card id (rank-major, `id % 4` indexing ♠♥♦♣) to a ZJH card
    with rank `id / 4 + 2` and the same physical suit, remapped to ZJH's
    internal ♠♥♣♦ index (`[0, 1, 3, 2][id % 4]`); `fairness::bundle` checks every
    opened card against the stored hand with `card_id::id_to_card`. 大小王
    (joker) rooms use the `card_proof` `zjh-jokers` id space instead:
    suit-major ♠♥♣♦ × 2..A, then 52 = small joker, 53 = big joker.
  - `pf_dealing.rs` — the player-seed commitment/reveal bookkeeping
    (`SeedInputs`, `client_seed_commitment`) shared with Hold'em's sealed dealer.
  - `card_proofs.rs` — the generic `mental-poker` `card_proof` deck pool and
    receipts used by 大小王 (joker) ZJH rooms and other card games.

  Like the dealer above, the ZJH dealer is a single trusted dealer, **not**
  mental poker: the server holds the deck while the hand runs, and the proofs
  show that the committed standard deck was neither swapped nor re-ordered after
  the commitment. These files are a source offer, not a build target: they
  reference closed server modules (`rate_limit`, `db_writer`, `app_state`,
  `auth`, `protocol`, `session_registry`, the generic card-room hub that creates
  the pending deal, advances the exchange and takes the final deck order at deal
  time, and the ZJH room/bot modules, and the room clock).

### Game outcome source

The files below are copied directly from the server at the source revision
identified by each release tag. They can be read as source without the rest of
the hosted server. Wallet and room services are private. Source inspection
shows how the rules work. A public source snapshot alone cannot authenticate
an individual result or identify the binary running online.

以下文件从每个发布标签对应的服务端版本直接同步。可查阅实际开奖、牌面判定
和派彩规则。钱包和房间服务保持私有。公开源码可用于核对规则，单凭源码无法
验证某一局的随机结果或确认线上运行的二进制。

| Game / 游戏 | Server source / 服务端源码 |
| --- | --- |
| Baccarat / 百家乐 | [Rules](baccarat/rules.rs): eight-deck shuffle, burn, refill threshold, drawing tableau, hand result and payouts |
| Roulette / 俄罗斯转盘 | [Rules](../draw-games/verifier/src/roulette.rs): uniform pocket draw and payouts |
| Slots / 老虎机 | [Rules](../draw-games/verifier/src/slots.rs): reel draws, paytable and jackpot |
| Fruit Machine / 水果机 | [Rules](../draw-games/verifier/src/fruit.rs): weighted stops, bonus lights, double and payouts |
| Claw Machine / 抓娃娃 | [Rules](../draw-games/verifier/src/claw.rs): catch radius, catch chance and prizes |
| Call/Fold, Lucky Draw, 2048 / 跟弃、幸运抽奖、2048 | [Draws and rewards](solo/casual.rs), [2048 board replay](../draw-games/verifier/src/puzzle2048.rs) |
| Blackjack / 21 点 | [Hand rules](blackjack/hand.rs), [shared deck proofs](card_proofs.rs) |
| Ten and a Half / 十点半 | [Hand rules and opening dice](tenhalf/hand.rs), [shared deck proofs](card_proofs.rs) |
| Big & Small / 押大小 | [Dice proof, payouts, and bonus prize](../draw-games/verifier/src/sicbo.rs) |
| Fish Prawn Crab, scratch cards and number lotteries / 鱼虾蟹、刮刮乐和数字彩票 | [Draw rules and offline verifier](../draw-games/) |

Solo rounds use the shared precommitted seed and player-seed protocol.
Baccarat shuffles with `OsRng` inside its rules module. The Big & Small bonus
is replayed from the wager seed using a separate deterministic stream.

## Deployed dependency reference

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
here — that would ship a fork of the closed server's private module surface. The lottery and dice draw implementation is also published in
[`draw-games/`](../draw-games). Other server integration modules, clients, and
the website remain closed.
