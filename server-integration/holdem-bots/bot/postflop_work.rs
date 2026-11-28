//! Optional postflop solves run inside the ordinary think pause. Lifecycle
//! processing and the advertised deadline never wait for a solver result.

use super::{multiway_river_model, river_model, turn_model, DecisionContext, OpponentModel};
use engine::{PlayerAction, Street};
use rand::rngs::StdRng;
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, Semaphore};

static WORKERS: Semaphore = Semaphore::const_new(1);
const MAX_WORK: Duration = Duration::from_millis(400);
const TURN: &str = "hu_turn_jam_model";
const TURN_RIVER: &str = "hu_turn_jam_river";

fn submit<T: Send + 'static>(
    solve: impl FnOnce(Instant) -> Option<T> + Send + 'static,
) -> Option<oneshot::Receiver<Option<T>>> {
    let permit = WORKERS.try_acquire().ok()?;
    let cpu = crate::cpu_admission::try_acquire()?;
    let (send, recv) = oneshot::channel();
    let deadline = Instant::now() + MAX_WORK;
    tokio::task::spawn_blocking(move || {
        let _permits = (permit, cpu);
        if !send.is_closed() && Instant::now() < deadline {
            let _ = send.send(solve(deadline));
        }
    });
    Some(recv)
}

#[derive(Default)]
pub(crate) struct Hand {
    river: Option<(river_model::Request, Outcome<river_model::Solved>)>,
    three_way: Option<(
        multiway_river_model::Request,
        Outcome<multiway_river_model::Solved>,
    )>,
    // At most the two heads-up seats. Each must have used the turn policy
    // itself before following that policy through a public river deal.
    turn_seats: Vec<u8>,
    // A seat which took its own river action outside the solved strategy
    // cannot resume that strategy at a later node of the same hand.
    river_off_policy: Vec<u8>,
}

enum Outcome<T> {
    Running(oneshot::Receiver<Option<T>>),
    Finished(Option<T>),
}

impl<T> Outcome<T> {
    fn ready(&mut self) -> Option<&T> {
        if let Self::Running(recv) = self {
            *self = Self::Finished(match recv.try_recv() {
                Ok(result) => result,
                Err(oneshot::error::TryRecvError::Closed) => None,
                Err(oneshot::error::TryRecvError::Empty) => return None,
            });
        }
        match self {
            Self::Finished(policy) => policy.as_ref(),
            Self::Running(_) => None,
        }
    }
}

pub(crate) enum Pending {
    River(river_model::Query),
    ThreeWay(multiway_river_model::Query),
    Turn(oneshot::Receiver<Option<turn_model::Decision>>, Street),
}

pub(crate) enum Policy<'a> {
    River(&'a river_model::Solved, river_model::Query),
    ThreeWay(
        &'a multiway_river_model::Solved,
        multiway_river_model::Query,
    ),
    Turn(turn_model::Decision, Street),
}

impl Policy<'_> {
    pub(super) fn decision(
        self,
        rng: &mut StdRng,
        model: Option<&OpponentModel<'_>>,
    ) -> Option<(PlayerAction, &'static str)> {
        match self {
            Self::River(solved, query) => solved.decision(query, rng, model),
            Self::ThreeWay(solved, query) => solved.decision(query, rng, model),
            Self::Turn(decision, street) => Some((
                decision.sample(rng),
                if street == Street::Turn {
                    TURN
                } else {
                    TURN_RIVER
                },
            )),
        }
    }
}

impl Hand {
    pub(super) fn admits_turn(&self, ctx: &DecisionContext<'_>) -> bool {
        self.turn_seats.contains(&ctx.seat)
            || !ctx.street_actions.iter().any(|(seat, _)| *seat == ctx.seat)
    }

