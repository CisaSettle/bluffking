//! mp-wasm — browser crypto core for engine-blind dealing (ADR-068). Live under
//! ADR-101's trusted-single-operator scope; no external audit has been done and
//! no trustless claim is made. A thin wasm-bindgen surface over `mental_poker::crypto_real`
//! so the browser runs the real threshold-ElGamal / DKG / Chaum–Pedersen path
//! locally. The server never receives plaintext cards.
//!
//! The browser CSPRNG path is OsRng → getrandom js.

use wasm_bindgen::prelude::*;

use curve25519_dalek::ristretto::RistrettoPoint;
use rand::rngs::OsRng;

use mental_poker::crypto_real::decrypt::{
    partial_decrypt, verify_and_open, DecryptionShare, ThresholdDecryptionProof, SCHEME,
};
use mental_poker::crypto_real::dkg::{
    schnorr_prove, schnorr_sign_bound, schnorr_verify, schnorr_verify_bound, DkgParty, SchnorrPok,
};
use mental_poker::crypto_real::ec::{
    deck_hash, is_identity_pubkey, point_from_hex, point_to_hex, Ct, CtWire, EncDeck, DECK_SIZE,
};
use mental_poker::crypto_real::ed25519_signer::Ed25519SignatureProvider;
use mental_poker::crypto_real::shuffle::Shuffle;
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
    recovery_prepared: Option<(String, String)>,
    recovery_masks: BTreeMap<String, mental_poker::crypto_real::recovery::RecoveryMask>,
    recovery_evaluations: BTreeMap<String, mental_poker::crypto_real::recovery::RecoveryEvaluation>,
}

#[wasm_bindgen]
impl WasmParty {
    #[wasm_bindgen(constructor)]
    pub fn new(party_id: String) -> WasmParty {
        let mut rng = OsRng;
        WasmParty {
            inner: DkgParty::generate(party_id, &mut rng),
            recovery_prepared: None,
            recovery_masks: BTreeMap::new(),
            recovery_evaluations: BTreeMap::new(),
        }
    }

    #[wasm_bindgen(getter)]
    pub fn party_id(&self) -> String {
        self.inner.party_id.clone()
    }

