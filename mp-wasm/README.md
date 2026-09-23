# mp-wasm — Phase-4 server-blind WEB crypto client (PROTOTYPE)

A thin `wasm-bindgen` wrapper over `mental_poker::crypto_real` (ADR-063), so a
browser can run DKG / verifiable shuffle / threshold decryption locally — the
server only ever sees ciphertext. **Prototype, pending external audit.** The
crate is deliberately **detached from the parent Cargo workspace** (it has its
own `[workspace]` table and `Cargo.lock`).

## Prerequisites

- Rust toolchain + [`wasm-pack`](https://rustwasm.github.io/wasm-pack/) (`cargo install wasm-pack` or `brew install wasm-pack`)

## Build

Web target → `pkg-web/` (gitignored):

```bash
cd mp-wasm
./build-web.sh
```

`build-web.sh` also vendors the artifacts into `../client-game/src/vendor/mp-wasm`
when that app tree exists (the private product repo). On a public OSS clone
there is no `client-game/`, so the vendor step is skipped and the artifacts
stay in `pkg-web/` — override the destination with `MP_WASM_VENDOR_DIR=<dir>`
if you want them copied elsewhere. <!-- U49 -->

A plain `cargo build` (native, no wasm target) also works and is the quick
compile check.
