// A server-blind dealing CLIENT, as its OWN OS process. It holds its secret x_i
// inside this process (in a WasmParty); it only ever sends public material +
// proofs + shares over the socket. argv: <party_id> <port>.
//
// ⚠️ DEMO-ONLY caveats (dual-AI OSS review) — this is a choreography test
// harness, NOT a production client:
//   U01: the `open-reply` below carries the OPENED hole-card PLAINTEXT back to
//        the coordinator, purely so the demo coordinator can print the hand
//        summary — i.e. in THIS demo the coordinator ends the hand knowing
//        every hole card. A production client must NEVER send an opened hole
//        card off-device pre-showdown. Each client here also prints its own
//        opened cards locally (stderr) — that local print is the
//        production-shaped path; the wire copy is demo display only.
//   U02: the joint key Q is DERIVED LOCALLY from the announced per-party
//        pubkeys (and this party's own q_i must be among them); a
//        coordinator-supplied q that mismatches is REJECTED — see `context`.
const path = require('node:path');
const net = require('node:net');
const wasm = require(path.join(__dirname, '..', 'pkg', 'mp_wasm.js'));
const { sendMsg, onMessages } = require('./transport.cjs');

const partyId = process.argv[2];
const port = parseInt(process.argv[3], 10);
const party = new wasm.WasmParty(partyId);
// U02 (dual-AI OSS review): keep our own registration so `context` can check
// the coordinator's announced pks actually contain OUR q_i.
const myRegJson = party.register();
const myQi = JSON.parse(myRegJson).q_i;
const lbl = (id) => '23456789TJQKA'[Math.floor(id / 4)] + '♣♦♥♠'[id % 4];
let q = null;
let pksJson = null;

const sock = net.connect(port, '127.0.0.1', () => {
  sendMsg(sock, { type: 'register', party_id: partyId, reg: myRegJson });
});

onMessages(sock, (msg) => {
  try {
    switch (msg.type) {
      case 'context': {
        // U02 (dual-AI OSS review): never trust a coordinator-supplied joint
        // key — a Byzantine coordinator could substitute a key it controls.
        // Require our own q_i among the announced pks, derive Q locally from
        // them, and reject a mismatching coordinator q. (Production clients
        // must ALSO verify every other party's DKG PoK; this demo coordinator
        // does not relay the registration proofs.)
        const mine = (msg.pks || []).find((p) => p.party_id === partyId);
        if (!mine || mine.q_i !== myQi) throw new Error('context pks do not contain my own q_i — rejecting');
        pksJson = JSON.stringify(msg.pks);
        const localQ = wasm.coord_joint_key(pksJson);
        if (msg.q !== undefined && msg.q !== localQ) throw new Error('coordinator-supplied joint key != locally derived Q — rejecting');
        q = localQ;
        break;
      }
      case 'shuffle-request': {
        const res = JSON.parse(party.shuffle(msg.deck, q, msg.round));
        sendMsg(sock, { type: 'shuffle-reply', party_id: partyId, round: msg.round, output_deck: res.output_deck, proof: res.proof });
        break;
      }
      case 'decrypt-request': {
        // Help decrypt a card this party does NOT own (its share goes to the owner).
        const share = party.partial_decrypt(msg.idx, msg.ct);
        sendMsg(sock, { type: 'decrypt-reply', party_id: partyId, idx: msg.idx, share });
        break;
      }
      case 'open-request': {
        // This party OWNS index msg.idx: add its own share to the relayed ones and open.
        const own = party.partial_decrypt(msg.idx, msg.ct);
        const all = msg.shares.concat([own]);
        const card = wasm.owner_open(msg.idx, msg.ct, pksJson, '[' + all.join(',') + ']');
        // U01 (dual-AI OSS review): the owner prints its own opened card
        // LOCALLY (stderr is inherited) — in production this local sink is the
        // ONLY place hole-card plaintext may appear before showdown.
        const isHole = msg.idx < 2 * JSON.parse(pksJson).length;
        console.error(`   [${partyId}] opened ${isHole ? 'MY HOLE card' : 'board card'} idx=${msg.idx} → ${lbl(card)}${isHole ? ' (production: stays local)' : ''}`);
        // U01 DEMO-ONLY: `card` rides back to the coordinator purely so the
        // demo can print its hand summary — a production client must NOT
        // include hole-card plaintext in open-reply.
        sendMsg(sock, { type: 'open-reply', party_id: partyId, idx: msg.idx, card });
        break;
      }
      case 'done':
        sock.end();
        process.exit(0);
    }
  } catch (e) {
    console.error(`[${partyId}] error: ${e.message || e}`);
    process.exit(2);
  }
});
sock.on('error', () => process.exit(3));
