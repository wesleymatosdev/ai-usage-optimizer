//! Burn-rate awareness + spend guardrails for credit-pool providers.
//!
//! A raw "8% of the pool" reading looks harmless and hides velocity: $5 in
//! one morning projects far past $60 if sustained. So spend-rate is a first
//! class signal — dollars/hour from the delta between real credit readings,
//! projected to the period's reset — and the daily soft cap warns (Telegram)
//! BEFORE the hard monthly ceiling refuses.
//!
//! `budget check` against a credit provider refuses a dispatch whose
//! estimated dollar cost would push cumulative consumption past the pool:
//! the ceiling actually refuses, it does not merely report.

use crate::config::Config;
use crate::credits::{self, BurnProjection};
use rusqlite::Connection;

/// What `budget check` decides for a credit-modeled provider.
#[derive(Debug, PartialEq)]
pub struct CreditDecision {
    pub allowed: bool,
    /// "pool" (hard refusal) when the dispatch would cross the monthly pool.
    pub breached: Option<&'static str>,
    /// Cause → consequence → action (non-happy-path rule).
    pub message: String,
    /// Set when the estimate would cross the trailing-24h soft cap: the
    /// dispatch is allowed, but this text should surface to the operator.
    pub soft_cap_warning: Option<String>,
    /// Burn projection when >= 2 readings exist (echoed for the CLI/JSON).
    pub burn: Option<BurnProjection>,
}

/// Dollars consumed inside the trailing 24h window (cumulative-reading
/// delta, clamped at 0 — dashboard corrections are never negative spend).
fn dollars_in_last_24h(conn: &Connection, cfg: &Config, provider: &str, now: i64) -> f64 {
    let state_now = credits::credit_state(conn, cfg, provider, now);
    // The reading closest to `now - 24h`: newest reading at-or-before it.
    let baseline: Option<f64> = baseline_dollar(conn, provider, now);
    let current = state_now.used_dollars.unwrap_or(0.0);
    match baseline {
        Some(base) if current > base => current - base,
        Some(_) => 0.0,
        // Fewer than two readings in the window: fall back to the oldest
        // known reading — the pool is small enough that any recorded use
        // counts against a soft cap.
        None => current,
    }
}

fn baseline_dollar(conn: &Connection, provider: &str, now: i64) -> Option<f64> {
    conn.query_row(
        "SELECT used_dollars FROM credit_events
         WHERE provider = ?1 AND at_unix <= ?2
         ORDER BY at_unix DESC, id DESC LIMIT 1",
        rusqlite::params![provider, now - credits::DAY_SECS],
        |row| row.get::<_, f64>(0),
    )
    .ok()
}

