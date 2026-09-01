//! Credit-balance provider model — the correction to percent-of-a-limit.
//!
//! Ollama Pro (and similar) is a MONTHLY CREDIT POOL in dollars, not a rate
//! limit: the durable quantity is `used / remaining` dollars with a reset
//! date. A percentage of a rate limit and dollars of a monthly pool are
//! different quantities; conflating them is what produced the sticky
//! `limit-hit → 100%` bug. Transient 429s are modeled separately
//! (`db::rate_limit_events`) and NEVER touch the balance figure.
//!
//! Pure and deterministic: everything here takes recorded readings plus an
//! explicit `now`, never the clock or the network.

use crate::db::CreditObservation;

#[derive(Debug, Clone)]
pub struct CreditPlan {
    /// Full monthly dollar pool (Ollama Pro: $60).
    pub monthly_pool_dollars: f64,
    /// When the pool resets (unix secs). `None` = reset date unknown.
    pub reset_at_unix: Option<i64>,
}

#[derive(Debug, PartialEq)]
pub struct CreditState {
    pub used_dollars: f64,
    pub remaining_dollars: f64,
    pub pool_dollars: f64,
    pub percent_used: f64,
    /// Dollars/hour derived from the earliest and latest readings inside the
    /// current period. `None` with fewer than two observations (a single
    /// point has no rate).
    pub burn_per_hour: Option<f64>,
    /// Burn-rate projection of spend at the reset date:
    /// used + rate * hours_until_reset. `None` when the rate or the reset
    /// date is unknown.
    pub projected_at_reset: Option<f64>,
    pub observed_at_unix: i64,
    /// True when the configured reset date has already passed — the reading
    /// belongs to a previous period and the pool is likely fresh.
    pub period_expired: bool,
}

impl CreditState {
    #[allow(dead_code)] // semantic helper; used by callers that need the gate
    pub fn is_exhausted(&self, critical_percent: f64) -> bool {
        self.percent_used >= critical_percent
    }
}