    pub(crate) fn prepare(
        &mut self,
        ctx: &DecisionContext<'_>,
        reads: Option<&OpponentModel<'_>>,
    ) -> Option<Pending> {
        let prior_river_fallback =
            ctx.street == Street::River && self.river_off_policy.contains(&ctx.seat);
        let turn = match ctx.street {
            Street::Turn if self.admits_turn(ctx) => turn_model::Request::from_context(ctx),
            Street::River if !prior_river_fallback && self.turn_seats.contains(&ctx.seat) => {
                turn_model::Request::after_turn(ctx)
            }
            _ => None,
        };
        if let Some((req, query)) = turn {
            let recv = submit(move |deadline| {
                let decision = req.solve(query, deadline)?;
                let (iterations, max_gap) = decision.quality();
                tracing::debug!(iterations, max_gap, "Shark turn solve");
                Some(decision)
            })?;
            return Some(Pending::Turn(recv, ctx.street));
        }

        if let Some((request, query)) = multiway_river_model::Request::from_context(ctx) {
            if !self
                .three_way
                .as_ref()
                .is_some_and(|(old, _)| old.same_root(&request))
            {
                self.three_way = None;
                let prepare_read = reads.is_some_and(|m| {
                    m.river_responder
                        .is_some_and(|(seat, _)| multiway_river_model::overfolds(m, seat))
                });
                let job = request.clone();
                let recv = submit(move |deadline| {
                    let mut solved = job.solve(deadline)?;
                    if prepare_read {
                        solved.prepare_overfolder(deadline);
                    }
                    let (iterations, gaps) = solved.quality();
                    tracing::debug!(iterations, ?gaps, "Shark three-way river solve");
                    Some(solved)
                })?;
                self.three_way = Some((request, Outcome::Running(recv)));
            }
            return Some(Pending::ThreeWay(query));
        }

        // The three-way subgame starts *after* the two checks and each player
        // acts only once in it. For a two-live river, prior checks form part
        // of the solved tree itself.
        if prior_river_fallback {
            return None;
        }
        let (request, query) = river_model::Request::from_context(ctx)?;
        if self
            .river
            .as_ref()
            .is_some_and(|(old, _)| old.same_root(&request))
        {
            return Some(Pending::River(query));
        }
        self.river = None;
        let job = request.clone();
        let recv = submit(move |deadline| {
            let solved = job.solve(deadline)?;
            let (iterations, [oop_gap, ip_gap]) = solved.quality();
            tracing::debug!(iterations, oop_gap, ip_gap, "Shark river solve");
            Some(solved)
        })?;
        self.river = Some((request, Outcome::Running(recv)));
        Some(Pending::River(query))
    }

