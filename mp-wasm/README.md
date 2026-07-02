# mp-wasm — Phase-4 server-blind WEB crypto client (PROTOTYPE)

<!-- U48 (dual-AI OSS review): run instructions for the runnable harnesses. -->

A thin `wasm-bindgen` wrapper over `mental_poker::crypto_real` (ADR-063), so a
browser can run DKG / verifiable shuffle / threshold decryption locally — the
server only ever sees ciphertext. **Prototype, pending external audit.** The
crate is deliberately **detached from the parent Cargo workspace** (it has its
own `[workspace]` table and `Cargo.lock`).

## Prerequisites

- Rust toolchain + [`wasm-pack`](https://rustwasm.github.io/wasm-pack/) (`cargo install wasm-pack` or `brew install wasm-pack`)
- Node.js (for the `.cjs` harnesses)

## Build

Two wasm-pack targets, two output dirs (both gitignored):

```bash
cd mp-wasm

# 1) Node target → pkg/  (used by all .cjs harnesses)
wasm-pack build --target nodejs --out-dir pkg --release

# 2) Web target → pkg-web/  (used by browser_demo.html)
./build-web.sh
```

`build-web.sh` also vendors the artifacts into `../client-game/src/vendor/mp-wasm`
when that app tree exists (the private product repo). On a public OSS clone
there is no `client-game/`, so the vendor step is skipped and the artifacts
stay in `pkg-web/` — override the destination with `MP_WASM_VENDOR_DIR=<dir>`
if you want them copied elsewhere. <!-- U49 -->

A plain `cargo build` (native, no wasm target) also works and is the quick
compile check.

## Runnable harnesses

| Harness | What it proves | Run |
|---|---|---|
| `parity_check.cjs` | wasm build is byte-identical to the Rust KAT vectors + a live DKG→encrypt→threshold-open roundtrip on the node CSPRNG | `node mp-wasm/parity_check.cjs` (after the nodejs build) |
| `choreography_e2e.cjs` | full server-blind hand across independent `WasmParty` clients + a zero-secret relay coordinator, in one process | `node mp-wasm/choreography_e2e.cjs` |
| `ws/coordinator.cjs` | the same choreography over REAL sockets between independent OS processes; `--drop <seat>` exercises n-of-n liveness (hand aborts) | `node mp-wasm/ws/coordinator.cjs` · `node mp-wasm/ws/coordinator.cjs --drop 1` |
| `browser_demo.html` | the real Phase-4 crypto running in a real browser, byte-identical to Rust | build with `./build-web.sh`, then serve the **repo root** over HTTP (it fetches `../mental-poker/tests/vectors/`): `python3 -m http.server 8080` → open `http://localhost:8080/mp-wasm/browser_demo.html` |

## Demo-only caveats (dual-AI OSS review)

The `ws/` choreography harness is a transport/liveness demo, **not** a
production client:

- **U01** — the owner client sends its opened hole-card plaintext back to the
  coordinator in `open-reply` purely so the demo can print a hand summary; in
  this demo the coordinator therefore ends the hand knowing every hole card.
  A production client must never send an opened hole card off-device
  pre-showdown. Each client also prints its own opened cards locally (stderr) —
  that local print is the production-shaped path.
- **U02** — clients derive the joint key `Q` locally from the announced
  per-party pubkeys (their own `q_i` must be present) and reject a mismatching
  coordinator-supplied `q`. Production clients must additionally verify every
  other party's DKG proof-of-knowledge; the demo coordinator does not relay
  the registration proofs.
