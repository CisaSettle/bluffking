use crate::ddz::rules::Card;
use serde::{Deserialize, Serialize};

pub const FIRST_CARD: Card = Card(0);

pub fn deck() -> impl Iterator<Item = Card> {
    (0..52).filter(|&id| in_deck(Card(id))).map(Card)
}

pub fn in_deck(card: Card) -> bool {
    card.0 < 52 && !matches!(card.0, 44 | 49..=51)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Single,
    Pair,
    Pairs,
    Straight,
    Triple,
    Plane,
    Bomb,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pattern {
    pub kind: Kind,
    high: u8,
    len: usize,
    groups: usize,
}

impl Pattern {
    fn new(kind: Kind, high: u8, len: usize, groups: usize) -> Self {
        Self {
            kind,
            high,
            len,
            groups,
        }
    }

    fn beats(self, target: Self) -> bool {
        if self.kind == Kind::Bomb && target.kind != Kind::Bomb {
            return true;
        }
        self.kind == target.kind
            && self.len == target.len
            && self.groups == target.groups
            && self.high > target.high
    }
}

type Ranks = [Vec<Card>; 16];

fn ranks(cards: &[Card]) -> Ranks {
    let mut ranks: Ranks = std::array::from_fn(|_| Vec::new());
    for &card in cards {
        ranks[card.rank() as usize].push(card);
    }
    ranks
}

fn counts(cards: &[Card]) -> [usize; 16] {
    let mut counts = [0; 16];
    for card in cards {
        counts[card.rank() as usize] += 1;
    }
    counts
}

/// Cards have already been checked for identity and ownership.
fn pattern(cards: &[Card], last_hand: bool) -> Option<Pattern> {
    let n = cards.len();
    let ranks = counts(cards);
    let low = ranks.iter().position(|&count| count != 0)?;
    let high = ranks.iter().rposition(|&count| count != 0)?;
    let groups = ranks[low..=high]
        .iter()
        .filter(|&&count| count != 0)
        .count();
    let consecutive = high < 15 && high + 1 - low == groups;
    let all = |count| ranks[low..=high].iter().all(|&n| n == count);
    let simple = match groups {
        1 if n == 1 => Some(Kind::Single),
        1 if n == 2 => Some(Kind::Pair),
        1 if n == 4 => Some(Kind::Bomb),
        _ if consecutive && n >= 5 && all(1) => Some(Kind::Straight),
        _ if consecutive && n >= 4 && all(2) => Some(Kind::Pairs),
        _ => None,
    };
    if let Some(kind) = simple {
        return Some(Pattern::new(kind, high as u8, n, groups));
    }

    if ranks[15] == 0 {
        // Prefer a complete airplane when the last hand can also be read
        // as a larger airplane with fewer wings.
        for groups in n.div_ceil(5).max(2)..=n / 3 {
            if !last_hand && n != groups * 5 {
                continue;
            }
            if let Some(end) = ((3 + groups - 1)..=14).rev().find(|&end| {
                ranks[end + 1 - groups..=end]
                    .iter()
                    .all(|&count| count == 3)
            }) {
                return Some(Pattern::new(Kind::Plane, end as u8, n, groups));
            }
        }
    }
    if n == 5 || (last_hand && (3..=4).contains(&n)) {
        let triple = ranks.iter().position(|&count| count == 3)?;
        return Some(Pattern::new(Kind::Triple, triple as u8, n, 1));
    }
    None
}

pub struct Turn<'a> {
    pub hand: &'a [Card],
    pub target: Option<Pattern>,
    pub opening: bool,
    pub next_single: bool,
}

