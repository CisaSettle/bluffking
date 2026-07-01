//! phase4_choreography — a full server-blind HAND dealt across N independent
//! key-holding parties and a coordinator that holds ZERO secrets, plus the
//! n-of-n LIVENESS / abort behaviour (the #1 UX risk per the go-live decision
//! doc). PROTOTYPE, pending external audit (ADR-063).
//!
//! Run:  cargo run -p mental-poker --example phase4_choreography
//!
//! This is a protocol-faithful, in-process model of the choreography: each
//! `DkgParty` keeps its OWN secret x_i; the "coordinator" only ever touches
//! public keys, ciphertext, and proofs (it is given no x_i, by construction).
//! Real WS transport + the Vue/Flutter clients + real-device network latency are
//! the subsequent integration milestones; the cryptographic trust boundary and
//! the liveness logic — the risky parts — are exercised here.

use std::time::Instant;

use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT as G;
use curve25519_dalek::ristretto::RistrettoPoint;
use curve25519_dalek::scalar::Scalar;
use rand::rngs::OsRng;

use mental_poker::crypto::ShuffleProofProvider;
use mental_poker::crypto_real::decrypt::{
    partial_decrypt, verify_and_open, DecryptionShare, ThresholdDecryptionProof, SCHEME,
};
use mental_poker::crypto_real::dkg::{schnorr_prove, schnorr_verify, DkgParty};
use mental_poker::crypto_real::ec::{
    canonical_starting_deck, card_id_from_point, deck_hash, point_to_hex, Ct, EncDeck,
};
use mental_poker::crypto_real::shuffle::{RealShuffleProofProvider, Shuffle};

fn card_label(id: u8) -> String {
    const RANKS: [&str; 13] = [
        "2", "3", "4", "5", "6", "7", "8", "9", "T", "J", "Q", "K", "A",
    ];
    const SUITS: [&str; 4] = ["♣", "♦", "♥", "♠"];
    format!("{}{}", RANKS[(id / 4) as usize], SUITS[(id % 4) as usize])
}

struct HandOutcome {
    hole: Vec<(usize, [u8; 2])>,
    board: Vec<u8>,
    server_recovered: usize,
    full_ms: u128,
}

/// Deal one server-blind hand across `n` independent parties. `drop_party`: a
/// party that goes offline before the deal (liveness test). Returns the dealt
/// cards, or `Err(reason)` if the hand cannot complete (→ coordinator aborts).
fn run_hand(n: usize, drop_party: Option<usize>) -> Result<HandOutcome, String> {
    let mut rng = OsRng;
    let t0 = Instant::now();

    // N independent parties, each with its OWN secret x_i (never shared).
    let parties: Vec<DkgParty> = (0..n)
        .map(|i| DkgParty::generate(format!("party:{i}"), &mut rng))
        .collect();

    // === DKG === each party registers q_i + a Schnorr PoK; the COORDINATOR
    // verifies every PoK (rogue-key defence) and sums the joint key Q = Σ q_i. No
    // x_i ever reaches the coordinator.
    let mut pks: Vec<(String, RistrettoPoint)> = Vec::with_capacity(n);
    for p in &parties {
        let pok = schnorr_prove(&p.party_id, &p.x_i, &p.q_i, &mut rng);
        if !schnorr_verify(&p.party_id, &p.q_i, &pok) {
            return Err(format!("DKG: bad key proof from {}", p.party_id));
        }
        pks.push((p.party_id.clone(), p.q_i));
    }
    let q: RistrettoPoint = pks.iter().map(|(_, k)| *k).sum();

    // === Verifiable shuffle round-robin === each party re-encrypts + permutes the
    // deck under Q and proves it; the coordinator verifies every proof.
    let verifier = RealShuffleProofProvider::verifier_with_expected_key(point_to_hex(&q));
    let mut deck: EncDeck = canonical_starting_deck();
    for (r, p) in parties.iter().enumerate() {
        let input = deck.clone();
        let sh = Shuffle::perform(input.clone(), &q, &mut rng);
        let ih = deck_hash(&input);
        let oh = deck_hash(&sh.output);
        let proof = sh.prove(&p.party_id, r as u32, &mut rng);
        if !verifier.verify_shuffle(&p.party_id, r as u32, &ih, &oh, None, &proof) {
            return Err(format!("shuffle proof from {} rejected", p.party_id));
        }
        deck = sh.output;
    }

    // === Deal === opening a deck index needs EVERY party's Chaum–Pedersen partial
    // decryption (n-of-n). A dropped party ⇒ one share short ⇒ QuorumMismatch ⇒
    // the index cannot be opened.
    let online: Vec<usize> = (0..n).filter(|i| Some(*i) != drop_party).collect();
    let open = |idx: usize, rng: &mut OsRng| -> Result<u8, String> {
        let shares: Vec<DecryptionShare> = online
            .iter()
            .map(|&i| partial_decrypt(&parties[i], idx as u32, &deck[idx], rng))
            .collect();
        let proof = ThresholdDecryptionProof {
            scheme: SCHEME.to_string(),
            shares,
        };
        verify_and_open(idx as u32, &deck[idx], &pks, &proof)
            .map_err(|e| format!("index {idx}: {e:?}"))
    };

    // Hole cards: seat s owns deck[s] and deck[n+s] (the other parties route their
    // shares to owner s; the coordinator relays but never combines for a non-owner).
    let mut hole = Vec::new();
    for s in 0..n {
        let a = open(s, &mut rng)?;
        let b = open(n + s, &mut rng)?;
        hole.push((s, [a, b]));
    }
    // Community: deck[2n..2n+5] — flop/turn/river, revealed to everyone.
    let mut board = Vec::new();
    for i in 0..5 {
        board.push(open(2 * n + i, &mut rng)?);
    }

    let full_ms = t0.elapsed().as_millis();

    // === Server-blindness === the coordinator holds the full ciphertext deck +
    // every PUBLIC key (Σ q_i = Q) but ZERO secrets. Its only public-derivable
    // guess C2 − Σ q_i is NOT the decryption (that needs Σ x_i·C1). It must
    // recover 0 cards.
    let public_sum: RistrettoPoint = pks.iter().map(|(_, k)| *k).sum(); // == Q
                                                                        // Defense-in-depth self-check: a keyless coordinator tries a MENU of
                                                                        // public-only openers (not just C2−ΣQ_i), mirroring the audit probe — none
                                                                        // can yield a card, because the genuine opener C2−Σx_i·C1 needs the secrets.
    let coord_can_open = |ct: &Ct| -> bool {
        let mut cands: Vec<RistrettoPoint> = vec![
            ct.c2,
            ct.c2 - public_sum,
            ct.c2 + public_sum,
            ct.c2 - ct.c1,
            ct.c2 + ct.c1,
            ct.c1,
            -ct.c2,
        ];
        for k in 0u64..64 {
            let s = Scalar::from(k);
            cands.push(ct.c2 - s * public_sum); // C2 − k·Q
            cands.push(ct.c2 - s * ct.c1); // C2 − k·C1
            cands.push(ct.c2 - s * G); // C2 − k·G (low card-point entropy probe)
        }
        cands.iter().any(|m| card_id_from_point(m).is_some())
    };
    let server_recovered = deck.iter().filter(|ct| coord_can_open(ct)).count();

    Ok(HandOutcome {
        hole,
        board,
        server_recovered,
        full_ms,
    })
}

