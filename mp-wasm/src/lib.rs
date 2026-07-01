//! mp-wasm — Phase-4 server-blind WEB crypto core (ADR-063), PROTOTYPE pending
//! external audit. A thin wasm-bindgen surface over `mental_poker::crypto_real`
//! so the browser runs the real threshold-ElGamal / DKG / Chaum–Pedersen path
//! locally. The server never receives plaintext cards.
//!
//! This first milestone proves the crypto **cross-compiles to wasm and runs with
//! byte-parity** to the Rust KAT vectors (`mental-poker/tests/vectors/
//! mp_phase4_ec.json`), and that the browser CSPRNG path (OsRng → getrandom js)
//! drives a full DKG → encrypt → threshold-open roundtrip in-runtime.

use wasm_bindgen::prelude::*;

use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT as G;
use curve25519_dalek::ristretto::RistrettoPoint;
use curve25519_dalek::scalar::Scalar;
use rand::rngs::OsRng;

use mental_poker::crypto::{ShuffleProof, ShuffleProofProvider};
use mental_poker::crypto_real::decrypt::{
    partial_decrypt, verify_and_open, DecryptionShare, ThresholdDecryptionProof, SCHEME,
};
use mental_poker::crypto_real::dkg::{
    schnorr_prove, schnorr_sign_bound, schnorr_verify, schnorr_verify_bound, verify_dkg, DkgParty,
    DkgRun, SchnorrPok,
};
use mental_poker::crypto_real::ec::{
    canonical_starting_deck, card_id_from_point, card_point, deck_hash, is_identity_pubkey,
    pedersen_h, point_from_hex, point_to_hex, Ct, CtWire, EncDeck, DECK_SIZE,
};
use mental_poker::crypto_real::ed25519_signer::Ed25519SignatureProvider;
use mental_poker::crypto_real::shuffle::{RealShuffleProofProvider, Shuffle};
use mental_poker::signing::{KeyDirectory, SignatureProvider};
use rand::RngCore;
use std::collections::BTreeMap;

/// ADR-078 §5 — the fixed internal signer-id for [`WasmSigner`] /
/// [`ed25519_verify`]. The PARTY identity lives in the signed CLAIM
/// (`party_id`/`seat`/`q_i`), not in the signer-id, so `ed25519_verify` is a pure
/// "does this vk sign this message" check; the vk→seat binding is enforced
/// client-side in `handlePartyRegistry`.
const SIGNER_ID: &str = "self";

// ---- JSON wire helpers (every cross-client message is a JSON string) ----
fn deck_to_wires(deck: &[Ct]) -> Vec<CtWire> {
    deck.iter().map(|c| c.to_wire()).collect()
}
fn wires_to_deck(wires: &[CtWire]) -> Result<EncDeck, String> {
    wires
        .iter()
        .map(|w| Ct::from_wire(w).ok_or_else(|| "bad ciphertext".to_string()))
        .collect()
}
fn parse_pks(pks_json: &str) -> Result<Vec<(String, RistrettoPoint)>, String> {
    #[derive(serde::Deserialize)]
    struct P {
        party_id: String,
        q_i: String,
    }
    let ps: Vec<P> = serde_json::from_str(pks_json).map_err(|e| e.to_string())?;
    ps.into_iter()
        .map(|p| {
            point_from_hex(&p.q_i)
                .map(|q| (p.party_id, q))
                .ok_or_else(|| "bad q_i".to_string())
        })
        .collect()
}

/// A server-blind dealing CLIENT: holds its OWN secret key share x_i. Every method
/// returns/accepts JSON strings (the wire) and never exposes x_i. This is what the
/// Vue/Flutter client will drive; here it is exercised across independent JS
/// instances + a relay coordinator (M3).
#[wasm_bindgen]
pub struct WasmParty {
    inner: DkgParty,
}

#[wasm_bindgen]
impl WasmParty {
    #[wasm_bindgen(constructor)]
    pub fn new(party_id: String) -> WasmParty {
        let mut rng = OsRng;
        WasmParty { inner: DkgParty::generate(party_id, &mut rng) }
    }

    #[wasm_bindgen(getter)]
    pub fn party_id(&self) -> String {
        self.inner.party_id.clone()
    }

