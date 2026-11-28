//! Preflop decisions when calling closes all betting in a two-player chip game.

use super::{hand_class, DecisionContext, HoleCards, PlayerAction, Street};

/// The live caller admits ordinary, heads-up chip EV before consulting this policy.
/// A compulsory wager reveals nothing about the opponent's hand. After voluntary
/// action its distribution is unknown, but a profitable call against *every*
/// possible holding remains profitable against any mixture of those holdings.
pub(super) fn decision(ctx: &DecisionContext<'_>) -> Option<PlayerAction> {
    if ctx.street != Street::Preflop
        || ctx.board.count() != 0
        || ctx.to_call.0 == 0
        || ctx.can_raise
        || ctx.hole.card1 == ctx.hole.card2
        || ctx.table.seats.len() != 2
    {
        return None;
    }

    let hero = ctx.table.seat_entry(ctx.seat)?;
    let villain = ctx.table.seats.iter().find(|s| s.seat != ctx.seat)?;
    let committed = hero.committed_this_street;
    if hero.folded
        || hero.all_in
        || villain.folded
        || hero.stack != ctx.stack_chips
        || hero.stack == 0
        || committed != ctx.committed_this_street.0
        || villain.all_in != (villain.stack == 0)
        || ctx.pot.0 != committed.checked_add(villain.committed_this_street)?
        || ctx.current_bet.0 < villain.committed_this_street
        || ctx.to_call.0 != ctx.current_bet.0.checked_sub(committed)?
        || (!villain.all_in && hero.stack > ctx.to_call.0)
    {
        return None;
    }
    // Uncalled chips return to their owner. In particular the nominal full BB
    // in current_bet may exceed what the short-stacked BB actually posted.
    let matched = committed
        .checked_add(hero.stack)?
        .min(villain.committed_this_street);
    let cost = matched.checked_sub(committed)?;
    if cost == 0 {
        return None;
    }
    let (minimum, uniform) = numerators(ctx.hole);
    let forced = ctx.street_actions.is_empty() && ctx.public_history.is_some_and(|h| h.is_empty());
    let (numerator, boards) = if forced {
        (uniform, BOARDS_PER_MATCHUP * 1_225)
    } else {
        (minimum, BOARDS_PER_MATCHUP)
    };
    // Equity = (2*wins + ties)/(2*boards). The contested pot is exactly
    // 2*matched. Cancel those factors of two before multiplying chip amounts.
    if u64::from(numerator) * u64::from(matched) >= u64::from(cost) * boards {
        Some(PlayerAction::Call)
    } else if forced {
        Some(PlayerAction::Fold)
    } else {
        None
    }
}

fn numerators(hole: HoleCards) -> (u32, u32) {
    let (high, low, suited) = hand_class(hole);
    let (row, column) = if suited { (low, high) } else { (high, low) };
    (
        MIN_NUMERATOR[row as usize][column as usize],
        RANDOM_NUMERATOR[row as usize][column as usize],
    )
}

/// Lower bounds for showdown equity against (worst exact holding, uniform
/// opponent), for a valid two-card holding on an otherwise unseen deck.
pub(super) fn equity_bounds(hole: HoleCards) -> (f64, f64) {
    let (minimum, uniform) = numerators(hole);
    let twice_boards = (2 * BOARDS_PER_MATCHUP) as f64;
    (
        (f64::from(minimum) / twice_boards).next_down(),
        (f64::from(uniform) / (twice_boards * 1_225.0)).next_down(),
    )
}

const BOARDS_PER_MATCHUP: u64 = 1_712_304; // C(48,5)

