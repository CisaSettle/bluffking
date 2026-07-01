//! Chinese (zh-CN) reasoning sentence library for the solver (ADR-043 §3.4.3).
//!
//! Templates are selected deterministically from the seed (`seed % 3`).
//! All output ≤ 280 Unicode chars; templates use `chars().count()` for the
//! length check (NOT `len()`, which counts bytes).
//!
//! Per OQ-4 recommendation: where `verdict × HandStrength` is rare/nonsensical
//! (e.g. `Mistake × Set`), we fall through to a band-aware generic template.

use super::advisor::{SolverAction, SolverVerdict};
use super::hand_strength::HandStrength;

/// Hard upper bound on the rendered string (Unicode chars). ADR-029 §4.3.
pub const MAX_REASONING_CHARS: usize = 280;

/// Substitution context for [`render`].
#[derive(Debug, Clone, Copy)]
pub struct RenderContext<'a> {
    pub verdict: SolverVerdict,
    pub hs: HandStrength,
    pub equity_pct: u8,
    pub pot_odds_pct: u8,
    pub recommended: SolverAction,
    pub hero_action: Option<SolverAction>,
    /// Hand label (e.g. "AKs", "AA"); already 169-grid notation, English.
    pub hand_label: &'a str,
    /// Position label (e.g. "BTN", "SB"). English.
    pub position: &'a str,
}

/// Render a Chinese reasoning sentence for the solver output.
///
/// Truncates at `MAX_REASONING_CHARS` Unicode chars if the template + subst
/// happens to exceed (shouldn't, but it's the last line of defense alongside
/// `server::coach::validate_solver_output`).
pub fn render(ctx: &RenderContext<'_>, seed: u64) -> String {
    let raw = pick_template(ctx, seed);
    truncate_chars(&raw, MAX_REASONING_CHARS)
}

/// Render a chat-mode reply that references a specific hand via `[hand:UUID]`
/// fence. ADR-043 §4.3 chat path.
pub fn render_chat_reply(ctx: &RenderContext<'_>, hand_id: &str, seed: u64) -> String {
    let body = pick_template(ctx, seed);
    let combined = format!("关于手牌 [hand:{hand_id}]：{body}");
    truncate_chars(&combined, MAX_REASONING_CHARS)
}

// ---------------------------------------------------------------------------
// Template selection
// ---------------------------------------------------------------------------

