use super::*;
use engine::{BoardCards, Chips, PlayerId};
use std::collections::{HashMap, HashSet};
use std::mem::size_of;

impl Model {
    pub(super) fn build(
        request: RiverRequest,
        budget: &mut Budget<'_>,
    ) -> Result<Self, RiverError> {
        budget.tick(1)?;
        validate_request(&request)?;
        let root_pot = request.players.iter().map(|p| p.prior_contribution).sum();
        let weights: Vec<Vec<f64>> = request
            .players
            .iter()
            .map(|p| {
                let total: f64 = p.range.iter().map(|h| h.weight).sum();
                p.range.iter().map(|h| h.weight / total).collect()
            })
            .collect();
        let mut model = Self {
            request,
            nodes: Vec::new(),
            joints: Vec::new(),
            weights,
            chance_mass: 0.0,
            root_pot,
        };
        budget.reserve(
            model
                .weights
                .iter()
                .map(|w| w.len() as u64 * (128 + model.request.limits.max_depth as u64 * 8 * 20))
                .sum(),
        )?;
        let used = model
            .request
            .board
            .iter()
            .copied()
            .fold(0, |mask, c| mask | card_mask(c));
        model.enumerate(0, used, &mut Vec::new(), 1.0, budget)?;
        if !model.chance_mass.is_finite() || model.chance_mass <= 0.0 {
            return Err(RiverError::Invalid(
                "no compatible joint card assignment".into(),
            ));
        }
        let n = model.request.players.len();
        let round = BettingRound::new(
            model
                .request
                .players
                .iter()
                .enumerate()
                .map(|(p, s)| (PlayerId(p as u64), Chips(s.stack)))
                .collect(),
            (model.request.dealer + 1) % n,
            Chips(0),
            Chips(model.request.big_blind),
        );
        model.build_node(round, 0, budget)?;
        Ok(model)
    }

    fn enumerate(
        &mut self,
        player: usize,
        used: u64,
        hands: &mut Vec<usize>,
        probability: f64,
        budget: &mut Budget<'_>,
    ) -> Result<(), RiverError> {
        budget.tick(1)?; // Charge rejected/partial tuples too.
        if player == self.request.players.len() {
            if self.joints.len() == budget.limits.max_assignments {
                return Err(RiverError::Budget("joint assignments"));
            }
            if !probability.is_finite() || probability <= 0.0 {
                return Err(RiverError::Numerical);
            }
            budget.reserve((size_of::<Joint>() + hands.len() * size_of::<usize>()) as u64 * 2)?;
            self.joints.push(Joint {
                hands: hands.clone(),
                probability,
            });
            self.chance_mass += probability;
            return Ok(());
        }
        for h in 0..self.request.players[player].range.len() {
            budget.tick(1)?;
            let mask = hand_mask(self.request.players[player].range[h].hand);
            if used & mask == 0 {
                hands.push(h);
                self.enumerate(
                    player + 1,
                    used | mask,
                    hands,
                    probability * self.weights[player][h],
                    budget,
                )?;
                hands.pop();
            }
        }
        Ok(())
    }

    fn build_node(
        &mut self,
        round: BettingRound,
        depth: usize,
        budget: &mut Budget<'_>,
    ) -> Result<usize, RiverError> {
        budget.tick(1)?;
        if depth > budget.limits.max_depth {
            return Err(RiverError::Budget("tree depth"));
        }
        if self.nodes.len() >= budget.limits.max_nodes {
            return Err(RiverError::Budget("public nodes"));
        }
        let n = self.request.players.len();
        // Conservative accounted storage includes Vec growth and bounded
        // traversal scratch. It is not a measurement of allocator/RSS overhead.
        budget.reserve(
            (size_of::<Node>() + n * size_of::<engine::round::PlayerState>() + 1024) as u64 * 2,
        )?;
        let actor = round.current_player().map(|p| p.0 as usize);
        let choices = actor.map_or_else(Vec::new, |p| self.choices(&round, p));
        let actions: Vec<_> = choices.iter().map(|(a, _)| a.clone()).collect();
        let cells = actor.map_or(0, |p| actions.len() * self.weights[p].len());
        budget.reserve(cells as u64 * 8 * 10)?;
        let payoffs = if actor.is_none() {
            self.terminal_payoffs(&round, budget)?
        } else {
            Vec::new()
        };
        if actor.is_some() && actions.is_empty() {
            return Err(RiverError::Invalid("no legal tree action".into()));
        }
        let index = self.nodes.len();
        self.nodes.push(Node {
            round,
            actor,
            actions,
            children: Vec::new(),
            regrets: vec![0.0; cells],
            average: vec![0.0; cells],
            payoffs,
        });
        for (_, child) in choices {
            let id = self.build_node(child, depth + 1, budget)?;
            self.nodes[index].children.push(id);
        }
        Ok(index)
    }

