# Texas Hold’em bot strategy / 德州扑克 AI 策略

This directory publishes the Rust decision code used by BluffKing’s Hold’em
table bots and the frozen policy files it reads. Start with
[`bot.rs`](bot.rs) for `DecisionContext` and the strategy entry points,
[`bot/shark.rs`](bot/shark.rs) for Shark, and
[`bot/preflop_blueprint.rs`](bot/preflop_blueprint.rs) for policy lookup.
[`bot/adapt.rs`](bot/adapt.rs), [`bot_adapt.rs`](bot_adapt.rs),
[`bot/opponent_memory/holdem.rs`](bot/opponent_memory/holdem.rs) and
[`bot/learning.rs`](bot/learning.rs) cover how historical betting statistics
and individual bot results affect decisions.

本目录公开 BluffKing 德州牌桌 AI 的 Rust 决策代码及实际读取的策略数据。
从 [`bot.rs`](bot.rs) 查看决策输入和入口，从 [`bot/shark.rs`](bot/shark.rs)
查看 Shark，从 [`bot/preflop_blueprint.rs`](bot/preflop_blueprint.rs) 查看翻前策略查表。
真人打法适应、对手记忆和策略学习在上面列出的对应模块中。

The source is copied unchanged from the server revision identified by the
mirror tag. It references the hosted session, database and runtime modules
and is provided for source review. The workspace builds the public engine
and solver crates. Player records and learned profiles are held by the
hosted service. The corresponding source for default dealing is
[`sealed_dealer.rs`](../sealed_dealer.rs).

镜像标签对应的服务端版本会原样同步这些源码。它依赖托管服务的会话、数据库和
运行时模块，可直接阅读审查。公开工作区提供引擎和求解器的构建目标。玩家记录
和线上累积的打法档案保存在托管服务。默认发牌实现见
[`sealed_dealer.rs`](../sealed_dealer.rs)。

## Preflop policy provenance / 翻前策略来源

The frozen policies were generated using
[GTOpen at `874f780`](https://github.com/MatthewPDingle/GTOpen/tree/874f780186fbe2291ea9e319ef191151da875644)
([Apache-2.0 license](https://github.com/MatthewPDingle/GTOpen/blob/874f780186fbe2291ea9e319ef191151da875644/LICENSE)).
The [export tools](scripts/) document training, export and policy evaluation.
`bot/preflop_blueprint/stacks.hex` is the hex-encoded policy catalog.
To inspect the packed bytes, decode it with
`xxd -r -p bot/preflop_blueprint/stacks.hex stacks.bin`.

冻结的翻前策略由上述版本的 GTOpen 生成。导出工具记录了训练、导出和评估方法。
`stacks.hex` 是十六进制编码的策略表，用上面的命令可还原为二进制。

The upstream context and changes in [the GTOpen training patch](scripts/gtopen/training.patch)
are available under [Apache-2.0](scripts/gtopen/LICENSE). BluffKing’s strategy
source follows the repository’s [AGPL-3.0 license](../../LICENSE).

GTOpen 训练补丁按 Apache-2.0 授权。BluffKing 策略源码按仓库的 AGPL-3.0 授权。
