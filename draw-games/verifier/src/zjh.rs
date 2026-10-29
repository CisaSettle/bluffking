//! 炸金花 (Zha Jin Hua) deals: one shuffled 52-card deck per hand, three
//! consecutive cards per dealt seat in ascending seat order.
//!
//! The committed `draw_id` is `<hand id>:<dealt seats>` (e.g. `…:0,2,5`), so the
//! roster that decides who holds which cards is fixed before play as well.
//!
//! Everyone at a table shares one deal, so like `sicbo-room` there is no client
//! seed. The commitment is shown when the hand starts; after the hand the seed
//! is revealed. That detects a substituted deck, but the server still knows the
//! deal while the hand is played.
use crate::proof::Reveal;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const DECK: usize = 52;
pub const MAX_SEATS: usize = 10;
const RANKS: &[u8; 13] = b"23456789TJQKA";
const SUITS: &[u8; 4] = b"shcd";

/// Canonical deck index → wire code: suit-major (♠ ♥ ♣ ♦), ranks 2..A.
pub fn code(index: usize) -> String {
    let rank = RANKS[index % 13] as char;
    let suit = SUITS[index / 13] as char;
    format!("{rank}{suit}")
}

/// Fisher-Yates over the canonical deck, driven by the verified seed stream.
pub fn deck(reveal: &Reveal) -> Result<Vec<usize>, String> {
    if reveal.commitment.game != "zjh" {
        return Err("not a zjh reveal".into());
    }
    let mut entropy = reveal.entropy()?;
    let mut deck: Vec<usize> = (0..DECK).collect();
    for i in (1..DECK).rev() {
        let j = entropy.below(i as u64 + 1) as usize;
        deck.swap(i, j);
    }
    Ok(deck)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeatHand {
    pub seat: usize,
    /// `None` when the record does not disclose this seat's cards.
    pub cards: Option<[String; 3]>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    kind: String,
    reveal: Reveal,
    hands: Vec<SeatHand>,
}

fn valid_seats(seats: &[usize]) -> bool {
    (2..=MAX_SEATS).contains(&seats.len())
        && seats.windows(2).all(|w| w[0] < w[1])
        && seats.iter().all(|&s| s < MAX_SEATS)
}

/// The committed `draw_id` for a hand dealt to `seats` (ascending).
pub fn draw_id(hand_id: &str, seats: &[usize]) -> Result<String, String> {
    if hand_id.is_empty() || hand_id.contains(':') || !valid_seats(seats) {
        return Err("zjh draw id needs a hand id and 2..=10 ascending seats below 10".into());
    }
    let seats: Vec<String> = seats.iter().map(usize::to_string).collect();
    Ok(format!("{hand_id}:{}", seats.join(",")))
}

/// Splits a committed `draw_id` into the hand id and its dealt seats.
pub fn parse_draw_id(draw_id: &str) -> Result<(&str, Vec<usize>), String> {
    let invalid = || "invalid zjh draw id".to_string();
    let (hand_id, seats) = draw_id.split_once(':').ok_or_else(invalid)?;
    let seats = seats
        .split(',')
        .map(|s| {
            // Canonical decimal only, so one roster has exactly one draw id.
            if s.is_empty()
                || (s.len() > 1 && s.starts_with('0'))
                || !s.bytes().all(|b| b.is_ascii_digit())
            {
                return Err(invalid());
            }
            s.parse().map_err(|_| invalid())
        })
        .collect::<Result<Vec<usize>, _>>()?;
    if hand_id.is_empty() || !valid_seats(&seats) {
        return Err(invalid());
    }
    Ok((hand_id, seats))
}

/// Every committed seat's three card codes, in ascending seat order.
pub fn deal(reveal: &Reveal) -> Result<Vec<(usize, [String; 3])>, String> {
    let (_, seats) = parse_draw_id(&reveal.commitment.draw_id)?;
    let deck = deck(reveal)?;
    Ok(seats
        .into_iter()
        .enumerate()
        .map(|(i, seat)| (seat, std::array::from_fn(|k| code(deck[i * 3 + k]))))
        .collect())
}

/// Checks the commitment and every disclosed seat's cards against the replay.
pub fn verify(value: &Value) -> Result<Value, String> {
    let proof: Proof = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    if proof.kind != "zjh" || proof.reveal.commitment.game != "zjh" {
        return Err("zjh proof kind does not match the committed game".into());
    }
    let dealt = deal(&proof.reveal)?;
    if proof.hands.len() != dealt.len()
        || proof
            .hands
            .iter()
            .zip(&dealt)
            .any(|(h, (seat, _))| h.seat != *seat)
    {
        return Err("zjh proof seats differ from the committed roster".into());
    }
    if proof.hands.iter().all(|h| h.cards.is_none()) {
        return Err("zjh proof discloses no cards to compare".into());
    }
    for (hand, (_, cards)) in proof.hands.iter().zip(&dealt) {
        if hand.cards.as_ref().is_some_and(|shown| shown != cards) {
            return Err(format!(
                "zjh seat {} cards do not match the reveal",
                hand.seat
            ));
        }
    }
    Ok(value.clone())
}