    pub(crate) fn result(&mut self, pending: Option<Pending>) -> Option<Policy<'_>> {
        match pending? {
            Pending::River(query) => Some(Policy::River(self.river.as_mut()?.1.ready()?, query)),
            Pending::ThreeWay(query) => {
                Some(Policy::ThreeWay(self.three_way.as_mut()?.1.ready()?, query))
            }
            Pending::Turn(mut recv, street) => Some(Policy::Turn(recv.try_recv().ok()??, street)),
        }
    }

    pub(crate) fn record(&mut self, seat: u8, street: Street, source: &str) {
        if street == Street::River
            && !matches!(
                source,
                TURN_RIVER
                    | "hu_river_line_model"
                    | "hu_river_folded_range_model"
                    | "three_way_river_bucket_model"
            )
            && !self.river_off_policy.contains(&seat)
        {
            self.river_off_policy.push(seat);
        }
        if street == Street::Turn {
            self.turn_seats.retain(|&s| s != seat);
            if source == TURN {
                self.turn_seats.push(seat);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::{preflop_blueprint::snapshot_with, table_decision_with_postflop};
    use engine::{Chips, GameHand, PlayerId, PokerRng};

    #[tokio::test]
    async fn actual_river_decision_uses_admitted_worker_and_reuses_the_public_root() {
        let mut hand = GameHand::new_with_rng(
            vec![
                (PlayerId(30), Chips(1000), 4),
                (PlayerId(42), Chips(1000), 7),
            ],
            0,
            Chips(10),
            Chips(5),
            PokerRng::from_seed_bytes([7; 32]),
        );
        hand.start().unwrap();
        let play = |h: &mut GameHand, a| {
            h.apply_action(h.snapshot().current_actor.unwrap(), a)
                .unwrap();
        };
        play(&mut hand, PlayerAction::Raise { amount: Chips(25) });
        play(&mut hand, PlayerAction::Call);
        for _ in 0..4 {
            play(&mut hand, PlayerAction::Check);
        }
        let mut work = Hand::default();
        let pending = snapshot_with(&hand, |_| {}, |ctx| work.prepare(ctx, None));
        // The test synchronizes with completion; the production room calls
        // try_recv just once when its existing think pause ends.
        let (_, Outcome::Running(rx)) = work.river.as_mut().unwrap() else {
            panic!("missing worker")
        };
        let result = rx.await.unwrap();
        work.river.as_mut().unwrap().1 = Outcome::Finished(result);
        let (action, _, source) = snapshot_with(
            &hand,
            |_| {},
            |ctx| table_decision_with_postflop(ctx, None, work.result(pending)),
        );
        assert_eq!(source, "hu_river_line_model");
        assert!(hand
            .validate_action(hand.snapshot().current_actor.unwrap(), &action)
            .is_ok());
        play(&mut hand, PlayerAction::Check);
        let next = snapshot_with(&hand, |_| {}, |ctx| work.prepare(ctx, None));
        assert!(matches!(work.result(next), Some(Policy::River(_, _))));
        // If OOP checked on a fallback, IP can still use the public model,
        // but OOP cannot borrow the solver's checking strategy facing this bet.
        let actor = hand.snapshot().current_actor.unwrap();
        let ip = hand
            .snapshot()
            .players
            .iter()
            .find(|p| p.player_id == actor)
            .unwrap()
            .seat;
        let oop = if ip == 4 { 7 } else { 4 };
        work.record(oop, Street::River, "range_policy");
        let bet = hand.snapshot().pot.0.div_ceil(2);
        play(&mut hand, PlayerAction::Raise { amount: Chips(bet) });
        assert!(snapshot_with(&hand, |_| {}, |ctx| work.prepare(ctx, None)).is_none());
        three_live_players_after_public_checks().await;
    }

    async fn three_live_players_after_public_checks() {
        // The first two checks precede the solved root. When the button bets,
        // those seats may still use the model for their first response.
        let mut game = (1..=64)
            .find_map(|seed| {
                let seats = (0..3)
                    .map(|s| (PlayerId(80 + u64::from(s)), Chips(1000), s))
                    .collect();
                let mut g = GameHand::new_with_rng(
                    seats,
                    0,
                    Chips(10),
                    Chips(5),
                    PokerRng::from_seed_bytes([seed; 32]),
                );
                g.start().unwrap();
                let mut play = |a| {
                    g.apply_action(g.snapshot().current_actor.unwrap(), a)
                        .unwrap()
                };
                play(PlayerAction::Raise { amount: Chips(25) });
                play(PlayerAction::Call);
                play(PlayerAction::Call);
                for _ in 0..8 {
                    play(PlayerAction::Check);
                }
                let has_reach = snapshot_with(
                    &g,
                    |_| {},
                    |ctx| {
                        let r = crate::bot::preflop_blueprint::flop_reach(ctx).unwrap();
                        r.weight(ctx.seat, ctx.hole).unwrap_or(0.0) > 0.0
                            && crate::bot::terminal::decision(ctx).is_none()
                    },
                );
                has_reach.then_some(g)
            })
            .expect("an actual button holding in the opening range");
        let mut work = Hand::default();
        work.record(1, Street::River, "range_policy");
        work.record(2, Street::River, "range_policy");
        let pending = snapshot_with(&game, |_| {}, |ctx| work.prepare(ctx, None));
        let (_, Outcome::Running(rx)) = work.three_way.as_mut().unwrap() else {
            panic!("missing three-way worker")
        };
        let solved = rx.await.unwrap();
        work.three_way.as_mut().unwrap().1 = Outcome::Finished(solved);
        let (action, _, source) = snapshot_with(
            &game,
            |_| {},
            |ctx| table_decision_with_postflop(ctx, None, work.result(pending)),
        );
        assert_eq!(source, "three_way_river_bucket_model");
        let actor = game.snapshot().current_actor.unwrap();
        assert!(game.validate_action(actor, &action).is_ok());
        game.apply_action(actor, PlayerAction::Raise { amount: Chips(38) })
            .unwrap();
        let reply = snapshot_with(&game, |_| {}, |ctx| work.prepare(ctx, None));
        assert!(matches!(work.result(reply), Some(Policy::ThreeWay(_, _))));
    }
}
