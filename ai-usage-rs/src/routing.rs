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
//! - `backoff`     — a TRANSIENT session 429/rate-limit event is active
//!   (within its TTL). Short-lived: retry shortly. Distinct from `exhausted`
//!   because a 429 on one subagent says nothing about plan/credit state.
//! - `exhausted`   — at/above the critical threshold on the DURABLE signal
//!   (monthly consumption percent, or an expired-limit session window):
//!   dispatches will fail for the rest of the period.
//! - `unknown`     — never observed: routing must not assume headroom.
//! - `unavailable` — the collector explicitly reported the quota source as
//!   unavailable (no percentage exists).
//!
//! Legacy sticky rows: a `limit-hit` observation older than
//! `db::LIMIT_HIT_TTL_SECS` is filtered out by `db::latest()` itself, so it
//! can never reach this module and render as plan exhaustion.

use crate::config::Config;
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
    /// Transient 429/rate-limit backoff — clears with the event TTL.
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
/// `in_backoff`: a currently-active transient rate-limit event exists (checked
/// by the caller against `db::rate_limit_events`, which is why it is a
/// parameter and not a DB read — this function stays pure).
pub fn classify(
    cfg: &Config,
    state: Option<&Observation>,
    provider: &str,
    in_backoff: bool,
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

    let reset_in_secs = reset_in_secs_from_note(&state.note, now_unix());
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

    if pct >= cfg.thresholds.critical {
        return VerdictedCandidate {
            verdict: Verdict::Exhausted,
            ..base
        };
    }

    // A live 429 is short-lived backoff, layered over whatever the durable
    // reading says. It NEVER manufactures a 100% reading (the old
    // `source == "limit-hit"` check did exactly that). The in_backoff flag
    // is the primary signal; the legacy observation row is belt-and-braces.
    if in_backoff || state.source == "limit-hit" {
        return VerdictedCandidate {
            verdict: Verdict::Backoff,
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
///
/// `backoff_providers`: providers with a currently-active transient 429 event
/// (computed by the caller from `db::active_rate_limits`, keeping this
/// function pure and testable).
pub fn classify_all(
    cfg: &Config,
    states: &HashMap<String, Observation>,
    backoff_providers: &std::collections::HashSet<String>,
) -> Vec<VerdictedCandidate> {
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
            let in_backoff = backoff_providers.contains(p);
            let mut c = classify(cfg, states.get(p), p, in_backoff);
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
    use std::collections::HashSet;

    fn cfg() -> Config {
        Config::default_config()
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
        let c = classify(&cfg(), None, "claude-pro", false);
        assert_eq!(c.verdict, Verdict::Unknown);
        assert!(!c.has_headroom);
        assert_eq!(c.percent, None);
    }

    #[test]
    fn unavailable_observation_stays_unavailable_not_zero_usage() {
        let c = classify(
            &cfg(),
            Some(&obs(None, "unavailable")),
            "zai-codeplus",
            false,
        );
        assert_eq!(c.verdict, Verdict::Unavailable);
        assert!(!c.has_headroom);
    }

    #[test]
    fn critical_reading_is_exhausted() {
        let c = classify(
            &cfg(),
            Some(&obs(Some(96.0), "direct-api")),
            "zai-codeplus",
            false,
        );
        assert_eq!(c.verdict, Verdict::Exhausted);
        assert!(!c.has_headroom);
    }

    #[test]
    fn limit_hit_is_transient_backoff_and_never_manufactures_exhaustion() {
        // THE regression from the incident: a 10%-used provider with a stale
        // limit-hit row was reported as 100% exhausted. A limit-hit is
        // short-lived backoff; the durable reading stays untouched.
        let c = classify(
            &cfg(),
            Some(&obs(Some(10.0), "limit-hit")),
            "ollama-pro",
            false,
        );
        assert_eq!(c.verdict, Verdict::Backoff);
        assert_eq!(
            c.percent,
            Some(10.0),
            "backoff must not overwrite the balance"
        );
    }

    #[test]
    fn active_rate_limit_event_overrides_eligible_with_backoff() {
        // A healthy 8% credit reading + a live 429 event → backoff, while
        // percent stays 8% (dollars untouched by the transient event).
        let c = classify(&cfg(), Some(&obs(Some(8.5), "credit")), "ollama-pro", true);
        assert_eq!(c.verdict, Verdict::Backoff);
        assert_eq!(c.percent, Some(8.5));
        assert!(!c.has_headroom, "backoff is not dispatchable while active");
    }

    #[test]
    fn expired_rate_limit_event_restores_eligibility() {
        let c = classify(&cfg(), Some(&obs(Some(8.5), "credit")), "ollama-pro", false);
        assert_eq!(c.verdict, Verdict::Eligible);
        assert!(c.has_headroom);
    }

    #[test]
    fn healthy_reading_is_eligible_with_headroom() {
        let c = classify(
            &cfg(),
            Some(&obs(Some(30.0), "manual")),
            "chatgpt-plus",
            false,
        );
        assert_eq!(c.verdict, Verdict::Eligible);
        assert!(c.has_headroom);
    }

    #[test]
    fn warning_level_reading_is_eligible_but_has_no_headroom() {
        let c = classify(
            &cfg(),
            Some(&obs(Some(91.0), "manual")),
            "chatgpt-plus",
            false,
        );
        assert_eq!(c.verdict, Verdict::Eligible);
        assert!(!c.has_headroom);
    }

    #[test]
    fn classify_all_marks_local_first_suppressed_eligible_providers() {
        let mut states = HashMap::new();
        states.insert("ollama-local".to_string(), obs(Some(40.0), "manual"));
        states.insert("claude-pro".to_string(), obs(Some(20.0), "manual"));

        let out = classify_all(&cfg(), &states, &HashSet::new());
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
    fn classify_all_marks_backoff_from_the_event_set() {
        let mut states = HashMap::new();
        states.insert("ollama-pro".to_string(), obs(Some(8.5), "credit"));
        let mut backoff = HashSet::new();
        backoff.insert("ollama-pro".to_string());

        let out = classify_all(&cfg(), &states, &backoff);
        let p = out.iter().find(|c| c.provider == "ollama-pro").unwrap();
        assert_eq!(p.verdict, Verdict::Backoff);
        assert!(!p.has_headroom);
        assert_eq!(p.percent, Some(8.5));

        // Same states, no active events → eligible.
        let out = classify_all(&cfg(), &states, &HashSet::new());
        assert_eq!(*c_verdict(&out, "ollama-pro"), Verdict::Eligible);
    }

    fn c_verdict<'a>(out: &'a [VerdictedCandidate], provider: &str) -> &'a Verdict {
        &out.iter()
            .find(|c| c.provider == provider)
            .expect("provider present")
            .verdict
    }

    #[test]
    fn local_first_suppression_requires_a_local_reading_and_respects_toggle() {
        let mut states = HashMap::new();
        states.insert("claude-pro".to_string(), obs(Some(20.0), "manual"));

        // No ollama-local reading → policy cannot fire.
        let out = classify_all(&cfg(), &states, &HashSet::new());
        assert_eq!(out[0].verdict, Verdict::Eligible);
        assert!(out[0].has_headroom);

        // Toggle off → pure percent ranking, never suppressed.
        let mut cfg = cfg();
        cfg.local_first = false;
        let mut with_local = states.clone();
        with_local.insert("ollama-local".to_string(), obs(Some(40.0), "manual"));
        let out = classify_all(&cfg, &with_local, &HashSet::new());
        assert_eq!(out[0].verdict, Verdict::Eligible);
        assert!(out[0].has_headroom);
    }

    #[test]
    fn classify_all_preserves_rotation_order() {
        let mut states = HashMap::new();
        states.insert("claude-pro".to_string(), obs(Some(10.0), "manual"));
        states.insert("chatgpt-plus".to_string(), obs(Some(20.0), "manual"));
        let out = classify_all(&cfg(), &states, &HashSet::new());
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
        let out = classify_all(&cfg(), &HashMap::new(), &HashSet::new());
        assert!(out.iter().all(|c| c.verdict == Verdict::Unknown));
        assert_eq!(out.len(), 5);
    }
}
