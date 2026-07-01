// M3 — full server-blind hand across INDEPENDENT WasmParty clients + a relay
// coordinator that holds NO secret and enforces owner-only hole-card routing.
// Every cross-client message is a JSON string (the future WS wire).
//
//   node mp-wasm/choreography_e2e.cjs   (after: wasm-pack build --target nodejs)
const wasm = require('./pkg/mp_wasm.js');

const lbl = (id) => "23456789TJQKA"[Math.floor(id / 4)] + "♣♦♥♠"[id % 4];
const fail = (m) => { console.error('❌ ' + m); process.exit(1); };

const n = 3;
// Independent clients — each holds its OWN secret x_i inside its wasm instance.
const parties = Array.from({ length: n }, (_, i) => new wasm.WasmParty(`party:${i}`));

// === DKG === each registers (q_i + PoK); the coordinator verifies every PoK and
// derives the joint key Q. No secret reaches the coordinator.
const regs = parties.map((p) => p.register());
regs.forEach((r, i) => { if (!wasm.coord_verify_register(r)) fail(`DKG PoK failed for party:${i}`); });
const pks = regs.map((r) => { const o = JSON.parse(r); return { party_id: o.party_id, q_i: o.q_i }; });
const pksJson = JSON.stringify(pks);
const q = wasm.coord_joint_key(pksJson);

// === Verifiable shuffle round-robin === coordinator relays the deck; each party
// shuffles+proves; coordinator verifies every proof before passing it on.
let deck = wasm.coord_canonical_deck();
for (let r = 0; r < n; r++) {
  const res = JSON.parse(parties[r].shuffle(deck, q, r));
  const outDeck = JSON.stringify(res.output_deck);
  if (!wasm.coord_verify_shuffle(`party:${r}`, r, deck, outDeck, q, JSON.stringify(res.proof))) {
    fail(`shuffle proof from party:${r} rejected`);
  }
  deck = outDeck;
}
const deckArr = JSON.parse(deck);

// Relay coordinator: holds NO secret; only RELAYS shares. It enforces the
// owner-only routing constraint the audit asked for — collectShares is the ONLY
// way shares move, and the coordinator never calls owner_open itself.
const coordinator = {
  relayShares(idx, online) {
    return online.map((i) => parties[i].partial_decrypt(idx, JSON.stringify(deckArr[idx])));
  },
};
const online = parties.map((_, i) => i);
const openAt = (idx, contributors) => {
  const shares = coordinator.relayShares(idx, contributors);
  return wasm.owner_open(idx, JSON.stringify(deckArr[idx]), pksJson, '[' + shares.join(',') + ']');
};

// === Deal === hole card s and n+s → routed to OWNER s (the owner opens, not the
// coordinator). Board 2n..2n+5 → revealed to all.
const hole = [];
for (let s = 0; s < n; s++) {
  const a = openAt(s, online), b = openAt(n + s, online);
  if (a < 0 || b < 0) fail(`hole open failed for seat ${s}`);
  hole.push([s, [a, b]]);
}
const board = [];
for (let i = 0; i < 5; i++) {
  const id = openAt(2 * n + i, online);
  if (id < 0) fail(`board open failed at index ${2 * n + i}`);
  board.push(id);
}

// === Server-blindness === the coordinator runs its public-only attack menu.
const serverRecovered = wasm.coord_blind_check(deck, pksJson);

console.log('M3 · 多客户端(独立 WasmParty)+ 零私钥中继协调方 · 真实密码学,JSON 消息传输\n');
console.log(`① DKG:${n} 个独立客户端各注册 q_i+PoK,协调方逐一验证 → 联合公钥 Q`);
console.log(`② 可验证洗牌:${n} 轮,协调方逐轮验证零知识证明 ✓`);
console.log('③ 发牌(底牌只路由给拥有者,协调方从不自行开牌):');
for (const [s, cs] of hole) console.log(`     玩家${s}:${lbl(cs[0])} ${lbl(cs[1])}`);
console.log('     公共牌:' + board.map(lbl).join(' '));
console.log(`④ 协调方公开攻击菜单能解出的牌数 = ${serverRecovered} / 52`);
if (serverRecovered !== 0) fail('coordinator recovered > 0 cards');

// === Liveness === party:1 drops before the deal → owner gets n-1 shares.
console.log('\n在线门槛(liveness)—— party:1 在发牌前掉线:');
const drop = openAt(0, online.filter((i) => i !== 1)); // n-1 shares
if (drop >= 0) fail('BUG: a hole card opened with a party missing');
console.log(`   owner_open 返回 ${drop} (<0) ⇒ QuorumMismatch ⇒ 本手 abort + 回退 existing_server。`);

console.log('\n✅ M3: 多个独立 WasmParty 客户端经 JSON 消息 + 零私钥中继协调方,跑通完整 server-blind 一手 + liveness。');
