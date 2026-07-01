<div align="center">

# BluffKing — Engine &amp; Mental-Poker

**A pure-Rust No-Limit Texas Hold'em rules engine, a Monte-Carlo equity / post-hand solver, and a verifiable card-dealing ("mental poker") crate — no IO, no async, no database.**

[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](LICENSE)
[![Rust 2021](https://img.shields.io/badge/Rust-2021-orange.svg)](https://www.rust-lang.org)

English · [中文](#中文)

</div>

---
<a id="english"></a>

## English

> [!NOTE]
> **This is the open-source subset of [BluffKing](https://bluffking.ai)** — the free, **no-real-money** Texas Hold'em study product. These crates are the poker **engine**, the **verifiable ("mental poker") dealing**, and the **postflop CFR solver** behind it; the game server, web/mobile clients, and website stay closed-source. Running at **[bluffking.ai](https://bluffking.ai)**, which offers this repo as the AGPL §13 Corresponding Source for the solver it serves. The `mental-poker` real-cryptography path (`crypto_real`) is a **prototype pending external audit** — do not rely on it for real-stakes play.

### What's here

| Crate | What it is |
|---|---|
| [`engine/`](engine) | Pure poker-logic crate. `GameHand` plays one complete hand end-to-end (blinds → flop/turn/river → side pots → result). Also hosts a Monte-Carlo **equity** estimator and a local **post-hand solver/coach**. No IO, no async, no DB — the same ruleset drives live play, tests, and deterministic replay. |
| [`mental-poker/`](mental-poker) | Verifiable, commit–reveal dealing: a commit–reveal scheme plus a signed, append-only hash-chain transcript and **offline verifiers** (`pf_verify`, `mp-verify`). Also includes a **prototype** server-blind crypto path (re-encryption-mixnet shuffle + threshold decryption). Depends on `engine` only for the `Card` type. |
| [`mp-wasm/`](mp-wasm) | A `wasm-bindgen` surface over `mental_poker::crypto_real` (the **prototype** server-blind path) so a browser can run the verifiable dealing locally. A detached crate with its own workspace. |
| [`gto-solver/`](gto-solver) | Wraps the open-source **AGPL-3.0** `postflop-solver` (Discounted-CFR) behind BluffKing engine types (ADR-012); powers the free public `POST /api/tools/poker/solve` study tool. Its AGPL dependency is why this repo is AGPL-3.0. |

### Why verifiable dealing

A trustworthy card game should let a player check, after the hand, that the cards
they were dealt followed from inputs committed *before* the hand — rather than
simply trusting the operator. `mental-poker` provides a commit–reveal protocol and
a signed, append-only hash-chain transcript: the dealer commits before the hand,
reveals after, and anyone can re-run the offline verifier to confirm the deal
followed from the committed inputs (tamper-evidence + reproducibility). The
stronger guarantee — a server that deals cards it cannot read (server-blind, so it
cannot manipulate what it cannot see) — is the `crypto_real` path, a **prototype
pending external audit**. Open-sourcing this is the point: fairness you can verify
beats fairness you are told to trust.

### Build &amp; test

```bash
cargo build                              # engine + mental-poker + gto-solver (no DB, no network)
cargo test -p engine                     # unit + integration
cargo test -p mental-poker               # incl. the offline transcript verifier
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
cargo deny check                         # supply-chain gate (advisories + licenses)
```

`mental-poker` ships a few binaries — `pf_verify`, `mp-verify`, `pf_demo_hand`
(`cargo run -p mental-poker --bin pf_verify -- --help`). `mp-wasm` targets `wasm32-unknown-unknown` — build it from inside the crate (see its `Cargo.toml`).

The test suite is the credibility layer, not an afterthought: property-based
invariants (`proptest`), chip-conservation checks (chips are never created or
destroyed across a hand + side pots), a **byte-identical deterministic replay**
test, the offline mental-poker transcript verifiers, and a clean-room
`preflop_chart_contract` test asserting the preflop ranges are self-generated
(no third-party charts). None of it needs a database.

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
> **这是 [BluffKing](https://bluffking.ai) 的开源子集** —— 免费、**不涉及真钱** 的德州扑克学习产品。这些 crate 是它背后的扑克 **引擎**、**可验证("mental poker")发牌** 与 **翻后 CFR 求解器**;游戏服务器、Web/移动客户端与官网保持闭源。线上运行于 **[bluffking.ai](https://bluffking.ai)**,该站将本仓库作为其求解器的 AGPL §13 对应源码对外提供。`mental-poker` 的真实密码学路径(`crypto_real`)是 **待外部审计的原型**,请勿用于真实赌注。

### 仓库内容

| Crate | 说明 |
|---|---|
| [`engine/`](engine) | 纯逻辑扑克引擎。`GameHand` 端到端打完一手牌(盲注 → 翻牌/转牌/河牌 → 边池 → 结算)。内含蒙特卡洛 **胜率(equity)** 估算与本地 **牌后求解器/教练**。无 IO、无 async、无数据库——同一套规则驱动实战、测试与确定性回放。 |
| [`mental-poker/`](mental-poker) | 可验证发牌(commit–reveal):commit–reveal 承诺-揭示方案 + 签名的只追加哈希链 transcript + **离线验证器**(`pf_verify`、`mp-verify`)。另含 **原型** 的服务端盲发路径(重加密混洗 + 门限解密)。仅依赖 `engine` 的 `Card` 类型。 |
| [`mp-wasm/`](mp-wasm) | 对 `mental_poker::crypto_real`(**原型** 服务端盲发路径)的 `wasm-bindgen` 封装,让浏览器本地运行可验证发牌。独立 crate,自带 workspace。 |
| [`gto-solver/`](gto-solver) | 封装开源 **AGPL-3.0** 的 `postflop-solver`(Discounted-CFR),隐藏在 BluffKing engine 类型之后(ADR-012);为免费公开的 `POST /api/tools/poker/solve` 学习工具提供支持。它的 AGPL 依赖是本仓库采用 AGPL 的原因。 |

### 为什么"可验证"发牌

可信的牌局应让玩家在牌后自行核对:发到的牌是否由 *开局前* 已承诺的输入推导而来
——而不是无条件信任运营方。`mental-poker` 提供 commit–reveal 协议与签名的只追加
哈希链 transcript:发牌方开局前承诺、牌后揭示,任何人都能离线重跑验证器,确认发牌
确由已承诺输入推导(防篡改 + 可复现)。更强的保证——服务器发出它自己都读不到的牌
(服务端盲发,读不到就无法操纵)——是 `crypto_real` 路径,**待外部审计的原型**。
把这部分开源正是重点:可被验证的公平,胜过让你信任的公平。

### 构建与测试

```bash
cargo build                              # engine + mental-poker + gto-solver(无需数据库/网络)
cargo test -p engine                     # 单元 + 集成
cargo test -p mental-poker               # 含离线 transcript 验证器
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
cargo deny check                         # 供应链门禁(漏洞 + 许可证)
```

测试是可信度的核心而非附属:基于属性的不变量(`proptest`)、筹码守恒校验(一手牌
+ 边池全程筹码不增不减)、**逐字节确定性回放** 测试、离线 mental-poker transcript
验证器,以及一个 clean-room `preflop_chart_contract` 测试(断言翻前范围为自生成、
无第三方图表)。全部无需数据库。

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
