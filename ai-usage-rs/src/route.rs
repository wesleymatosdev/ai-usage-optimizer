//! Session/route decision engine — "what should open on a new session / cron
//! / worker dispatch" given ai-usage's own provider data.
//!
//! Pure and deterministic: takes a snapshot of provider states plus a task
//! hint and returns a ranked list of candidates. Never touches the network
//! or a live ollama-launch endpoint — callers build `ProviderState` from
//! `db::latest()` + `config::Config` (see `main.rs`), or from synthetic
//! fixtures in tests.
//!
//! Priority order (highest to lowest), per the routing incident this exists
//! to prevent (flash:cloud dispatched right after an Anthropic weekly reset,
//! when Sonnet was the better pick):
//!   1. verified headroom       — percent known and below the warning line
//!   2. near-limit avoidance    — percent >= warning is penalized hard
//!   3. near-reset patience     — a near-limit window whose reset lands
//!      inside the task's expected runtime is worse than waiting; don't
//!      burn a nearly-dead window when a fresh one is about to open
//!   4. cost tier               — subscription > metered > local
//!   5. task fitness            — does this provider fit the task class

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskClass {
    Reasoning,
    Extraction,
    Classifier,
}

impl TaskClass {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "reasoning" => Some(TaskClass::Reasoning),
            "extraction" => Some(TaskClass::Extraction),
            "classifier" => Some(TaskClass::Classifier),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            TaskClass::Reasoning => "reasoning",
            TaskClass::Extraction => "extraction",
            TaskClass::Classifier => "classifier",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostTier {
    Subscription,
    Metered,
    Local,
}

impl CostTier {
    /// Lower is better. Used only as a tie-break weight once headroom /
    /// near-limit / patience have already been accounted for.
    fn weight(&self) -> f64 {
        match self {
            CostTier::Subscription => 0.0,
            CostTier::Metered => 6.0,
            CostTier::Local => 3.0,
        }
    }
}

/// A single provider's known state, ready for the decision engine. Built
/// from `db::Observation` + `config::Config` in production, or hand-written
/// in tests — never fetched live by this module.
#[derive(Debug, Clone)]
pub struct ProviderState {
    pub provider: String,
    /// Percent of the tightest window used (0-100). `None` = no verified
    /// observation yet.
    pub percent: Option<f64>,
    /// Seconds until the next window reset, if known from a parsed
    /// `reset_at` / `Xh resets` note.
    pub reset_in_secs: Option<i64>,
    /// Extra resets banked (e.g. Codex "1 reset banked") — small tie-break
    /// bonus since the provider has more slack than the raw percent implies.
    pub banked_resets: u32,
    pub cost_tier: CostTier,
    /// Task classes this provider/model is a good fit for. Empty = fits
    /// everything (neutral, no bonus/penalty).
    pub task_fitness: Vec<TaskClass>,
}

pub struct RouteRequest {
    pub task_class: TaskClass,
    /// How long the caller expects the dispatched work to run, in seconds.
    /// Used only for near-reset patience comparisons.
    pub expected_runtime_secs: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RouteCandidate {
    pub provider: String,
    pub reason: String,
    /// 0.0-1.0. 1.0 = fully verified headroom with a clean task fit.
    pub confidence: f64,
    score: f64,
}

pub struct RouteDecision {
    pub recommended: Option<RouteCandidate>,
    pub ranked: Vec<RouteCandidate>,
}

pub struct Thresholds {
    pub warning: f64,
    pub critical: f64,
}

/// Score one provider. Higher is better. `None` means the provider must be
/// excluded outright (unverified — never dispatch blind).
fn score(
    state: &ProviderState,
    req: &RouteRequest,
    thresholds: &Thresholds,
) -> Option<(f64, String, bool)> {
    let pct = state.percent?;

    let near_limit = pct >= thresholds.warning;
    let critical = pct >= thresholds.critical;

    // Near-reset patience: a near-limit window whose reset lands inside the
    // task's expected runtime is worse than it looks — you'd burn most of
    // the task fighting a window that resets mid-flight anyway. Penalize it
    // below providers with real headroom, but don't exclude it outright (it
    // may still be the only option).
    let patience_penalty = match (near_limit, state.reset_in_secs) {
        (true, Some(reset_secs)) if reset_secs <= req.expected_runtime_secs => 40.0,
        (true, _) => 20.0,
        (false, _) => 0.0,
    };

    if critical && patience_penalty > 0.0 {
        // Critical + no imminent reset relief: essentially unusable.
        // Still scored (not excluded) so it can be a last-resort fallback,
        // but heavily penalized.
    }

    let headroom_score = 100.0 - pct;
    let fitness_bonus = if state.task_fitness.is_empty() {
        0.0
    } else if state.task_fitness.contains(&req.task_class) {
        8.0
    } else {
        -8.0
    };
    let banked_bonus = (state.banked_resets as f64).min(3.0) * 2.0;
    let cost_penalty = state.cost_tier.weight();

    let total = headroom_score - patience_penalty + fitness_bonus + banked_bonus - cost_penalty;

    let mut reason_parts = vec![format!("{:.0}% used", pct)];
    if near_limit {
        reason_parts.push(format!(
            "near-limit (>= {:.0}% warning line)",
            thresholds.warning
        ));
    }
    if let (true, Some(reset_secs)) = (near_limit, state.reset_in_secs) {
        if reset_secs <= req.expected_runtime_secs {
            reason_parts.push(format!(
                "reset lands in {}s (within expected {}s runtime) — waiting beats burning this window",
                reset_secs, req.expected_runtime_secs
            ));
        }
    }
    if state.banked_resets > 0 {
        reason_parts.push(format!("{} reset(s) banked", state.banked_resets));
    }
    reason_parts.push(match state.cost_tier {
        CostTier::Subscription => "subscription tier".to_string(),
        CostTier::Metered => "credits-metered tier".to_string(),
        CostTier::Local => "local/unmetered tier".to_string(),
    });
    if !state.task_fitness.is_empty() {
        if state.task_fitness.contains(&req.task_class) {
            reason_parts.push(format!("fits {} work", req.task_class.as_str()));
        } else {
            reason_parts.push(format!("not tuned for {} work", req.task_class.as_str()));
        }
    }

    Some((total, reason_parts.join(", "), near_limit))
}

/// Decide which provider should open the next session/worker/cron dispatch.
pub fn decide(
    states: &[ProviderState],
    req: &RouteRequest,
    thresholds: &Thresholds,
) -> RouteDecision {
    // `fit_with_headroom`: this provider explicitly fits the requested task
    // class AND has verified headroom (below the warning line) — a
    // genuinely usable, on-task candidate, not just a cheap one.
    // `mismatched`: this provider explicitly does NOT fit the requested
    // task class (task_fitness is non-empty and excludes it). Empty
    // task_fitness is neutral — neither flag applies.
    let mut scored: Vec<(RouteCandidate, bool, bool)> = states
        .iter()
        .filter_map(|s| {
            score(s, req, thresholds).map(|(total, reason, near_limit)| {
                // Confidence: verified + comfortably under warning = high;
                // near-limit or unfit = lower, never negative/over 1.
                let confidence = if !near_limit {
                    (1.0 - s.percent.unwrap_or(0.0) / 100.0).clamp(0.05, 1.0)
                } else {
                    0.2
                };
                let fits = !s.task_fitness.is_empty() && s.task_fitness.contains(&req.task_class);
                let mismatched =
                    !s.task_fitness.is_empty() && !s.task_fitness.contains(&req.task_class);
                let candidate = RouteCandidate {
                    provider: s.provider.clone(),
                    reason,
                    confidence,
                    score: total,
                };
                (candidate, fits && !near_limit, mismatched)
            })
        })
        .collect();

    scored.sort_by(|a, b| {
        b.0.score
            .partial_cmp(&a.0.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Gate: cost tier and raw headroom score alone must never let an
    // explicitly unfit provider outrank a provider that both fits the task
    // class and has verified headroom. Weighting tricks (bigger fitness
    // bonus) only shift *where* the crossover happens; they don't remove
    // it. This gate removes it outright: if the top-scored pick is a known
    // mismatch and a fit-with-headroom alternative exists, promote the
    // best-scoring such alternative (scored list is already sorted, so the
    // first match found after the top is the best one).
    if let Some((_, _, top_mismatched)) = scored.first() {
        if *top_mismatched {
            if let Some(idx) = scored
                .iter()
                .position(|(_, fit_with_headroom, _)| *fit_with_headroom)
            {
                if idx != 0 {
                    let promoted = scored.remove(idx);
                    scored.insert(0, promoted);
                }
            }
        }
    }

    let ranked: Vec<RouteCandidate> = scored.into_iter().map(|(c, _, _)| c).collect();
    let recommended = ranked.first().cloned();
    RouteDecision {
        recommended,
        ranked,
    }
}

/// Build `ProviderState` entries from live observations for the `route` CLI
/// command. Defaults (cost tier, task fitness, banked resets) are hints
/// only — this function never makes a network call.
pub fn states_from_observations(
    order: &[String],
    observations: &HashMap<String, crate::db::Observation>,
    hints: &HashMap<String, (CostTier, Vec<TaskClass>)>,
) -> Vec<ProviderState> {
    order
        .iter()
        .filter_map(|provider| {
            let obs = observations.get(provider)?;
            let (cost_tier, task_fitness) = hints
                .get(provider)
                .cloned()
                .unwrap_or((CostTier::Metered, Vec::new()));
            let banked_resets = if obs.note.to_lowercase().contains("reset banked") {
                1
            } else {
                0
            };
            Some(ProviderState {
                provider: provider.clone(),
                percent: obs.percent,
                reset_in_secs: None,
                banked_resets,
                cost_tier,
                task_fitness,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thresholds() -> Thresholds {
        Thresholds {
            warning: 90.0,
            critical: 95.0,
        }
    }

    fn req(task_class: TaskClass, expected_runtime_secs: i64) -> RouteRequest {
        RouteRequest {
            task_class,
            expected_runtime_secs,
        }
    }

    fn state(provider: &str, percent: f64, cost_tier: CostTier) -> ProviderState {
        ProviderState {
            provider: provider.to_string(),
            percent: Some(percent),
            reset_in_secs: None,
            banked_resets: 0,
            cost_tier,
            task_fitness: Vec::new(),
        }
    }

    #[test]
    fn prefers_verified_headroom_over_near_limit() {
        let states = vec![
            state("flash-cloud", 92.0, CostTier::Subscription),
            state("sonnet", 5.0, CostTier::Subscription),
        ];
        let decision = decide(&states, &req(TaskClass::Reasoning, 600), &thresholds());
        assert_eq!(decision.recommended.unwrap().provider, "sonnet");
    }

    #[test]
    fn excludes_unverified_providers_entirely() {
        let states = vec![ProviderState {
            provider: "mystery".to_string(),
            percent: None,
            reset_in_secs: None,
            banked_resets: 0,
            cost_tier: CostTier::Subscription,
            task_fitness: Vec::new(),
        }];
        let decision = decide(&states, &req(TaskClass::Reasoning, 600), &thresholds());
        assert!(decision.recommended.is_none());
        assert!(decision.ranked.is_empty());
    }

    #[test]
    fn near_reset_patience_beats_burning_a_near_dead_window() {
        // Only candidate is near-limit, but its reset lands well inside the
        // task's expected runtime — a fresh candidate with real headroom
        // should still beat it if one exists.
        let near_dead = ProviderState {
            provider: "chatgpt-plus".to_string(),
            percent: Some(93.0),
            reset_in_secs: Some(120), // resets in 2 minutes
            banked_resets: 0,
            cost_tier: CostTier::Subscription,
            task_fitness: Vec::new(),
        };
        let fresh = state("claude-pro", 10.0, CostTier::Subscription);
        let decision = decide(
            &[near_dead, fresh],
            &req(TaskClass::Reasoning, 1800),
            &thresholds(),
        );
        assert_eq!(decision.recommended.unwrap().provider, "claude-pro");
    }

    #[test]
    fn near_reset_patience_is_noted_when_its_the_only_option() {
        let near_dead = ProviderState {
            provider: "chatgpt-plus".to_string(),
            percent: Some(93.0),
            reset_in_secs: Some(120),
            banked_resets: 0,
            cost_tier: CostTier::Subscription,
            task_fitness: Vec::new(),
        };
        let decision = decide(
            &[near_dead],
            &req(TaskClass::Reasoning, 1800),
            &thresholds(),
        );
        let rec = decision.recommended.unwrap();
        assert_eq!(rec.provider, "chatgpt-plus");
        assert!(rec.reason.contains("waiting beats burning this window"));
    }

    #[test]
    fn cost_tier_breaks_ties_between_equal_headroom() {
        let states = vec![
            state("ollama-pro", 10.0, CostTier::Metered),
            state("claude-pro", 10.0, CostTier::Subscription),
            state("ollama-local", 10.0, CostTier::Local),
        ];
        let decision = decide(&states, &req(TaskClass::Extraction, 600), &thresholds());
        let order: Vec<&str> = decision
            .ranked
            .iter()
            .map(|c| c.provider.as_str())
            .collect();
        assert_eq!(order, vec!["claude-pro", "ollama-local", "ollama-pro"]);
    }

    #[test]
    fn task_fitness_breaks_ties_within_same_cost_tier() {
        let mut reasoning_fit = state("sonnet", 20.0, CostTier::Subscription);
        reasoning_fit.task_fitness = vec![TaskClass::Reasoning];
        let mut classifier_only = state("flash-cloud", 20.0, CostTier::Subscription);
        classifier_only.task_fitness = vec![TaskClass::Classifier];

        let decision = decide(
            &[classifier_only, reasoning_fit],
            &req(TaskClass::Reasoning, 600),
            &thresholds(),
        );
        assert_eq!(decision.recommended.unwrap().provider, "sonnet");
    }

    #[test]
    fn local_is_the_fallback_when_everything_metered_is_near_limit() {
        let states = vec![
            state("claude-pro", 96.0, CostTier::Subscription),
            state("zai-codeplus", 91.0, CostTier::Subscription),
            state("ollama-local", 0.0, CostTier::Local),
        ];
        let decision = decide(&states, &req(TaskClass::Extraction, 600), &thresholds());
        assert_eq!(decision.recommended.unwrap().provider, "ollama-local");
    }

    #[test]
    fn banked_resets_give_a_small_edge_between_equal_percent() {
        let mut with_bank = state("chatgpt-plus", 40.0, CostTier::Subscription);
        with_bank.banked_resets = 2;
        let without_bank = state("claude-pro", 40.0, CostTier::Subscription);

        let decision = decide(
            &[without_bank, with_bank],
            &req(TaskClass::Reasoning, 600),
            &thresholds(),
        );
        assert_eq!(decision.recommended.unwrap().provider, "chatgpt-plus");
    }

    #[test]
    fn fit_with_headroom_beats_cheap_unfit_provider() {
        // Reproduces the live defect found in review: an unfit, cheap
        // local/classifier-only provider (0% used, so a high raw headroom
        // score) must not outrank a subscription provider that both fits
        // the task class and has clear, verified headroom (well under the
        // warning line). This is the exact incident class the task exists
        // to prevent: don't let cost tier alone win over task fitness when
        // a fit, well-provisioned alternative exists.
        let mut classifier_only = state("ollama-local", 0.0, CostTier::Local);
        classifier_only.task_fitness = vec![TaskClass::Classifier];

        let mut reasoning_fit = state("claude-pro", 32.0, CostTier::Subscription);
        reasoning_fit.task_fitness = vec![TaskClass::Reasoning];

        let decision = decide(
            &[classifier_only, reasoning_fit],
            &req(TaskClass::Reasoning, 600),
            &thresholds(),
        );
        assert_eq!(decision.recommended.unwrap().provider, "claude-pro");
    }

    #[test]
    fn mismatch_gate_does_not_fire_without_a_fit_alternative() {
        // If the only candidates are unfit, the gate must not exclude them
        // (no fit-with-headroom alternative exists to promote) — an unfit
        // provider is still better than no provider.
        let mut classifier_only = state("ollama-local", 0.0, CostTier::Local);
        classifier_only.task_fitness = vec![TaskClass::Classifier];

        let decision = decide(
            &[classifier_only],
            &req(TaskClass::Reasoning, 600),
            &thresholds(),
        );
        assert_eq!(decision.recommended.unwrap().provider, "ollama-local");
    }

    #[test]
    fn no_provider_available_returns_none() {
        let decision = decide(&[], &req(TaskClass::Reasoning, 600), &thresholds());
        assert!(decision.recommended.is_none());
    }
}