fn pick_template(ctx: &RenderContext<'_>, seed: u64) -> String {
    let action_zh = action_zh(ctx.recommended);
    let hero_zh = ctx
        .hero_action
        .map(crate::solver::templates_zh::action_zh)
        .unwrap_or("无操作");
    let band_zh = strength_zh(ctx.hs);
    let eq = ctx.equity_pct;
    let po = ctx.pot_odds_pct;
    let label = ctx.hand_label;
    let pos = ctx.position;

    // 3-way variant selection (deterministic from seed).
    let variant = (seed % 3) as usize;

    match (ctx.verdict, ctx.hs) {
        // --- Good × strong made hands ---
        (SolverVerdict::Good, HandStrength::FullHousePlus)
        | (SolverVerdict::Good, HandStrength::Flush)
        | (SolverVerdict::Good, HandStrength::Straight)
        | (SolverVerdict::Good, HandStrength::Set)
        | (SolverVerdict::Good, HandStrength::TwoPair)
        | (SolverVerdict::Good, HandStrength::Overpair) => match variant {
            0 => format!("{label} 在这块面非常强（胜率 {eq}%），{action_zh} 是教科书价值打法。"),
            1 => format!("怪兽牌 {label}：胜率 {eq}%，{pos} 位置上 {action_zh} 最大化价值。"),
            _ => format!("{label} 已经是 {band_zh}，对方赔率 {po}%，{action_zh} 让对手付出最大成本。"),
        },
        (SolverVerdict::Good, HandStrength::PairTopStrongKicker) => match variant {
            0 => format!("顶对强踢脚 {label}（胜率 {eq}%）：{action_zh} 既施压又控制底池。"),
            1 => format!("{pos} 上拿 {label} 命中顶对：胜率 {eq}%，{action_zh} 是标准价值线。"),
            _ => format!("顶对强踢脚是清晰的价值范围（胜率 {eq}%），{action_zh} 是求解器推荐。"),
        },
        (SolverVerdict::Good, HandStrength::DrawStrong) => match variant {
            0 => format!("强听牌 + 位置优势：胜率 {eq}%，{action_zh} 既施压又保留主动权。"),
            1 => format!("{label} 在这块面有 ≥8 张牌可改进（胜率 {eq}%），{action_zh} 是半诈唬最佳选择。"),
            _ => format!("强听牌（胜率 {eq}%）应该主动出击，{action_zh} 在 {pos} 上 +EV。"),
        },
        (SolverVerdict::Good, _) => match variant {
            0 => format!("{action_zh} 是当前 {band_zh} 的最佳处理：胜率 {eq}% 高于对方所需赔率 {po}%。"),
            1 => format!("{label} 在 {pos} 上 {band_zh}：胜率 {eq}%，{action_zh} 长期 +EV。"),
            _ => format!("教练评价：{action_zh} 正确。{band_zh}（胜率 {eq}%）应按此线行进。"),
        },

        // --- Ok × marginal pair / draw spots ---
        (SolverVerdict::Ok, HandStrength::PairTopWeakKicker) => match variant {
            0 => format!("顶对副牌弱（胜率 {eq}%）：{hero_zh} 可以接受，但要做好被压制时弃牌的准备。"),
            1 => format!("{label} 顶对踢脚弱，建议 {action_zh}；目前 {hero_zh} 不算大错但不是求解器首选。"),
            _ => format!("顶对弱踢脚胜率 {eq}%、对方赔率 {po}%：{action_zh} 更平衡，{hero_zh} 偏被动。"),
        },
        (SolverVerdict::Ok, HandStrength::DrawStrong) => match variant {
            0 => format!("强听牌 {label}（胜率 {eq}%）：{hero_zh} 可行，{action_zh} 更主动。"),
            1 => format!("听牌价值清晰：胜率 {eq}%、底池赔率 {po}%；{action_zh} 是教科书选项，但 {hero_zh} 也可接受。"),
            _ => format!("{band_zh}（胜率 {eq}%）：{hero_zh} 是合理偏被动的选择；求解器建议 {action_zh}。"),
        },
        (SolverVerdict::Ok, HandStrength::PairMiddle) => match variant {
            0 => format!("中对 {label}（胜率 {eq}%）：{hero_zh} OK，{action_zh} 是更标准的处理。"),
            1 => format!("{band_zh}：对方赔率 {po}%，胜率 {eq}%。{hero_zh} 可行，{action_zh} 平衡更佳。"),
            _ => format!("中对在多人池容易被压制；{hero_zh} 不算亏 EV，{action_zh} 长期更稳。"),
        },
        (SolverVerdict::Ok, _) => match variant {
            0 => format!("{band_zh}（胜率 {eq}%）：{hero_zh} 可以接受，{action_zh} 在 {pos} 上更标准。"),
            1 => format!("近似 +EV：胜率 {eq}%、赔率 {po}%。{action_zh} 是求解器策略，{hero_zh} 偏差不大。"),
            _ => format!("{label} 在这块面是 {band_zh}：{hero_zh} 可行，{action_zh} 更平衡。"),
        },

        // --- Mistake × specific bands ---
        (SolverVerdict::Mistake, HandStrength::PureBluffNoEquity) => match variant {
            0 => format!("这手牌没胜率（{eq}%）也没听牌——按底池赔率 {po}% 应直接弃牌，{hero_zh} 长期亏 EV。"),
            1 => format!("{label} 在这块面是 {band_zh}：胜率 {eq}%，{action_zh} 才是 +EV，{hero_zh} 是纯亏损线。"),
            _ => format!("纯诈唬区间应丢弃：胜率 {eq}% 远低于赔率 {po}%。{hero_zh} 错误，正确是 {action_zh}。"),
        },
        (SolverVerdict::Mistake, HandStrength::Overpair) => match variant {
            0 => format!("超对在这块面胜率 {eq}%，对手赔率 {po}%；{action_zh} 才是 +EV，{hero_zh} 把价值打成了赔付。"),
            1 => format!("{label} 是超对但被动 {hero_zh} 是错失价值：{action_zh} 才能最大化 EV（胜率 {eq}%）。"),
            _ => format!("超对清晰价值范围（胜率 {eq}%）：应 {action_zh}；{hero_zh} 错过了一条街价值。"),
        },
        (SolverVerdict::Mistake, HandStrength::PairWeak) => match variant {
            0 => format!("{label} 弱对（胜率 {eq}%）赔率 {po}%：{action_zh} 才是正确，{hero_zh} 长期亏。"),
            1 => format!("弱对不适合继续投入：胜率 {eq}% < 赔率 {po}%。{hero_zh} 错误，{action_zh} 更稳。"),
            _ => format!("{band_zh}（胜率 {eq}%）：{hero_zh} 在 {pos} 上是 -EV，{action_zh} 才是求解器策略。"),
        },
        (SolverVerdict::Mistake, HandStrength::DrawStrong) => match variant {
            0 => format!("强听牌（{eq}%）应当 {action_zh}：{hero_zh} 浪费了主动权。"),
            1 => format!("{band_zh} 价值清晰：胜率 {eq}% 高于赔率 {po}%；{action_zh} 才是 +EV，{hero_zh} 偏弱。"),
            _ => format!("{label} 在 {pos} 上拿到强听牌：{action_zh} 才能施压，{hero_zh} 漏掉机会。"),
        },
        (SolverVerdict::Mistake, _) => match variant {
            0 => format!("{label} 在 {pos} 上 {band_zh}：胜率 {eq}%、赔率 {po}%；{action_zh} 才是 +EV，{hero_zh} 长期亏。"),
            1 => format!("{band_zh}（胜率 {eq}%）：{hero_zh} 是 -EV 选择，正确应该 {action_zh}。"),
            _ => format!("教练评价：本手 {band_zh} 的求解器策略是 {action_zh}；{hero_zh} 与求解器偏差较大（胜率 {eq}% vs 赔率 {po}%）。"),
        },
    }
}