    /// DKG registration message `{party_id, q_i, pok}` — public material only.
    pub fn register(&self) -> String {
        let mut rng = OsRng;
        let pok = schnorr_prove(&self.inner.party_id, &self.inner.x_i, &self.inner.q_i, &mut rng);
        serde_json::json!({
            "party_id": self.inner.party_id,
            "q_i": point_to_hex(&self.inner.q_i),
            "pok": pok,
        })
        .to_string()
    }

    /// Verifiably shuffle the deck under the joint key Q → `{output_deck, proof}`.
    /// The permutation witness never leaves the party.
    pub fn shuffle(&self, deck_json: String, q_hex: String, round: u32) -> Result<String, String> {
        let mut rng = OsRng;
        let q = point_from_hex(&q_hex).ok_or("bad q_hex")?;
        let wires: Vec<CtWire> = serde_json::from_str(&deck_json).map_err(|e| e.to_string())?;
        // The deck comes from the untrusted coordinator (ADR-063/068
        // T-ACTIVE-COORDINATOR). Validate its length BEFORE proving: an empty deck
        // would underflow `prod_blind[n - 1]` in the permutation prover and TRAP
        // the whole wasm instance instead of returning `Err`, and an over-large
        // deck is O(n) needless work. The shuffle only ever operates on the full
        // 52-card deck, so require exactly that.
        if wires.len() != DECK_SIZE {
            return Err(format!("deck must have {DECK_SIZE} cards, got {}", wires.len()));
        }
        let input = wires_to_deck(&wires)?;
        let sh = Shuffle::perform(input, &q, &mut rng);
        let proof = sh.prove(&self.inner.party_id, round, &mut rng);
        Ok(serde_json::json!({
            "output_deck": deck_to_wires(&sh.output),
            "proof": proof,
        })
        .to_string())
    }

    /// This party's Chaum–Pedersen partial decryption of one deck index → a
    /// `DecryptionShare` JSON. Needs the secret x_i (held internally).
    pub fn partial_decrypt(&self, idx: u32, ct_json: String) -> Result<String, String> {
        let mut rng = OsRng;
        let w: CtWire = serde_json::from_str(&ct_json).map_err(|e| e.to_string())?;
        let ct = Ct::from_wire(&w).ok_or("bad ciphertext")?;
        let share = partial_decrypt(&self.inner, idx, &ct, &mut rng);
        serde_json::to_string(&share).map_err(|e| e.to_string())
    }

    /// ADR-078 §5.1 — produce `vk_dlog_binding`: a message-binding Schnorr
    /// signature UNDER this party's secret `x_i` (verifiable against the PUBLIC
    /// `q_i`) over `message` = the canonical `bindClaim_v2` utf8 bytes (INCLUDING
    /// `ed25519_vk`). Returns the `{"r","s"}` JSON (the existing `SchnorrPok`
    /// shape).
    ///
    /// This is the load-bearing TH-K close: a coordinator that does NOT hold this
    /// party's `x_i` cannot produce a passing `vk_dlog_binding` over this party's
    /// `q_i` — so it cannot mint its own Ed25519 vk over a COPIED real `q_i`. The
    /// secret `x_i` never leaves the party (mirrors `register`/`partial_decrypt`).
    /// Nonce is a fresh `Scalar::random(OsRng)` per signature (inside
    /// `schnorr_sign_bound`).
    pub fn schnorr_sign_bind(&self, message: String) -> String {
        let mut rng = OsRng;
        let sig = schnorr_sign_bound(
            &self.inner.party_id,
            &self.inner.x_i,
            &self.inner.q_i,
            message.as_bytes(),
            &mut rng,
        );
        // SchnorrPok serializes infallibly to `{"r":..,"s":..}`.
        serde_json::to_string(&sig).unwrap_or_else(|_| "{}".to_string())
    }
}

// ---- ADR-078 §5 — Ed25519 per-hand showdown-attestation signer ----

/// ADR-078 §3.1/§5 — a per-hand Ed25519 signer. Holds the 32-byte secret seed
/// INTERNALLY (never exported), under the fixed internal signer-id [`SIGNER_ID`].
/// Mirrors [`WasmParty`]'s `x_i` discipline: only the public `verifying_key` is
/// ever exposed, so a holder of public keys alone (including the coordinator)
/// cannot forge this seat's showdown attestation (the asymmetry boundary).
#[wasm_bindgen]
pub struct WasmSigner {
    provider: Ed25519SignatureProvider,
}

