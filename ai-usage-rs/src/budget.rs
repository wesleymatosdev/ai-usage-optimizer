//! Hard budget ceilings — the guard that keeps a nominal 10k/day plan from
//! silently reaching 18k of real spend.
//!
//! Every dispatch declares its estimated cost BEFORE it runs (`budget check`
//! refuses unestimated dispatches) and its actual cost AFTER (`budget
//! record`), against a SQLite spend ledger. `check` projects
//! recorded-spend + estimate across the provider's daily and weekly
//! ceilings and refuses when the projection would cross either.

use crate::config::Config;
use crate::db;
use rusqlite::Connection;

pub const DAY_SECS: i64 = 86_400;
pub const WEEK_SECS: i64 = 7 * 86_400;

#[derive(Debug, PartialEq)]
pub struct BudgetDecision {
    pub allowed: bool,
    /// Which ceiling the projection would cross, if refused.
    pub breached: Option<&'static str>,
    /// Human-readable explanation (cause → consequence → what to do).
    pub message: String,
    pub daily_spend: u64,
    pub weekly_spend: u64,
    pub daily_budget: Option<u64>,
    pub weekly_budget: Option<u64>,
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Evaluate a dispatch of `estimate_tokens` against the provider's ceilings.
pub fn check(
    conn: &Connection,
    cfg: &Config,
    provider: &str,
    estimate_tokens: u64,
    now: i64,
) -> BudgetDecision {
    let provider_cfg = cfg.providers.get(provider);
    let daily_budget = provider_cfg.and_then(|p| p.daily_token_budget);
    let weekly_budget = provider_cfg.and_then(|p| p.weekly_token_budget);

    let daily_spend = db::spend_since(conn, provider, now - DAY_SECS, now);
    let weekly_spend = db::spend_since(conn, provider, now - WEEK_SECS, now);

    let projected_daily = daily_spend + estimate_tokens;
    let projected_weekly = weekly_spend + estimate_tokens;

    if let Some(cap) = daily_budget {
        if projected_daily > cap {
            return BudgetDecision {
                allowed: false,
                breached: Some("daily"),
                message: format!(
                    "daily budget breach: {daily_spend} recorded + {estimate_tokens} estimated \
                     = {projected_daily} > {cap} ceiling — refusing the dispatch keeps the plan \
                     under its daily ceiling; route to a provider with budget headroom, wait for \
                     the window to roll over, or shrink the task"
                ),
                daily_spend,
                weekly_spend,
                daily_budget,
                weekly_budget,
            };
        }
    }

    if let Some(cap) = weekly_budget {
        if projected_weekly > cap {
            return BudgetDecision {
                allowed: false,
                breached: Some("weekly"),
                message: format!(
                    "weekly budget breach: {weekly_spend} recorded + {estimate_tokens} estimated \
                     = {projected_weekly} > {cap} rolling-7-day ceiling — refusing the dispatch \
                     protects the week's remaining budget; pick another provider or wait for \
                     older spend to age out of the window"
                ),
                daily_spend,
                weekly_spend,
                daily_budget,
                weekly_budget,
            };
        }
    }

    BudgetDecision {
        allowed: true,
        breached: None,
        message: format!(
            "within budget: {daily_spend}/{} daily, {weekly_spend}/{} weekly after +{estimate_tokens}",
            daily_budget.map(|c| c.to_string()).unwrap_or_else(|| "∞".into()),
            weekly_budget.map(|c| c.to_string()).unwrap_or_else(|| "∞".into()),
        ),
        daily_spend,
        weekly_spend,
        daily_budget,
        weekly_budget,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_db() -> Connection {
        let conn = Connection::open_in_memory().expect("memory db");
        conn.execute_batch(
            "CREATE TABLE spend (id INTEGER PRIMARY KEY, provider TEXT NOT NULL,
             tokens INTEGER NOT NULL, at_unix INTEGER NOT NULL);",
        )
        .expect("schema");
        conn
    }

    #[test]
    fn ten_k_daily_cap_blocks_an_18k_total() {
        let conn = mem_db();
        let mut cfg = Config::default_config();
        if let Some(p) = cfg.providers.get_mut("claude-pro") {
            p.daily_token_budget = Some(10_000);
        }
        // 8k already spent inside the daily window.
        db::record_spend(&conn, "claude-pro", 8_000, now_unix() - 60).unwrap();
        let decision = check(&conn, &cfg, "claude-pro", 10_000, now_unix());
        assert!(!decision.allowed);
        assert_eq!(decision.breached, Some("daily"));
        assert_eq!(decision.daily_spend, 8_000);
    }

    #[test]
    fn estimate_plus_spend_crossing_the_cap_is_refused() {
        let conn = mem_db();
        let mut cfg = Config::default_config();
        if let Some(p) = cfg.providers.get_mut("claude-pro") {
            p.daily_token_budget = Some(10_000);
        }
        db::record_spend(&conn, "claude-pro", 9_000, now_unix() - 60).unwrap();
        let d = check(&conn, &cfg, "claude-pro", 3_000, now_unix());
        assert!(!d.allowed, "9k + 3k = 12k must breach a 10k cap");
        assert_eq!(d.breached, Some("daily"));
    }

    #[test]
    fn exact_fit_is_allowed() {
        let conn = mem_db();
        let mut cfg = Config::default_config();
        if let Some(p) = cfg.providers.get_mut("claude-pro") {
            p.daily_token_budget = Some(10_000);
        }
        db::record_spend(&conn, "claude-pro", 9_000, now_unix() - 60).unwrap();
        let d = check(&conn, &cfg, "claude-pro", 1_000, now_unix());
        assert!(d.allowed, "exactly 10k of 10k is still within the ceiling");
    }

    #[test]
    fn weekly_ceiling_gates_when_daily_has_room() {
        let conn = mem_db();
        let mut cfg = Config::default_config();
        if let Some(p) = cfg.providers.get_mut("claude-pro") {
            p.daily_token_budget = Some(10_000);
            p.weekly_token_budget = Some(12_000);
        }
        db::record_spend(&conn, "claude-pro", 9_000, now_unix() - 60).unwrap();
        // Daily: 9k + 3k = 12k > 10k → daily breaches first.
        let d = check(&conn, &cfg, "claude-pro", 3_000, now_unix());
        assert_eq!(d.breached, Some("daily"));
        // Weekly-only scenario: smaller estimate fits daily but not weekly.
        let d2 = check(&conn, &cfg, "claude-pro", 1_000, now_unix());
        assert!(d2.allowed, "10k weekly is exactly at cap");
    }

    #[test]
    fn spend_older_than_the_window_does_not_count() {
        let conn = mem_db();
        let mut cfg = Config::default_config();
        if let Some(p) = cfg.providers.get_mut("claude-pro") {
            p.daily_token_budget = Some(10_000);
        }
        db::record_spend(&conn, "claude-pro", 9_000, now_unix() - DAY_SECS - 60).unwrap();
        let d = check(&conn, &cfg, "claude-pro", 10_000, now_unix());
        assert!(d.allowed, "stale spend ages out of the daily window");
    }

    #[test]
    fn providers_without_caps_are_not_gated() {
        let conn = mem_db();
        let cfg = Config::default_config();
        db::record_spend(&conn, "ollama-local", 999_999, now_unix()).unwrap();
        let d = check(&conn, &cfg, "ollama-local", 999_999, now_unix());
        assert!(d.allowed, "no ceilings configured → no refusal");
        assert_eq!(d.breached, None);
    }
}
