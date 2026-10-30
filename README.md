<div align="center">

# BluffKing — Poker &amp; Draw Games

**Pure-Rust poker rules, equity / post-hand solvers and verifiable dealing, plus public lottery/dice draw rules and an offline proof verifier.**

[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](LICENSE)
[![Rust 2021](https://img.shields.io/badge/Rust-2021-orange.svg)](https://www.rust-lang.org)

English · [中文](#中文)

</div>

---
<a id="english"></a>

## English

> [!NOTE]
> **This is the open-source subset of [BluffKing](https://bluffking.ai)** — a fair, **no-real-money** game center funded by memberships, offering Texas Hold'em alongside mahjong, card, board, dice and casual games on the web, Telegram, iOS and Android. These crates are the poker **engine**, the **verifiable ("mental poker") dealing**, and the **postflop CFR solver** behind its Texas Hold'em tables and public poker tools; the game server (except its sealed dealer, published as [`server-integration/sealed_dealer.rs`](server-integration/sealed_dealer.rs), plus lottery/dice draw and settlement source in [`draw-games/`](draw-games)), web/mobile clients, and website stay closed-source. Running at **[bluffking.ai](https://bluffking.ai)**, which offers this repo as the AGPL §13 Corresponding Source for the solver it serves. Its real server-blind cryptography (`crypto_real`) is **live in production for opt-in all-human "engine-blind" tables** (the server can't see your folded or pre-showdown cards) — cross-vendor AI-audited (Claude + OpenAI Codex — the audit consensus is public) and open-source: read the code and run the offline verifier yourself. (The former experimental *co-shuffle* entry was removed; mock cryptography is retained for offline protocol demonstrations.)

### What's here

| Crate | What it is |
|---|---|
| [`engine/`](engine) | Pure poker-logic crate. `GameHand` plays one complete hand end-to-end (blinds → flop/turn/river → side pots → result). Also hosts a Monte-Carlo **equity** estimator and a local **post-hand solver/coach**. No IO, no async, no DB — the same ruleset drives live play and deterministic replay. |
| [`mental-poker/`](mental-poker) | Verifiable dealing: the default Hold'em scheme (`pf2`, `pf-shuffle-v2`) — the dealer locks an encrypted, verifiably shuffled standard deck and commits to it before the deal, player seeds decide the public order, and shown cards are opened with decryption proofs while unshown cards stay encrypted — plus a signed, append-only hash-chain transcript and **offline verifiers** (`pf_verify`, `mp-verify`). Also includes the real **server-blind** crypto path (n-of-n re-encryption-mixnet shuffle + threshold decryption) that powers the live *engine-blind* tables (cross-vendor AI-audited; open-source + independently verifiable). Depends on `engine` only for the `Card` type. |
| [`mp-wasm/`](mp-wasm) | A `wasm-bindgen` surface over `mental_poker::crypto_real` (the real server-blind path) so a browser can run the verifiable dealing locally. A detached crate with its own workspace. |
| [`gto-solver/`](gto-solver) | Wraps the open-source **AGPL-3.0** `postflop-solver` (Discounted-CFR) behind BluffKing engine types (ADR-012); powers the free public `POST /api/tools/poker/solve` study tool. Its AGPL dependency is why this repo is AGPL-3.0. |
| [`draw-games/verifier/`](draw-games/verifier) | Shared deterministic lottery, scratch-card and Sicbo rules, pinned drand verification, and the network-free `draw-verify` CLI. Detached workspace; used by the backend code in this snapshot. Public scheduled-draw proofs were verified available on 2026-10-25; see the dated evidence below. |

### Lottery and dice draws

[`draw-games/`](draw-games) publishes the inputs, draw/prize rules and selected settlement/recovery source for SSQ, Daletou, TOTO, Happy8, Fucai3D, scratch cards and Sicbo. Scheduled draws bind a fixed future drand round and verify its BLS signature; scratch cards and solo Sicbo combine a precommitted server seed with a browser-generated player seed. Room Sicbo exposes commitments and reveals, with explicit operator-preknowledge and abort limitations. The shared [`verifier/`](draw-games/verifier) crate builds independently and replays proofs without a server; raw [`src/`](draw-games/src) files are integration source, not a standalone server.

**Production readback, 2026-10-25:** SSQ, Daletou, TOTO, Happy8 and Fucai3D returned `proof_status=available` under `bluffking-lottery-drand-v2-minute`; [TOTO proof example](https://app.bluffking.ai/api/draws/toto/29881827/proof). The live manifest identified `web-2026.10.25.68` / `08305a4af48208bafb9d1553ba0f367e64e109fd`. That deployed source also includes scratch and Sicbo commitment/proof integration; this inspection placed no new bets and made no scratch purchase. Proofs require matching backend/client versions. Publishing source does not attest the deployed binary, actual wallet credits or publication time. Legacy records remain without proofs; saved pre-bet receipts provide the input comparison, not a signed timestamp or global append-only log. [Protocol, retention, commands and source guide (中文 / English)](draw-games/index.html).

```bash
cargo build --locked --manifest-path draw-games/verifier/Cargo.toml
./draw-games/verifier/target/debug/draw-verify proof.json --receipt pre-draw-receipt.json
```

### Why verifiable dealing

A trustworthy card game should let a player check, after the hand, that the cards
they were dealt followed from inputs committed *before* the hand — rather than
simply trusting the operator. `mental-poker` provides a commit–reveal protocol and
a signed, append-only hash-chain transcript: the dealer commits before the hand,
reveals after, and anyone can re-run the offline verifier to confirm the deal
followed from the committed inputs (tamper-evidence + reproducibility). The
`crypto_real` path provides stronger secrecy for unshown hole cards within the
advanced encrypted-dealing hand — **live for the engine-blind table class** (ADR-070), cross-vendor AI-audited (audit consensus public) and open-source. Open-sourcing this is the point: fairness you can verify
beats fairness you are told to trust.

Advanced encrypted dealing assumes one trusted operator and has delivery and recovery limits; it does not prove a remote binary or prevent operator censorship or settlement manipulation. See the current [trust boundary](https://bluffking.ai/en/blog/advanced-encrypted-dealing/).

### Build &amp; verify

```bash
cargo build                              # engine + mental-poker + gto-solver (no DB, no services — network only for the one pinned git dependency on first fetch) [U54]
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
cargo deny check                         # supply-chain gate (advisories + licenses)
```

**Cross-vendor AI audit + how to verify:** [`audits/cross-vendor-ai-audit.html`](audits/cross-vendor-ai-audit.html) — the fairness trust model (open source + cross-vendor AI dual-audit + a runnable verifier), fact-gated to the ADRs.

`mental-poker` ships a few binaries — `pf_verify`, `mp-verify`, `pf_demo_hand`
(`cargo run -p mental-poker --bin pf_verify -- --help`). `mp-wasm` targets `wasm32-unknown-unknown` — build it from inside the crate (see its `Cargo.toml`).

The offline verifiers (`pf_verify`, `mp-verify`) are the runnable fairness
check: `pf_verify` checks a Hold'em hand's verification bundle (shuffle proof,
player seeds, decryption proofs) and prints the proven cards; `mp-verify`
replays a published transcript. They need no database. The crates carry no unit or integration test
suite; the upstream project verifies behavior with one end-to-end suite
against the running game.

### License, trademark, third-party

- **Code:** AGPL-3.0-only — see [`LICENSE`](LICENSE).
- **Brand:** the **BluffKing** name, logo, and visual identity are **not**
  licensed with the code — see [`TRADEMARKS.md`](TRADEMARKS.md). Forks must rebrand.
- **Dependencies:** mostly permissive (MIT / Apache-2.0 / BSD-3-Clause), with
  ONE copyleft exception — the `gto-solver` crate depends on the **AGPL-3.0**
  `postflop-solver` (pinned commit), which is why this repo is AGPL-3.0. See
  [`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md).

> The upstream service that uses these crates is a separate, **closed-source**
> product. As the sole copyright holder, the project owner may use this code in
> that closed product and may dual-license it; third parties receive only the
> AGPL grant — and AGPL §13 means that if you run a modified version as a network
> service, you must offer your users the corresponding source.

### Contributing &amp; security

See [`CONTRIBUTING.md`](CONTRIBUTING.md) (note the contributor inbound-terms
section) and [`SECURITY.md`](SECURITY.md).

---
<a id="中文"></a>

## 中文

> [!NOTE]
> **这是 [BluffKing](https://bluffking.ai) 的开源子集** —— BluffKing 是一个**不涉及真钱**、靠会员服务盈利的公平游戏中心,提供德州扑克以及麻将、棋牌、骰子与休闲游戏(网页、Telegram、iOS 与 Android)。这些 crate 是其德州扑克牌桌与公开扑克工具背后的扑克 **引擎**、**可验证("mental poker")发牌** 与 **翻后 CFR 求解器**;游戏服务器(其中的密封发牌进程除外,已作为 [`server-integration/sealed_dealer.rs`](server-integration/sealed_dealer.rs) 公开,彩票/骰宝开奖与结算源码另见 [`draw-games/`](draw-games))、Web/移动客户端与官网保持闭源。线上运行于 **[bluffking.ai](https://bluffking.ai)**,该站将本仓库作为其求解器的 AGPL §13 对应源码对外提供。它的真实服务端盲发密码学(`crypto_real`)已在生产环境 **为可选的全真人「engine-blind」牌桌启用**(服务器看不到你已弃牌或摊牌前的底牌)——经 AI 跨厂商独立审计(Claude + OpenAI Codex,审计共识已公开)、开源可复核:代码你能读、离线验证器你能自己跑。(旧实验性 *co-shuffle* 入口已移除；mock 加密仅保留于离线协议演示。)

### 仓库内容

| Crate | 说明 |
|---|---|
| [`engine/`](engine) | 纯逻辑扑克引擎。`GameHand` 端到端打完一手牌(盲注 → 翻牌/转牌/河牌 → 边池 → 结算)。内含蒙特卡洛 **胜率(equity)** 估算与本地 **牌后求解器/教练**。无 IO、无 async、无数据库——同一套规则驱动实战与确定性回放。 |
| [`mental-poker/`](mental-poker) | 可验证发牌:默认德州方案(`pf2`,`pf-shuffle-v2`)——发牌前发牌方锁定一副逐张加密、可验证洗牌的标准牌并公布承诺,玩家种子决定公开的发牌顺序,亮出的牌用解密证明打开,未亮出的牌保持加密;另含签名的只追加哈希链 transcript + **离线验证器**(`pf_verify`、`mp-verify`)。另含真实的**服务端盲发**路径(n-of-n 重加密混洗 + 门限解密),驱动线上 *engine-blind* 牌桌(经 AI 跨厂商审计;开源且可独立验证)。仅依赖 `engine` 的 `Card` 类型。 |
| [`mp-wasm/`](mp-wasm) | 对 `mental_poker::crypto_real`(真实服务端盲发路径)的 `wasm-bindgen` 封装,让浏览器本地运行可验证发牌。独立 crate,自带 workspace。 |
| [`gto-solver/`](gto-solver) | 封装开源 **AGPL-3.0** 的 `postflop-solver`(Discounted-CFR),隐藏在 BluffKing engine 类型之后(ADR-012);为免费公开的 `POST /api/tools/poker/solve` 学习工具提供支持。它的 AGPL 依赖是本仓库采用 AGPL 的原因。 |
| [`draw-games/verifier/`](draw-games/verifier) | 彩票、刮刮乐与骰宝共用的确定性规则、固定 drand 签名验证和不联网的 `draw-verify` 命令行。自带工作区,由本版本后台代码直接引用。2026-10-25 已核实定期开奖公开证明可用，证据见下文。 |

### 彩票与骰宝开奖

[`draw-games/`](draw-games) 公开双色球、大乐透、TOTO、快乐8、福彩3D、刮刮乐和骰宝的开奖输入、抽样、奖级计算与相关结算/恢复源码。定期开奖固定未来 drand 轮次并验证 BLS 签名；刮刮乐及单人骰宝结合预先承诺的服务器种子与浏览器玩家种子。骰宝房间公开承诺与揭示,同时保留运营方预知结果和中止的明确限制。共用的 [`verifier/`](draw-games/verifier) 可以独立构建、离线重算证明；原始 [`src/`](draw-games/src) 是服务集成源码,不能作为独立服务器构建。

**生产核实（2026-10-25）：**双色球、大乐透、TOTO、快乐8和福彩3D均返回 `proof_status=available`，使用 `bluffking-lottery-drand-v2-minute`；[TOTO 证明示例](https://app.bluffking.ai/api/draws/toto/29881827/proof)。线上清单标识为 `web-2026.10.25.68` / `08305a4af48208bafb9d1553ba0f367e64e109fd`。该生产版本源码亦包含刮刮乐和骰宝的承诺/证明集成；本次没有新购刮刮乐或下注。生成证明仍需要配套后台与客户端。发布源码不证明线上二进制身份、实际钱包入账或公布时间。历史记录无证明时不会补造；开奖前保存的回执用于核对输入,不是签名时间戳或全局只追加日志。[协议、保存期限、使用命令与源码导航(中文 / English)](draw-games/index.html)。

```bash
cargo build --locked --manifest-path draw-games/verifier/Cargo.toml
./draw-games/verifier/target/debug/draw-verify proof.json --receipt pre-draw-receipt.json
```

### 为什么"可验证"发牌

可信的牌局应让玩家在牌后自行核对:发到的牌是否由 *开局前* 已承诺的输入推导而来
——而不是无条件信任运营方。`mental-poker` 提供 commit–reveal 协议与签名的只追加
哈希链 transcript:发牌方开局前承诺、牌后揭示,任何人都能离线重跑验证器,确认发牌
确由已承诺输入推导(防篡改 + 可复现)。更强的保证——服务器发出它自己都读不到的牌
(服务端盲发,保护该手未公开底牌的保密性)——是 `crypto_real` 路径——**已为 engine-blind 牌桌启用**(ADR-070),经 AI 跨厂商审计(审计共识已公开)、开源可复核。
把这部分开源正是重点:可被验证的公平,胜过让你信任的公平。

高级加密发牌以可信单一运营者为边界，并有交付与恢复限制；它不证明远端二进制，也不能排除运营方审查交付或操纵结算。见当前[信任边界](https://bluffking.ai/zh/blog/advanced-encrypted-dealing/)。

### 构建与核验

```bash
cargo build                              # engine + mental-poker + gto-solver(无需数据库/服务;仅首次拉取那一个固定的 git 依赖时联网)[U54]
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
cargo deny check                         # 供应链门禁(漏洞 + 许可证)
```

离线验证器(`pf_verify`、`mp-verify`)是可运行的公平性核验:`pf_verify` 核对一手德州的
验证包(洗牌证明、玩家种子、解密证明)并打印被证明的牌;`mp-verify` 回放公开的 transcript。
都无需数据库。各 crate 不带单元/集成测试套件;
上游项目只用一套针对运行中游戏的端到端测试来验证行为。

**跨厂商 AI 审计 + 如何自己核:** [`audits/cross-vendor-ai-audit.html`](audits/cross-vendor-ai-audit.html) —— 公平信任模型(开源 + 跨厂商 AI 双审 + 可运行验证器),事实可溯源到 ADR。

### 许可证 / 商标 / 第三方

- **代码:** AGPL-3.0-only,见 [`LICENSE`](LICENSE)。
- **品牌:** **BluffKing** 名称、logo 与视觉识别 **不** 随代码授权,见
  [`TRADEMARKS.md`](TRADEMARKS.md);fork 必须更名换标。
- **依赖:** 大部分为宽松许可(MIT / Apache-2.0 / BSD-3-Clause),有一个 copyleft
  例外——`gto-solver` crate 依赖 **AGPL-3.0** 的 `postflop-solver`(固定 commit),这
  正是本仓库采用 AGPL-3.0 的原因。见 [`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md)。

> 使用这些 crate 的线上服务是 **闭源** 的独立产品。作为唯一版权持有者,作者可在该闭源
> 产品中使用本代码并可双重授权;第三方仅获得 AGPL 授权——根据 AGPL §13,若你将修改版
> 作为网络服务运行,必须向用户提供对应源码。

### 贡献与安全

见 [`CONTRIBUTING.md`](CONTRIBUTING.md)(注意贡献者条款一节)与
[`SECURITY.md`](SECURITY.md)。
