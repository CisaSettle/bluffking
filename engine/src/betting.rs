//! Pure betting arithmetic shared by Hold'em and short deck.
//!
//! The callers still own action validation, street progression, hand ranking,
//! and the lifetime of a refund. Chip amounts retain their caller's integer
//! width (`u32` for Hold'em, `u64` for short deck).

use std::iter::Sum;
use std::ops::Sub;

/// A full raise, including cumulative short all-ins, reopens action.
pub fn action_reopened<Amount>(bet: Amount, faced: Amount, full_raise: Amount) -> bool
where
    Amount: Copy + Ord + Default + Sub<Output = Amount>,
{
    full_raise > Amount::default() && bet >= faced && bet - faced >= full_raise
}

/// How a game treats folded contributions when constructing its pots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PotPolicy {
    /// Every contribution defines a boundary. Unmatched layers, including
    /// folded excess above all live players, refund their contributors.
    RefundUnmatched,
    /// Live contributions define boundaries. The highest live layer also
    /// receives folded dead money above it. Short deck has already returned
    /// unmatched street bets before reaching this hand-level calculation.
    SweepFoldedExcess,
}

/// One participant's contribution, in the caller's deterministic seat order.
#[derive(Debug, Clone, Copy)]
pub struct Contribution<Id, Amount> {
    pub id: Id,
    pub amount: Amount,
    pub live: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PotLayer<Id, Amount> {
    pub cap: Amount,
    pub amount: Amount,
    pub eligible: Vec<Id>,
    pub refund_to: Vec<Id>,
}

/// Partition all contributed chips once, preserving the input order of every
/// eligible/refund list. Ranking and odd-chip order remain the caller's rules.
/// `Amount` is an unsigned chip integer; its default value is zero and the
/// caller's table limits ensure the total fits, as with its stack arithmetic.
pub fn pot_layers<Id, Amount>(
    contributions: &[Contribution<Id, Amount>],
    policy: PotPolicy,
) -> Vec<PotLayer<Id, Amount>>
where
    Id: Copy + PartialEq,
    Amount: Copy + Ord + Default + Sub<Output = Amount> + Sum<Amount>,
{
    if !contributions.iter().any(|p| p.live) {
        return Vec::new();
    }
    let mut levels: Vec<Amount> = contributions
        .iter()
        .filter(|p| policy == PotPolicy::RefundUnmatched || p.live)
        .map(|p| p.amount)
        .collect();
    levels.sort_unstable();
    levels.dedup();
    let mut previous = Amount::default();
    let mut pots = Vec::with_capacity(levels.len());
    for (index, &level) in levels.iter().enumerate() {
        let sweep = policy == PotPolicy::SweepFoldedExcess && index + 1 == levels.len();
        let amount: Amount = contributions
            .iter()
            .map(|p| {
                let capped = if sweep { p.amount } else { p.amount.min(level) };
                if capped > previous {
                    capped - previous
                } else {
                    Amount::default()
                }
            })
            .sum();
        let eligible: Vec<Id> = contributions
            .iter()
            .filter(|p| p.live && p.amount >= level)
            .map(|p| p.id)
            .collect();
        let refund_to = if policy == PotPolicy::RefundUnmatched {
            let contributors: Vec<Id> = contributions
                .iter()
                .filter(|p| p.amount > previous)
                .map(|p| p.id)
                .collect();
            if eligible.is_empty() || (eligible.len() == 1 && contributors == eligible) {
                contributors
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        if amount > Amount::default() {
            pots.push(PotLayer {
                cap: level,
                amount,
                eligible,
                refund_to,
            });
        }
        previous = level;
    }
    pots
}