// ---------------------------------------------------------------------------
// Localized labels
// ---------------------------------------------------------------------------

/// Map a solver action to its Chinese verb form.
pub fn action_zh(action: SolverAction) -> &'static str {
    match action {
        SolverAction::Fold => "弃牌",
        SolverAction::Check => "过牌",
        SolverAction::Call => "跟注",
        SolverAction::Raise => "加注",
        SolverAction::AllIn => "全下",
    }
}

/// Map a hand-strength band to a Chinese label.
pub fn strength_zh(hs: HandStrength) -> &'static str {
    match hs {
        HandStrength::PureBluffNoEquity => "无对无听",
        HandStrength::DrawWeak => "弱听牌",
        HandStrength::DrawStrong => "强听牌",
        HandStrength::PairWeak => "弱对",
        HandStrength::PairMiddle => "中对",
        HandStrength::PairTopWeakKicker => "顶对弱踢脚",
        HandStrength::PairTopStrongKicker => "顶对强踢脚",
        HandStrength::Overpair => "超对",
        HandStrength::TwoPair => "两对",
        HandStrength::Set => "暗三条",
        HandStrength::Straight => "顺子",
        HandStrength::Flush => "同花",
        HandStrength::FullHousePlus => "葫芦或更强",
    }
}

/// Truncate to `max` Unicode scalar values (NOT bytes).
fn truncate_chars(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        chars[..max].iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_for(verdict: SolverVerdict, hs: HandStrength) -> RenderContext<'static> {
        RenderContext {
            verdict,
            hs,
            equity_pct: 65,
            pot_odds_pct: 30,
            recommended: SolverAction::Raise,
            hero_action: Some(SolverAction::Call),
            hand_label: "AKs",
            position: "BTN",
        }
    }

    #[test]
    fn all_combinations_under_280_chars() {
        // Ensure every (verdict, hand_strength) pair * 3 variants renders ≤ 280 chars.
        for &verdict in &[
            SolverVerdict::Good,
            SolverVerdict::Ok,
            SolverVerdict::Mistake,
        ] {
            for &hs in HandStrength::all().iter() {
                for seed in 0..3u64 {
                    let ctx = ctx_for(verdict, hs);
                    let rendered = render(&ctx, seed);
                    let len = rendered.chars().count();
                    assert!(
                        len <= MAX_REASONING_CHARS,
                        "verdict={:?} hs={:?} seed={} produced {} chars: {}",
                        verdict,
                        hs,
                        seed,
                        len,
                        rendered
                    );
                    assert!(!rendered.is_empty(), "rendered must not be empty");
                }
            }
        }
    }

    #[test]
    fn deterministic_same_seed_same_output() {
        let ctx = ctx_for(SolverVerdict::Good, HandStrength::Overpair);
        assert_eq!(render(&ctx, 42), render(&ctx, 42));
    }

    #[test]
    fn variant_changes_with_seed() {
        let ctx = ctx_for(SolverVerdict::Good, HandStrength::Overpair);
        // At least two of the three variants are distinct.
        let r0 = render(&ctx, 0);
        let r1 = render(&ctx, 1);
        let r2 = render(&ctx, 2);
        assert!(r0 != r1 || r1 != r2 || r0 != r2, "variants must differ");
    }

    #[test]
    fn chat_reply_includes_hand_fence() {
        let ctx = ctx_for(SolverVerdict::Good, HandStrength::Overpair);
        let reply = render_chat_reply(&ctx, "abc-123", 0);
        assert!(reply.contains("[hand:abc-123]"), "must include hand fence");
    }

    #[test]
    fn action_zh_all_variants() {
        // Each action maps to a non-empty Chinese verb.
        for a in [
            SolverAction::Fold,
            SolverAction::Check,
            SolverAction::Call,
            SolverAction::Raise,
            SolverAction::AllIn,
        ] {
            assert!(!action_zh(a).is_empty());
        }
    }

    #[test]
    fn strength_zh_all_variants() {
        for hs in HandStrength::all() {
            assert!(!strength_zh(hs).is_empty());
        }
    }

    #[test]
    fn truncate_chars_unicode_safe() {
        let s = "太".repeat(300);
        let truncated = truncate_chars(&s, 280);
        assert_eq!(truncated.chars().count(), 280);
    }
}
