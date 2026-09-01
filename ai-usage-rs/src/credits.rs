//! Monthly credit-balance model for prepaid-credit providers (Ollama Pro).
//!
//! The tool used to model everything as percent-of-a-rate-limit, which made
//! two unrelated signals look identical:
//!   - a SESSION rate limit — transient, clears in minutes, means "retry shortly"
//!   - MONTHLY plan consumption — durable, means "you have spent X of $60"
//!
//! A 429 on one subagent rendered as plan exhaustion (ollama-pro pinned at
//! `100.0% limit-hit` for 30+ hours while the real pool was ~8% consumed),
//! and every routing decision made that morning rested on the false reading.
//!
//! The model now is: monthly credit DOLLARS consumed / remaining, reset date,
//! burn rate, projected month-end — with transient 429s as a short-lived
//! backoff flag that never touches the balance figure.
//!
//! Dollar consumption is sourced from REAL readings only
//! (`ai-usage credit <provider> record <dollars-used>` mirrors the provider's
//! dashboard) — never inferred from error events. Burn rate comes from the
//! delta between consecutive readings, never from a 429.

use crate::config::Config;
use rusqlite::{params, Connection, OptionalExtension};

/// Backoff TTL default: a 429 on one subagent means "retry shortly", not
/// "plan dead". Configurable per provider via `rate_limit_backoff_secs`.
pub const DEFAULT_BACKOFF_SECS: i64 = 15 * 60;

/// Seconds in a day — the trailing window for daily soft-cap accounting.
pub const DAY_SECS: i64 = 86_400;

