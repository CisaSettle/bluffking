//! Eight-deck punto banco on a Super 6 (no-commission) table. Independently
//! implemented from the tableau: https://github.com/jconradi/baccarat-engine
//! (reference, no vendored code). Paytable: Nevada Gaming Control Board rules
//! of play (GameAce Live Multi-Baccarat; EZ Baccarat Dragon 7 / Panda 8) and
//! the Wizard of Odds Super 6 / pair side-bet tables (eight decks).
use serde::{Deserialize, Serialize};

/// One wager per area, in whole beans/points. Areas a request leaves out are 0.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Bets {
    pub player: i64,
    pub banker: i64,
    pub tie: i64,
    /// Player's first two cards share a rank: 11:1.
    pub player_pair: i64,
    /// Banker's first two cards share a rank: 11:1.
    pub banker_pair: i64,
    /// At least one hand's first two cards share a rank: 5:1.
    pub any_pair: i64,
    /// Banker wins on a total of 6: 15:1 (12:1 would be a ~30% house edge).
    pub super6: i64,
    /// 龙7: Banker wins with a three-card 7: 40:1.
    pub dragon7: i64,
    /// 熊猫8: Player wins with a three-card 8: 25:1.
    pub panda8: i64,
}
pub const STEP: i64 = 20;
/// Whole wager per round.
pub const MAX_TOTAL: i64 = 10_000;
/// Each side-bet area (pairs, 超级6, 龙7, 熊猫8).
pub const SIDE_MAX: i64 = 1_000;
impl Bets {
    fn sides(self) -> [i64; 6] {
        [
            self.player_pair,
            self.banker_pair,
            self.any_pair,
            self.super6,
            self.dragon7,
            self.panda8,
        ]
    }
    pub fn has_pair_bets(self) -> bool {
        self.player_pair != 0 || self.banker_pair != 0 || self.any_pair != 0
    }
    pub fn total(self) -> Option<i64> {
        let sides = self.sides();
        if sides.iter().any(|&stake| stake > SIDE_MAX) {
            return None;
        }
        [self.player, self.banker, self.tie]
            .into_iter()
            .chain(sides)
            .try_fold(0i64, |sum, stake| {
                if stake < 0 || stake % STEP != 0 {
                    return None;
                }
                sum.checked_add(stake)
            })
            .filter(|v| (STEP..=MAX_TOTAL).contains(v))
    }
    /// Net result. Super 6 table: a Banker win pays 1:1 with no commission,
    /// except a Banker win on 6 pays 1:2 (half). Player/Banker push on a tie.
    pub fn delta(self, o: &Outcome) -> i64 {
        let side = |stake: i64, hit: bool, odds: i64| if hit { stake * odds } else { -stake };
        let main = match o.winner {
            Winner::Player => self.player - self.banker - self.tie,
            Winner::Banker if o.banker_total == 6 => self.banker / 2 - self.player - self.tie,
            Winner::Banker => self.banker - self.player - self.tie,
            Winner::Tie => self.tie * 8,
        };
        let banker_won = o.winner == Winner::Banker;
        let player_won = o.winner == Winner::Player;
        main + side(self.player_pair, o.player_pair, 11)
            + side(self.banker_pair, o.banker_pair, 11)
            + side(self.any_pair, o.player_pair || o.banker_pair, 5)
            + side(self.super6, banker_won && o.banker_total == 6, 15)
            + side(
                self.dragon7,
                banker_won && o.banker_total == 7 && o.banker.len() == 3,
                40,
            )
            + side(
                self.panda8,
                player_won && o.player_total == 8 && o.player.len() == 3,
                25,
            )
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Winner {
    Player,
    Banker,
    Tie,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Outcome {
    pub player: Vec<String>,
    pub banker: Vec<String>,
    pub player_total: u8,
    pub banker_total: u8,
    pub winner: Winner,
    pub natural: bool,
    /// First two cards of that hand share a rank.
    #[serde(default)]
    pub player_pair: bool,
    #[serde(default)]
    pub banker_pair: bool,
}

/// Refill a shoe before the next round when fewer cards remain.
pub const MIN_SHOE_CARDS: usize = 16;

fn point(card: u8) -> u8 {
    let rank = card % 13 + 1;
    if rank >= 10 {
        0
    } else {
        rank
    }
}
fn pair(cards: &[u8]) -> bool {
    cards[0] % 13 == cards[1] % 13
}
fn total(cards: &[u8]) -> u8 {
    cards.iter().map(|c| point(*c)).sum::<u8>() % 10
}
fn card_name(card: u8) -> String {
    let rank = [
        "A", "2", "3", "4", "5", "6", "7", "8", "9", "T", "J", "Q", "K",
    ][card as usize % 13];
    let suit = ["c", "d", "h", "s"][card as usize / 13];
    format!("{rank}{suit}")
}
fn banker_draws(banker: u8, third: Option<u8>) -> bool {
    match third {
        None => banker <= 5,
        Some(p) => match banker {
            0..=2 => true,
            3 => p != 8,
            4 => (2..=7).contains(&p),
            5 => (4..=7).contains(&p),
            6 => (6..=7).contains(&p),
            _ => false,
        },
    }
}
pub fn deal(cards: &mut Vec<u8>) -> Option<Outcome> {
    if cards.len() < 6 || cards.iter().any(|c| *c >= 52) {
        return None;
    }
    let mut player = vec![cards.pop()?];
    let mut banker = vec![cards.pop()?];
    player.push(cards.pop()?);
    banker.push(cards.pop()?);
    let natural = total(&player) >= 8 || total(&banker) >= 8;
    if !natural {
        let third = if total(&player) <= 5 {
            let c = cards.pop()?;
            player.push(c);
            Some(point(c))
        } else {
            None
        };
        if banker_draws(total(&banker), third) {
            banker.push(cards.pop()?);
        }
    }
    let (p, b) = (total(&player), total(&banker));
    let (player_pair, banker_pair) = (pair(&player), pair(&banker));
    Some(Outcome {
        player: player.into_iter().map(card_name).collect(),
        banker: banker.into_iter().map(card_name).collect(),
        player_total: p,
        banker_total: b,
        winner: if p > b {
            Winner::Player
        } else if b > p {
            Winner::Banker
        } else {
            Winner::Tie
        },
        natural,
        player_pair,
        banker_pair,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baccarat_pair_payouts_settle_independently_on_a_tie() {
        let bets = Bets {
            player_pair: 20,
            banker_pair: 20,
            any_pair: 20,
            ..Bets::default()
        };
        assert_eq!(bets.total(), Some(60));
        assert_eq!(
            Bets {
                any_pair: SIDE_MAX + STEP,
                ..bets
            }
            .total(),
            None
        );

        for (player, banker, expected) in [
            (false, false, -60),
            (true, false, 300),
            (false, true, 300),
            (true, true, 540),
        ] {
            let outcome = Outcome {
                player: vec![],
                banker: vec![],
                player_total: 0,
                banker_total: 0,
                winner: Winner::Tie,
                natural: false,
                player_pair: player,
                banker_pair: banker,
            };
            assert_eq!(bets.delta(&outcome), expected);
        }
    }
}
