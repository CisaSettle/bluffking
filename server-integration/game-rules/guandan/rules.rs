use serde::{Deserialize, Serialize};

/// Physical identity, not face identity: two copies of all 54 cards.
/// Suit order clubs, diamonds, spades, hearts; ranks 2..A; jokers 52,53.
pub type Card = u8;
const MAX_BOMB_SIZE: u8 = 10;
pub fn rank(c: Card) -> u8 {
    if c % 54 >= 52 {
        15 + c % 54 - 52
    } else {
        c % 54 % 13 + 2
    }
}
pub fn suit(c: Card) -> u8 {
    c % 54 / 13
}
pub fn wild(c: Card, level: u8) -> bool {
    rank(c) == level && suit(c) == 3
}
pub fn power(r: u8, level: u8) -> u8 {
    if r >= 15 {
        r + 2
    } else if r == level {
        16
    } else {
        r
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Single,
    Pair,
    Triple,
    FullHouse,
    Straight,
    Pairs,
    Triples,
    Bomb,
    StraightFlush,
    JokerBomb,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pattern {
    pub kind: Kind,
    pub rank: u8,
    pub size: u8,
    pub pair: u8,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Play {
    pub cards: Vec<Card>,
    pub pattern: Pattern,
}
impl Pattern {
    fn tier(self) -> u8 {
        match self.kind {
            Kind::JokerBomb => 30,
            Kind::Bomb => {
                if self.size >= 6 {
                    self.size + 10
                } else {
                    self.size
                }
            }
            Kind::StraightFlush => 6,
            _ => 0,
        }
    }
    pub fn beats(self, other: Self, level: u8) -> bool {
        if self.tier() != other.tier() {
            return self.tier() > other.tier();
        }
        if self.kind != other.kind || self.size != other.size {
            return false;
        }
        self.strength(level) > other.strength(level)
    }
    /// Within a kind: sequences compare by natural rank, the rest by power.
    fn strength(self, level: u8) -> u8 {
        if matches!(
            self.kind,
            Kind::Straight | Kind::Pairs | Kind::Triples | Kind::StraightFlush
        ) {
            self.rank
        } else {
            power(self.rank, level)
        }
    }
    /// Strongest-first order key for competing readings of the same cards:
    /// bomb tier, then strength, then a fixed kind/pair tiebreak.
    fn reading_key(self, level: u8) -> (u8, u8, u8, u8) {
        (
            self.tier(),
            self.strength(level),
            self.kind as u8,
            power(self.pair, level),
        )
    }
    fn needs(self) -> Option<Vec<u8>> {
        use Kind::*;
        let r = self.rank;
        if self.kind != FullHouse && self.pair != 0 {
            return None;
        }
        Some(match self.kind {
            Single if (2..=16).contains(&r) && self.size == 1 => vec![r],
            Pair if (2..=16).contains(&r) && self.size == 2 => vec![r; 2],
            Triple if (2..=14).contains(&r) && self.size == 3 => vec![r; 3],
            Bomb if (2..=14).contains(&r) && (4..=MAX_BOMB_SIZE).contains(&self.size) => {
                vec![r; self.size as usize]
            }
            FullHouse
                if (2..=14).contains(&r)
                    && (2..=16).contains(&self.pair)
                    && r != self.pair
                    && self.size == 5 =>
            {
                vec![r, r, r, self.pair, self.pair]
            }
            Straight | StraightFlush if (5..=14).contains(&r) && self.size == 5 => {
                (r - 4..=r).map(|n| if n == 1 { 14 } else { n }).collect()
            }
            Pairs if (3..=14).contains(&r) && self.size == 6 => (r - 2..=r)
                .flat_map(|n| [if n == 1 { 14 } else { n }; 2])
                .collect(),
            Triples if (2..=14).contains(&r) && self.size == 6 => (r - 1..=r)
                .flat_map(|n| [if n == 1 { 14 } else { n }; 3])
                .collect(),
            JokerBomb if self.size == 4 && r == 16 => vec![15, 15, 16, 16],
            _ => return None,
        })
    }
}
fn templates() -> Vec<Pattern> {
    let mut ps = Vec::new();
    for r in 2..=16 {
        for (kind, size) in [
            (Kind::Single, 1),
            (Kind::Pair, 2),
            (Kind::Triple, 3),
            (Kind::Straight, 5),
            (Kind::StraightFlush, 5),
            (Kind::Pairs, 6),
            (Kind::Triples, 6),
        ] {
            ps.push(Pattern {
                kind,
                rank: r,
                size,
                pair: 0,
            });
        }
        for size in 4..=MAX_BOMB_SIZE {
            ps.push(Pattern {
                kind: Kind::Bomb,
                rank: r,
                size,
                pair: 0,
            });
        }
        for pair in 2..=16 {
            ps.push(Pattern {
                kind: Kind::FullHouse,
                rank: r,
                size: 5,
                pair,
            });
        }
    }
    ps.push(Pattern {
        kind: Kind::JokerBomb,
        rank: 16,
        size: 4,
        pair: 0,
    });
    ps
}
/// Fill each template slot in display order. Wildcards never impersonate jokers.
fn fill_slots(hand: &[Card], p: Pattern, level: u8, flush: Option<u8>) -> Option<Vec<Card>> {
    let needs = p.needs()?;
    let mut slots: Vec<Option<Card>> = vec![None; needs.len()];
    for (slot, &r) in needs.iter().enumerate() {
        if let Some(&c) = hand.iter().find(|&&c| {
            !slots.contains(&Some(c))
                && !wild(c, level)
                && rank(c) == r
                && flush.is_none_or(|s| suit(c) == s)
        }) {
            slots[slot] = Some(c);
        }
    }
    for (slot, &r) in needs.iter().enumerate() {
        if slots[slot].is_some() {
            continue;
        }
        let &c = hand.iter().find(|&&c| {
            !slots.contains(&Some(c))
                && wild(c, level)
                && r < 15
                && (p.kind != Kind::Single || r == level)
        })?;
        slots[slot] = Some(c);
    }
    Some(slots.into_iter().flatten().collect())
}
/// Table display order for a legal play: the pattern's rank sequence, low to
/// high (A-2-3-4-5, not card-id order), with wildcards in the slot they fill.
pub fn display_order(cards: &[Card], p: Pattern, level: u8) -> Vec<Card> {
    let ordered = if p.kind == Kind::StraightFlush {
        (0..4).find_map(|s| fill_slots(cards, p, level, Some(s)))
    } else {
        fill_slots(cards, p, level, None)
    };
    ordered
        .filter(|o| o.len() == cards.len())
        .unwrap_or_else(|| cards.to_vec())
}
fn distinct_physical_cards(cards: &[Card]) -> bool {
    let mut seen = [false; 108];
    cards.iter().all(|&card| {
        seen.get_mut(card as usize)
            .is_some_and(|used| !std::mem::replace(used, true))
    })
}

fn fits_selection(cards: &[Card], p: Pattern, level: u8) -> bool {
    if p.kind == Kind::StraightFlush {
        (0..4).any(|s| fill_slots(cards, p, level, Some(s)).is_some())
    } else {
        fill_slots(cards, p, level, None).is_some()
    }
}

pub fn valid(cards: &[Card], p: Pattern, level: u8) -> bool {
    cards.len() == p.size as usize
        && distinct_physical_cards(cards)
        && fits_selection(cards, p, level)
}
/// Every legal reading of a selection, strongest first; the client defaults to
/// the first one. The cards are the same whichever reading is declared, so the
/// stronger one never costs anything: 5♠-9♠ is a straight flush (a bomb), not a
/// straight, whether leading or following. Following, every reading here
/// already beats the table, so a weaker one would only let the next player
/// answer more cheaply. The player can still pick another reading.
pub fn declarations(cards: &[Card], level: u8, target: Option<Pattern>) -> Vec<Pattern> {
    if !distinct_physical_cards(cards) {
        return Vec::new();
    }
    let mut out: Vec<Pattern> = move_templates()
        .iter()
        .map(|(p, _)| *p)
        .filter(|&p| {
            cards.len() == p.size as usize
                && target.is_none_or(|t| p.beats(t, level))
                && fits_selection(cards, p, level)
        })
        .collect();
    out.sort_by_key(|p| std::cmp::Reverse(p.reading_key(level)));
    out
}
fn move_templates() -> &'static [(Pattern, Vec<u8>)] {
    static TEMPLATES: std::sync::OnceLock<Vec<(Pattern, Vec<u8>)>> = std::sync::OnceLock::new();
    TEMPLATES.get_or_init(|| {
        templates()
            .into_iter()
            .filter_map(|p| p.needs().map(|needs| (p, needs)))
            .collect()
    })
}
pub fn moves(hand: &[Card], level: u8, target: Option<Pattern>) -> Vec<Play> {
    // Retain the incoming physical card order within ranks, including the two
    // deck copies and the hearts that can substitute for another rank.
    let mut ranks: [Vec<Card>; 17] = std::array::from_fn(|_| Vec::new());
    let mut wilds = Vec::new();
    for &c in hand {
        if wild(c, level) {
            wilds.push(c);
        } else {
            ranks[rank(c) as usize].push(c);
        }
    }
    let fill_indexed = |p: Pattern, needs: &[u8], flush: Option<u8>| {
        if needs.len() > hand.len() {
            return None;
        }
        // The largest template is the ten-card bomb. Most templates fail;
        // allocate a returned card vector only after every slot is filled.
        let mut used = [0; MAX_BOMB_SIZE as usize];
        let mut count = 0;
        let mut missing = 0;
        for &r in needs {
            if let Some(&c) = ranks[r as usize]
                .iter()
                .find(|&&c| !used[..count].contains(&c) && flush.is_none_or(|s| suit(c) == s))
            {
                used[count] = c;
                count += 1;
            } else {
                if r >= 15 || (p.kind == Kind::Single && r != level) || missing == wilds.len() {
                    return None;
                }
                missing += 1;
            }
        }
        for _ in 0..missing {
            let &c = wilds.iter().find(|c| !used[..count].contains(c))?;
            used[count] = c;
            count += 1;
        }
        let used = &mut used[..count];
        used.sort_unstable();
        Some(used.to_vec())
    };
    let mut out = Vec::new();
    for (p, needs) in move_templates()
        .iter()
        .filter(|(p, _)| target.is_none_or(|t| p.beats(t, level)))
    {
        let p = *p;
        if p.kind == Kind::StraightFlush {
            for s in 0..4 {
                if let Some(cards) = fill_indexed(p, needs, Some(s)) {
                    out.push(Play { cards, pattern: p });
                }
            }
        } else if let Some(cards) = fill_indexed(p, needs, None) {
            out.push(Play { cards, pattern: p });
        }
    }
    out.sort_by_key(|p| {
        (
            p.pattern.tier(),
            std::cmp::Reverse(p.pattern.size),
            power(p.pattern.rank, level),
        )
    });
    out
}
/// The hint list for the player to act: the legal plays of [`moves`], one
/// reading per card set (the strongest, as in [`declarations`]), cheapest
/// first. Cost, in order: spending a bomb (bombs always come last, smallest
/// first); splitting a natural bomb, straight flush or the four jokers; using
/// a 红心 wildcard; breaking up a pair or triple. Leading then offers
/// singles, pairs and triples before longer combinations; both lead and
/// follow end on the lowest strength. Deterministic, one pass over `moves`.
pub fn hints(hand: &[Card], level: u8, target: Option<Pattern>) -> Vec<Play> {
    let mut plays = moves(hand, level, target);
    plays.sort_by(|a, b| {
        a.cards.cmp(&b.cards).then_with(|| {
            b.pattern
                .reading_key(level)
                .cmp(&a.pattern.reading_key(level))
        })
    });
    plays.dedup_by(|later, kept| later.cards == kept.cards);
    let shape = HandShape::new(hand, level);
    let leading = target.is_none();
    // Stable sorting retains the physical-card order above when costs tie.
    plays.sort_by_cached_key(|p| shape.cost(p, level, leading));
    plays
}
/// Natural (non-wild) card counts of a hand, for hint costs.
struct HandShape {
    ranks: [u8; 17],
    suited: [[u8; 15]; 4],
    /// Natural straight flushes held, as (suit, top rank).
    flushes: Vec<(u8, u8)>,
}
impl HandShape {
    fn new(hand: &[Card], level: u8) -> Self {
        let mut ranks = [0u8; 17];
        let mut suited = [[0u8; 15]; 4];
        for &c in hand.iter().filter(|&&c| !wild(c, level)) {
            ranks[rank(c) as usize] += 1;
            if rank(c) < 15 {
                suited[suit(c) as usize][rank(c) as usize] += 1;
            }
        }
        let flushes = (0..4u8)
            .flat_map(|s| (5..=14u8).map(move |top| (s, top)))
            .filter(|&(s, top)| Self::run(top).all(|r| suited[s as usize][r as usize] > 0))
            .collect();
        Self {
            ranks,
            suited,
            flushes,
        }
    }
    fn run(top: u8) -> impl Iterator<Item = u8> {
        (top - 4..=top).map(|n| if n == 1 { 14 } else { n })
    }
    #[allow(clippy::type_complexity)]
    fn cost(
        &self,
        p: &Play,
        level: u8,
        leading: bool,
    ) -> (u8, u8, u8, u8, u8, u8, std::cmp::Reverse<u8>) {
        let mut used = [0u8; 17];
        let mut used_suited = [[0u8; 15]; 4];
        let mut wilds = 0u8;
        for &c in &p.cards {
            if wild(c, level) {
                wilds += 1;
            } else {
                used[rank(c) as usize] += 1;
                if rank(c) < 15 {
                    used_suited[suit(c) as usize][rank(c) as usize] += 1;
                }
            }
        }
        let kind = p.pattern.kind;
        let mut split = 0u8;
        let mut broken = 0u8;
        for (r, &take) in used.iter().enumerate().skip(2) {
            let have = self.ranks[r];
            if take == 0 || take == have {
                continue;
            }
            if have >= 4 && r < 15 {
                // Playing that rank's own bomb is spending it, not splitting it.
                if !(kind == Kind::Bomb && p.pattern.rank as usize == r) && have - take < 4 {
                    split += 1;
                }
            } else {
                broken += 1;
            }
        }
        if kind != Kind::StraightFlush {
            split += self
                .flushes
                .iter()
                .filter(|&&(s, top)| {
                    Self::run(top).any(|r| {
                        self.suited[s as usize][r as usize] == used_suited[s as usize][r as usize]
                    })
                })
                .count()
                .min(1) as u8;
        }
        if kind != Kind::JokerBomb
            && self.ranks[15] == 2
            && self.ranks[16] == 2
            && used[15] + used[16] > 0
        {
            split += 1;
        }
        let long = leading && !matches!(kind, Kind::Single | Kind::Pair | Kind::Triple);
        (
            p.pattern.tier(),
            split,
            wilds,
            broken,
            u8::from(long),
            p.pattern.strength(level),
            std::cmp::Reverse(p.pattern.size),
        )
    }
}
