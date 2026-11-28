//! The draw–discard–claim core of the tile games with a claim window
//! (成都血战, 捉鸡麻将; 卡五星 is the same shape with three seats): melds,
//! the match phase ([`claim_phase!`]), the claim window and its resolution
//! order, and the zero-sum settlement rows. Generic over the seat count `N`
//! and the game's action; each game keeps its own scoring, payouts and
//! extra phases.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::Tile;

/// A pung or kong in front of a seat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meld {
    pub tile: Tile,
    pub kind: MeldKind,
    pub from: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeldKind {
    Pung,
    ExposedKong,
    ConcealedKong,
    AddedKong,
}

/// The move needs tiles or a meld the seat does not hold. Legal moves never
/// do; a stale or corrupt state refuses the move instead of stopping the room.
const MISSING: &str = "illegal_action";

/// Applies one move to `state` as a whole: a refused move (`Err`) leaves it
/// exactly as it was, however far the move got before it was refused.
pub fn all_or_nothing<T: Clone, E>(
    state: &mut T,
    apply: impl FnOnce(&mut T) -> Result<(), E>,
) -> Result<(), E> {
    let before = state.clone();
    let result = apply(state);
    if result.is_err() {
        *state = before;
    }
    result
}

/// Takes `n` copies of `tile` out of `tiles`, or nothing when it holds fewer.
pub fn remove_tiles(tiles: &mut Vec<Tile>, tile: Tile, n: usize) -> Result<(), &'static str> {
    if copies(tiles, tile) < n {
        debug_assert!(
            false,
            "move removes tile {tile} x{n} the hand does not hold"
        );
        return Err(MISSING);
    }
    for _ in 0..n {
        if let Some(i) = tiles.iter().position(|&t| t == tile) {
            tiles.remove(i);
        }
    }
    Ok(())
}

/// How many copies of `tile` the hand holds (four: a concealed kong).
pub fn copies(tiles: &[Tile], tile: Tile) -> usize {
    tiles.iter().filter(|&&t| t == tile).count()
}

/// A seat's concealed tiles, melds and discard pile.
pub trait TileSeat {
    fn hand(&mut self) -> (&mut Vec<Tile>, &mut Vec<Meld>, &mut Vec<Tile>);

    /// A kong of `kind` on `tile` (`from`: the discarder, or the seat itself):
    /// an added kong upgrades the seat's pung, any other moves the tiles from
    /// the hand into a new meld. The replacement draw is the game's. Changes
    /// nothing when the seat lacks the tiles or the pung.
    fn kong_meld(&mut self, tile: Tile, from: usize, kind: MeldKind) -> Result<(), &'static str> {
        let (tiles, melds, _) = self.hand();
        if kind == MeldKind::AddedKong {
            let Some(pung) = melds
                .iter_mut()
                .find(|m| m.tile == tile && m.kind == MeldKind::Pung)
            else {
                debug_assert!(false, "added kong on {tile} without its pung");
                return Err(MISSING);
            };
            remove_tiles(tiles, tile, 1)?;
            pung.kind = kind;
        } else {
            let n = if kind == MeldKind::ConcealedKong {
                4
            } else {
                3
            };
            remove_tiles(tiles, tile, n)?;
            melds.push(Meld { tile, kind, from });
        }
        Ok(())
    }
    /// The seat pungs `tile` discarded by `from`; the caller takes it off
    /// that pile once this succeeds.
    fn pung(&mut self, tile: Tile, from: usize) -> Result<(), &'static str> {
        let (tiles, melds, _) = self.hand();
        remove_tiles(tiles, tile, 2)?;
        melds.push(Meld {
            tile,
            kind: MeldKind::Pung,
            from,
        });
        Ok(())
    }
    /// A robbed added kong: the tile leaves the hand for the discard pile.
    fn robbed(&mut self, tile: Tile) -> Result<(), &'static str> {
        let (tiles, _, discards) = self.hand();
        remove_tiles(tiles, tile, 1)?;
        discards.push(tile);
        Ok(())
    }
}

/// The game's action as the claim window sees it.
pub trait ClaimAction: Clone + PartialEq {
    const PASS: Self;
    const HU: Self;
    /// `Some(true)` claims an exposed kong, `Some(false)` a pung.
    fn claim(&self) -> Option<bool>;
}

/// The claim window after a discard, or after an added kong that can be
/// robbed (`rob`); `kong`: the tile came off a kong's replacement draw.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound(serialize = "A: Serialize", deserialize = "A: Deserialize<'de>"))]
pub struct ClaimWindow<A, const N: usize> {
    pub from: usize,
    pub tile: Tile,
    pub rob: bool,
    pub kong: bool,
    /// One answer per seat; a seat that cannot claim has passed already.
    #[serde(with = "per_seat")]
    pub choices: [Option<A>; N],
}

/// A decided [`ClaimWindow`]: its tile and what it resolves to.
pub struct Decided {
    pub from: usize,
    pub tile: Tile,
    pub rob: bool,
    pub kong: bool,
    pub resolution: Resolution,
}