/// Derive the credit state from the latest reading plus an earlier baseline
/// inside the same period (for burn rate). `baseline.used` may be HIGHER than
/// the latest reading (a correction / manual reset) — burn stays None then,
/// because a negative rate is meaningless.
pub fn credit_state(
    plan: &CreditPlan,
    latest: &CreditObservation,
    baseline: Option<&CreditObservation>,
    now: i64,
) -> CreditState {
    let period_expired = plan.reset_at_unix.is_some_and(|r| r <= now);
    let percent_used = if plan.monthly_pool_dollars > 0.0 {
        (latest.used_dollars / plan.monthly_pool_dollars * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };

    let burn_per_hour = baseline.and_then(|base| {
        let elapsed_hours = (latest.at_unix - base.at_unix) as f64 / 3600.0;
        if elapsed_hours <= 0.0 || latest.used_dollars < base.used_dollars {
            return None;
        }
        Some((latest.used_dollars - base.used_dollars) / elapsed_hours)
    });

    let projected_at_reset = match (burn_per_hour, plan.reset_at_unix) {
        (Some(rate), Some(reset)) if reset > now => {
            let hours_left = (reset - now) as f64 / 3600.0;
            Some(latest.used_dollars + rate * hours_left)
        }
        _ => None,
    };

    CreditState {
        used_dollars: latest.used_dollars,
        remaining_dollars: (plan.monthly_pool_dollars - latest.used_dollars).max(0.0),
        pool_dollars: plan.monthly_pool_dollars,
        percent_used,
        burn_per_hour,
        projected_at_reset,
        observed_at_unix: latest.at_unix,
        period_expired,
    }
}

#[derive(Debug, PartialEq)]
pub struct CreditBudgetDecision {
    pub allowed: bool,
    pub projected_dollars: f64,
    pub pool_dollars: f64,
    pub message: String,
}

/// The dollar-form budget gate for credit providers: a dispatch is refused
/// when recorded spend + estimate would cross the monthly pool. Dollars in,
/// dollars out — no tokens, no percentages.
pub fn credit_budget_check(
    plan: &CreditPlan,
    used_dollars: f64,
    estimate_dollars: f64,
) -> CreditBudgetDecision {
    let projected = used_dollars + estimate_dollars;
    if projected > plan.monthly_pool_dollars {
        CreditBudgetDecision {
            allowed: false,
            projected_dollars: projected,
            pool_dollars: plan.monthly_pool_dollars,
            message: format!(
                "credit budget breach: ${used:.2} recorded + ${est:.2} estimated = \
                 ${projected:.2} > ${pool:.2} monthly pool — refusing the dispatch keeps \
                 the credit balance from going negative; route to another provider, wait \
                 for the reset date, or shrink the task",
                used = used_dollars,
                est = estimate_dollars,
                projected = projected,
                pool = plan.monthly_pool_dollars,
            ),
        }
    } else {
        CreditBudgetDecision {
            allowed: true,
            projected_dollars: projected,
            pool_dollars: plan.monthly_pool_dollars,
            message: format!(
                "within credit budget: ${used:.2} + ${est:.2} = ${projected:.2} of ${pool:.2} \
                 after this dispatch",
                used = used_dollars,
                est = estimate_dollars,
                projected = projected,
                pool = plan.monthly_pool_dollars,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(reset_in_secs: Option<i64>, now: i64) -> CreditPlan {
        CreditPlan {
            monthly_pool_dollars: 60.0,
            reset_at_unix: reset_in_secs.map(|s| now + s),
        }
    }

    fn obs(used: f64, at: i64) -> CreditObservation {
        CreditObservation {
            used_dollars: used,
            note: "test".into(),
            at_unix: at,
        }
    }

    #[test]
    fn percent_is_dollars_over_pool_not_percent_of_a_limit() {
        let now = 1_000_000;
        let st = credit_state(&plan(None, now), &obs(5.10, now), None, now);
        assert!((st.percent_used - 8.5).abs() < 0.01, "5.10/60 = 8.5%");
        assert!((st.remaining_dollars - 54.90).abs() < 1e-9);
        assert!(!st.period_expired);
    }

    #[test]
    fn burn_rate_comes_from_two_real_observations() {
        let now = 1_000_000;
        let base = obs(2.00, now - 3600); // an hour ago
        let latest = obs(5.00, now);
        let state = credit_state(&plan(Some(3 * 86_400), now), &latest, Some(&base), now);
        let burn = state.burn_per_hour.expect("two points → burn rate");
        assert!((burn - 3.0).abs() < 1e-9, "$3 in 1h = $3/h, got {burn}");
    }

    #[test]
    fn single_observation_has_no_burn_rate_and_no_projection() {
        let now = 1_000_000;
        let state = credit_state(&plan(Some(86_400), now), &obs(5.0, now), None, now);
        assert_eq!(state.burn_per_hour, None);
        assert_eq!(state.projected_at_reset, None);
    }

    #[test]
    fn projection_extrapolates_burn_to_the_reset_date() {
        let now = 1_000_000;
        // $2 spent in the last hour (burn $2/h); reset lands 2 hours from NOW
        // → 5 + 2/h × 2h = 9 at the reset date.
        let base = obs(3.0, now - 3600);
        let latest = obs(5.0, now);
        let state = credit_state(&plan(Some(2 * 3600), now), &latest, Some(&base), now);
        let projected = state
            .projected_at_reset
            .expect("burn + reset date → projection");
        assert!((projected - 9.0).abs() < 1e-9, "got {projected}");
    }

    #[test]
    fn expired_reset_date_flags_period_expired_and_suppresses_projection() {
        let now = 1_000_000;
        let plan = CreditPlan {
            monthly_pool_dollars: 60.0,
            reset_at_unix: Some(now - 60),
        };
        let state = credit_state(&plan, &obs(5.0, now), None, now);
        assert!(state.period_expired);
        assert_eq!(state.projected_at_reset, None);
    }

    #[test]
    fn dollar_budget_refuses_a_dispatch_crossing_the_pool() {
        let p = CreditPlan {
            monthly_pool_dollars: 60.0,
            reset_at_unix: None,
        };
        let d = credit_budget_check(&p, 55.0, 10.0);
        assert!(!d.allowed);
        assert_eq!(d.projected_dollars, 65.0);
        assert!(d.message.contains("credit budget breach"));
    }

    #[test]
    fn dollar_budget_allows_a_fitting_dispatch() {
        let p = CreditPlan {
            monthly_pool_dollars: 60.0,
            reset_at_unix: None,
        };
        let d = credit_budget_check(&p, 55.0, 5.0);
        assert!(d.allowed, "exactly at the pool is allowed");
    }
}
