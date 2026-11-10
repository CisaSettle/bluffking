use super::*;

enum Evaluation {
    Policy,
    BestResponse(Option<Profile>),
}

impl Model {
    pub(super) fn profile_work(&self) -> u64 {
        self.nodes.iter().map(|n| n.regrets.len() as u64 + 1).sum()
    }
    fn reach_work(&self) -> u64 {
        self.weights.iter().map(|w| w.len() as u64).sum()
    }

    fn normalized_profile(&self, average: bool) -> Profile {
        self.nodes
            .iter()
            .map(|node| {
                let Some(actor) = node.actor else {
                    return Vec::new();
                };
                let hands = self.weights[actor].len();
                let source = if average {
                    &node.average
                } else {
                    &node.regrets
                };
                let mut result = vec![0.0; source.len()];
                for h in 0..hands {
                    let total: f64 = (0..node.actions.len()).map(|a| source[a * hands + h]).sum();
                    for a in 0..node.actions.len() {
                        result[a * hands + h] = if total > 0.0 {
                            source[a * hands + h] / total
                        } else {
                            1.0 / node.actions.len() as f64
                        };
                    }
                }
                result
            })
            .collect()
    }

    pub(super) fn train(
        &mut self,
        budget: &mut Budget<'_>,
    ) -> Result<(Profile, DeviationMetrics, u32), RiverError> {
        for iteration in 1..=budget.limits.max_iterations {
            for hero in 0..self.weights.len() {
                // Freeze the entire profile for this player's pass. Regrets
                // aggregate all compatible worlds before the CFR+ clamp.
                budget.tick(self.profile_work() * 3)?;
                let profile = self.normalized_profile(false);
                self.update(
                    0,
                    hero,
                    &profile,
                    &self.weights.clone(),
                    iteration as f64,
                    budget,
                )?;
            }
            if iteration == 1 || iteration % 10 == 0 || iteration == budget.limits.max_iterations {
                budget.tick(self.profile_work() * 3)?;
                let profile = self.normalized_profile(true);
                let metrics = self.evaluate(&profile, budget)?;
                if metrics.max_gain_fraction_of_pot <= budget.limits.target_max_gap_fraction
                    || iteration == budget.limits.max_iterations
                {
                    return Ok((profile, metrics, iteration));
                }
            }
        }
        Err(RiverError::Numerical)
    }

    fn terminal_values(
        &self,
        node: usize,
        hero: usize,
        reach: &[Vec<f64>],
        budget: &mut Budget<'_>,
    ) -> Result<Vec<f64>, RiverError> {
        budget.tick((self.joints.len() * self.weights.len()) as u64)?;
        let mut values = vec![0.0; self.weights[hero].len()];
        for (j, joint) in self.joints.iter().enumerate() {
            let counterfactual: f64 = joint
                .hands
                .iter()
                .enumerate()
                .filter(|(p, _)| *p != hero)
                .map(|(p, h)| reach[p][*h])
                .product();
            values[joint.hands[hero]] +=
                counterfactual * self.nodes[node].payoffs[j * self.weights.len() + hero];
        }
        Ok(values)
    }

    fn update(
        &mut self,
        index: usize,
        hero: usize,
        profile: &Profile,
        reach: &[Vec<f64>],
        weight: f64,
        budget: &mut Budget<'_>,
    ) -> Result<Vec<f64>, RiverError> {
        budget.tick(1)?;
        let Some(actor) = self.nodes[index].actor else {
            return self.terminal_values(index, hero, reach, budget);
        };
        let hands = self.weights[actor].len();
        let children = self.nodes[index].children.clone();
        let mut results = Vec::with_capacity(children.len());
        for (a, child) in children.into_iter().enumerate() {
            budget.tick(self.reach_work() + hands as u64)?;
            let mut next = reach.to_vec();
            for (h, r) in next[actor].iter_mut().enumerate() {
                *r *= profile[index][a * hands + h];
            }
            results.push(self.update(child, hero, profile, &next, weight, budget)?);
        }
        let mut values = vec![0.0; self.weights[hero].len()];
        for (a, child) in results.iter().enumerate() {
            for (h, value) in values.iter_mut().enumerate() {
                *value += child[h]
                    * if actor == hero {
                        profile[index][a * hands + h]
                    } else {
                        1.0
                    };
            }
        }
        if actor == hero {
            budget.tick((values.len() * results.len()) as u64)?;
            let node = &mut self.nodes[index];
            for (a, child) in results.iter().enumerate() {
                for (h, value) in values.iter().enumerate() {
                    let cell = a * hands + h;
                    node.regrets[cell] = (node.regrets[cell] + child[h] - value).max(0.0);
                    // Separate own-realization average for each player; never
                    // average joint profiles or opponent-reach-weight policies.
                    node.average[cell] += weight * reach[hero][h] * profile[index][cell];
                    if !node.regrets[cell].is_finite() || !node.average[cell].is_finite() {
                        return Err(RiverError::Numerical);
                    }
                }
            }
        }
        Ok(values)
    }