impl Turn<'_> {
    pub fn check(&self, cards: &[Card]) -> Result<Pattern, &'static str> {
        let mut seen = 0u64;
        for &card in cards {
            if !in_deck(card) || !self.hand.contains(&card) || seen & (1 << card.0) != 0 {
                return Err("invalid_cards");
            }
            seen |= 1 << card.0;
        }
        if self.opening && !cards.contains(&FIRST_CARD) {
            return Err("opening_card");
        }
        let shape = pattern(cards, cards.len() == self.hand.len()).ok_or("invalid_pattern")?;
        let hand = counts(self.hand);
        if shape.kind != Kind::Bomb && cards.iter().any(|card| hand[card.rank() as usize] == 4) {
            return Err("split_bomb");
        }
        if shape.kind == Kind::Single
            && self.next_single
            && hand.iter().rposition(|&count| (1..=3).contains(&count)) != Some(shape.high as usize)
        {
            return Err("highest_single");
        }
        if self.target.is_some_and(|target| !shape.beats(target)) {
            return Err("must_beat");
        }
        Ok(shape)
    }

    /// Try one representative for each rank pattern. Suits are interchangeable
    /// except the opening spade, which sorts first in its rank and among wings.
    pub fn suggestion(&self) -> Option<Vec<Card>> {
        if self.check(self.hand).is_ok() {
            return Some(self.hand.to_vec());
        }
        let hand = ranks(self.hand);
        let mut best = None;
        let mut consider = |cards: Vec<Card>| {
            if let Ok(shape) = self.check(&cards) {
                let length = if self.target.is_none() {
                    16 - cards.len()
                } else {
                    0
                };
                let key = (shape.kind == Kind::Bomb, length, shape.high);
                if best.as_ref().is_none_or(|(old, _)| key < *old) {
                    best = Some((key, cards));
                }
            }
        };

        for rank in &hand[3..] {
            match rank.len() {
                4 => consider(rank.clone()),
                1..=3 => {
                    consider(rank[..1].to_vec());
                    if rank.len() >= 2 {
                        consider(rank[..2].to_vec());
                    }
                }
                _ => {}
            }
        }

        // The longest sequence possible in a 16-card hand stops at ace.
        for (copies, min_groups) in [(1, 5), (2, 2), (3, 1)] {
            for low in 3..=14 {
                for high in low..=14 {
                    let core = &hand[low..=high];
                    if core
                        .iter()
                        .any(|rank| rank.len() < copies || rank.len() == 4)
                    {
                        break;
                    }
                    if core.len() < min_groups {
                        continue;
                    }
                    let mut cards: Vec<_> = core
                        .iter()
                        .flat_map(|rank| &rank[..copies])
                        .copied()
                        .collect();
                    if copies == 3 {
                        // A standalone triple may take the spade 2. An airplane
                        // cannot use any 2, including as an attached card.
                        let last_rank = if low == high { 15 } else { 14 };
                        let wings = hand[3..=last_rank]
                            .iter()
                            .enumerate()
                            .filter(|(i, rank)| !(low..=high).contains(&(i + 3)) && rank.len() != 4)
                            .flat_map(|(_, rank)| rank)
                            .take(core.len() * 2);
                        cards.extend(wings.copied());
                        if cards.len() != core.len() * 5 {
                            continue;
                        }
                    }
                    consider(cards);
                }
            }
        }
        best.map(|(_, cards)| cards)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cards(codes: &str) -> Vec<Card> {
        let mut cards: Vec<_> = codes
            .split_whitespace()
            .map(|s| Card::parse(s).unwrap())
            .collect();
        cards.sort_unstable();
        cards
    }

    fn lead(hand: &[Card]) -> Turn<'_> {
        Turn {
            hand,
            target: None,
            opening: false,
            next_single: false,
        }
    }

    #[test]
    fn complete_airplane_takes_precedence_when_going_out() {
        let target = cards("3s 3h 3d 4s 4h 4d 5s 5h 5d 7c 8c 9c Tc Jc Qc");
        let hand = cards("6s 6h 6d 7s 7h 7d 8s 8h 8d 9s 9h 9d Ts Js Qs");
        let turn = Turn {
            target: pattern(&target, false),
            ..lead(&hand)
        };
        assert_eq!(turn.check(&hand).unwrap().groups, 3);
        assert_eq!(turn.suggestion().unwrap(), hand);
    }

    #[test]
    fn deck_shapes_and_last_hand_exceptions() {
        let deck: Vec<_> = deck().map(Card::code).collect();
        assert_eq!(deck.len(), 48);
        for removed in ["As", "2h", "2d", "2c"] {
            assert!(!deck.iter().any(|code| code == removed));
        }
        for (codes, kind) in [
            ("3s", Kind::Single),
            ("3s 3h", Kind::Pair),
            ("3s 3h 4s 4h", Kind::Pairs),
            ("3s 4s 5s 6s 7s", Kind::Straight),
            ("3s 3h 3d 4s 4h", Kind::Triple),
            ("3s 3h 3d 4s 4h 4d 5s 6s 7s 8s", Kind::Plane),
            ("3s 3h 3d 3c", Kind::Bomb),
        ] {
            let hand = cards(codes);
            assert_eq!(lead(&hand).check(&hand).unwrap().kind, kind);
        }
        for codes in ["3s 3h 3d", "3s 3h 3d 5s", "3s 3h 3d 4s 4h 4d 5s"] {
            let final_play = cards(codes);
            assert!(lead(&final_play).check(&final_play).is_ok());
            let mut larger = final_play.clone();
            larger.push(Card::parse("9s").unwrap());
            assert_eq!(lead(&larger).check(&final_play), Err("invalid_pattern"));
        }
        for codes in ["Js Qs Ks Ah 2s", "3s 3h 3d 4s 4h 4d 2s"] {
            let hand = cards(codes);
            assert_eq!(lead(&hand).check(&hand), Err("invalid_pattern"));
        }
    }

    #[test]
    fn protects_bombs_and_restricts_the_opening_and_last_card() {
        let hand = cards("3s 3h 3d 3c 4s 5s 7s 7h 7d 7c");
        let opening = Turn {
            opening: true,
            ..lead(&hand)
        };
        assert_eq!(opening.check(&cards("3s")), Err("split_bomb"));
        assert_eq!(opening.check(&cards("5s")), Err("opening_card"));
        assert_eq!(opening.suggestion(), Some(cards("3s 3h 3d 3c")));

        let last_card = Turn {
            next_single: true,
            ..lead(&hand)
        };
        assert_eq!(last_card.check(&cards("4s")), Err("highest_single"));
        assert!(last_card.check(&cards("5s")).is_ok());
        assert!(last_card.check(&cards("7s 7h 7d 7c")).is_ok());
        assert_eq!(last_card.check(&cards("5s 5s")), Err("invalid_cards"));
    }

    #[test]
    fn responses_match_shape_and_size_or_use_a_higher_bomb() {
        let first = cards("3s 4s 5s 6s 7s");
        let hand = cards("4h 5h 6h 7h 8h 9h Ts Td Tc Th");
        let respond = Turn {
            target: Some(lead(&first).check(&first).unwrap()),
            ..lead(&hand)
        };
        assert_eq!(respond.check(&cards("4h 5h 6h 7h 8h 9h")), Err("must_beat"));
        assert_eq!(
            respond.check(&cards("4h 5h 6h 7h 8h")).unwrap().kind,
            Kind::Straight
        );
        let bomb = respond.check(&cards("Ts Td Tc Th")).unwrap();
        let other = cards("9s 9h 9d 9c Js Jh Jd Jc");
        let bomb_response = Turn {
            target: Some(bomb),
            ..lead(&other)
        };
        assert_eq!(bomb_response.check(&cards("9s 9h 9d 9c")), Err("must_beat"));
        assert_eq!(bomb_response.suggestion(), Some(cards("Js Jh Jd Jc")));
    }
}
