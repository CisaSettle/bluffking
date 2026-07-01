// A server-blind dealing CLIENT, as its OWN OS process. It holds its secret x_i
// inside this process (in a WasmParty); it only ever sends public material +
// proofs + shares over the socket. argv: <party_id> <port>.
const path = require('node:path');
const net = require('node:net');
const wasm = require(path.join(__dirname, '..', 'pkg', 'mp_wasm.js'));
const { sendMsg, onMessages } = require('./transport.cjs');

const partyId = process.argv[2];
const port = parseInt(process.argv[3], 10);
const party = new wasm.WasmParty(partyId);
let q = null;
let pksJson = null;

const sock = net.connect(port, '127.0.0.1', () => {
  sendMsg(sock, { type: 'register', party_id: partyId, reg: party.register() });
});

onMessages(sock, (msg) => {
  try {
    switch (msg.type) {
      case 'context':
        q = msg.q;
        pksJson = JSON.stringify(msg.pks);
        break;
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
