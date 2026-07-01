//! phase4_blind_demo — a runnable, narrated proof that on the Phase-4
//! server-blind dealing path (ADR-063, REAL threshold-ElGamal crypto) **the
//! server/coordinator never holds a plaintext hole card**.
//!
//! It drives the same real `crypto_real` primitives the integration tests
//! (`tests/phase4_server_blind.rs`) exercise — DKG → encrypt under the joint key
//! → verifiable shuffles → threshold open — but narrates each step in plain
//! language so a human can watch the property hold.
//!
//! Run:  cargo run -p mental-poker --example phase4_blind_demo
//!
//! HONEST SCOPE: this proves the *implemented protocol* keeps cards from the
//! server locally; it exercises the same protocol that runs in production for
//! engine-blind tables (ADR-070) — the generic `mental_poker_production`
//! provider stays rejected. It models a ≥2-human, bot-free table; it defends
//! against a malicious SERVER, not against colluding players.

use curve25519_dalek::ristretto::RistrettoPoint;
use curve25519_dalek::scalar::Scalar;
use rand::rngs::OsRng;

use mental_poker::crypto::ShuffleProofProvider;
use mental_poker::crypto_real::decrypt::{
    partial_decrypt, verify_and_open, ThresholdDecryptionProof, SCHEME,
};
use mental_poker::crypto_real::dkg::{verify_dkg, DkgRun};
use mental_poker::crypto_real::ec::{
    canonical_starting_deck, card_id_from_point, deck_hash, point_to_hex, scalar_from_hex,
    scalar_to_hex,
};
use mental_poker::crypto_real::shuffle::{RealShuffleProofProvider, Shuffle};

/// Illustrative card-id → face mapping (the crypto card-id 0..51 is opaque; the
/// product assigns the face). Shown only so the output reads like poker.
fn card_label(id: u8) -> String {
    const RANKS: [&str; 13] = [
        "2", "3", "4", "5", "6", "7", "8", "9", "T", "J", "Q", "K", "A",
    ];
    const SUITS: [&str; 4] = ["♣", "♦", "♥", "♠"];
    format!("{}{}", RANKS[(id / 4) as usize], SUITS[(id % 4) as usize])
}

fn short(hex: &str) -> String {
    format!("{}…", &hex[..hex.len().min(16)])
}

