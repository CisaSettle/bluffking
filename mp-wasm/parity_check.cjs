// Phase-4 web crypto parity check: load the wasm build (real crypto_real) and
// assert byte-identical output to the Rust KAT vectors, plus a live
// DKG→encrypt→threshold-open roundtrip driven by the browser/node CSPRNG.
//
//   node mp-wasm/parity_check.cjs        (after: wasm-pack build --target nodejs)
const { readFileSync } = require('node:fs');
const path = require('node:path');
const wasm = require('./pkg/mp_wasm.js');

const kat = JSON.parse(
  readFileSync(path.join(__dirname, '../mental-poker/tests/vectors/mp_phase4_ec.json'), 'utf8')
);

let pass = true;
function check(name, got, want) {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}`);
  if (!ok) {
    pass = false;
    console.log('   got :', got);
    console.log('   want:', want);
  }
}

check('KAT-1 pedersen_h', wasm.kat_pedersen_h(), kat.kat1_pedersen_h);
check('KAT-2 card_points (52)', JSON.parse(wasm.kat_card_points()), kat.kat2_card_points);
const ct = JSON.parse(wasm.kat_fixed_ct());
check('KAT-3 fixed ciphertext c1', ct.c1, kat.kat3_fixed_ciphertext.c1);
check('KAT-3 fixed ciphertext c2', ct.c2, kat.kat3_fixed_ciphertext.c2);
check('KAT-4 deck_hash v2', wasm.kat_deck_hash_v2(), kat.kat4_deck_hash_v2);

const rt = wasm.selftest_roundtrip();
const rtOk = rt === 'ok:7';
console.log(
  `${rtOk ? 'PASS' : 'FAIL'}  roundtrip DKG→encrypt→threshold-open (browser CSPRNG) → ${rt}`
);
if (!rtOk) pass = false;

console.log(
  pass
    ? '\n✅ ALL PARITY CHECKS PASS — the real Phase-4 crypto runs in wasm, byte-identical to Rust.'
    : '\n❌ PARITY FAILED'
);
process.exit(pass ? 0 : 1);
