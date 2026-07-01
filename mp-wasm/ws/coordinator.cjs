// M3 transport — the COORDINATOR process: a TCP relay that holds NO secret
// (only public-only wasm fns; it never constructs a WasmParty or calls
// owner_open). It spawns N independent CLIENT processes, then drives the full
// server-blind hand over real sockets: DKG → verifiable shuffles → deal (hole
// shares routed only to the owner) → server-blind check. `--drop <seat>` kills a
// client before the deal to exercise n-of-n liveness over real transport.
//
//   node mp-wasm/ws/coordinator.cjs            (happy path)
//   node mp-wasm/ws/coordinator.cjs --drop 1   (liveness: kill party:1)
const net = require('node:net');
const path = require('node:path');
const { spawn } = require('node:child_process');
const wasm = require(path.join(__dirname, '..', 'pkg', 'mp_wasm.js'));
const { sendMsg, onMessages } = require('./transport.cjs');

const n = 3;
const PORT = 8131 + Math.floor(process.pid % 200);
const dropIdx = process.argv.includes('--drop') ? parseInt(process.argv[process.argv.indexOf('--drop') + 1], 10) : -1;
const lbl = (id) => '23456789TJQKA'[Math.floor(id / 4)] + '♣♦♥♠'[id % 4];
const fail = (m) => { console.error('❌ ' + m); process.exit(1); };

(async () => {
  const sockets = new Map();
  const inbox = [];
  const waiters = [];
  const deliver = (m) => {
    const i = waiters.findIndex((w) => w.match(m));
    if (i >= 0) { const [w] = waiters.splice(i, 1); w.resolve(m); }
    else inbox.push(m);
  };
  const waitFor = (match, ms = 6000) => {
    const i = inbox.findIndex(match);
    if (i >= 0) { const [m] = inbox.splice(i, 1); return Promise.resolve(m); }
    return new Promise((resolve, reject) => {
      const w = { match, resolve };
      waiters.push(w);
      w.t = setTimeout(() => { const k = waiters.indexOf(w); if (k >= 0) waiters.splice(k, 1); reject(new Error('timeout')); }, ms);
    });
  };

  const server = net.createServer((sock) => {
    sock.on('error', () => {}); // a SIGKILLed client resets its socket (liveness test) — don't crash
    onMessages(sock, (msg) => { if (msg.type === 'register') sockets.set(msg.party_id, sock); deliver(msg); });
  });
  await new Promise((r) => server.listen(PORT, '127.0.0.1', r));

  // Spawn N independent client processes (each holds its own secret).
  const kids = [];
  for (let i = 0; i < n; i++) {
    kids.push(spawn(process.execPath, [path.join(__dirname, 'client.cjs'), `party:${i}`, String(PORT)], { stdio: ['ignore', 'ignore', 'inherit'] }));
  }
  const order = Array.from({ length: n }, (_, i) => `party:${i}`);

  // === DKG === collect registrations, verify each PoK, derive Q.
  const regs = {};
  for (let i = 0; i < n; i++) {
    const { reg, party_id } = await waitFor((m) => m.type === 'register');
    if (!wasm.coord_verify_register(reg)) fail(`DKG PoK failed for ${party_id}`);
    regs[party_id] = JSON.parse(reg);
  }
  const pks = order.map((id) => ({ party_id: id, q_i: regs[id].q_i }));
  const pksJson = JSON.stringify(pks);
  const q = wasm.coord_joint_key(pksJson);
  for (const id of order) sendMsg(sockets.get(id), { type: 'context', q, pks });

  // === Verifiable shuffle round-robin ===
  let deck = wasm.coord_canonical_deck();
  for (let r = 0; r < n; r++) {
    sendMsg(sockets.get(`party:${r}`), { type: 'shuffle-request', deck, round: r });
    const m = await waitFor((x) => x.type === 'shuffle-reply' && x.round === r && x.party_id === `party:${r}`);
    const outDeck = JSON.stringify(m.output_deck);
    if (!wasm.coord_verify_shuffle(`party:${r}`, r, deck, outDeck, q, JSON.stringify(m.proof))) fail(`shuffle proof from party:${r} rejected`);
    deck = outDeck;
  }
  const deckArr = JSON.parse(deck);

  // Liveness: kill a client right before the deal.
  if (dropIdx >= 0) { kids[dropIdx].kill('SIGKILL'); }

  // === Deal === opener(idx)=owner; coordinator relays the OTHER n-1 shares to the
  // owner (never holds the full set for a hole card, never opens itself).
  const openIndex = async (idx, ms) => {
    const opener = idx < 2 * n ? idx % n : 0;
    const ct = JSON.stringify(deckArr[idx]);
    const shares = [];
    for (let i = 0; i < n; i++) if (i !== opener) sendMsg(sockets.get(`party:${i}`), { type: 'decrypt-request', idx, ct });
    for (let i = 0; i < n - 1; i++) { const m = await waitFor((x) => x.type === 'decrypt-reply' && x.idx === idx, ms); shares.push(m.share); }
    sendMsg(sockets.get(`party:${opener}`), { type: 'open-request', idx, ct, shares });
    const m = await waitFor((x) => x.type === 'open-reply' && x.idx === idx && x.party_id === `party:${opener}`, ms);
    return m.card;
  };

  if (dropIdx >= 0) {
    // Expect the open of an index needing the dropped party to time out → abort.
    try {
      await openIndex(0, 2500);
      fail('BUG: a hole card opened despite a dropped client');
    } catch (e) {
      console.log(`M3-transport · liveness over real sockets — killed party:${dropIdx} before the deal:`);
      console.log(`   coordinator's decrypt-request to the dropped client timed out (${e.message}) ⇒ hand un-completable (n-of-n)`);
      console.log('   ⇒ ABORT this hand ⇒ fall back to existing_server for the next hand.');
      kids.forEach((k) => k.kill());
      server.close();
      process.exit(0);
    }
  }

  const hole = [];
  for (let s = 0; s < n; s++) hole.push([s, [await openIndex(s), await openIndex(n + s)]]);
  const board = [];
  for (let i = 0; i < 5; i++) board.push(await openIndex(2 * n + i));

  const serverRecovered = wasm.coord_blind_check(deck, pksJson);

  console.log('M3-transport · 独立客户端进程 + 零私钥协调进程,经真实 socket 跑通 server-blind 一手\n');
  console.log(`① ${n} 个独立 OS 进程客户端连接 → 各自 DKG 注册 + PoK,协调进程逐一验证 → 联合公钥 Q`);
  console.log(`② 可验证洗牌:${n} 轮,跨进程传输,协调进程逐轮验证 ✓`);
  console.log('③ 发牌(底牌分片只路由给拥有者进程,协调进程从不开牌):');
  for (const [s, cs] of hole) console.log(`     玩家${s}:${lbl(cs[0])} ${lbl(cs[1])}`);
  console.log('     公共牌:' + board.map(lbl).join(' '));
  console.log(`④ 协调进程(零私钥)公开攻击菜单能解出的牌数 = ${serverRecovered} / 52`);

  for (const id of order) sendMsg(sockets.get(id), { type: 'done' });
  server.close();
  setTimeout(() => { kids.forEach((k) => k.kill()); process.exit(serverRecovered === 0 ? 0 : 1); }, 200);
})().catch((e) => fail(e.message || String(e)));