/// Evaluate a dispatch estimated at `estimated_dollars` against the
/// provider's monthly credit pool and daily soft cap.
pub fn check_credits(
    conn: &Connection,
    cfg: &Config,
    provider: &str,
    estimated_dollars: f64,
    now: i64,
) -> Result<CreditDecision, String> {
    let state = credits::credit_state(conn, cfg, provider, now);
    if state.pool_dollars <= 0.0 {
        return Err(format!(
            "{provider} has no monthly_credit_dollars configured — a credit check \
             needs a pool; set it in the provider config"
        ));
    }
    let Some(used) = state.used_dollars else {
        return Err(format!(
            "{provider} has no recorded credit reading — record the dashboard \
             balance first: ai-usage credit record {provider} <dollars-used>"
        ));
    };

    let projected = used + estimated_dollars;
    let mut soft_cap_warning = None;

    if let Some(cap) = cfg
        .providers
        .get(provider)
        .and_then(|p| p.daily_credit_soft_cap)
    {
        let today = dollars_in_last_24h(conn, cfg, provider, now);
        if today + estimated_dollars > cap {
            soft_cap_warning = Some(format!(
                "daily soft cap: ${today:.2} consumed in the last 24h + \
                 ${estimated_dollars:.2} estimated = ${:.2} crosses the ${cap:.2} soft cap — \
                 allowed, but if this rate sustains the month's pool burns early; consider \
                 local or subscription routes",
                today + estimated_dollars
            ));
        }
    }

    if projected > state.pool_dollars {
        return Ok(CreditDecision {
            allowed: false,
            breached: Some("monthly-pool"),
            message: format!(
                "monthly credit breach: ${used:.2} recorded + ${estimated_dollars:.2} estimated \
                 = ${projected:.2} > ${:.2} pool — refusing the dispatch keeps the plan inside \
                 its $ credit ceiling; route to a subscription-seat or local provider, wait \
                 for the reset, or shrink the task",
                state.pool_dollars
            ),
            soft_cap_warning,
            burn: state.burn.clone(),
        });
    }

    if let Some(burn) = &state.burn {
        if burn.projected_overrun > 0.0 {
            return Ok(CreditDecision {
                allowed: false,
                breached: Some("burn-projection"),
                message: format!(
                    "burn-rate projection overruns the pool: ${:.2}/h projects ${:.2} by reset \
                     (+${:.2} past ${:.2}) — refusing speculative dispatches is the only lever \
                     that changes the trajectory; local or subscription routes instead",
                    burn.dollars_per_hour,
                    burn.projected_at_reset,
                    burn.projected_overrun,
                    state.pool_dollars
                ),
                soft_cap_warning,
                burn: state.burn.clone(),
            });
        }
    }

    Ok(CreditDecision {
        allowed: true,
        breached: None,
        message: format!(
            "within budget: ${used:.2} of ${:.2} pool, ${:.2} remaining, \
             +${estimated_dollars:.2} estimated",
            state.pool_dollars, state.remaining_dollars
        ),
        soft_cap_warning,
        burn: state.burn.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn mem_db() -> Connection {
        let conn = Connection::open_in_memory().expect("memory db");
        conn.execute_batch(
            "CREATE TABLE credit_events (id INTEGER PRIMARY KEY, provider TEXT NOT NULL,
             used_dollars REAL NOT NULL, at_unix INTEGER NOT NULL);",
        )
        .expect("schema");
        conn
    }

    fn cfg() -> Config {
        Config::default_config()
    }

    #[test]
    fn dispatch_crossing_the_pool_is_refused() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE credit_events (id INTEGER PRIMARY KEY, provider TEXT NOT NULL,
             used_dollars REAL NOT NULL, at_unix INTEGER NOT NULL);",
        )
        .unwrap();
        let cfg = cfg();
        credits::record_credit(&conn, "ollama-pro", 55.0, 0).unwrap();
        let d = check_credits(&conn, &cfg, "ollama-pro", 10.0, 1_000).unwrap();
        assert!(!d.allowed);
        assert_eq!(d.breached, Some("monthly-pool"));
        assert!(d.message.contains("refusing the dispatch"));
        assert!(d.message.contains("$65.00 > $60"));
    }

    #[test]
    fn exact_fit_is_allowed() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE credit_events (id INTEGER PRIMARY KEY, provider TEXT NOT NULL,
             used_dollars REAL NOT NULL, at_unix INTEGER NOT NULL);",
        )
        .unwrap();
        let cfg = cfg();
        credits::record_credit(&conn, "ollama-pro", 55.0, 0).unwrap();
        let d = check_credits(&conn, &cfg, "ollama-pro", 5.0, 1_000).unwrap();
        assert!(d.allowed, "$55 + $5 = $60 of $60 is exactly at the ceiling");
    }

    #[test]
    fn unconfigured_pool_or_no_readings_is_an_error_not_a_pass() {
        let conn = Connection::open_in_memory().unwrap();
        let mut no_pool = cfg();
        if let Some(p) = no_pool.providers.get_mut("ollama-pro") {
            p.monthly_credit_dollars = None;
        }
        let e = check_credits(&conn, &no_pool, "ollama-pro", 1.0, 0).unwrap_err();
        assert!(e.contains("no monthly_credit_dollars"));
        let cfg = cfg();
        let e = check_credits(&conn, &cfg, "ollama-pro", 1.0, 0).unwrap_err();
        assert!(e.contains("no recorded credit reading"));
    }

    #[test]
    fn soft_cap_crossing_warns_but_allows() {
        let mut soft_cap_cfg = cfg();
        if let Some(p) = soft_cap_cfg.providers.get_mut("ollama-pro") {
            p.daily_credit_soft_cap = Some(8.0);
        }
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE credit_events (id INTEGER PRIMARY KEY, provider TEXT NOT NULL,
             used_dollars REAL NOT NULL, at_unix INTEGER NOT NULL);",
        )
        .unwrap();
        credits::record_credit(&conn, "ollama-pro", 6.0, 0).unwrap();
        let d = check_credits(&conn, &soft_cap_cfg, "ollama-pro", 5.0, 3600).unwrap();
        assert!(d.allowed, "soft cap warns, never refuses");
        let warning = d.soft_cap_warning.expect("crosses the $8 soft cap");
        assert!(warning.contains("soft cap"));
    }

    #[test]
    fn burn_projection_overrun_refuses_speculative_dispatch() {
        let conn = mem_db();
        let cfg = cfg();
        // $0.60/h sustained burn projects past $60 before the reset.
        credits::record_credit(&conn, "ollama-pro", 5.05, 0).unwrap();
        credits::record_credit(&conn, "ollama-pro", 6.15, 2 * 3600).unwrap();
        let d = check_credits(&conn, &cfg, "ollama-pro", 0.5, 2 * 3600).unwrap();
        assert!(!d.allowed, "projected overrun must refuse");
        assert_eq!(d.breached, Some("burn-projection"));
    }
}