impl Default for WasmSigner {
    fn default() -> Self {
        Self::new()
    }
}

#[wasm_bindgen]
impl WasmSigner {
    /// Generate a fresh Ed25519 keypair from the browser CSPRNG (OsRng →
    /// getrandom js/wasm_js). The 32-byte seed is moved into the provider and
    /// dropped from this scope; only the provider (holding it) survives.
    #[wasm_bindgen(constructor)]
    pub fn new() -> WasmSigner {
        let mut seed = [0u8; 32];
        OsRng.fill_bytes(&mut seed);
        WasmSigner {
            provider: Ed25519SignatureProvider::new().with_signer(SIGNER_ID, &seed),
        }
    }

    /// The PUBLIC Ed25519 verifying key, 64-hex. Safe to publish (the asymmetry
    /// boundary); registered via `mp_register_key.ed25519_vk`.
    #[wasm_bindgen(getter)]
    pub fn verifying_key(&self) -> String {
        // `with_signer` always stores the matching vk under SIGNER_ID.
        self.provider
            .verifying_key_hex(SIGNER_ID)
            .unwrap_or_default()
    }

    /// Deterministic RFC-8032 Ed25519 signature over `message` (the utf8 bytes of
    /// the canonical claim — `bindClaim_v2` for `ed25519_vk_binding`, or
    /// `attestClaim` for the showdown attestation), 128-hex. The secret never
    /// leaves this object.
    pub fn sign(&self, message: String) -> String {
        self.provider.sign(SIGNER_ID, message.as_bytes())
    }
}

/// ADR-078 §5 — stateless Ed25519 verify: does `signature_hex` (128-hex) verify
/// under `vk_hex` (64-hex verifying key) over `message`? Returns `false` on any
/// malformed input (bad hex, wrong length, non-canonical vk, bad sig) — no panic
/// (threat T8/T9). Used client-side to verify a peer's `ed25519_vk_binding` and
/// `MpShowdownAttest` under the peer's pinned vk.
#[wasm_bindgen]
pub fn ed25519_verify(vk_hex: String, message: String, signature_hex: String) -> bool {
    // Rebuild a verifier-only provider from a one-entry public directory.
    // verifier_from_directory rejects a malformed/non-canonical vk; verify
    // rejects a malformed/wrong signature.
    let dir = KeyDirectory {
        keys: BTreeMap::from([(SIGNER_ID.to_string(), vk_hex)]),
        is_mock: false,
    };
    match Ed25519SignatureProvider::verifier_from_directory(&dir) {
        Some(v) => v.verify(SIGNER_ID, message.as_bytes(), &signature_hex),
        None => false,
    }
}

/// ADR-078 §5.1 — stateless verify of a `vk_dlog_binding` against the PUBLIC
/// `q_i` (no secret needed). `q_i_hex` is the 64-hex Ristretto point; `sig_json`
/// is the `{"r","s"}` `SchnorrPok` JSON; `message` is the canonical
/// `bindClaim_v2` utf8 bytes. Returns `false` on any malformed input (T9). Called
/// in `handlePartyRegistry` for each peer to prove the binder ALSO holds
/// `x_i = log_G(q_i)` (closes TH-K — a minted vk over a COPIED real `q_i` fails
/// here because the coordinator lacks `x_i`).
#[wasm_bindgen]
pub fn schnorr_verify_bind(
    party_id: String,
    q_i_hex: String,
    message: String,
    sig_json: String,
) -> bool {
    let q = match point_from_hex(&q_i_hex) {
        Some(q) => q,
        None => return false,
    };
    let sig: SchnorrPok = match serde_json::from_str(&sig_json) {
        Ok(s) => s,
        Err(_) => return false,
    };
    schnorr_verify_bound(&party_id, &q, message.as_bytes(), &sig)
}

// ---- Coordinator: PUBLIC-ONLY operations (it holds no secret share) ----