    fn choices(&self, round: &BettingRound, actor: usize) -> Vec<(PlayerAction, BettingRound)> {
        let player = &round.player_states()[actor];
        let to_call = round.to_call(player.player_id).0;
        let maximum = player.stack.0 + player.contributed.0;
        let mut actions = vec![
            PlayerAction::Fold,
            if to_call == 0 {
                PlayerAction::Check
            } else {
                PlayerAction::Call
            },
        ];
        for &amount in &self.request.raise_to {
            if amount >= round.min_raise_to().0 && amount < maximum {
                actions.push(PlayerAction::Raise {
                    amount: Chips(amount),
                });
            }
        }
        // An exact/short all-in call is already represented by Call.
        if player.stack.0 > to_call {
            actions.push(PlayerAction::AllIn);
        }
        actions
            .into_iter()
            .filter_map(|action| {
                let mut next = round.clone();
                next.apply_action(player.player_id, &action)
                    .ok()
                    .map(|_| (action, next))
            })
            .collect()
    }

    fn terminal_payoffs(
        &self,
        round: &BettingRound,
        budget: &mut Budget<'_>,
    ) -> Result<Vec<f64>, RiverError> {
        let n = self.request.players.len();
        let len = self
            .joints
            .len()
            .checked_mul(n)
            .ok_or(RiverError::Budget("payoff cells"))?;
        budget.reserve(len as u64 * 8)?;
        let prior: Vec<_> = self
            .request
            .players
            .iter()
            .enumerate()
            .map(|(p, s)| (PlayerId(p as u64), Chips(s.prior_contribution)))
            .collect();
        let pots = round
            .side_pots_with_prior(&prior)
            .map_err(|e| RiverError::Invalid(e.to_string()))?;
        let order: HashMap<_, _> = (0..n)
            .map(|p| (p as u64, (p + n - (self.request.dealer + 1) % n) % n))
            .collect();
        let board = BoardCards {
            flop: Some(
                self.request.board[..3]
                    .try_into()
                    .map_err(|_| RiverError::Numerical)?,
            ),
            turn: Some(self.request.board[3]),
            river: Some(self.request.board[4]),
        };
        let no_forfeits = HashSet::new();
        let mut values = Vec::with_capacity(len);
        for joint in &self.joints {
            budget.tick((n * n) as u64)?;
            let eligible: Vec<_> = round
                .player_states()
                .iter()
                .enumerate()
                .filter(|(_, s)| !s.folded)
                .map(|(p, s)| {
                    (
                        s.player_id,
                        self.request.players[p].range[joint.hands[p]].hand,
                    )
                })
                .collect();
            let results = engine::game::settle_pots(&pots, &eligible, &board, &order, &no_forfeits);
            let mut payoff: Vec<_> = round
                .player_states()
                .iter()
                .enumerate()
                .map(|(p, s)| {
                    -(self.request.players[p].prior_contribution as f64 + s.contributed.0 as f64)
                })
                .collect();
            for result in results {
                for winner in result.winners {
                    payoff[winner.player_id.0 as usize] += winner.amount_won.0 as f64;
                }
            }
            if payoff.iter().sum::<f64>() != 0.0 {
                return Err(RiverError::Numerical);
            }
            values.extend(payoff);
        }
        Ok(values)
    }

    pub(super) fn follow(
        &self,
        profile: &Profile,
        path: &[RiverStep],
    ) -> Result<(usize, Vec<Vec<f64>>), RiverError> {
        let mut node = 0;
        let mut reach = self.weights.clone();
        for step in path {
            let current = &self.nodes[node];
            if current.actor != Some(step.player) {
                return Err(RiverError::Unreachable);
            }
            let action = current
                .actions
                .iter()
                .position(|a| *a == step.action)
                .ok_or(RiverError::Unreachable)?;
            let count = self.weights[step.player].len();
            for (h, r) in reach[step.player].iter_mut().enumerate() {
                *r *= profile[node][action * count + h];
            }
            node = current.children[action];
        }
        Ok((node, reach))
    }