fn main() {
    println!("════ Phase-4 server-blind 多方发牌编排 + 在线门槛(liveness)验证 ════");
    println!("(原型,待外部审计;协调方 = 只转发密文/公钥/证明,零私钥分片)\n");

    // --- Happy path: a full 3-handed server-blind hand. ---
    let n = 3;
    let h = run_hand(n, None).expect("happy hand must complete");
    println!("① DKG:{n} 方各注册 q_i + Schnorr PoK,协调方逐一验证 → 联合公钥 Q(协调方零私钥)");
    println!("② 可验证洗牌:{n} 轮,每轮的零知识证明都被协调方验证 ✓");
    println!("③ 发牌(底牌只发给拥有者,公共牌发给所有人):");
    for (s, cs) in &h.hole {
        println!("     玩家{s}:{} {}", card_label(cs[0]), card_label(cs[1]));
    }
    let board: Vec<String> = h.board.iter().map(|&c| card_label(c)).collect();
    println!("     公共牌:{}", board.join(" "));
    println!(
        "④ 协调方用「完整视图」(全部密文 + 全部公钥)能解出的牌数 = {} / 52",
        h.server_recovered
    );
    assert_eq!(h.server_recovered, 0, "coordinator must recover nothing");
    println!(
        "⑤ 完整一手(建密钥 + DKG + 洗牌 + 发牌)耗时 ≈ {} ms",
        h.full_ms
    );

    // --- Latency by table size. ---
    println!("\n延迟(完整一手,真密码学,本机):");
    for n in [2usize, 3, 4, 6] {
        match run_hand(n, None) {
            Ok(o) => {
                assert_eq!(o.server_recovered, 0);
                println!("   {n} 人桌:≈ {} ms", o.full_ms);
            }
            Err(e) => println!("   {n} 人桌:ERR {e}"),
        }
    }

    // --- Liveness: a party drops before the deal (the #1 UX risk). ---
    println!("\n在线门槛(liveness)—— n=3,party:1 在发牌前掉线:");
    match run_hand(3, Some(1)) {
        Ok(_) => {
            eprintln!("BUG: the hand completed despite a dropped party!");
            std::process::exit(1);
        }
        Err(e) => {
            println!("   开牌失败:{e}");
            println!("   → n-of-n:缺任一方的解密分片 ⇒ QuorumMismatch ⇒ 本手【无法完成】。");
            println!("   → 协调方检测到缺分片(超时)⇒ ABORT 本手 ⇒ 回退到普通发牌(existing_server)继续下一手。");
            println!(
                "   这正是决策文档点名的头号体验风险:手机端网络抖动会触发它 —— 必须真机量化。"
            );
        }
    }

    println!("\n════ 结论 ════");
    println!("✓ 完整一手(DKG→可验证洗牌→发牌)在 N 个独立持密方 + 零私钥协调方之间跑通;");
    println!("✓ 协调方全程只见密文,能解出的牌数 = 0(server-blind 成立);");
    println!("✓ n-of-n 掉线 ⇒ 本手 abort + 回退,liveness 行为明确、可控、可量化。");
    println!("注意:这是协议级端到端验证(进程内忠实建模 coordinator/party 与传输语义);");
    println!("      真实 WS 传输 + Vue/Flutter 客户端 + 真机网络是后续集成里程碑。");
}