/// Coordinator verifies a party's DKG registration PoK (rogue-key defence).
#[wasm_bindgen]
pub fn coord_verify_register(reg_json: String) -> bool {
    #[derive(serde::Deserialize)]
    struct Reg {
        party_id: String,
        q_i: String,
        pok: SchnorrPok,
    }
    let r: Reg = match serde_json::from_str(&reg_json) {
        Ok(r) => r,
        Err(_) => return false,
    };
    match point_from_hex(&r.q_i) {
        // F2/F-CRYPTO-15: reject an identity (x_i = 0) party key — the Schnorr
        // PoK verifies trivially for x = 0, so this must be a distinct gate. An
        // identity key silently degrades n-of-n confidentiality; the coordinator-
        // side verifier mirrors the server registration reject so the client and
        // server agree on which keys are valid.
        Some(q) if is_identity_pubkey(&q) => false,
        Some(q) => schnorr_verify(&r.party_id, &q, &r.pok),
        None => false,
    }
}

/// Coordinator computes the joint key Q = Σ q_i. `pks_json = [{party_id,q_i}]`.
#[wasm_bindgen]
pub fn coord_joint_key(pks_json: String) -> Result<String, String> {
    let pks = parse_pks(&pks_json)?;
    let q: RistrettoPoint = pks.iter().map(|(_, k)| *k).sum();
    Ok(point_to_hex(&q))
}

/// Coordinator's canonical starting deck (public commitment to canonical order).
#[wasm_bindgen]
pub fn coord_canonical_deck() -> String {
    serde_json::to_string(&deck_to_wires(&canonical_starting_deck())).unwrap()
}

/// Hex of `crypto_real::ec::deck_hash` over a JSON ciphertext deck (ADR-068 §2.5).
///
/// This is the CANONICAL real-path deck hash the server uses as `ct_deck_hash`
/// (`mp_dealing.rs`). The web client recomputes it over the committed
/// `mp_final_deck.deck_ct` and compares against the server-sent
/// `final_deck_hash` to detect a Byzantine coordinator that substituted the
/// deck (T-ACTIVE-COORDINATOR). Returns `"ERR:..."` on a malformed deck so the
/// caller can `mp_abort` instead of trusting a bad frame.
#[wasm_bindgen]
pub fn coord_deck_hash(deck_json: String) -> String {
    let wires: Vec<CtWire> = match serde_json::from_str(&deck_json) {
        Ok(w) => w,
        Err(e) => return format!("ERR:bad-json:{e}"),
    };
    match wires_to_deck(&wires) {
        Ok(deck) => hex::encode(deck_hash(&deck)),
        Err(e) => format!("ERR:{e}"),
    }
}

/// Coordinator verifies a shuffle proof against the pinned joint key (public-only).
#[wasm_bindgen]
pub fn coord_verify_shuffle(
    party_id: String,
    round: u32,
    input_deck_json: String,
    output_deck_json: String,
    q_hex: String,
    proof_json: String,
) -> bool {
    let parse = |s: &str| -> Option<EncDeck> {
        let w: Vec<CtWire> = serde_json::from_str(s).ok()?;
        wires_to_deck(&w).ok()
    };
    let (indeck, outdeck) = match (parse(&input_deck_json), parse(&output_deck_json)) {
        (Some(a), Some(b)) => (a, b),
        _ => return false,
    };
    let proof: ShuffleProof = match serde_json::from_str(&proof_json) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let v = RealShuffleProofProvider::verifier_with_expected_key(q_hex);
    v.verify_shuffle(&party_id, round, &deck_hash(&indeck), &deck_hash(&outdeck), None, &proof)
}

/// Coordinator's server-blindness self-check: how many cards a KEYLESS coordinator
/// can recover from public data (attack menu). MUST be 0.
#[wasm_bindgen]
pub fn coord_blind_check(deck_json: String, pks_json: String) -> Result<usize, String> {
    let wires: Vec<CtWire> = serde_json::from_str(&deck_json).map_err(|e| e.to_string())?;
    let deck = wires_to_deck(&wires)?;
    let pks = parse_pks(&pks_json)?;
    let qsum: RistrettoPoint = pks.iter().map(|(_, k)| *k).sum();
    let opens = |ct: &Ct| -> bool {
        let mut cands = vec![ct.c2, ct.c2 - qsum, ct.c2 - ct.c1, ct.c1, -ct.c2];
        for k in 0u64..64 {
            let s = Scalar::from(k);
            cands.push(ct.c2 - s * qsum);
            cands.push(ct.c2 - s * ct.c1);
            cands.push(ct.c2 - s * G);
        }
        cands.iter().any(|m| card_id_from_point(m).is_some())
    };
    Ok(deck.iter().filter(|ct| opens(ct)).count())
}

