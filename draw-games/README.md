# 彩票与骰宝：公开过程，独立复核

[中文 / English：完整说明与语言切换](index.html)

本目录公开双色球、大乐透、TOTO、快乐8、福彩3D、刮刮乐、骰宝、鱼虾蟹和百家乐的开奖、发牌和中奖计算源码，并提供独立离线验证器。采用仓库根目录的 [AGPL-3.0-only 许可证](../LICENSE)，公开内容由上游逐文件同步。

**生产核实（2026-10-25）：定期开奖证明已上线。**双色球、大乐透、TOTO、快乐8和福彩3D的公开证明接口均返回 `proof_status=available`，计划协议为 `bluffking-lottery-drand-v2-minute`；可按游戏与期号读取 [生产证明示例](https://app.bluffking.ai/api/draws/toto/29881827/proof)。本次生产发布标识为 `web-2026.10.25.68`，源码提交为 `08305a4af48208bafb9d1553ba0f367e64e109fd`。

刮刮乐、单人骰宝和骰宝房间的承诺与证明实现已包含在上述生产版本源码中；本次仅对定期开奖进行了公开接口取证，没有新购刮刮乐或下注骰宝。发布源码或标签本身仍不能证明线上运行的二进制。定期开奖只处理预先公开证明计划的期次；缺失证明时不补造承诺。刮刮乐只支持当前每张 1 金豆的福利卡和对应奖励证明。

定期开奖在购买前固定未来 drand 轮次，验证签名后按公开规则生成号码。刮刮乐与单人骰宝使用预先承诺的服务器种子和浏览器玩家种子。骰宝房间持久化签名接单凭证，锁定有效投注的签名名单，再等候未来的固定 drand Quicknet 轮次。骰点取决于已承诺的服务端种子、签名名单与经验证的信标。本人签名接单凭证可与名单中的哈希核对，名单不披露姓名或投注金额。其他种子是否来自真人仍需额外证据。骰宝金豆大奖可用投注种子和专属证明复算。

鱼虾蟹房间在下注前固定截止时间及未来 drand 轮次。封盘后等待并验证原定签名，再计算骰点。

百家乐押注前公布每靴的 Merkle 根和本轮起始位置。局后提供已发牌的索引、盐值和 Merkle 路径，离线验证器依此核对牌面、补牌与胜负。服务器持有整靴，验证材料只打开本局已发牌。投注者可从本人的历史下载包含已结算投注的版本，比较下注前保存的投注条件并复算派彩。

俄罗斯转盘、老虎机、水果机、娃娃机和 2048 共用 [单人开奖验证协议](verifier/src/solo.rs)。服务器首先发出绑定局号的种子承诺，浏览器保存该承诺、玩家种子和下注／操作输入后再提交。服务器只受理该承诺一次，受理后的同一请求恢复同一结果。证明揭示后可复算轮盘号码和返还、老虎机符号和大奖、水果机跑灯与比倍、娃娃机抓取与大奖，以及 2048 初始随机种子。领取 2048 返还后，导出文件还包括操作和显示的返还。离线验证器按同一套规则重放并核对返还金额。运营方仍可拒绝受理或中止请求。

同一协议也覆盖 Call / Fold 与幸运抽奖的金豆奖励。幸运抽奖的候选名单留在设备上，提交内容只有名单哈希和人数；验证器可复算获选位置。选择导出私密证明会附上名单和随机盐，能核对实际获选姓名，请勿公开分享私密证明。

[verifier/](verifier) 是后台代码与离线验证器共用的可独立构建规则库。从公开仓库根目录运行：

```sh
cargo build --locked --manifest-path draw-games/verifier/Cargo.toml
./draw-games/verifier/target/debug/draw-verify proof.json --receipt pre-draw-receipt.json
```

骰宝签名文件可用 `draw-verify proof.json --evidence-key TRUSTED_KEY_HEX` 核对。请先从 `GET /api/history/draw-evidence-key` 取得并保存运营方 Ed25519 公钥。下注人登录后可用 `GET /api/history/sicbo-round-proof?round_id=UUID` 找回本人最新的 `acceptance`、签名名单及结算或作废的 `proof`，也可加 `revision=N` 取指定的历史修订。接单和名单分别封装为 `{"kind":"sicbo-wager-v1","acceptance":…}` 或 `{"kind":"sicbo-roster-v1","roster":…}` 交给命令行。可用 `--receipt sicbo-wager-acceptance.json` 将本人签名凭证与名单或结算证明核对。签名中的接单和锁定时间由运营方记录，没有外部时间戳。下注人可在信标出现前保存签名名单，事后核对是否使用该名单。

浏览器导出的 `{proof, receipt}` 文件可直接作为唯一参数。没有开奖前回执时仍能重算，但不能声称已与先前看到的承诺核对。验证器不联网，也不证明实际钱包入账、发布时间或部署身份。

完整协议、无需服务器的定期开奖重算、保存期限与源码导航见 [index.html](index.html)，默认中文，可切换 English。

A `board-replay-v1` document from a completed Gomoku, Xiangqi, or Chinese Checkers match can be checked with the same command. The verifier replays the public moves with the production rule code and compares the final board and rule-derived result. For endings based on a recorded event such as a timeout or draw agreement, the verification result identifies the supplied event as reported and confirms only the board, moves, and their consistency with it.