    /// DKG registration message `{party_id, q_i, pok}` — public material only.
    pub fn register(&self) -> String {
        let mut rng = OsRng;
        let pok = schnorr_prove(
            &self.inner.party_id,
            &self.inner.x_i,
            &self.inner.q_i,
            &mut rng,
        );
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
        // The deck comes from the untrusted coordinator (ADR-068
        // T-ACTIVE-COORDINATOR). Validate its length BEFORE proving: an empty deck
        // would underflow `prod_blind[n - 1]` in the permutation prover and TRAP
        // the whole wasm instance instead of returning `Err`, and an over-large
        // deck is O(n) needless work. The shuffle only ever operates on the full
        // 52-card deck, so require exactly that.
        if wires.len() != DECK_SIZE {
            return Err(format!(
                "deck must have {DECK_SIZE} cards, got {}",
                wires.len()
            ));
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

    /// Prepare immutable card-scoped backups. Masks stay in this WASM party.
    pub fn recovery_prepare(
        &mut self,
        binding: String,
        deck_json: String,
        keys_json: String,
    ) -> Result<String, String> {
        use mental_poker::crypto_real::recovery as r;
        if let Some((old, payload)) = &self.recovery_prepared {
            return if old == &binding {
                Ok(payload.clone())
            } else {
                Err("recovery_reprepare".into())
            };
        }
        let keys: Vec<r::RecoveryKey> =
            serde_json::from_str(&keys_json).map_err(|_| "recovery_keys")?;
        let deck: Vec<CtWire> = serde_json::from_str(&deck_json).map_err(|_| "recovery_deck")?;
        if deck.len() != 52 {
            return Err("recovery_deck".into());
        }
        let n = keys.len();
        let seat = keys
            .iter()
            .position(|k| k.party_id == self.inner.party_id)
            .ok_or("recovery_party")?;
        let mut bundles = Vec::new();
        let mut masks = BTreeMap::new();
        for index in [seat, seat + n].into_iter().chain(2 * n..2 * n + 5) {
            let ct =
                Ct::from_wire(deck.get(index).ok_or("recovery_index")?).ok_or("recovery_ct")?;
            let (bundle, mask) = r::create(
                &self.inner,
                binding.clone(),
                index as u32,
                &ct,
                keys.clone(),
                index < 2 * n,
                &mut OsRng,
            )?;
            if let Some(mask) = mask {
                masks.insert(r::digest(&bundle)?, mask);
            }
            bundles.push(bundle);
        }
        let payload = serde_json::to_string(&bundles).map_err(|_| "recovery_encoding")?;
        self.recovery_masks = masks;
        self.recovery_prepared = Some((binding, payload.clone()));
        Ok(payload)
    }

    /// Verify and retain ONLY this recipient's curve-point evaluation.
    pub fn recovery_receive(&mut self, bundle_json: String) -> Result<String, String> {
        use mental_poker::crypto_real::recovery as r;
        let bundle: r::RecoveryBundle =
            serde_json::from_str(&bundle_json).map_err(|_| "recovery_bundle")?;
        let id = r::digest(&bundle)?;
        if self.recovery_evaluations.contains_key(&id) {
            return Ok(id);
        }
        if self.recovery_evaluations.len() >= 63 {
            return Err("recovery_capacity".into());
        }
        let evaluation = r::receive(&self.inner, &bundle)?;
        self.recovery_evaluations.insert(id.clone(), evaluation);
        Ok(id)
    }

    /// The client must independently authorize the exact card before this call.
    pub fn recovery_release(&self, id: String) -> Result<String, String> {
        serde_json::to_string(
            self.recovery_evaluations
                .get(&id)
                .ok_or("recovery_missing")?,
        )
        .map_err(|_| "recovery_encoding".into())
    }

    /// Called only when the owner signs an all-in intent (or forced-blind
    /// admission). Publishing these masks irreversibly changes confidentiality.
    pub fn recovery_masks(&self) -> Result<String, String> {
        serde_json::to_string(&self.recovery_masks).map_err(|_| "recovery_encoding".into())
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
    let proof = ThresholdDecryptionProof {
        scheme: SCHEME.to_string(),
        shares,
    };
    match verify_and_open(idx, &ct, &pks, &proof) {
        Ok(id) => id as i32,
        Err(_) => -1,
    }
}

/// Batched [`owner_open`]: open SEVERAL cards under ONE parse of the party
/// pubkey list. `owner_open` re-parses `pks_json` (n hex points → Ristretto
/// decompressions) on every call, so opening k cards paid that cost k times;
/// here it is paid once.
///
/// * `deck_json`  — `[{"idx":<deck index>,"ct":{"c1":"..","c2":".."}}, ...]`
/// * `pks_json`   — the same party directory `owner_open` takes
/// * `shares_json` — `[[share,...], ...]`, ALIGNED BY POSITION with `deck_json`
///
/// Returns a JSON array of card ids (`-1` for a card whose quorum/proof check
/// fails — same per-card verdict as `owner_open`, so one bad card never
/// invalidates the others), or `"ERR:<reason>"` on a structural problem
/// (malformed JSON, length mismatch, bad pubkey/ciphertext) — the same
/// `ERR:` convention as [`coord_joint_key`].
///
/// Verification is UNCHANGED: each card still goes through `verify_and_open`
/// with its own deck index bound into the transcript. `owner_open` is kept for
/// single-card callers and for compatibility with existing embedders.
#[wasm_bindgen]
pub fn owner_open_all(deck_json: String, pks_json: String, shares_json: String) -> String {
    #[derive(serde::Deserialize)]
    struct Entry {
        idx: u32,
        ct: CtWire,
    }

    let err = |reason: &str| format!("ERR:{reason}");

    let entries: Vec<Entry> = match serde_json::from_str(&deck_json) {
        Ok(v) => v,
        Err(e) => return err(&format!("bad deck json: {e}")),
    };
    let share_sets: Vec<Vec<DecryptionShare>> = match serde_json::from_str(&shares_json) {
        Ok(v) => v,
        Err(e) => return err(&format!("bad shares json: {e}")),
    };
    if entries.len() != share_sets.len() {
        return err("deck/shares length mismatch");
    }
    // Parsed ONCE for the whole batch — the point of this entry point.
    let pks = match parse_pks(&pks_json) {
        Ok(p) => p,
        Err(e) => return err(&e),
    };

    let mut ids: Vec<i32> = Vec::with_capacity(entries.len());
    for (entry, shares) in entries.iter().zip(share_sets) {
        let Some(ct) = Ct::from_wire(&entry.ct) else {
            return err("bad ciphertext");
        };
        let proof = ThresholdDecryptionProof {
            scheme: SCHEME.to_string(),
            shares,
        };
        ids.push(match verify_and_open(entry.idx, &ct, &pks, &proof) {
            Ok(id) => id as i32,
            Err(_) => -1,
        });
    }
    match serde_json::to_string(&ids) {
        Ok(json) => json,
        Err(e) => err(&e.to_string()),
    }
}

/// Verifiable Hold'em dealing v2 (`mental_poker::pf2`): verify a hand's bundle
/// (shuffle proof against the standard deck, the commitment, the player-seed
/// order and every opened card). Returns `{"ok":true,"verified":{…}}` with the
/// proven cards, or `{"ok":false,"error":"…"}`. The caller still compares the
/// result with what its own device witnessed during the hand.
#[wasm_bindgen]
pub fn pf2_verify(bundle_json: String) -> String {
    let result = serde_json::from_str::<mental_poker::pf2::Bundle>(&bundle_json)
        .map_err(|_| "malformed bundle".to_string())
        .and_then(|bundle| mental_poker::pf2::verify(&bundle).map_err(|e| e.to_string()));
    match result {
        Ok(verified) => serde_json::json!({"ok": true, "verified": verified}).to_string(),
        Err(error) => serde_json::json!({"ok": false, "error": error}).to_string(),
    }
}

/// Zha Jin Hua's three-card layout over the same encrypted-deck verifier.
#[wasm_bindgen]
pub fn zjh_pf2_verify(bundle_json: String) -> String {
    let result = serde_json::from_str::<mental_poker::pf2_zjh::Bundle>(&bundle_json)
        .map_err(|_| "malformed bundle".to_string())
        .and_then(|bundle| mental_poker::pf2_zjh::verify(&bundle).map_err(|e| e.to_string()));
    match result {
        Ok(verified) => serde_json::json!({"ok": true, "verified": verified}).to_string(),
        Err(error) => serde_json::json!({"ok": false, "error": error}).to_string(),
    }
}

/// Generic physical-card manifest and selective-decryption verifier.
#[wasm_bindgen]
pub fn cards_verify(bundle_json: String) -> String {
    use mental_poker::card_proof;
    let result = if bundle_json.len() > card_proof::MAX_BUNDLE_BYTES {
        Err("card bundle too large".to_string())
    } else {
        serde_json::from_str::<card_proof::Bundle>(&bundle_json)
            .map_err(|_| "malformed card bundle".to_string())
            .and_then(|bundle| card_proof::verify(&bundle).map_err(|e| e.to_string()))
    };
    match result {
        Ok(verified) => serde_json::json!({"ok": true, "verified": verified}).to_string(),
        Err(error) => serde_json::json!({"ok": false, "error": error}).to_string(),
    }
}

/// Public recovery checks shared by the browser, worker and coordinator.
#[wasm_bindgen]
pub fn recovery_check(request: String) -> Result<String, String> {
    use mental_poker::crypto_real::recovery as r;
    let value: serde_json::Value =
        serde_json::from_str(&request).map_err(|_| "recovery_request")?;
    match value["op"].as_str() {
        Some("digest") => r::digest(&value["value"]),
        Some("bundle") => {
            let bundle: r::RecoveryBundle =
                serde_json::from_value(value["bundle"].clone()).map_err(|_| "recovery_bundle")?;
            r::validate_bundle(&bundle)?;
            r::digest(&bundle)
        }
        Some("mask") => {
            let core: r::RecoveryCore =
                serde_json::from_value(value["core"].clone()).map_err(|_| "recovery_core")?;
            let mask: r::RecoveryMask =
                serde_json::from_value(value["mask"].clone()).map_err(|_| "recovery_mask")?;
            r::verify_mask(&core, &mask)?;
            Ok("ok".into())
        }
        _ => Err("recovery_operation".into()),
    }
}
