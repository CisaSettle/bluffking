//! One 卡五星 hand on the shared card table: the rules engine ([`Game`])
//! plus what the action events need — an action cursor, the seats each
//! action changed, and the acting seat's own auto-play notice.
use serde::{Deserialize, Serialize};

use super::collusion::Tracker;
use super::game::{Action, Game, Phase};
use super::rules::{Meld, Tile};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoReason {
    Bot,
    Timeout,
    Ready,
    Disconnected,
    Leaving,
}

/// Feedback for the acting seat only: even an automatic pass can disclose a
/// private claim opportunity. Concealed kong tiles are omitted as well.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoPlay {
    pub seat: usize,
    /// The hand revision this action produced.
    pub revision: usize,
    pub reason: AutoReason,
    pub action: String,
    pub tile: Option<Tile>,
}

impl AutoPlay {
    fn new(seat: usize, revision: usize, reason: AutoReason, action: &Action) -> Self {
        let (action, tile) = match action {
            Action::Discard { tile } => ("discard", Some(*tile)),
            Action::Ready { tile } => ("ready", Some(*tile)),
            Action::Pung => ("pung", None),
            Action::Kong { .. } => ("kong", None),
            Action::Hu => ("hu", None),
            Action::Pass => ("pass", None),
        };
        Self {
            seat,
            revision,
            reason,
            action: action.into(),
            tile,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hand {
    pub game: Game,
    /// Applied actions this hand (human, bot or clock): the action cursor.
    pub cursor: usize,
    /// Per applied action, the seats (bit = seat) whose public row changed.
    pub public_dirty: Vec<u8>,
    /// Per applied action, the seats whose own private fields changed.
    pub private_dirty: Vec<u8>,
    /// Cursor at which the open claim window began: claims sent against any
    /// revision of the same window are still current.
    pub claims_from: usize,
    pub last_auto: Option<AutoPlay>,
    /// Cursor at which `last_auto` was last set or cleared.
    pub auto_changed_at: usize,
    /// Rolling collusion streaks, carried from hand to hand.
    pub tracker: Tracker,
}

/// What a seat's public row shows; compared before and after an action.
#[derive(PartialEq)]
struct PublicSig {
    count: usize,
    melds: Vec<Meld>,
    discards: usize,
    ready: bool,
    delta: i64,
}

/// What a seat's own private fields show.
#[derive(PartialEq)]
struct PrivateSig {
    tiles: Vec<Tile>,
    pending: bool,
    drawn: Option<Tile>,
}

impl Hand {
    pub fn new(game: Game, tracker: Tracker) -> Self {
        Self {
            game,
            cursor: 0,
            public_dirty: Vec::new(),
            private_dirty: Vec::new(),
            claims_from: 0,
            last_auto: None,
            auto_changed_at: 0,
            tracker,
        }
    }

    /// Seats that still owe a decision.
    pub fn pending(&self) -> impl Iterator<Item = usize> + '_ {
        (0..3).filter(move |&s| self.game.phase.deciding(s))
    }

    /// The tile `seat` drew this turn; shown only to that seat.
    pub fn drawn(&self, seat: usize) -> Option<Tile> {
        self.game.phase.drawn_for(seat)
    }

    fn public_sig(&self, i: usize) -> PublicSig {
        let s = &self.game.seats[i];
        PublicSig {
            count: s.tiles.len(),
            melds: s.melds.clone(),
            discards: s.discards.len(),
            ready: s.ready,
            delta: self.game.deltas[i],
        }
    }

    fn private_sig(&self, i: usize) -> PrivateSig {
        PrivateSig {
            tiles: self.game.seats[i].tiles.clone(),
            pending: self.pending().any(|s| s == i),
            drawn: self.drawn(i),
        }
    }

    /// Applies one action and records what it changed. `auto` is the reason
    /// when the table played it for the seat.
    pub fn act(
        &mut self,
        seat: usize,
        action: Action,
        auto: Option<AutoReason>,
    ) -> Result<(), &'static str> {
        let before_public: [PublicSig; 3] = std::array::from_fn(|i| self.public_sig(i));
        let before_private: [PrivateSig; 3] = std::array::from_fn(|i| self.private_sig(i));
        let was_claims = matches!(self.game.phase, Phase::Claims(_));
        let feedback = auto.map(|reason| AutoPlay::new(seat, self.cursor + 1, reason, &action));
        self.game.act(seat, action)?;
        self.cursor += 1;
        if !was_claims && matches!(self.game.phase, Phase::Claims(_)) {
            self.claims_from = self.cursor;
        }
        let mut public = 0u8;
        let mut private = 0u8;
        for i in 0..3 {
            if self.public_sig(i) != before_public[i] {
                public |= 1 << i;
            }
            if self.private_sig(i) != before_private[i] {
                private |= 1 << i;
            }
        }
        // A manual action clears the notice; an automatic one replaces it.
        let notice_before = self.last_auto.as_ref().map(|a| a.seat);
        if feedback.is_some() || self.last_auto.is_some() {
            self.last_auto = feedback;
            self.auto_changed_at = self.cursor;
            for s in notice_before
                .into_iter()
                .chain(self.last_auto.as_ref().map(|a| a.seat))
            {
                private |= 1 << s;
            }
        }
        self.public_dirty.push(public);
        self.private_dirty.push(private);
        Ok(())
    }

    /// Seats whose public rows changed in the actions since `from`.
    pub fn public_changed(&self, from: usize) -> u8 {
        let from = from.min(self.public_dirty.len());
        self.public_dirty[from..].iter().fold(0, |a, b| a | b)
    }

    /// Seats whose private fields changed in the actions since `from`.
    pub fn private_changed(&self, from: usize) -> u8 {
        let from = from.min(self.private_dirty.len());
        self.private_dirty[from..].iter().fold(0, |a, b| a | b)
    }
}