/// Owner combines the n shares routed to it for a card it is entitled to → card id
/// (or -1 on failure / quorum mismatch). The coordinator never calls this for a
/// card it does not own (owner-only routing is enforced by the relay).
#[wasm_bindgen]
pub fn owner_open(idx: u32, ct_json: String, pks_json: String, shares_json: String) -> i32 {
    let w: CtWire = match serde_json::from_str(&ct_json) {
        Ok(w) => w,
        Err(_) => return -1,
    };
    let ct = match Ct::from_wire(&w) {
        Some(c) => c,
        None => return -1,
    };
    let pks = match parse_pks(&pks_json) {
        Ok(p) => p,
        Err(_) => return -1,
    };
    let shares: Vec<DecryptionShare> = match serde_json::from_str(&shares_json) {
        Ok(s) => s,
        Err(_) => return -1,
    };
    let proof = ThresholdDecryptionProof { scheme: SCHEME.to_string(), shares };
    match verify_and_open(idx, &ct, &pks, &proof) {
        Ok(id) => id as i32,
        Err(_) => -1,
    }
}

/// KAT-1: the Pedersen generator H = hash_to_ristretto("mp:gen-H:v1").
#[wasm_bindgen]
pub fn kat_pedersen_h() -> String {
    point_to_hex(pedersen_h())
}

/// KAT-2: all 52 card-point commitments, as a JSON array of compressed hex.
#[wasm_bindgen]
pub fn kat_card_points() -> String {
    let pts: Vec<String> = (0..DECK_SIZE as u8)
        .map(|id| format!("\"{}\"", point_to_hex(&card_point(id))))
        .collect();
    format!("[{}]", pts.join(","))
}

/// KAT-3: a fixed ElGamal ciphertext of card 7 under Q=12345·G, r=67890.
#[wasm_bindgen]
pub fn kat_fixed_ct() -> String {
    let q = Scalar::from(12345u64) * G;
    let r = Scalar::from(67890u64);
    let w = Ct::encrypt_card(7, &q, &r).to_wire();
    format!("{{\"c1\":\"{}\",\"c2\":\"{}\"}}", w.c1, w.c2)
}

/// KAT-4: deck_hash v2 of a fixed 52-card encrypted deck.
#[wasm_bindgen]
pub fn kat_deck_hash_v2() -> String {
    let q = Scalar::from(12345u64) * G;
    let deck: Vec<Ct> = (0..DECK_SIZE as u8)
        .map(|id| Ct::encrypt_card(id, &q, &Scalar::from((id as u64) + 1)))
        .collect();
    hex::encode(deck_hash(&deck))
}

