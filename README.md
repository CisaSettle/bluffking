<div align="center">

# BluffKing — Engine &amp; Mental-Poker

**A pure-Rust No-Limit Texas Hold'em rules engine, a Monte-Carlo equity / post-hand solver, and a provably-fair card-dealing ("mental poker") crate — no IO, no async, no database.**

[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](LICENSE)
[![Rust 2021](https://img.shields.io/badge/Rust-2021-orange.svg)](https://www.rust-lang.org)

English · [中文](#中文)

</div>

> [!WARNING]
> **This repository is a curated, logic-only subset of a larger commercial product, prepared for open-sourcing.** Before it is made public, read [`PUBLISH-BLOCKERS.md`](PUBLISH-BLOCKERS.md) — at least one data-provenance item must be resolved first (run `scripts/prepublish-check.sh`). The `mental-poker` real-cryptography path (`crypto_real`) is a **prototype pending external audit** and is **not** wired into any production build — do not rely on it for real stakes.

---
<a id="english"></a>

## English

### What's here

| Crate | What it is |
|---|---|
| [`engine/`](engine) | Pure poker-logic crate. `GameHand` plays one complete hand end-to-end (blinds → flop/turn/river → side pots → result). Also hosts a Monte-Carlo **equity** estimator and a local **post-hand solver/coach**. No IO, no async, no DB — the same ruleset drives live play, tests, and deterministic replay. |
| [`mental-poker/`](mental-poker) | Provably-fair dealing: a commit–reveal scheme plus an Ed25519-signed hash-chain transcript and **offline verifiers** (`pf_verify`, `mp-verify`). Also includes a **prototype** server-blind crypto path (re-encryption-mixnet shuffle + threshold decryption). Depends on `engine` only for the `Card` type. |
| [`mp-wasm/`](mp-wasm) | A `wasm-bindgen` wrapper over `mental-poker` so a browser can run the verifiable dealing locally. A detached crate (its own workspace); see [`mp-wasm/README.md`](mp-wasm/README.md). |
| [`gto-solver/`](gto-solver) | Wraps the open-source **AGPL-3.0** `postflop-solver` (Discounted-CFR) behind BluffKing engine types (ADR-012); powers the free public `POST /api/tools/poker/solve` study tool. Its AGPL dependency is why this repo is AGPL-3.0. |

### Why "provably fair"

A trustworthy card game must let a player verify, after the fact, that the deck
was shuffled fairly and not manipulated. `mental-poker` does this with a
commit–reveal protocol and a signed, append-only transcript: the dealer commits
to a seed before the hand, reveals it after, and anyone can re-run the offline
verifier to confirm the cards followed from the committed inputs. Open-sourcing
this part is a feature — the whole point is that it is **externally verifiable**.

### Build &amp; test

```bash
cargo build                              # engine + mental-poker (no DB, no network)
cargo test -p engine                     # unit + integration
cargo test -p mental-poker               # incl. the offline transcript verifier
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
cargo deny check                         # supply-chain gate (advisories + licenses)
```

`mental-poker` ships a few binaries — `pf_verify`, `mp-verify`, `pf_demo_hand`
(`cargo run -p mental-poker --bin pf_verify -- --help`). `mp-wasm` targets
`wasm32-unknown-unknown`; see its README.

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

### 仓库内容

| Crate | 说明 |
|---|---|
| [`engine/`](engine) | 纯逻辑扑克引擎。`GameHand` 端到端打完一手牌(盲注 → 翻牌/转牌/河牌 → 边池 → 结算)。内含蒙特卡洛 **胜率(equity)** 估算与本地 **牌后求解器/教练**。无 IO、无 async、无数据库——同一套规则驱动实战、测试与确定性回放。 |
| [`mental-poker/`](mental-poker) | 可验证公平发牌:commit–reveal 承诺-揭示方案 + Ed25519 签名的哈希链 transcript + **离线验证器**(`pf_verify`、`mp-verify`)。另含 **原型** 的服务端盲发路径(重加密混洗 + 门限解密)。仅依赖 `engine` 的 `Card` 类型。 |
| [`mp-wasm/`](mp-wasm) | 对 `mental-poker` 的 `wasm-bindgen` 封装,让浏览器本地运行可验证发牌。独立 crate(自带 workspace),见 [`mp-wasm/README.md`](mp-wasm/README.md)。 |
| [`gto-solver/`](gto-solver) | 封装开源 **AGPL-3.0** 的 `postflop-solver`(Discounted-CFR),隐藏在 BluffKing engine 类型之后(ADR-012);为免费公开的 `POST /api/tools/poker/solve` 学习工具提供支持。它的 AGPL 依赖是本仓库采用 AGPL 的原因。 |

### 为什么"可验证公平"

可信的牌局必须让玩家在牌后能自行验证:这副牌是否被公平洗过、有无被操纵。
`mental-poker` 通过 commit–reveal 协议与签名的只追加 transcript 实现:发牌方在
开局前对种子做承诺、牌后揭示,任何人都能离线重跑验证器,确认发出的牌确实由已
承诺的输入推导而来。把这一部分开源本身就是卖点——它的全部价值就在于 **可被外部
验证**。

### 构建与测试

```bash
cargo build                              # engine + mental-poker(无需数据库/网络)
cargo test -p engine                     # 单元 + 集成
cargo test -p mental-poker               # 含离线 transcript 验证器
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
cargo deny check                         # 供应链门禁(漏洞 + 许可证)
```

### 许可证 / 商标 / 第三方

- **代码:** AGPL-3.0-only,见 [`LICENSE`](LICENSE)。
- **品牌:** **BluffKing** 名称、logo 与视觉识别 **不** 随代码授权,见
  [`TRADEMARKS.md`](TRADEMARKS.md);fork 必须更名换标。
- **依赖:** 全部为宽松许可(MIT / Apache-2.0 / BSD-3-Clause),见
  [`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md)。

> 使用这些 crate 的线上服务是 **闭源** 的独立产品。作为唯一版权持有者,作者可在该闭源
> 产品中使用本代码并可双重授权;第三方仅获得 AGPL 授权——根据 AGPL §13,若你将修改版
> 作为网络服务运行,必须向用户提供对应源码。

### 贡献与安全

见 [`CONTRIBUTING.md`](CONTRIBUTING.md)(注意贡献者条款一节)与
[`SECURITY.md`](SECURITY.md)。
