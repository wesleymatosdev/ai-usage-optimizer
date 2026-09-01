//! Usage-aware routing — explicit verdicts per provider.
//!
//! The old recommendation silently dropped any provider without a numeric
//! reading, which made "unknown" and "exhausted" indistinguishable and could
//! route work to a provider whose state was never verified. Every candidate
//! now carries an explicit verdict:
//!
//! - `eligible`    — verified reading, below the warning threshold, no hard
//!   limit recorded: may take work.
//! - `exhausted`   — at/above the critical threshold, or a limit-hit was
//!   recorded: dispatches will fail.
//! - `unknown`     — never observed: routing must not assume headroom.
//! - `unavailable` — the collector explicitly reported the quota source as
//!   unavailable (no percentage exists).

use crate::config::Config;
use crate::db::Observation;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Eligible,
    Exhausted,
    Unknown,
    Unavailable,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Eligible => "eligible",
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
}

/// Classify one provider's latest observation into a routing verdict.
pub fn classify(cfg: &Config, state: Option<&Observation>, provider: &str) -> VerdictedCandidate {
    let Some(state) = state else {
        return VerdictedCandidate {
            provider: provider.to_string(),
            percent: None,
            source: String::new(),
            note: String::new(),
            verdict: Verdict::Unknown,
            has_headroom: false,
        };
    };

    let base = VerdictedCandidate {
        provider: provider.to_string(),
        percent: state.percent,
        source: state.source.clone(),
        note: state.note.clone(),
        verdict: Verdict::Unknown,
        has_headroom: false,
    };

    let Some(pct) = state.percent else {
        return VerdictedCandidate {
            verdict: Verdict::Unavailable,
            ..base
        };
    };

    if state.source == "limit-hit" || pct >= cfg.thresholds.critical {
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
pub fn classify_all(
    cfg: &Config,
    states: &HashMap<String, Observation>,
) -> Vec<VerdictedCandidate> {
    cfg.rotation_order
        .iter()
        .map(|p| classify(cfg, states.get(p), p))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let c = classify(&cfg(), None, "claude-pro");
        assert_eq!(c.verdict, Verdict::Unknown);
        assert!(!c.has_headroom);
        assert_eq!(c.percent, None);
    }

    #[test]
    fn unavailable_observation_stays_unavailable_not_zero_usage() {
        let c = classify(&cfg(), Some(&obs(None, "unavailable")), "zai-codeplus");
        assert_eq!(c.verdict, Verdict::Unavailable);
        assert!(!c.has_headroom);
    }

    #[test]
    fn critical_reading_is_exhausted() {
        let c = classify(&cfg(), Some(&obs(Some(96.0), "direct-api")), "zai-codeplus");
        assert_eq!(c.verdict, Verdict::Exhausted);
        assert!(!c.has_headroom);
    }

    #[test]
    fn limit_hit_source_is_exhausted_even_below_critical() {
        let c = classify(&cfg(), Some(&obs(Some(10.0), "limit-hit")), "ollama-pro");
        assert_eq!(c.verdict, Verdict::Exhausted);
    }

    #[test]
    fn healthy_reading_is_eligible_with_headroom() {
        let c = classify(&cfg(), Some(&obs(Some(30.0), "manual")), "chatgpt-plus");
        assert_eq!(c.verdict, Verdict::Eligible);
        assert!(c.has_headroom);
    }

    #[test]
    fn warning_level_reading_is_eligible_but_has_no_headroom() {
        let c = classify(&cfg(), Some(&obs(Some(91.0), "manual")), "chatgpt-plus");
        assert_eq!(c.verdict, Verdict::Eligible);
        assert!(!c.has_headroom);
    }

    #[test]
    fn classify_all_preserves_rotation_order() {
        let mut states = HashMap::new();
        states.insert("claude-pro".to_string(), obs(Some(10.0), "manual"));
        states.insert("chatgpt-plus".to_string(), obs(Some(20.0), "manual"));
        let out = classify_all(&cfg(), &states);
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
        let out = classify_all(&cfg(), &HashMap::new());
        assert!(out.iter().all(|c| c.verdict == Verdict::Unknown));
        assert_eq!(out.len(), 5);
    }
}