#[derive(Debug, Clone, PartialEq)]
pub struct BackoffState {
    /// True when the newest rate event is younger than the provider's TTL.
    pub active: bool,
    /// Seconds until the backoff clears (0 when already clear).
    pub clear_in_secs: i64,
    /// TTL applied, for display.
    pub ttl_secs: i64,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreditState {
    pub provider: String,
    /// Cumulative dollars used this period, from the newest reading.
    pub used_dollars: Option<f64>,
    /// Monthly credit pool from config (e.g. 60.0); None = unconfigured.
    pub pool_dollars: f64,
    /// used/pool*100 — feeds the existing percent machinery. None when no
    /// reading exists or the pool is unconfigured.
    pub percent: Option<f64>,
    /// pool minus used (0.0 when nothing recorded).
    pub remaining_dollars: f64,
    /// Unix seconds when the credit period resets.
    pub reset_at_unix: i64,
    /// Burn projection when >= 2 readings exist and the pool is configured.
    pub burn: Option<BurnProjection>,
    /// Newest reading timestamp (unix) when available.
    pub last_reading_unix: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BurnProjection {
    /// Average dollars/hour across the reading window.
    pub dollars_per_hour: f64,
    /// Window the rate was computed over (seconds between first and last
    /// reading, clamped to >= 60s to avoid divide-by-tiny).
    pub window_secs: i64,
    /// Projected dollars at the period's reset timestamp.
    pub projected_at_reset: f64,
    /// Projected month-end overspend (dollars past the pool), 0 when within.
    pub projected_overrun: f64,
    /// Newest reading timestamp (unix) — the projection's anchor.
    pub as_of_unix: i64,
}

fn backoff_ttl_secs(cfg: &Config, provider: &str) -> i64 {
    cfg.providers
        .get(provider)
        .and_then(|p| p.rate_limit_backoff_secs)
        .unwrap_or(DEFAULT_BACKOFF_SECS)
}

/// Record a 429/session rate limit as a TRANSIENT event with a TTL. Never
/// writes an observation row, never touches the credit balance.
pub fn record_rate_event(
    conn: &Connection,
    provider: &str,
    note: &str,
    at_unix: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO rate_events (provider, note, at_unix) VALUES (?1, ?2, ?3)",
        params![provider, note, at_unix],
    )?;
    Ok(())
}

/// The provider's backoff state: active iff the newest rate event is younger
/// than the TTL. Older events are invisible — the sticky bug made them
/// permanent.
pub fn backoff_state(conn: &Connection, cfg: &Config, provider: &str, now: i64) -> BackoffState {
    let ttl = backoff_ttl_secs(cfg, provider);
    let row: Option<(i64, String)> = conn
        .query_row(
            "SELECT at_unix, note FROM rate_events
             WHERE provider = ?1 ORDER BY id DESC LIMIT 1",
            params![provider],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .unwrap_or(None);
    match row {
        Some((at, note)) if now - at < ttl => BackoffState {
            active: true,
            clear_in_secs: ttl - (now - at),
            ttl_secs: ttl,
            note,
        },
        _ => BackoffState {
            active: false,
            clear_in_secs: 0,
            ttl_secs: ttl,
            note: String::new(),
        },
    }
}

/// Record a cumulative dollar reading from a real source (provider dashboard,
/// billing email). Cumulative — the newest reading is the balance truth; the
/// delta between readings is burn.
pub fn record_credit(
    conn: &Connection,
    provider: &str,
    used_dollars: f64,
    at_unix: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO credit_events (provider, used_dollars, at_unix) VALUES (?1, ?2, ?3)",
        params![provider, used_dollars, at_unix],
    )?;
    Ok(())
}

/// Newest cumulative dollar reading. (Superseded by `credit_state`, which
/// also derives percent/remaining/burn; kept for direct balance lookups.)
#[allow(dead_code)]
pub fn latest_credit(conn: &Connection, provider: &str) -> Option<(f64, i64)> {
    conn.query_row(
        "SELECT used_dollars, at_unix FROM credit_events
         WHERE provider = ?1 ORDER BY id DESC LIMIT 1",
        params![provider],
        |row| Ok((row.get::<_, f64>(0)?, row.get::<_, i64>(1)?)),
    )
    .optional()
    .unwrap_or(None)
}

/// All readings for a provider, oldest first (burn-rate window input).
pub fn credit_history(conn: &Connection, provider: &str) -> Vec<(f64, i64)> {
    let mut stmt = match conn.prepare(
        "SELECT used_dollars, at_unix FROM credit_events
         WHERE provider = ?1 ORDER BY at_unix ASC, id ASC",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map(params![provider], |row| {
        Ok((row.get::<_, f64>(0)?, row.get::<_, i64>(1)?))
    });
    match rows {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

/// Collapse a credit history into burn-rate analytics. Returns None with
/// fewer than two readings — one point cannot produce a rate.
pub fn burn_rate(history: &[(f64, i64)], pool: f64, reset_at_unix: i64) -> Option<BurnProjection> {
    if history.len() < 2 {
        return None;
    }
    let first = history.first()?;
    let last = history.last()?;
    let window_secs = (last.1 - first.1).max(60);
    let dollars = (last.0 - first.0).max(0.0);
    let rate = dollars / (window_secs as f64 / 3600.0);
    let secs_to_reset = (reset_at_unix - last.1).max(0);
    let projected = last.0 + rate * (secs_to_reset as f64 / 3600.0);
    Some(BurnProjection {
        dollars_per_hour: rate,
        window_secs,
        projected_at_reset: projected,
        projected_overrun: (projected - pool).max(0.0),
        as_of_unix: last.1,
    })
}

/// Assemble the provider's credit state: newest reading, percent of pool,
/// remaining dollars, and the burn projection when >= 2 readings exist.
pub fn credit_state(conn: &Connection, cfg: &Config, provider: &str, now: i64) -> CreditState {
    let pool = cfg
        .providers
        .get(provider)
        .and_then(|p| p.monthly_credit_dollars)
        .unwrap_or(0.0);
    let reset_at = reset_at_unix(cfg, provider, now);
    let latest = conn
        .query_row(
            "SELECT used_dollars, at_unix FROM credit_events
             WHERE provider = ?1 ORDER BY id DESC LIMIT 1",
            params![provider],
            |row| Ok((row.get::<_, f64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()
        .unwrap_or(None);
    let (used, percent, last_unix) = match latest {
        Some((u, at)) => {
            let pct = if pool > 0.0 {
                Some((u / pool * 100.0).clamp(0.0, 100.0))
            } else {
                None
            };
            (Some(u), pct, Some(at))
        }
        None => (None, None, None),
    };
    CreditState {
        provider: provider.to_string(),
        used_dollars: used,
        pool_dollars: pool,
        percent,
        remaining_dollars: pool - used.unwrap_or(0.0),
        reset_at_unix: reset_at,
        burn: credit_state_burn(conn, provider, pool, reset_at),
        last_reading_unix: last_unix,
    }
}

fn credit_state_burn(
    conn: &Connection,
    provider: &str,
    pool: f64,
    reset_at_unix: i64,
) -> Option<BurnProjection> {
    if pool <= 0.0 {
        return None;
    }
    let history = credit_history(conn, provider);
    burn_rate(&history, pool, reset_at_unix)
}

/// Reset timestamp for the credit period. Config may carry
/// `credit_reset_at` (ISO 8601); default: 3 weeks out from the first
/// reading (Ollama Pro's stated cadence) — 21 days after the oldest event.
pub fn reset_at_unix(cfg: &Config, provider: &str, now: i64) -> i64 {
    if let Some(p) = cfg.providers.get(provider) {
        if let Some(ts) = p.credit_reset_at.as_deref() {
            if let Some(unix) = crate::collectors::claude::parse_iso_to_unix(ts) {
                return unix as i64;
            }
        }
    }
    // Fallback matches the live plan: resets ~3 weeks after the period began.
    now + 21 * 86_400
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_db() -> Connection {
        let conn = Connection::open_in_memory().expect("memory db");
        conn.execute_batch(
            "CREATE TABLE rate_events (id INTEGER PRIMARY KEY, provider TEXT NOT NULL,
             note TEXT NOT NULL, at_unix INTEGER NOT NULL);
             CREATE TABLE credit_events (id INTEGER PRIMARY KEY, provider TEXT NOT NULL,
             used_dollars REAL NOT NULL, at_unix INTEGER NOT NULL);",
        )
        .expect("schema");
        conn
    }

    fn cfg_with_credit(pool: f64, backoff: Option<i64>) -> Config {
        let mut cfg = Config::default_config();
        let p = cfg.providers.get_mut("ollama-pro").unwrap();
        p.monthly_credit_dollars = Some(pool);
        p.rate_limit_backoff_secs = backoff;
        cfg
    }

    #[test]
    fn rate_event_expires_after_ttl() {
        let conn = mem_db();
        let cfg = cfg_with_credit(60.0, Some(900));
        record_rate_event(&conn, "ollama-pro", "429 session limit", 1_000).unwrap();
        // Within TTL: backoff active.
        let active = backoff_state(&conn, &cfg, "ollama-pro", 1_000 + 899);
        assert!(active.active);
        assert_eq!(active.clear_in_secs, 1);
        // At TTL: expired.
        let expired = backoff_state(&conn, &cfg, "ollama-pro", 1_000 + 900);
        assert!(!expired.active);
        assert_eq!(expired.clear_in_secs, 0);
    }

    #[test]
    fn default_backoff_is_15_minutes() {
        assert_eq!(DEFAULT_BACKOFF_SECS, 900);
        let conn = mem_db();
        record_rate_event(&conn, "ollama-pro", "429", 5_000).unwrap();
        let cfg = cfg_with_credit(60.0, None);
        assert!(backoff_state(&conn, &cfg, "ollama-pro", 5_000 + 60).active);
        assert!(!backoff_state(&conn, &cfg, "ollama-pro", 5_000 + DEFAULT_BACKOFF_SECS).active);
    }

    #[test]
    fn rate_event_never_touches_observations_or_credits() {
        let conn = mem_db();
        record_rate_event(&conn, "ollama-pro", "429 on glm-5.2:cloud", 1_000).unwrap();
        let st = credit_state(&conn, &cfg_with_credit(60.0, None), "ollama-pro", 2_000);
        assert_eq!(st.used_dollars, None, "a 429 must not create spend");
        assert_eq!(st.percent, None);
        assert!(st.burn.is_none(), "one reading cannot produce a rate");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM credit_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn cumulative_readings_drive_percent_and_remaining() {
        let conn = mem_db();
        let cfg = cfg_with_credit(60.0, None);
        record_credit(&conn, "ollama-pro", 5.05, 1_000).unwrap();
        let st = credit_state(&conn, &cfg, "ollama-pro", 2_000);
        assert_eq!(st.used_dollars, Some(5.05));
        assert_eq!(st.remaining_dollars, 54.95);
        let pct = st.percent.expect("pool known → percent");
        assert!((pct - 8.416).abs() < 0.01, "5.05/60 = 8.4%, got {pct}");
        // A newer, higher cumulative reading is the truth (no double-count).
        record_credit(&conn, "ollama-pro", 6.20, 4_000).unwrap();
        let st = credit_state(&conn, &cfg, "ollama-pro", 5_000);
        assert_eq!(st.used_dollars, Some(6.20));
        assert_eq!(st.remaining_dollars, 53.80);
    }

    #[test]
    fn burn_rate_projects_month_end_overrun() {
        let conn = mem_db();
        let cfg = cfg_with_credit(60.0, None);
        // $5.05 this morning, $1.10 spent in the following 2 hours.
        record_credit(&conn, "ollama-pro", 5.05, 0).unwrap();
        record_credit(&conn, "ollama-pro", 6.15, 2 * 3600).unwrap();
        let st = credit_state(&conn, &cfg, "ollama-pro", 2 * 3600);
        let burn = st.burn.expect("two readings → projection");
        assert!((burn.dollars_per_hour - 0.55).abs() < 1e-9);
        // Reset lands 21 days after `now` → 504h remaining at $0.55/h.
        assert!((burn.projected_at_reset - (6.15 + 0.55 * 21.0 * 24.0)).abs() < 1e-6);
        assert!(burn.projected_overrun > 60.0, "0.55/h projects past $60");
    }

    #[test]
    fn falling_cumulative_reading_is_never_negative_burn() {
        let conn = mem_db();
        let cfg = cfg_with_credit(60.0, None);
        record_credit(&conn, "ollama-pro", 10.0, 0).unwrap();
        record_credit(&conn, "ollama-pro", 8.0, 3600).unwrap();
        let burn = credit_state(&conn, &cfg, "ollama-pro", 3600)
            .burn
            .expect("two readings");
        assert_eq!(
            burn.dollars_per_hour, 0.0,
            "dashboard corrections clamp at 0"
        );
        assert_eq!(burn.projected_overrun, 0.0);
    }

    #[test]
    fn credit_reset_config_overrides_the_default_period() {
        let mut cfg = Config::default_config();
        if let Some(p) = cfg.providers.get_mut("ollama-pro") {
            p.credit_reset_at = Some("2026-09-22T00:00:00Z".to_string());
        }
        let now =
            crate::collectors::claude::parse_iso_to_unix("2026-09-01T12:00:00Z").unwrap() as i64;
        assert_eq!(
            reset_at_unix(&cfg, "ollama-pro", now),
            crate::collectors::claude::parse_iso_to_unix("2026-09-22T00:00:00Z").unwrap() as i64
        );
    }
}