    /// Counterfactual values, integrating opponent cards BEFORE choosing the
    /// best action for a hero combo. Visits zero-own-reach branches as well.
    fn values(
        &self,
        index: usize,
        hero: usize,
        profile: &Profile,
        reach: &[Vec<f64>],
        evaluation: &mut Evaluation,
        budget: &mut Budget<'_>,
    ) -> Result<Vec<f64>, RiverError> {
        budget.tick(1)?;
        let Some(actor) = self.nodes[index].actor else {
            return self.terminal_values(index, hero, reach, budget);
        };
        let hands = self.weights[actor].len();
        let mut results = Vec::with_capacity(self.nodes[index].children.len());
        for (a, &child) in self.nodes[index].children.iter().enumerate() {
            budget.tick(self.reach_work() + hands as u64)?;
            let mut next = reach.to_vec();
            if actor != hero {
                for (h, r) in next[actor].iter_mut().enumerate() {
                    *r *= profile[index][a * hands + h];
                }
            }
            results.push(self.values(child, hero, profile, &next, evaluation, budget)?);
        }
        budget.tick((results.len() * self.weights[hero].len()) as u64)?;
        let mut values = vec![0.0; self.weights[hero].len()];
        for (h, value) in values.iter_mut().enumerate() {
            if matches!(evaluation, Evaluation::BestResponse(_)) && actor == hero {
                let (choice, maximum) = results
                    .iter()
                    .enumerate()
                    .map(|(a, v)| (a, v[h]))
                    .max_by(|a, b| a.1.total_cmp(&b.1))
                    .ok_or(RiverError::Numerical)?;
                *value = maximum;
                if let Evaluation::BestResponse(Some(target)) = evaluation {
                    for a in 0..results.len() {
                        target[index][a * hands + h] = if a == choice { 1.0 } else { 0.0 };
                    }
                }
            } else {
                *value = results
                    .iter()
                    .enumerate()
                    .map(|(a, v)| {
                        v[h] * if actor == hero {
                            profile[index][a * hands + h]
                        } else {
                            1.0
                        }
                    })
                    .sum();
            }
        }
        if values.iter().any(|v| !v.is_finite()) {
            return Err(RiverError::Numerical);
        }
        Ok(values)
    }

    pub(super) fn ev(
        &self,
        profile: &Profile,
        hero: usize,
        budget: &mut Budget<'_>,
    ) -> Result<f64, RiverError> {
        let values = self.values(
            0,
            hero,
            profile,
            &self.weights,
            &mut Evaluation::Policy,
            budget,
        )?;
        Ok(self.root_ev(hero, &values))
    }

    fn root_ev(&self, hero: usize, values: &[f64]) -> f64 {
        values
            .iter()
            .zip(&self.weights[hero])
            .map(|(v, w)| v * w)
            .sum::<f64>()
            / self.chance_mass
    }

    pub(super) fn response(
        &self,
        profile: &Profile,
        hero: usize,
        budget: &mut Budget<'_>,
    ) -> Result<Profile, RiverError> {
        budget.tick(self.profile_work())?;
        let mut response = Evaluation::BestResponse(Some(profile.clone()));
        self.values(0, hero, profile, &self.weights, &mut response, budget)?;
        match response {
            Evaluation::BestResponse(Some(profile)) => Ok(profile),
            _ => Err(RiverError::Numerical),
        }
    }

    pub(super) fn evaluate(
        &self,
        profile: &Profile,
        budget: &mut Budget<'_>,
    ) -> Result<DeviationMetrics, RiverError> {
        let slack = self
            .request
            .players
            .iter()
            .map(|p| p.stack as f64 + p.prior_contribution as f64)
            .sum::<f64>()
            * 1e-10;
        let mut policy_ev = Vec::new();
        let mut best_response_ev = Vec::new();
        let mut unilateral_gain = Vec::new();
        for hero in 0..self.weights.len() {
            let ev = self.ev(profile, hero, budget)?;
            let best = self.values(
                0,
                hero,
                profile,
                &self.weights,
                &mut Evaluation::BestResponse(None),
                budget,
            )?;
            let best = self.root_ev(hero, &best);
            if !ev.is_finite() || !best.is_finite() || best < ev - slack {
                return Err(RiverError::Numerical);
            }
            policy_ev.push(ev);
            best_response_ev.push(best);
            unilateral_gain.push((best - ev).max(0.0));
        }
        if policy_ev.iter().sum::<f64>().abs() > slack {
            return Err(RiverError::Numerical);
        }
        Ok(DeviationMetrics {
            nash_conv_chips: unilateral_gain.iter().sum(),
            // Include arithmetic slack in the admission criterion. This is a
            // finite-precision check, not a proof of exact zero-error Nash.
            max_gain_fraction_of_pot: (unilateral_gain.iter().copied().fold(0.0, f64::max) + slack)
                / self.root_pot as f64,
            policy_ev,
            best_response_ev,
            unilateral_gain,
            numeric_slack_chips: slack,
        })
    }

    pub(super) fn teacher_values(
        &self,
        profile: &Profile,
        budget: &mut Budget<'_>,
    ) -> Result<Vec<Vec<TeacherValue>>, RiverError> {
        let mut records = Vec::new();
        for hero in 0..self.weights.len() {
            let values = self.values(
                0,
                hero,
                profile,
                &self.weights,
                &mut Evaluation::Policy,
                budget,
            )?;
            let mut mass = vec![0.0; self.weights[hero].len()];
            for joint in &self.joints {
                mass[joint.hands[hero]] += joint.probability;
            }
            let mut hands = Vec::new();
            for (h, &probability) in mass.iter().enumerate() {
                if probability > 0.0 {
                    hands.push(TeacherValue {
                        hand: self.request.players[hero].range[h].hand,
                        joint_reach_probability: probability / self.chance_mass,
                        expected_net_chips: values[h] * self.weights[hero][h] / probability,
                    });
                }
            }
            records.push(hands);
        }
        Ok(records)
    }
}
