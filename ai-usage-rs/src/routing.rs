//! Usage-aware routing — explicit verdicts per provider.
//!
//! The old recommendation silently dropped any provider without a numeric
//! reading, which made "unknown" and "exhausted" indistinguishable and could
//! route work to a provider whose state was never verified. Every candidate
//! now carries an explicit verdict:
//!
//! - `eligible`    — verified reading, below the warning threshold, no hard
//!   limit recorded: may take work.
//! - `local-first` — eligible with headroom on raw percentages, but the
//!   local-first policy suppresses it because the unmetered local runtime is
//!   comparably fresh: policy-refused, must never be dispatched.
//! - `backoff`     — a 429/session rate limit was recorded INSIDE the
//!   provider's TTL window: transient, means "retry shortly". Distinct from
//!   `exhausted` — the monthly credit pool is untouched by a 429, and the
//!   verdict self-clears when the TTL expires.
//! - `exhausted`   — at/above the critical threshold, or a LEGACY sticky
//!   limit-hit observation still stands: dispatches will fail.
//! - `unknown`     — never observed: routing must not assume headroom.
//! - `unavailable` — the collector explicitly reported the quota source as
//!   unavailable (no percentage exists).

use crate::config::Config;
use crate::credits;
use crate::db::Observation;
use std::collections::HashMap;

/// Local-first: prefer the unmetered local runtime. A metered provider only
/// wins when its reading is at least this many points FRESHER than the local
/// reading — its capacity advantage has to be worth the metered spend.
/// (Comparing the other direction would only ever suppress candidates that
/// would lose the pure-percent ranking anyway, making the policy a no-op.)
pub const LOCAL_FIRST_MARGIN: f64 = 25.0;

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Eligible,
    LocalFirst,
    Backoff,
    Exhausted,
    Unknown,
    Unavailable,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Eligible => "eligible",
            Verdict::LocalFirst => "local-first",
            Verdict::Backoff => "backoff",
            Verdict::Exhausted => "exhausted",
            Verdict::Unknown => "unknown",
            Verdict::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct VerdictedCandidate {
    pub provider: String,
    pub percent: Option<f64>,
    pub source: String,
    pub note: String,
    pub verdict: Verdict,
    pub has_headroom: bool,
    /// Seconds until the next window reset, parsed from the note when present.
    pub reset_in_secs: Option<i64>,
}

/// Classify one provider's latest observation into a routing verdict.
///
/// `conn` supplies the transient rate-event table: an active backoff verdict
/// comes from the provider's TTL'd rate_events row, never from the
/// observation stream, so a 429 can never masquerade as plan exhaustion.
/// `now` seeds both the backoff TTL check and note-based reset parsing
/// (tests pass a fixed clock; callers pass the real one).
pub fn classify(
    conn: &rusqlite::Connection,
    cfg: &Config,
    state: Option<&Observation>,
    provider: &str,
    now: i64,
) -> VerdictedCandidate {
    let Some(state) = state else {
        return VerdictedCandidate {
            provider: provider.to_string(),
            percent: None,
            source: String::new(),
            note: String::new(),
            verdict: Verdict::Unknown,
            has_headroom: false,
            reset_in_secs: None,
        };
    };

    let reset_in_secs = reset_in_secs_from_note(&state.note, now);
    let base = VerdictedCandidate {
        provider: provider.to_string(),
        percent: state.percent,
        source: state.source.clone(),
        note: state.note.clone(),
        verdict: Verdict::Unknown,
        has_headroom: false,
        reset_in_secs,
    };

    let Some(pct) = state.percent else {
        return VerdictedCandidate {
            verdict: Verdict::Unavailable,
            ..base
        };
    };

    // Transient 429/session limit recorded inside the TTL window → `backoff`,
    // which self-clears. The percent reading (credit pool or manual) stays
    // visible and untouched — the two signals never merge again.
    let backoff = credits::backoff_state(conn, cfg, provider, now);
    if backoff.active && state.source != "limit-hit" {
        return VerdictedCandidate {
            verdict: Verdict::Backoff,
            ..base
        };
    }

    // A LEGACY sticky limit-hit observation (pre-credit-model row) still
    // means hard exhaustion — but only in its unmigrated form. The
    // migration rewrites old rows to `limit-hit-consumed`, so a stale 100%
    // observation from a subagent 429 stops poisoning the verdict the
    // moment the new binary opens the DB. For credit-pool providers the
    // percent-of-pool replaces any stale percentage entirely.
    let credit_modeled = cfg
        .providers
        .get(provider)
        .and_then(|p| p.monthly_credit_dollars)
        .unwrap_or(0.0)
        > 0.0;
    if !credit_modeled && (state.source == "limit-hit" || pct >= cfg.thresholds.critical) {
        return VerdictedCandidate {
            verdict: Verdict::Exhausted,
            ..base
        };
    }

    VerdictedCandidate {
        verdict: Verdict::Eligible,
        has_headroom: pct < cfg.thresholds.warning,
        ..base
    }
}