/// See [`ClaimWindow::decided`].
pub enum Resolution {
    /// These seats win on the tile, in turn order after the discarder.
    Win(Vec<usize>),
    /// Nobody robbed the added kong: it stands.
    KongStands,
    /// `seat` takes the tile: an exposed kong (`kong`) or a pung.
    Claim { seat: usize, kong: bool },
    /// Nobody claims: the next seat draws.
    Pass,
}

impl<A: ClaimAction, const N: usize> ClaimWindow<A, N> {
    /// A window on `tile` from `from`; a seat that cannot claim it has
    /// already passed.
    pub fn open(
        from: usize,
        tile: Tile,
        rob: bool,
        kong: bool,
        can_claim: impl Fn(usize) -> bool,
    ) -> Self {
        Self {
            from,
            tile,
            rob,
            kong,
            choices: std::array::from_fn(|s| (!can_claim(s)).then_some(A::PASS)),
        }
    }
    /// What the window resolves to once every seat has chosen (`None`
    /// before): wins (turn order after the discarder) first, then a robbable
    /// kong that nobody robbed, then the first claimant after the discarder.
    pub fn decided(&self) -> Option<Decided> {
        if self.choices.iter().any(Option::is_none) {
            return None;
        }
        let seats = (1..N).map(|n| (self.from + n) % N);
        let wins: Vec<usize> = seats
            .clone()
            .filter(|&s| self.choices[s] == Some(A::HU))
            .collect();
        let resolution = if !wins.is_empty() {
            Resolution::Win(wins)
        } else if self.rob {
            Resolution::KongStands
        } else {
            seats
                .into_iter()
                .find_map(|seat| {
                    let kong = self.choices[seat].as_ref()?.claim()?;
                    Some(Resolution::Claim { seat, kong })
                })
                .unwrap_or(Resolution::Pass)
        };
        Some(Decided {
            from: self.from,
            tile: self.tile,
            rob: self.rob,
            kong: self.kong,
            resolution,
        })
    }
}

/// `[T; N]` as a JSON array of exactly `N` entries.
mod per_seat {
    use super::*;
    use serde::de::Error;

    pub fn serialize<S: Serializer, T: Serialize, const N: usize>(
        a: &[T; N],
        s: S,
    ) -> Result<S::Ok, S::Error> {
        a.as_slice().serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>, T: Deserialize<'de>, const N: usize>(
        d: D,
    ) -> Result<[T; N], D::Error> {
        let v = Vec::<T>::deserialize(d)?;
        let len = v.len();
        v.try_into()
            .map_err(|_| D::Error::invalid_length(len, &"one entry per seat"))
    }
}

/// One payment of a zero-sum settlement; `kind` is the game's payout name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transfer {
    pub from: usize,
    pub to: usize,
    pub amount: i64,
    pub kind: String,
}

/// A winning hand with the game's score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Winner<S> {
    pub seat: usize,
    pub tile: Tile,
    pub score: S,
    pub self_draw: bool,
}

/// The match phase of a claim-window game with `$seats` seats, in the
/// caller's `Action` and `Tile`: `$extra` unit variants first (成都's 定缺
/// `Missing`), then a seat's turn, the claim window and the end. Also makes
/// the caller's `Action` (with `Pass`, `Hu`, `Pung` and `Kong { .. }`) the
/// window's [`ClaimAction`].
macro_rules! claim_phase {
    ($seats:expr $(; $($extra:ident),* $(,)?)?) => {
        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        pub enum Phase {
            $($($extra,)*)?
            Turn {
                seat: usize,
                drawn: Option<Tile>,
                kong: bool,
            },
            Claims($crate::mahjong::ClaimWindow<Action, { $seats }>),
            End,
        }
        impl $crate::mahjong::ClaimAction for Action {
            const PASS: Self = Action::Pass;
            const HU: Self = Action::Hu;
            fn claim(&self) -> Option<bool> {
                match self {
                    Action::Kong { .. } => Some(true),
                    Action::Pung => Some(false),
                    _ => None,
                }
            }
        }
        impl Phase {
            /// `seat`'s turn after it claimed a pung: no draw.
            pub fn after_pung(seat: usize) -> Self {
                Phase::Turn {
                    seat,
                    drawn: None,
                    kong: false,
                }
            }
            /// The tile `seat` drew this turn.
            pub fn drawn_for(&self, seat: usize) -> Option<Tile> {
                match self {
                    Phase::Turn {
                        seat: turn, drawn, ..
                    } if *turn == seat => *drawn,
                    _ => None,
                }
            }
            /// The claim window, once every seat has answered it.
            pub fn decided(&self) -> Option<$crate::mahjong::Decided> {
                match self {
                    Phase::Claims(w) => w.decided(),
                    _ => None,
                }
            }
            /// `seat` is to play, or still has to answer the claim window.
            pub fn deciding(&self, seat: usize) -> bool {
                match self {
                    Phase::Turn { seat: turn, .. } => *turn == seat,
                    Phase::Claims(w) => w.choices[seat].is_none(),
                    _ => false,
                }
            }
        }
    };
}
pub(crate) use claim_phase;