/// Full server-blind roundtrip driven by the browser CSPRNG (OsRng → getrandom
/// js): DKG(n=3) → encrypt card 7 under the joint key → each party
/// partial-decrypts with a Chaum–Pedersen proof → combine + verify → recover the
/// card. Returns `"ok:7"` on success, `"ERR:..."` otherwise. Proves the RANDOM
/// crypto paths (not just deterministic KATs) execute in the wasm runtime.
#[wasm_bindgen]
pub fn selftest_roundtrip() -> String {
    let mut rng = OsRng;
    let run = DkgRun::simulate(3, &mut rng);
    match verify_dkg(&run.commitments, &run.shares) {
        Ok(q) if q == run.joint_key => {}
        Ok(_) => return "ERR:dkg-key-mismatch".to_string(),
        Err(e) => return format!("ERR:dkg-verify:{e:?}"),
    }
    let pks: Vec<(String, RistrettoPoint)> =
        run.parties.iter().map(|p| (p.party_id.clone(), p.q_i)).collect();

    let card = 7u8;
    let r = Scalar::random(&mut rng);
    let ct = Ct::encrypt_card(card, &run.joint_key, &r);
    let proof = ThresholdDecryptionProof {
        scheme: SCHEME.to_string(),
        shares: run.parties.iter().map(|p| partial_decrypt(p, 0, &ct, &mut rng)).collect(),
    };
    match verify_and_open(0, &ct, &pks, &proof) {
        Ok(id) if id == card => format!("ok:{id}"),
        Ok(id) => format!("ERR:wrong-card:{id}"),
        Err(e) => format!("ERR:open:{e:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use curve25519_dalek::traits::Identity;

    /// F2 / F-CRYPTO-15: the coordinator-side register verifier must reject an
    /// IDENTITY (x_i = 0) party key, even when the accompanying Schnorr PoK is
    /// genuinely valid for x = 0 (the PoK gate alone cannot catch it). This
    /// mirrors the server-side registration reject so the web verifier and the
    /// server agree on which keys are valid.
    #[test]
    fn coord_verify_register_rejects_identity_key() {
        let mut rng = OsRng;
        let party_id = "party:0";
        let id_point = RistrettoPoint::identity();
        // A PoK that genuinely verifies for x = 0 — only `is_identity_pubkey` can
        // reject this key, NOT the PoK check.
        let pok = schnorr_prove(party_id, &Scalar::ZERO, &id_point, &mut rng);
        assert!(
            schnorr_verify(party_id, &id_point, &pok),
            "the identity PoK for x=0 must itself verify — proving the reject is the \
             identity gate, not the PoK gate"
        );
        let reg = serde_json::json!({
            "party_id": party_id,
            "q_i": point_to_hex(&id_point),
            "pok": pok,
        });
        assert!(
            !coord_verify_register(reg.to_string()),
            "coord_verify_register MUST reject the identity (x_i = 0) party key (F2)"
        );
    }

    /// Sanity: a well-formed non-identity key with a matching PoK is accepted, so
    /// the identity reject above is the discriminating factor, not a blanket false.
    #[test]
    fn coord_verify_register_accepts_real_key() {
        let mut rng = OsRng;
        let party = DkgParty::generate("party:0".to_string(), &mut rng);
        let pok = schnorr_prove(&party.party_id, &party.x_i, &party.q_i, &mut rng);
        let reg = serde_json::json!({
            "party_id": party.party_id,
            "q_i": point_to_hex(&party.q_i),
            "pok": pok,
        });
        assert!(
            coord_verify_register(reg.to_string()),
            "a valid non-identity registration must still be accepted"
        );
    }

    // ---- ADR-078 §5 — WasmSigner (Ed25519) + the bind wrappers ----

    /// ADR-078 §5: `WasmSigner` keygen → `verifying_key` is a 64-hex public key
    /// distinct from any secret, and a fresh signer yields a fresh key (the seed
    /// rides OsRng → getrandom; here native OsRng).
    #[test]
    fn wasm_signer_keygen_exposes_only_public_vk() {
        let s = WasmSigner::new();
        let vk = s.verifying_key();
        assert_eq!(vk.len(), 64, "Ed25519 vk is 32 bytes / 64 hex chars");
        assert!(hex::decode(&vk).is_ok(), "vk must be valid hex");
        // Two independent signers get distinct keys (real CSPRNG keygen).
        let s2 = WasmSigner::new();
        assert_ne!(vk, s2.verifying_key(), "fresh keygen must differ");
    }

    /// ADR-078 §5: sign → `ed25519_verify` round-trip under the right vk; a
    /// DIFFERENT signer's vk rejects the sig (cross-key forge), a tampered message
    /// rejects, and malformed input is a clean reject (no panic).
    #[test]
    fn wasm_signer_sign_verify_round_trip_and_forge_reject() {
        let signer = WasmSigner::new();
        let vk = signer.verifying_key();
        let msg = "mp:showdown-attest:v1 over this exact claim".to_string();
        let sig = signer.sign(msg.clone());
        assert_eq!(sig.len(), 128, "Ed25519 sig is 64 bytes / 128 hex chars");

        // Round-trip: verifies under the signer's own vk.
        assert!(ed25519_verify(vk.clone(), msg.clone(), sig.clone()));

        // Cross-key forge: another signer's vk must NOT accept this sig.
        let other = WasmSigner::new();
        assert!(
            !ed25519_verify(other.verifying_key(), msg.clone(), sig.clone()),
            "a different vk must reject the sig (asymmetry / forge-reject)"
        );

        // Tampered message rejects under the same vk.
        assert!(
            !ed25519_verify(vk.clone(), "tampered claim".to_string(), sig.clone()),
            "a tampered message must reject"
        );

        // Malformed inputs are a clean reject (no panic).
        assert!(!ed25519_verify("not-hex".to_string(), msg.clone(), sig.clone()));
        assert!(!ed25519_verify(vk.clone(), msg.clone(), "00".to_string()));
        assert!(!ed25519_verify("ab".repeat(31), msg, sig));
    }

    /// ADR-078 §3.2: the Ed25519 self-signature (`ed25519_vk_binding`) over a
    /// `bindClaim_v2`-shaped message round-trips under the signer's vk — this is
    /// the field-(1) binding the client verifies in `handlePartyRegistry`.
    #[test]
    fn wasm_signer_bind_claim_self_sig_round_trip() {
        let signer = WasmSigner::new();
        let vk = signer.verifying_key();
        let bind_claim = format!(
            r#"{{"v":"mp:sigkey-bind:v2","session_id":"s","hand_id":"h","party_id":"party:0","q_i":"<q>","ed25519_vk":"{vk}"}}"#
        );
        let self_sig = signer.sign(bind_claim.clone());
        assert!(
            ed25519_verify(vk, bind_claim.clone(), self_sig.clone()),
            "ed25519_vk_binding must verify under the signer's own vk"
        );
        // A different vk rejects the self-sig (the TH-K field-1 cannot be reused
        // under a minted vk over the SAME claim).
        let attacker = WasmSigner::new();
        assert!(!ed25519_verify(
            attacker.verifying_key(),
            bind_claim,
            self_sig
        ));
    }

    /// ADR-078 §5.1 / TH-K: `WasmParty.schnorr_sign_bind` over `bindClaim_v2`
    /// verifies via the free `schnorr_verify_bind` against the party's REAL
    /// PUBLIC `q_i`; and a sig produced under a DIFFERENT `x_i` (the coordinator
    /// minting its own vk over a COPIED real `q_i`) is REJECTED against the
    /// honest `q_i` — the crypto unit that a v1-only self-sig binding cannot
    /// satisfy.
    #[test]
    fn wasm_party_vk_dlog_binding_round_trip_and_forge_reject() {
        let mut rng = OsRng;
        // The honest party (holds x_i internally via WasmParty).
        let honest = WasmParty::new("party:0".to_string());
        // Recover the honest party's public q_i from its register JSON.
        let reg: serde_json::Value = serde_json::from_str(&honest.register()).unwrap();
        let q_i_hex = reg["q_i"].as_str().unwrap().to_string();

        let bind_claim =
            r#"{"v":"mp:sigkey-bind:v2","q_i":"<honest>","ed25519_vk":"deadbeef"}"#.to_string();

        // The honest vk_dlog_binding verifies against the real q_i.
        let sig_json = honest.schnorr_sign_bind(bind_claim.clone());
        assert!(
            schnorr_verify_bind(
                "party:0".to_string(),
                q_i_hex.clone(),
                bind_claim.clone(),
                sig_json
            ),
            "the honest vk_dlog_binding must verify against the real q_i"
        );

        // FORGE (TH-K shape): the coordinator signs under its OWN x' over the SAME
        // claim, then presents that sig against the HONEST q_i. It must FAIL — the
        // coordinator lacks the honest x_i, so it cannot bind a vk' to q_i.
        let forger = WasmParty::new("party:0".to_string()); // a different x'
        let forged_json = forger.schnorr_sign_bind(bind_claim.clone());
        assert!(
            !schnorr_verify_bind("party:0".to_string(), q_i_hex.clone(), bind_claim, forged_json),
            "a vk_dlog_binding produced under a different x_i must NOT verify against the honest q_i (TH-K)"
        );

        // Malformed sig / q_i are clean rejects.
        assert!(!schnorr_verify_bind(
            "party:0".to_string(),
            "not-a-point".to_string(),
            "m".to_string(),
            r#"{"r":"00","s":"00"}"#.to_string()
        ));
        let _ = &mut rng;
    }
}