// Counts from independently enumerating the 47,008 distinct concrete matchups
// under suit relabeling and player exchange. For each chosen hero holding,
// consider all 1,225 legal opposing hands. Sum 2*wins + ties for RANDOM;
// take the smallest individual 2*wins + ties for MIN. Each matchup has the
// same number of boards. Order is 2..A, suited above the diagonal.
// SHA-256 of the 169 rows "row,col,sum,min\n" in row-major decimal ASCII:
// 2f06c6bac85f62e16630f762d4526c25e5320d5edc7b72815a117bf30f656ce9.
#[rustfmt::skip]
const RANDOM_NUMERATOR: [[u32; 13]; 13] = [
    [2111584984, 1509598969, 1545030496, 1587834115, 1580267589, 1600694981, 1689453378, 1779377877, 1881081228, 1987582097, 2104663674, 2232309054, 2407127850],
    [1355167195, 2252502302, 1621085688, 1665177248, 1658490084, 1679565482, 1714702682, 1814998261, 1916867042, 2023386467, 2140331266, 2267684534, 2442426752],
    [1392777591, 1474421119, 2392189918, 1739031009, 1733992584, 1755639231, 1791395097, 1840073188, 1952021583, 2058576574, 2175405007, 2302489977, 2476546510],
    [1438290523, 1521359660, 1600669542, 2530717766, 1809508009, 1832252004, 1868726945, 1918098843, 1980790433, 2097020718, 2213753197, 2340593683, 2513853498],
    [1429501394, 1513508859, 1594595021, 1675721321, 2654886824, 1903411309, 1939972106, 1989685474, 2053132211, 2122991082, 2249124793, 2376160871, 2513136230],
    [1450834082, 1535517681, 1617221098, 1699535755, 1775500515, 2778697066, 2010998885, 2060559942, 2124380950, 2195100337, 2278058571, 2413791403, 2558365366],
    [1544974255, 1572500215, 1654850021, 1737944702, 1814018446, 1889946808, 2901489486, 2131165248, 2195502632, 2266034488, 2350025012, 2446287516, 2598632551],
    [1640215041, 1678876575, 1706209587, 1790032326, 1866476357, 1942260018, 2017740169, 3022906058, 2266533059, 2335121239, 2419101794, 2516603411, 2633762831],
    [1748047643, 1786903085, 1825060325, 1856391409, 1933626279, 2009815656, 2085879427, 2161828275, 3146852786, 2413376906, 2494750222, 2592119485, 2710163670],
    [1860482433, 1899374667, 1937585709, 1979306579, 2007136533, 2084229054, 2160086896, 2233964817, 2317721143, 3249956564, 2527960911, 2624790516, 2743317574],
    [1984112061, 2022875163, 2060977485, 2102610047, 2140532877, 2171644643, 2248588890, 2322450407, 2403431287, 2438834411, 3352976364, 2659738722, 2777557648],
    [2118914099, 2157382145, 2195213225, 2236598359, 2274745067, 2315189251, 2350127356, 2425293951, 2506133992, 2540944046, 2578159800, 3456618076, 2812619406],
    [2304332573, 2342761931, 2379892121, 2420453161, 2419862398, 2468473350, 2511742479, 2549507227, 2631264233, 2666570709, 2703008959, 2740271595, 3574419148],
];
#[rustfmt::skip]
const MIN_NUMERATOR: [[u32; 13]; 13] = [
    [586110, 539252, 536827, 531396, 460672, 405866, 411177, 404147, 399328, 388205, 378405, 369928, 414616],
    [360014, 601340, 562096, 590059, 519470, 464664, 411201, 417845, 413026, 401903, 392103, 383626, 428314],
    [353290, 411468, 610620, 631490, 578133, 523462, 469999, 417869, 426724, 415601, 405801, 397324, 442012],
    [347766, 411637, 475220, 614816, 621814, 582125, 528797, 476667, 426748, 429299, 419499, 411022, 455710],
    [271173, 335190, 399061, 462644, 605046, 614608, 572902, 520907, 470988, 414765, 418639, 410162, 403006],
    [211871, 275972, 339989, 403860, 451419, 598900, 610348, 565896, 516112, 459889, 404989, 410186, 409774],
    [218070, 218094, 282195, 346212, 394059, 442594, 594934, 607714, 561101, 505013, 450113, 396536, 409798],
    [210635, 225707, 225731, 289832, 337825, 386648, 435183, 592702, 605820, 550002, 495237, 441660, 396148],
    [205582, 220654, 235726, 235750, 283827, 332796, 381619, 430154, 593264, 608720, 555669, 502227, 456715],
    [193598, 208670, 223742, 238814, 222814, 271867, 320836, 369659, 435107, 593930, 542019, 488577, 443065],
    [183018, 198090, 213162, 228234, 227282, 212258, 261311, 310280, 376016, 360992, 595028, 474927, 429415],
    [173842, 188914, 203986, 219058, 218106, 218130, 203106, 252159, 318041, 303017, 287993, 594628, 415765],
    [222373, 237445, 252517, 267589, 210334, 217718, 217742, 202718, 268686, 253662, 238638, 223614, 1712304],
];