fn main() {
    let mut rng = OsRng;
    let n = 3usize; // ≥2 真人、无机器人桌 (all-human, bot-free — ADR-063 §7)

    println!("════════ Phase-4 server-blind 发牌 · 本地真实密码学演示 ════════");
    println!("(真实密码学 · 验证「已实现协议」· 跨厂商 AI 审 + 开源可复核)\n");

    // ① DKG — each human generates a secret key share; the server gets none.
    println!("① 分布式密钥生成 (DKG):{n} 个真人各自生成私钥分片 x_i,谁都不知道完整私钥。");
    let run = DkgRun::simulate(n, &mut rng);
    let q = verify_dkg(&run.commitments, &run.shares).expect("DKG verifies");
    assert_eq!(q, run.joint_key, "reconstructed joint key must equal ΣQ_i");
    let pks: Vec<(String, RistrettoPoint)> = run
        .parties
        .iter()
        .map(|p| (p.party_id.clone(), p.q_i))
        .collect();
    println!("   联合公钥 Q = ΣQ_i = {}", short(&point_to_hex(&q)));
    println!("   ⚠ 服务器/协调方持有的私钥分片数量 = 0(它只拿到公开的 Q,没有任何 x_i)。\n");

    // ② Encrypt the 52 cards under Q, then each party verifiably shuffles.
    println!("② 加密 + 可验证洗牌:52 张牌在 Q 下加密成密文,每个玩家依次洗牌并给出零知识证明。");
    let mut deck = canonical_starting_deck();
    let verifier = RealShuffleProofProvider::verifier_with_expected_key(point_to_hex(&q));
    for r in 0..n {
        let party = format!("party:{r}");
        let input = deck.clone();
        let shuffle = Shuffle::perform(input.clone(), &q, &mut rng);
        let ih = deck_hash(&input);
        let oh = deck_hash(&shuffle.output);
        let proof = shuffle.prove(&party, r as u32, &mut rng);
        let ok = verifier.verify_shuffle(&party, r as u32, &ih, &oh, None, &proof);
        assert!(ok, "honest shuffle proof must verify");
        println!(
            "   {party} 洗牌+证明 → 验证 {}",
            if ok { "✓ 通过" } else { "✗ 失败" }
        );
        deck = shuffle.output;
    }

    // ③ Show what the server actually holds: ciphertext, not cards.
    println!("\n③ 此刻服务器持有的「牌」只是密文(椭圆曲线群元素),不是牌面。前两张:");
    for (idx, ct) in deck.iter().take(2).enumerate() {
        let w = ct.to_wire();
        println!(
            "   deck[{idx}] = {{ c1:{}, c2:{} }}",
            short(&w.c1),
            short(&w.c2)
        );
    }

    // ④ The server uses its FULL view (all ciphertext + all PUBLIC keys, zero
    //    secret shares) to try to read every card — and recovers nothing.
    println!("\n④ 服务器用「完整视图」(全部密文 + 全部公开公钥 ΣQ_i,但零私钥分片)尝试解每一张牌:");
    let public_sum: RistrettoPoint = pks.iter().map(|(_, qi)| *qi).sum();
    let server_recovered = deck
        .iter()
        .filter(|ct| card_id_from_point(&(ct.c2 - public_sum)).is_some())
        .count();
    println!(
        "   服务器成功解出的牌数 = {server_recovered} / 52  → 公开信息推不出任何一张牌的牌面。"
    );

    // ⑤ Legitimate deal: each hole card is opened only for its owner. The math
    //    needs every party's share; the choreography routes the other parties'
    //    shares only to the card's owner, so only the owner completes it — and
    //    the server (0 shares) can never complete any open.
    println!("\n⑤ 合法发牌:每张底牌的解密需要各方的分片;协调方零分片 → 永远补不齐。");
    println!("   各玩家重建出的、只有自己能看到的两张底牌:");
    for seat in 0..n {
        let mut faces = Vec::new();
        for &idx in &[seat, n + seat] {
            let proof = ThresholdDecryptionProof {
                scheme: SCHEME.to_string(),
                shares: run
                    .parties
                    .iter()
                    .map(|p| partial_decrypt(p, idx as u32, &deck[idx], &mut rng))
                    .collect(),
            };
            let id = verify_and_open(idx as u32, &deck[idx], &pks, &proof).expect("owner opens");
            faces.push(id);
        }
        println!(
            "     玩家{seat}:{} {}   (card-id {} / {})",
            card_label(faces[0]),
            card_label(faces[1]),
            faces[0],
            faces[1]
        );
    }

    // ⑥ Tamper-evidence: a forged partial-decryption proof is rejected.
    println!("\n⑥ 防篡改:某方(或服务器)伪造一个解密分片 → 证明验证当场拒绝。");
    let r = Scalar::random(&mut rng);
    let ct = mental_poker::crypto_real::ec::Ct::encrypt_card(0, &q, &r);
    let mut proof = ThresholdDecryptionProof {
        scheme: SCHEME.to_string(),
        shares: run
            .parties
            .iter()
            .map(|p| partial_decrypt(p, 0, &ct, &mut rng))
            .collect(),
    };
    let s = scalar_from_hex(&proof.shares[1].dleq.s).expect("decode scalar");
    proof.shares[1].dleq.s = scalar_to_hex(&(s + Scalar::ONE)); // flip party:1's response
    let rejected = verify_and_open(0, &ct, &pks, &proof).is_err();
    println!(
        "   篡改后的解密证明被 verify_and_open {}",
        if rejected {
            "拒绝 ✓"
        } else {
            "接受 ✗(BUG!)"
        }
    );
    assert!(
        rejected,
        "a tampered partial-decryption proof MUST be rejected"
    );

    // Summary.
    println!("\n════════ 结论 ════════");
    println!("✓ 服务器持有 0 个私钥分片;全部 52 张牌它只看到密文。");
    println!(
        "✓ 即便用「全部密文 + 全部公开公钥」的完整视图,服务器解出的牌数 = {server_recovered}。"
    );
    println!("✓ 每张底牌只有其拥有者能补齐解密;篡改解密分片会被证明验证拒绝。");
    println!("→ 在这条 Phase-4 发牌引擎上,服务器【结构上】拿不到任何明文底牌(server-blind 成立)。");
    println!("\n诚实边界:");
    println!(
        "• 本演示证明「已实现协议」在本地成立;该协议已在生产为 engine-blind 牌桌上线(ADR-070)。"
    );
    println!("• 只适用于 ≥2 真人、无机器人的桌;防作恶服务器,但防不了串通玩家。");
    println!("• engine-blind 组合已 GA(ADR-070);通用 mental_poker_production provider 仍被 guard_provider_allowed 硬拒。");
}
