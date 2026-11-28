//! Classic three-player Dou Dizhu. No wild cards; sequences stop at ace.
use serde::Serialize;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Card(pub u8);
impl Card {
    pub fn rank(self) -> u8 {
        if self.0 < 52 {
            self.0 / 4 + 3
        } else {
            self.0 - 36
        }
    }
    pub fn code(self) -> String {
        if self.0 == 52 {
            return "SJ".into();
        }
        if self.0 == 53 {
            return "BJ".into();
        }
        format!(
            "{}{}",
            b"3456789TJQKA2"[(self.0 / 4) as usize] as char,
            b"shdc"[(self.0 % 4) as usize] as char
        )
    }
    pub fn parse(s: &str) -> Option<Self> {
        if s == "SJ" {
            return Some(Self(52));
        }
        if s == "BJ" {
            return Some(Self(53));
        }
        let b = s.as_bytes();
        if b.len() != 2 {
            return None;
        }
        Some(Self(
            (b"3456789TJQKA2".iter().position(|r| *r == b[0])? * 4
                + b"shdc".iter().position(|r| *r == b[1])?) as u8,
        ))
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Single,
    Pair,
    Triple,
    TripleSingle,
    TriplePair,
    Straight,
    Pairs,
    Plane,
    PlaneSingles,
    PlanePairs,
    FourSingles,
    FourPairs,
    Bomb,
    Rocket,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Pattern {
    pub kind: Kind,
    pub high: u8,
    pub len: usize,
}
impl Pattern {
    pub fn beats(self, other: Self) -> bool {
        if self.kind == Kind::Rocket {
            return other.kind != Kind::Rocket;
        }
        if other.kind == Kind::Rocket {
            return false;
        }
        if self.kind == Kind::Bomb && other.kind != Kind::Bomb {
            return true;
        }
        self.kind == other.kind && self.len == other.len && self.high > other.high
    }
}
pub fn evaluate(cards: &[Card]) -> Option<Pattern> {
    use Kind::*;
    let n = cards.len();
    if n == 0 || n > 20 {
        return None;
    }
    let mut seen = [false; 54];
    let mut counts = [0u8; 18];
    for &c in cards {
        if c.0 >= 54 || seen[c.0 as usize] {
            return None;
        }
        seen[c.0 as usize] = true;
        counts[c.rank() as usize] += 1;
    }
    let mut present = [0; 15];
    let mut distinct = 0;
    for (rank, &count) in counts.iter().enumerate().skip(3) {
        if count > 0 {
            present[distinct] = rank;
            distinct += 1;
        }
    }
    let ranks = &present[..distinct];
    let high = *ranks.last()? as u8;
    let out = |kind, high| Some(Pattern { kind, high, len: n });
    if n == 1 {
        return out(Single, high);
    }
    if n == 2 && counts[16] == 1 && counts[17] == 1 {
        return out(Rocket, 17);
    }
    if ranks.len() == 1 {
        return out(
            match n {
                2 => Pair,
                3 => Triple,
                4 => Bomb,
                _ => return None,
            },
            high,
        );
    }
    if n == 4 || n == 5 {
        if let Some(r) = (3..16).find(|&r| counts[r] == 3) {
            if n == 4 {
                return out(TripleSingle, r as u8);
            }
            if ranks.len() == 2 {
                return out(TriplePair, r as u8);
            }
        }
    }
    let consecutive = high <= 14 && ranks.windows(2).all(|r| r[1] == r[0] + 1);
    if n >= 5 && ranks.len() == n && consecutive {
        return out(Straight, high);
    }
    if n >= 6 && n.is_multiple_of(2) && ranks.iter().all(|&r| counts[r] == 2) && consecutive {
        return out(Pairs, high);
    }
    for (unit, kind) in [(3, Plane), (4, PlaneSingles), (5, PlanePairs)] {
        if !n.is_multiple_of(unit) || n / unit < 2 {
            continue;
        }
        let k = n / unit;
        for start in 3..=14usize.saturating_sub(k - 1) {
            if !(start..start + k).all(|r| counts[r] == 3) {
                continue;
            }
            let mut wings = ranks
                .iter()
                .copied()
                .filter(|r| !(start..start + k).contains(r));
            let valid = match kind {
                Plane => wings.next().is_none(),
                PlaneSingles => wings.all(|r| counts[r] <= 2),
                PlanePairs => ranks.len() == 2 * k && wings.all(|r| counts[r] == 2),
                _ => false,
            };
            if valid {
                return out(kind, (start + k - 1) as u8);
            }
        }
    }
    if let Some(r) = (3..16).find(|&r| counts[r] == 4) {
        if n == 6 {
            return out(FourSingles, r as u8);
        }
        if n == 8 && ranks.len() == 3 && ranks.iter().filter(|&&x| x != r).all(|&x| counts[x] == 2)
        {
            return out(FourPairs, r as u8);
        }
    }
    None
}

fn choose(
    groups: &[&[Card]],
    n: usize,
    at: usize,
    chosen: &mut Vec<Card>,
    out: &mut Vec<Vec<Card>>,
    stop_at: Option<usize>,
    target: Option<Pattern>,
) {
    if stop_at.is_some_and(|limit| out.len() >= limit) {
        return;
    }
    if n == 0 {
        if evaluate(chosen).is_some_and(|pattern| target.is_none_or(|t| pattern.beats(t))) {
            out.push(chosen.clone());
        }
        return;
    }
    if groups.len().saturating_sub(at) < n {
        return;
    }
    for i in at..=groups.len() - n {
        let old = chosen.len();
        chosen.extend_from_slice(groups[i]);
        choose(groups, n - 1, i + 1, chosen, out, stop_at, target);
        chosen.truncate(old);
    }
}
/// Rank-directed candidate generation: no access to other players' cards.
pub fn moves(cards: &[Card], target: Option<Pattern>) -> Vec<Vec<Card>> {
    moves_inner(cards, target, None)
        .into_iter()
        .map(|(cards, _)| cards)
        .collect()
}

/// A bounded legal candidate set for the latency-sensitive room bot. Basic
/// groups and sequences remain complete; only interchangeable wing
/// combinations on triples, planes and fours are capped per body. Each move
/// comes with its pattern.
pub(crate) fn bot_moves(cards: &[Card], target: Option<Pattern>) -> Vec<(Vec<Card>, Pattern)> {
    moves_inner(cards, target, Some(4))
}

fn moves_inner(
    cards: &[Card],
    target: Option<Pattern>,
    wing_limit: Option<usize>,
) -> Vec<(Vec<Card>, Pattern)> {
    if target.is_some_and(|t| t.kind == Kind::Rocket) {
        return Vec::new();
    }
    let mut groups: Vec<Vec<Card>> = vec![vec![]; 18];
    for &c in cards {
        groups[c.rank() as usize].push(c);
    }
    let mut candidates = Vec::new();
    for g in &groups {
        for n in 1..=g.len() {
            candidates.push(g[..n].to_vec());
        }
    }
    if !groups[16].is_empty() && !groups[17].is_empty() {
        candidates.push(vec![groups[16][0], groups[17][0]]);
    }
    let mut attach = |mut body: Vec<Card>, body_ranks: &[usize], wing_count: usize, pair: bool| {
        // A body with wings can never be a bomb or a rocket. To beat a play
        // on the table it must have the same number of cards.
        let length = body.len() + wing_count * if pair { 2 } else { 1 };
        if target.is_some_and(|t| t.len != length || t.kind == Kind::Bomb) {
            return;
        }
        let mut wings = Vec::new();
        for (rank, group) in groups.iter().enumerate().skip(3) {
            if body_ranks.contains(&rank) {
                continue;
            }
            if pair {
                if let Some(cards) = group.get(..2) {
                    wings.push(cards);
                }
            } else {
                wings.extend(group.iter().map(std::slice::from_ref));
            }
        }
        let stop_at = wing_limit.map(|limit| candidates.len() + limit);
        choose(
            &wings,
            wing_count,
            0,
            &mut body,
            &mut candidates,
            stop_at,
            target,
        );
    };
    for (r, group) in groups.iter().enumerate().take(16).skip(3) {
        if group.len() >= 3 {
            attach(group[..3].to_vec(), &[r], 1, false);
            attach(group[..3].to_vec(), &[r], 1, true);
        }
        if group.len() == 4 {
            attach(group.clone(), &[r], 2, false);
            attach(group.clone(), &[r], 2, true);
        }
    }
    for start in 3..14 {
        for end in start + 1..=14 {
            let k = end - start + 1;
            if target.is_some_and(|t| t.kind == Kind::Bomb || (t.len != 4 * k && t.len != 5 * k)) {
                continue;
            }
            if (start..=end).all(|r| groups[r].len() >= 3) {
                let rs: Vec<_> = (start..=end).collect();
                let body = (start..=end)
                    .flat_map(|r| groups[r][..3].iter().copied())
                    .collect::<Vec<_>>();
                attach(body.clone(), &rs, k, false);
                attach(body, &rs, k, true);
            }
        }
    }
    for unit in 1..=3 {
        for start in 3..14 {
            for end in start + 1..=14 {
                let k = end - start + 1;
                let min = if unit == 1 {
                    5
                } else if unit == 2 {
                    3
                } else {
                    2
                };
                if k >= min
                    && target.is_none_or(|t| t.kind != Kind::Bomb && t.len == k * unit)
                    && (start..=end).all(|r| groups[r].len() >= unit)
                {
                    candidates.push(
                        (start..=end)
                            .flat_map(|r| groups[r][..unit].iter().copied())
                            .collect(),
                    );
                }
            }
        }
    }
    let mut legal: Vec<(Vec<Card>, Pattern)> = candidates
        .into_iter()
        .filter_map(|mut cs| {
            let p = evaluate(&cs).filter(|p| target.is_none_or(|t| p.beats(t)))?;
            cs.sort();
            Some((cs, p))
        })
        .collect();
    legal.sort_by(|a, b| a.0.cmp(&b.0));
    legal.dedup_by(|a, b| a.0 == b.0);
    legal.sort_by_key(|(cs, p)| {
        (
            matches!(p.kind, Kind::Bomb | Kind::Rocket),
            std::cmp::Reverse(cs.len()),
            p.high,
        )
    });
    legal
}
