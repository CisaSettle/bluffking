//! Deterministic three-dice draws and the actual small/big payout rules.
//!
//! A `sicbo` draw mixes the committed server seed with the bettor's client
//! seed. A `sicbo-room` draw commits the server seed before betting opens, then
//! folds in browser seeds. Current room draws also bind a signed wager roster
//! to a fixed drand pulse scheduled after the roster locks. Each player can
//! check their own signed acceptance against the roster without publishing
//! their stake or account in the shared proof.
use crate::beacon::{self, Beacon};
use crate::evidence::SignedEvidence;
use crate::proof::{self, seed_bytes, sha256_fields, Reveal};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// A wager is any whole stake from 1 up to the bettor's balance (梭哈 stakes
/// all of it); the server refuses a stake above what the bettor can cover.
pub const MIN_STAKE: i64 = 1;
pub const MAX_STAKE: i64 = 1_000_000_000_000;

/// A separate purpose-bound stream determines the fixed bean jackpot. The
/// bettor's round seed identifies the wager without exposing an account ID.
pub const JACKPOT_BONUS: i64 = 10;

pub fn jackpot(reveal: &Reveal, player_seed: &str, won: bool) -> i64 {
    if !won {
        return 0;
    }
    let seed = sha256_fields(&[
        b"bluffking-sicbo-jackpot-v2",
        reveal.commitment.game.as_bytes(),
        reveal.commitment.draw_id.as_bytes(),
        reveal.server_seed.as_bytes(),
        reveal.client_seed.as_bytes(),
        player_seed.as_bytes(),
    ]);
    if proof::Entropy::new(seed).below(10_000) == 0 {
        JACKPOT_BONUS
    } else {
        0
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Small,
    Big,
}

pub const ACCEPTANCE_KIND: &str = "sicbo-wager-v1";
pub const RECOVERY_KIND: &str = "sicbo-wager-recovery-v1";
pub const ROSTER_KIND: &str = "sicbo-roster-v1";
pub const VOID_KIND: &str = "sicbo-void-v1";
pub const SETTLEMENT_KIND: &str = "sicbo-settled-v1";

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WalletUnit {
    Beans,
    Practice,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Acceptance {
    pub kind: String,
    pub round_id: String,
    pub commitment: String,
    pub accepted_at_ms: i64,
    pub revision: u64,
    pub unit: WalletUnit,
    /// A per-revision salt conceals the stake and revision in the public hash.
    pub nonce: String,
    pub wager: Option<Wager>,
    pub client_seed: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Wager {
    pub side: Side,
    pub stake: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Roster {
    pub kind: String,
    pub round_id: String,
    pub commitment: String,
    pub locked_at_ms: i64,
    pub beacon_round: u64,
    pub active_receipts: Vec<String>,
    pub player_seeds: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Terminal {
    pub kind: String,
    pub round_id: String,
    pub commitment: String,
    pub recorded_at_ms: i64,
    pub reason: Option<String>,
    pub roster_hash: Option<String>,
    pub dice: Option<[u8; 3]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Audit {
    pub roster: SignedEvidence,
    pub beacon: Beacon,
    pub terminal: SignedEvidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acceptance: Option<SignedEvidence>,
}

/// The first pulse at least one full Quicknet period after the roster locks.
pub fn roster_round(locked_at_ms: i64) -> Result<u64, String> {
    let draw_ms = locked_at_ms
        .checked_add(beacon::PERIOD_MS)
        .ok_or("roster time overflow")?;
    beacon::round_at_or_after(draw_ms)
}

pub fn evidence_hash(evidence: &SignedEvidence) -> String {
    hex::encode(sha256_fields(&[
        b"bluffking-sicbo-evidence-v1",
        evidence.public_key.as_bytes(),
        evidence.payload.as_bytes(),
        evidence.signature.as_bytes(),
    ]))
}

pub fn beacon_client_seed(roster: &SignedEvidence, beacon: &Beacon) -> Result<String, String> {
    let randomness = beacon::verify(beacon)?;
    Ok(hex::encode(sha256_fields(&[
        b"bluffking-sicbo-beacon-v1",
        &seed_bytes(&evidence_hash(roster))?,
        &beacon.round.to_be_bytes(),
        &randomness,
    ])))
}

pub fn verify_roster(evidence: &SignedEvidence, key: &str) -> Result<Roster, String> {
    let claim: Roster = evidence.verify(key)?;
    let _ = proof::dealer_client_seed("sicbo-room", &claim.player_seeds)?;
    if claim.kind != ROSTER_KIND
        || claim.beacon_round != roster_round(claim.locked_at_ms)?
        || claim.active_receipts.len() > MAX_ROOM_SEEDS
        || claim.active_receipts.len() < claim.player_seeds.len()
        || !claim
            .active_receipts
            .windows(2)
            .all(|pair| pair[0] < pair[1])
        || claim
            .active_receipts
            .iter()
            .any(|hash| seed_bytes(hash).is_err())
    {
        return Err("invalid Sic Bo roster".into());
    }
    Ok(claim)
}

pub fn draw_audited(
    reveal: &Reveal,
    roster: &SignedEvidence,
    beacon: &Beacon,
    expected_public_key: &str,
    player_seeds: &[String],
) -> Result<[u8; 3], String> {
    let claim = verify_roster(roster, expected_public_key)?;
    if reveal.commitment.game != "sicbo-room"
        || claim.round_id != reveal.commitment.draw_id
        || claim.commitment != reveal.commitment.commitment
        || claim.beacon_round != beacon.round
        || claim.player_seeds != player_seeds
        || reveal.client_seed != beacon_client_seed(roster, beacon)?
    {
        return Err("Sic Bo reveal does not match its signed roster and beacon".into());
    }
    let mut entropy = reveal.entropy()?;
    Ok(std::array::from_fn(|_| (entropy.below(6) + 1) as u8))
}

pub fn verify_acceptance(evidence: &SignedEvidence, key: &str) -> Result<Acceptance, String> {
    let claim: Acceptance = evidence.verify(key)?;
    let valid_wager = match (&claim.wager, &claim.client_seed) {
        (Some(wager), Some(seed)) => {
            (MIN_STAKE..=MAX_STAKE).contains(&wager.stake) && seed_bytes(seed).is_ok()
        }
        (None, None) => true,
        _ => false,
    };
    if claim.kind != ACCEPTANCE_KIND
        || claim.revision == 0
        || seed_bytes(&claim.nonce).is_err()
        || seed_bytes(&claim.commitment).is_err()
        || !valid_wager
    {
        return Err("invalid Sic Bo wager acceptance".into());
    }
    Ok(claim)
}

pub(crate) fn acceptance_in_roster(
    evidence: &SignedEvidence,
    claim: &Acceptance,
    roster: &Roster,
) -> Result<bool, String> {
    if claim.round_id != roster.round_id || claim.commitment != roster.commitment {
        return Err("Sic Bo roster differs from the signed acceptance".into());
    }
    if !roster.active_receipts.contains(&evidence_hash(evidence)) {
        return Ok(false);
    }
    if claim.accepted_at_ms > roster.locked_at_ms
        || !claim
            .client_seed
            .as_ref()
            .is_some_and(|seed| roster.player_seeds.contains(seed))
    {
        return Err("Invalid active Sic Bo wager in the signed roster".into());
    }
    Ok(true)
}

/// Verifies a void independently of the room or a settlement history row.
pub fn verify_void(value: &Value, key: &str) -> Result<Value, String> {
    let evidence: SignedEvidence =
        serde_json::from_value(value["terminal"].clone()).map_err(|error| error.to_string())?;
    let terminal: Terminal = evidence.verify(key)?;
    let reason = terminal.reason.as_deref();
    if terminal.kind != VOID_KIND
        || terminal.dice.is_some()
        || !matches!(
            reason,
            Some("server_restart" | "room_closed" | "invalid_draw")
        )
        || value["round_id"].as_str() != Some(&terminal.round_id)
        || seed_bytes(&terminal.commitment).is_err()
    {
        return Err("invalid Sic Bo void".into());
    }
    if let Some(roster) = value.get("roster").filter(|v| !v.is_null()) {
        let roster: SignedEvidence =
            serde_json::from_value(roster.clone()).map_err(|error| error.to_string())?;
        let claim = verify_roster(&roster, key)?;
        if claim.round_id != terminal.round_id
            || claim.commitment != terminal.commitment
            || terminal.recorded_at_ms < claim.locked_at_ms
            || terminal.roster_hash.as_deref() != Some(&evidence_hash(&roster))
        {
            return Err("Sic Bo void does not match its roster".into());
        }
    } else if terminal.roster_hash.is_some() {
        return Err("missing Sic Bo void roster".into());
    }
    Ok(value.clone())
}

/// Room seats (humans and AI) bound how many player seeds a round can list.
const MAX_ROOM_SEEDS: usize = 8;

/// Replay the three dice from the verified seed transcript.
pub fn draw(reveal: &Reveal, player_seeds: &[String]) -> Result<[u8; 3], String> {
    if !matches!(reveal.commitment.game.as_str(), "sicbo" | "sicbo-room") {
        return Err("Not a Sic Bo reveal".into());
    }
    proof::dice(reveal, player_seeds)
}

pub fn wins(dice: [u8; 3], side: Side) -> bool {
    if dice.iter().any(|die| !(1..=6).contains(die)) || (dice[0] == dice[1] && dice[1] == dice[2]) {
        return false;
    }
    let total: u8 = dice.iter().sum();
    match side {
        Side::Small => (4..=10).contains(&total),
        Side::Big => (11..=17).contains(&total),
    }
}

/// Callers validate the positive stake against MIN_STAKE..=MAX_STAKE first.
pub fn delta(dice: [u8; 3], side: Side, stake: i64) -> i64 {
    if wins(dice, side) {
        stake
    } else {
        -stake
    }
}

/// The room-wide proof contains no individual wager or account information:
/// player seeds are random and listed in sorted order, not by seat.
pub fn draw_proof(reveal: &Reveal, dice: [u8; 3], player_seeds: &[String]) -> Value {
    let mut proof = json!({
        "kind": reveal.commitment.game,
        "reveal": reveal,
        "dice": dice,
        "total": dice.iter().sum::<u8>(),
        "triple": dice[0] == dice[1] && dice[1] == dice[2],
    });
    if reveal.commitment.game == "sicbo-room" {
        proof["player_seeds"] = json!(player_seeds);
    }
    proof
}

/// Attaches the payout rule to a replayed draw, for either a solo or room bet.
pub fn payout_proof(
    reveal: &Reveal,
    dice: [u8; 3],
    player_seeds: &[String],
    side: Side,
    stake: i64,
) -> Value {
    let mut proof = draw_proof(reveal, dice, player_seeds);
    proof["side"] = json!(side);
    proof["stake"] = json!(stake);
    proof["won"] = json!(wins(dice, side));
    proof["delta"] = json!(delta(dice, side, stake));
    proof
}

/// Attach the separately credited bean jackpot to the bettor's own proof.
pub fn add_jackpot(proof: &mut Value, player_seed: &str, bonus: i64) {
    proof["jackpot_seed"] = json!(player_seed);
    proof["jackpot_bonus"] = json!(bonus);
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    kind: String,
    reveal: Reveal,
    dice: [u8; 3],
    total: u8,
    triple: bool,
    player_seeds: Option<Vec<String>>,
    audit: Option<Audit>,
    side: Option<Side>,
    stake: Option<i64>,
    won: Option<bool>,
    delta: Option<i64>,
    jackpot_seed: Option<String>,
    jackpot_bonus: Option<i64>,
}

/// Checks the commitment, exact dice, summary and (when present) payout.
/// Returns the reconstructed proof; any changed asserted result is rejected.
pub fn verify(value: &Value) -> Result<Value, String> {
    verify_with_key(value, None)
}

pub fn verify_with_key(value: &Value, pinned_key: Option<&str>) -> Result<Value, String> {
    if value["kind"] == ROSTER_KIND {
        let evidence: SignedEvidence =
            serde_json::from_value(value["roster"].clone()).map_err(|error| error.to_string())?;
        let claim = verify_roster(&evidence, pinned_key.unwrap_or(&evidence.public_key))?;
        return serde_json::to_value(claim).map_err(|error| error.to_string());
    }
    if value["kind"] == ACCEPTANCE_KIND {
        let evidence: SignedEvidence = serde_json::from_value(value["acceptance"].clone())
            .map_err(|error| error.to_string())?;
        let claim = verify_acceptance(&evidence, pinned_key.unwrap_or(&evidence.public_key))?;
        return serde_json::to_value(claim).map_err(|error| error.to_string());
    }
    if value["kind"] == VOID_KIND {
        let key = pinned_key
            .or_else(|| value["terminal"]["public_key"].as_str())
            .ok_or("missing Sic Bo evidence key")?;
        return verify_void(value, key);
    }
    let proof: Proof = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    if proof.kind != proof.reveal.commitment.game {
        return Err("sicbo proof kind does not match the committed game".into());
    }
    let seeds = match (proof.kind.as_str(), proof.player_seeds.as_deref()) {
        ("sicbo-room", Some(seeds)) => seeds,
        ("sicbo", None) => &[],
        _ => return Err("Unexpected seed list for the Sic Bo game".into()),
    };
    let mut verified_bettor = None;
    let dice = if let Some(audit) = &proof.audit {
        let key = pinned_key.unwrap_or(&audit.roster.public_key);
        let dice = draw_audited(&proof.reveal, &audit.roster, &audit.beacon, key, seeds)?;
        let terminal: Terminal = audit.terminal.verify(key)?;
        if terminal.kind != SETTLEMENT_KIND
            || terminal.round_id != proof.reveal.commitment.draw_id
            || terminal.commitment != proof.reveal.commitment.commitment
            || terminal.recorded_at_ms < beacon::round_time_ms(audit.beacon.round)
            || terminal.reason.is_some()
            || terminal.roster_hash.as_deref() != Some(&evidence_hash(&audit.roster))
            || terminal.dice != Some(dice)
        {
            return Err("Sic Bo settlement does not match the reveal".into());
        }
        if let Some(acceptance) = &audit.acceptance {
            let roster: Roster = audit.roster.verify(key)?;
            let claim = verify_acceptance(acceptance, key)?;
            if !acceptance_in_roster(acceptance, &claim, &roster)?
                || claim.wager
                    != proof
                        .side
                        .zip(proof.stake)
                        .map(|(side, stake)| Wager { side, stake })
            {
                return Err("Sic Bo wager acceptance is not in the signed roster".into());
            }
            verified_bettor = Some(claim);
        } else if proof.stake.is_some() {
            return Err("missing Sic Bo wager acceptance".into());
        }
        dice
    } else {
        if pinned_key.is_some() {
            return Err("missing signed Sic Bo proof".into());
        }
        draw(&proof.reveal, seeds)?
    };
    if proof.dice != dice
        || proof.total != dice.iter().sum::<u8>()
        || proof.triple != (dice[0] == dice[1] && dice[1] == dice[2])
    {
        return Err("sicbo draw does not match the reveal".into());
    }
    match (proof.side, proof.stake, proof.won, proof.delta) {
        (Some(side), Some(stake), Some(won), Some(claimed_delta)) => {
            if proof.kind == "sicbo-room" && seeds.is_empty() {
                return Err("A settled wager requires a player contribution".into());
            }
            if !(MIN_STAKE..=MAX_STAKE).contains(&stake) {
                return Err("sicbo stake is outside the allowed range".into());
            }
            if won != wins(dice, side) || claimed_delta != delta(dice, side, stake) {
                return Err("sicbo payout does not match the rules".into());
            }
            let mut replayed = payout_proof(&proof.reveal, dice, seeds, side, stake);
            match (
                verified_bettor.as_ref().map(|claim| claim.unit),
                proof.jackpot_seed,
                proof.jackpot_bonus,
            ) {
                (None | Some(WalletUnit::Practice), None, None) => {}
                (None | Some(WalletUnit::Beans), Some(seed), Some(bonus))
                    if proof.kind == "sicbo-room"
                        && seeds.contains(&seed)
                        && verified_bettor
                            .as_ref()
                            .is_none_or(|claim| claim.client_seed.as_ref() == Some(&seed))
                        && jackpot(&proof.reveal, &seed, won) == bonus =>
                {
                    add_jackpot(&mut replayed, &seed, bonus);
                }
                _ => {
                    return Err(
                        "The Sic Bo jackpot does not match the bettor seed and rules".into(),
                    )
                }
            }
            if let Some(audit) = proof.audit {
                replayed["audit"] = json!(audit);
            }
            Ok(replayed)
        }
        (None, None, None, None)
            if proof.kind == "sicbo-room"
                && proof.jackpot_seed.is_none()
                && proof.jackpot_bonus.is_none() =>
        {
            let mut replayed = draw_proof(&proof.reveal, dice, seeds);
            if let Some(audit) = proof.audit {
                replayed["audit"] = json!(audit);
            }
            Ok(replayed)
        }
        _ => Err("sicbo payout proof must include side, stake, won and delta".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{document::verify_document, proof::commit, proof::dealer_client_seed};

    #[test]
    fn jackpot_uses_the_bean_bettors_saved_seed() {
        let game = "sicbo-room";
        let draw_id = "room-round-1";
        let server_seed = format!("{:064x}", 0x267);
        let seeds = ["01".repeat(32)];
        let commitment = commit(game, draw_id, &server_seed).unwrap();
        let reveal = Reveal {
            commitment: commitment.clone(),
            server_seed,
            client_seed: dealer_client_seed(game, &seeds).unwrap(),
        };
        let dice = draw(&reveal, &seeds).unwrap();
        assert_eq!(dice, [6, 6, 4]);
        assert_eq!(jackpot(&reveal, &seeds[0], true), JACKPOT_BONUS);
        let mut proof = payout_proof(&reveal, dice, &seeds, Side::Big, 5);
        assert!(verify_document(&proof, None).is_ok());
        add_jackpot(&mut proof, &seeds[0], JACKPOT_BONUS);
        assert!(verify_document(&proof, None).is_ok());
        proof["jackpot_bonus"] = json!(0);
        assert!(verify_document(&proof, None).is_err());
    }
}

#[cfg(test)]
mod signed_tests {
    use super::*;
    use crate::evidence::EvidenceSigner;

    #[test]
    fn signed_sicbo_replay_binds_wager_roster_beacon_and_void() {
        let signer = EvidenceSigner::new(&[25; 32]);
        let key = signer.public_key();
        let round_id = "f47ac10b-58cc-4372-a567-0e02b2c3d479".to_owned();
        let server_seed = hex::encode([9; 32]);
        let commitment = proof::commit("sicbo-room", &round_id, &server_seed).unwrap();
        let lock = beacon::round_time_ms(1);
        let seed = hex::encode([10; 32]);
        let acceptance = signer
            .sign(&Acceptance {
                kind: ACCEPTANCE_KIND.into(),
                round_id: round_id.clone(),
                commitment: commitment.commitment.clone(),
                accepted_at_ms: lock - 1,
                revision: 1,
                unit: WalletUnit::Beans,
                nonce: hex::encode([11; 32]),
                wager: Some(Wager {
                    side: Side::Big,
                    stake: 100,
                }),
                client_seed: Some(seed.clone()),
            })
            .unwrap();
        let seeds = vec![seed];
        let roster = signer
            .sign(&Roster {
                kind: ROSTER_KIND.into(),
                round_id: round_id.clone(),
                commitment: commitment.commitment.clone(),
                locked_at_ms: lock,
                beacon_round: roster_round(lock).unwrap(),
                active_receipts: vec![evidence_hash(&acceptance)],
                player_seeds: seeds.clone(),
            })
            .unwrap();
        // drand Quicknet round 2, the first pulse one period after chain genesis.
        let pulse = Beacon {
            round: 2,
            signature: "b6b6a585449b66eb12e875b64fcbab3799861a00e4dbf092d99e969a5eac57dd3f798acf61e705fe4f093db926626807".into(),
        };
        let reveal = Reveal {
            commitment,
            server_seed,
            client_seed: beacon_client_seed(&roster, &pulse).unwrap(),
        };
        let dice = draw_audited(&reveal, &roster, &pulse, &key, &seeds).unwrap();
        let settled = Terminal {
            kind: SETTLEMENT_KIND.into(),
            round_id: round_id.clone(),
            commitment: reveal.commitment.commitment.clone(),
            recorded_at_ms: beacon::round_time_ms(pulse.round),
            reason: None,
            roster_hash: Some(evidence_hash(&roster)),
            dice: Some(dice),
        };
        let mut proof = payout_proof(&reveal, dice, &seeds, Side::Big, 100);
        add_jackpot(
            &mut proof,
            &seeds[0],
            jackpot(&reveal, &seeds[0], wins(dice, Side::Big)),
        );
        proof["audit"] = json!(Audit {
            roster: roster.clone(),
            beacon: pulse,
            terminal: signer.sign(&settled).unwrap(),
            acceptance: Some(acceptance.clone()),
        });
        assert!(verify_with_key(&proof, Some(&key)).is_ok());
        assert!(
            verify_with_key(&proof, Some(&EvidenceSigner::new(&[26; 32]).public_key())).is_err()
        );

        let mut bad = proof.clone();
        bad["audit"]["beacon"]["round"] = json!(3);
        assert!(verify_with_key(&bad, Some(&key)).is_err());
        bad = proof.clone();
        bad["audit"]["acceptance"] = Value::Null;
        assert!(verify_with_key(&bad, Some(&key)).is_err());

        bad = proof.clone();
        bad.as_object_mut().unwrap().remove("jackpot_seed");
        bad.as_object_mut().unwrap().remove("jackpot_bonus");
        assert!(verify_with_key(&bad, Some(&key)).is_err());

        let verify = crate::document::verify_document_with_key;
        let mut recovery = json!({
            "kind": RECOVERY_KIND,
            "acceptance": acceptance,
            "roster": roster,
            "proof": proof,
        });
        let result = verify(&recovery, None, Some(&key)).unwrap();
        assert_eq!(result["wager_status"], "active");
        assert_eq!(result["wager_revision"], 1);
        let mut missing = recovery.clone();
        missing.as_object_mut().unwrap().remove("acceptance");
        assert!(verify(&missing, None, Some(&key)).is_err());
        missing = recovery.clone();
        missing["acceptance"]["signature"] = json!("00".repeat(64));
        assert!(verify(&missing, None, Some(&key)).is_err());

        let mut receipt = json!({
            "protocol": "bluffking-receipt-v1",
            "commitment": reveal.commitment,
            "client_seed": seeds[0],
            "terms": {"kind": "sicbo-room", "round_id": round_id, "bet_revision": 0,
                      "side": "big", "stake": 100, "unit": "beans"},
        });
        assert_eq!(
            verify(&proof, Some(&receipt), Some(&key)).unwrap()["receipt_match"],
            "matched"
        );
        receipt["terms"]["bet_revision"] = json!(1);
        assert!(verify(&proof, Some(&receipt), Some(&key)).is_err());
        receipt["terms"]["bet_revision"] = json!(0);
        for unit in [Value::Null, json!("unknown"), json!("practice")] {
            receipt["terms"]["unit"] = unit;
            assert!(verify(&proof, Some(&receipt), Some(&key)).is_err());
        }
        receipt["terms"].as_object_mut().unwrap().remove("unit");
        assert!(verify(&proof, Some(&receipt), Some(&key)).is_err());
        receipt["terms"]["unit"] = json!("beans");
        receipt["client_seed"] = json!("22".repeat(32));
        assert!(verify(&proof, Some(&receipt), Some(&key)).is_err());

        recovery["proof"] = Value::Null;
        assert_eq!(
            verify(&recovery, None, Some(&key)).unwrap()["wager_status"],
            "active"
        );
        recovery["roster"] = Value::Null;
        assert_eq!(
            verify(&recovery, None, Some(&key)).unwrap()["wager_status"],
            "pending_roster"
        );

        let mut void = json!({
            "kind": VOID_KIND,
            "round_id": round_id,
            "roster": roster,
            "terminal": signer.sign(&Terminal {
                kind: VOID_KIND.into(),
                reason: Some("server_restart".into()),
                dice: None,
                ..settled
            }).unwrap(),
        });
        assert!(verify_with_key(&void, Some(&key)).is_ok());
        recovery["proof"] = void.clone();
        assert!(verify(&recovery, None, Some(&key)).is_err());
        recovery["roster"] = json!(roster);
        assert_eq!(
            verify(&recovery, None, Some(&key)).unwrap()["wager_status"],
            "active"
        );
        void["roster"] = Value::Null;
        assert!(verify_with_key(&void, Some(&key)).is_err());
    }
}