/// Classify every provider in rotation order.
///
/// This is the single source of truth for verdicts: the local-first policy
/// from `recommendation()` is applied HERE, so a provider the policy refuses
/// is marked `local-first` with `has_headroom: false` — machine consumers
/// reading `recommend --json` never see a suppressed provider as dispatchable.
pub fn classify_all(
    conn: &rusqlite::Connection,
    cfg: &Config,
    states: &HashMap<String, Observation>,
) -> Vec<VerdictedCandidate> {
    let now = now_unix();
    let local_pct = states.get("ollama-local").and_then(|s| s.percent);
    let suppresses = |provider: &str, pct: f64| -> bool {
        if !cfg.local_first || provider == "ollama-local" {
            return false;
        }
        match local_pct {
            Some(local_pct) => pct + LOCAL_FIRST_MARGIN > local_pct,
            None => false,
        }
    };
    cfg.rotation_order
        .iter()
        .map(|p| {
            let mut c = classify(conn, cfg, states.get(p), p, now);
            if c.verdict == Verdict::Eligible
                && c.has_headroom
                && suppresses(p, c.percent.expect("eligible implies percent"))
            {
                c.verdict = Verdict::LocalFirst;
                c.has_headroom = false;
            }
            c
        })
        .collect()
}

/// Parse a reset timestamp out of an observation note and return seconds
/// from `now` until the reset. Understands the shapes the collectors emit:
///   "(resets 2026-09-01T12:00:00+00:00)"  /  "(resets in 47m)"
/// Returns None when the note carries no parseable reset.
pub fn reset_in_secs_from_note(note: &str, now_unix: i64) -> Option<i64> {
    let idx = note.find("(resets ")?;
    let rest = &note[idx + "(resets ".len()..];
    let end = rest.find(')')?;
    let body = &rest[..end];

    // "resets in 47m" style (relative).
    if let Some(stripped) = body.strip_prefix("in ") {
        let digits: String = stripped
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !digits.is_empty() {
            let unit = stripped
                .chars()
                .skip(digits.len())
                .find(|c| !c.is_whitespace())?;
            let n: i64 = digits.parse().ok()?;
            return Some(match unit {
                'h' => n * 3600,
                'm' => n * 60,
                's' => n,
                _ => return None,
            });
        }
    }

    // Absolute timestamp — reuse the collectors' ISO parser.
    let ts = crate::collectors::claude::parse_iso_to_unix(body)?;
    let delta = (ts as i64) - now_unix;
    Some(delta.max(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn cfg() -> Config {
        Config::default_config()
    }

    /// In-memory DB with the tables classify consults (rate_events etc.).
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

    fn obs(percent: Option<f64>, source: &str) -> Observation {
        Observation {
            percent,
            source: source.to_string(),
            note: "test".to_string(),
            at: "2026-09-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn missing_observation_is_unknown_and_has_no_headroom() {
        let conn = mem_db();
        let c = classify(&conn, &cfg(), None, "claude-pro", 1_000);
        assert_eq!(c.verdict, Verdict::Unknown);
        assert!(!c.has_headroom);
        assert_eq!(c.percent, None);
    }

    #[test]
    fn unavailable_observation_stays_unavailable_not_zero_usage() {
        let conn = mem_db();
        let c = classify(
            &conn,
            &cfg(),
            Some(&obs(None, "unavailable")),
            "zai-codeplus",
            1,
        );
        assert_eq!(c.verdict, Verdict::Unavailable);
        assert!(!c.has_headroom);
    }

    #[test]
    fn critical_reading_is_exhausted() {
        let conn = mem_db();
        let c = classify(
            &conn,
            &cfg(),
            Some(&obs(Some(96.0), "direct-api")),
            "zai-codeplus",
            1,
        );
        assert_eq!(c.verdict, Verdict::Exhausted);
        assert!(!c.has_headroom);
    }

    #[test]
    fn limit_hit_source_is_exhausted_even_below_critical() {
        let conn = mem_db();
        let mut cfg = cfg();
        // claude-pro is NOT credit-modeled: a legacy sticky limit-hit
        // observation still means hard exhaustion there.
        if let Some(p) = cfg.providers.get_mut("claude-pro") {
            p.monthly_credit_dollars = None;
        }
        let c = classify(
            &conn,
            &cfg,
            Some(&obs(Some(10.0), "limit-hit")),
            "claude-pro",
            1,
        );
        assert_eq!(c.verdict, Verdict::Exhausted);
    }

    #[test]
    fn credit_pool_provider_ignores_stale_percent_observations() {
        let conn = mem_db();
        let cfg = cfg(); // ollama-pro is credit-modeled ($60 pool)
        let c = classify(
            &conn,
            &cfg,
            Some(&obs(Some(100.0), "limit-hit-consumed")),
            "ollama-pro",
            1_000,
        );
        assert_ne!(
            c.verdict,
            Verdict::Exhausted,
            "a stale 100% row must not pin a credit provider"
        );
    }

    #[test]
    fn active_rate_event_is_backoff_not_exhausted() {
        let conn = mem_db();
        let cfg = cfg();
        credits::record_rate_event(&conn, "ollama-pro", "429 session limit", 900).unwrap();
        let c = classify(
            &conn,
            &cfg,
            Some(&obs(Some(8.4), "manual")),
            "ollama-pro",
            1_000,
        );
        assert_eq!(c.verdict, Verdict::Backoff);
        assert_eq!(c.percent, Some(8.4), "the real reading stays visible");
    }

    #[test]
    fn expired_rate_event_returns_to_eligible() {
        let conn = mem_db();
        let cfg = cfg();
        credits::record_rate_event(&conn, "ollama-pro", "429", 0).unwrap();
        let c = classify(
            &conn,
            &cfg,
            Some(&obs(Some(20.0), "manual")),
            "ollama-pro",
            credits::DEFAULT_BACKOFF_SECS,
        );
        assert_eq!(c.verdict, Verdict::Eligible);
        assert!(c.has_headroom);
    }

    #[test]
    fn healthy_reading_is_eligible_with_headroom() {
        let conn = mem_db();
        let c = classify(
            &conn,
            &cfg(),
            Some(&obs(Some(30.0), "manual")),
            "chatgpt-plus",
            1,
        );
        assert_eq!(c.verdict, Verdict::Eligible);
        assert!(c.has_headroom);
    }

    #[test]
    fn warning_level_reading_is_eligible_but_has_no_headroom() {
        let conn = mem_db();
        let c = classify(
            &conn,
            &cfg(),
            Some(&obs(Some(91.0), "manual")),
            "chatgpt-plus",
            1,
        );
        assert_eq!(c.verdict, Verdict::Eligible);
        assert!(!c.has_headroom);
    }

    #[test]
    fn classify_all_marks_local_first_suppressed_eligible_providers() {
        let conn = mem_db();
        let mut states = HashMap::new();
        states.insert("ollama-local".to_string(), obs(Some(40.0), "manual"));
        states.insert("claude-pro".to_string(), obs(Some(20.0), "manual"));

        let out = classify_all(&conn, &cfg(), &states);
        let claude = out.iter().find(|c| c.provider == "claude-pro").unwrap();
        // 20 + 25 = 45 > 40 → the local-first policy suppresses claude-pro
        // even though it is eligible with headroom on raw percentages.
        assert_eq!(claude.verdict, Verdict::LocalFirst);
        assert!(!claude.has_headroom);

        let local = out.iter().find(|c| c.provider == "ollama-local").unwrap();
        assert_eq!(local.verdict, Verdict::Eligible);
        assert!(local.has_headroom);
    }

    #[test]
    fn local_first_suppression_requires_a_local_reading_and_respects_toggle() {
        let conn = mem_db();
        let mut states = HashMap::new();
        states.insert("claude-pro".to_string(), obs(Some(20.0), "manual"));

        // No ollama-local reading → policy cannot fire.
        let out = classify_all(&conn, &cfg(), &states);
        assert_eq!(out[0].verdict, Verdict::Eligible);
        assert!(out[0].has_headroom);

        // Toggle off → pure percent ranking, never suppressed.
        let mut cfg = cfg();
        cfg.local_first = false;
        let mut with_local = states.clone();
        with_local.insert("ollama-local".to_string(), obs(Some(40.0), "manual"));
        let out = classify_all(&conn, &cfg, &with_local);
        assert_eq!(out[0].verdict, Verdict::Eligible);
        assert!(out[0].has_headroom);
    }

    #[test]
    fn classify_all_preserves_rotation_order() {
        let conn = mem_db();
        let mut states = HashMap::new();
        states.insert("claude-pro".to_string(), obs(Some(10.0), "manual"));
        states.insert("chatgpt-plus".to_string(), obs(Some(20.0), "manual"));
        let out = classify_all(&conn, &cfg(), &states);
        assert_eq!(
            out.iter().map(|c| c.provider.as_str()).collect::<Vec<_>>(),
            vec![
                "claude-pro",
                "zai-codeplus",
                "chatgpt-plus",
                "ollama-pro",
                "ollama-local"
            ]
        );
        assert_eq!(out[0].verdict, Verdict::Eligible);
        assert_eq!(out[1].verdict, Verdict::Unknown);
    }

    #[test]
    fn classify_all_covers_providers_missing_from_the_state_map() {
        let conn = mem_db();
        let out = classify_all(&conn, &cfg(), &HashMap::new());
        assert!(out.iter().all(|c| c.verdict == Verdict::Unknown));
        assert_eq!(out.len(), 5);
    }
}