    pub(super) fn decision(
        &self,
        profile: &Profile,
        path: &[RiverStep],
        hand: HoleCards,
    ) -> Result<RiverDecision, RiverError> {
        let (index, reach) = self.follow(profile, path)?;
        let node = &self.nodes[index];
        let player = node.actor.ok_or(RiverError::Unreachable)?;
        if hand.card1 == hand.card2 {
            return Err(RiverError::Unreachable);
        }
        let h = self.request.players[player]
            .range
            .iter()
            .position(|w| hand_mask(w.hand) == hand_mask(hand))
            .ok_or(RiverError::Unreachable)?;
        let joint_mass: f64 = self
            .joints
            .iter()
            .filter(|j| j.hands[player] == h)
            .map(|j| {
                j.hands
                    .iter()
                    .enumerate()
                    .map(|(p, h)| reach[p][*h])
                    .product::<f64>()
            })
            .sum();
        if !joint_mass.is_finite() || joint_mass <= 0.0 {
            return Err(RiverError::Unreachable);
        }
        let probabilities = (0..node.actions.len())
            .map(|a| profile[index][a * self.weights[player].len() + h])
            .collect::<Vec<_>>();
        validate_probabilities(&probabilities, node.actions.len())?;
        Ok(RiverDecision {
            player,
            hand,
            actions: node.actions.clone(),
            probabilities,
            pot: self.root_pot + node.round.pot_total().0,
            stacks: node
                .round
                .player_states()
                .iter()
                .map(|s| s.stack.0)
                .collect(),
            river_contributions: node
                .round
                .player_states()
                .iter()
                .map(|s| s.contributed.0)
                .collect(),
        })
    }
}

fn validate_request(req: &RiverRequest) -> Result<(), RiverError> {
    let bad = |message: &str| RiverError::Invalid(message.into());
    let n = req.players.len();
    let l = &req.limits;
    if !(2..=9).contains(&n)
        || req.dealer >= n
        || req.big_blind == 0
        || req.big_blind > 1_000_000_000
    {
        return Err(bad(
            "need 2..9 clockwise players, valid dealer and positive big blind",
        ));
    }
    if req
        .board
        .iter()
        .copied()
        .fold(0, |mask, c| mask | card_mask(c))
        .count_ones()
        != 5
    {
        return Err(bad("duplicate board card"));
    }
    if req.raise_to.len() > 12
        || req.raise_to.contains(&0)
        || req.raise_to.windows(2).any(|w| w[0] >= w[1])
    {
        return Err(bad(
            "raise_to must be strictly increasing, positive, at most 12 sizes",
        ));
    }
    if l.max_nodes == 0
        || l.max_nodes > 100_000
        || l.max_assignments == 0
        || l.max_assignments > 100_000
        || l.max_depth == 0
        || l.max_depth > 128
        || l.max_storage_bytes == 0
        || l.max_storage_bytes > 512_000_000
        || l.max_work == 0
        || l.max_work > 10_000_000_000
        || l.max_iterations == 0
        || l.max_iterations > 100_000
        || !l.target_max_gap_fraction.is_finite()
        || !(0.0..=1.0).contains(&l.target_max_gap_fraction)
    {
        return Err(bad("invalid resource or quality limit"));
    }
    let total: u64 = req
        .players
        .iter()
        .map(|p| u64::from(p.stack) + u64::from(p.prior_contribution))
        .sum();
    let pot: u64 = req
        .players
        .iter()
        .map(|p| u64::from(p.prior_contribution))
        .sum();
    if pot == 0 || total > 1_000_000_000 {
        return Err(bad(
            "root pot must be positive; total chips at most 1 billion",
        ));
    }
    let maximum = req
        .players
        .iter()
        .map(|p| p.prior_contribution)
        .max()
        .unwrap_or(0);
    if req
        .players
        .iter()
        .any(|p| p.stack > 0 && p.prior_contribution != maximum)
    {
        return Err(bad(
            "live stacks must have matched prior-street contributions",
        ));
    }
    let board_mask = req
        .board
        .iter()
        .copied()
        .fold(0, |mask, c| mask | card_mask(c));
    for p in &req.players {
        if p.range.is_empty() || p.range.len() > 1326 {
            return Err(bad("empty or oversized range"));
        }
        let mut seen = HashSet::new();
        for weighted in &p.range {
            let mask = hand_mask(weighted.hand);
            if mask.count_ones() != 2 || mask & board_mask != 0 || !seen.insert(mask) {
                return Err(bad("duplicate or board-blocked range combo"));
            }
            if !weighted.weight.is_finite() || !(1e-12..=1.0).contains(&weighted.weight) {
                return Err(bad("range weights must be in [1e-12,1]"));
            }
        }
    }
    Ok(())
}
