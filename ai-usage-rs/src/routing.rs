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
//!   local-first policy suppresses it because a genuinely METERED local
//!   reading is comparably fresh: policy-refused, must never be dispatched.
//!   This never fires against the unmetered-local sentinel (percent 0.0,
//!   source `ollama-local-unlimited`) — that 0.0 means "no ceiling exists",
//!   not "0% used", and treating it as a real percentage would suppress
//!   every metered provider any time local Ollama is reachable.
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
///
/// This margin is only meaningful against a REAL metered percentage. The
/// `ollama-local` collector reports `percent: 0.0` for an unmetered runtime
/// (source `ollama-local-unlimited`) — that 0.0 is a "no ceiling exists"
/// sentinel, not a real 0% usage reading. Comparing `pct + MARGIN > 0`
/// against it is true for every provider at every usage level, which
/// structurally suppresses all cloud providers any time local Ollama is up.
/// The fix: the margin comparison only fires when the local reading is a
/// genuine metered percentage (any other source). An unmetered local still
/// gets preferred in practice — `recommendation()` picks the lowest percent
/// among eligible candidates, and 0.0 already sorts first — so dropping the
/// absolute-suppression path for the unmetered case costs nothing and
/// restores the metered providers' visibility for every OTHER purpose
/// (candidate listing, `excluded` map, future consumers).
pub const LOCAL_FIRST_MARGIN: f64 = 25.0;

/// The `source` value the Ollama collector writes when local capacity is
/// unmetered (see `collectors::ollama::LocalSnapshot`). Percent 0.0 under
/// this source is a sentinel ("no ceiling"), never a real usage reading.
pub const UNMETERED_LOCAL_SOURCE: &str = "ollama-local-unlimited";

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
    let raw_reset_delta = raw_reset_delta_from_note(&state.note, now_unix());
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

    // The window this percent was measured against has already rolled over
    // (the note's parsed reset timestamp is in the past). Carrying the
    // pre-reset percent forward as still-current usage is the sticky
    // staleness bug: Wesley observed the CLI reporting 44% used while the
    // provider's own portal had already reset to 1%. Nothing here can know
    // the true post-reset percent without a fresh collect, so the honest
    // state is Unknown — it forces a re-probe on the next read/collect
    // instead of silently trusting data from a window that no longer
    // exists. This only affects observations whose note carries a
    // parseable reset timestamp (zai/hermes/claude-style); readings with no
    // such note (manual, credit) are unaffected.
    if raw_reset_delta.is_some_and(|secs| secs < 0) {
        return VerdictedCandidate {
            verdict: Verdict::Unknown,
            percent: None,
            ..base
        };
    }

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
    let local = states.get("ollama-local");
    let local_pct = local.and_then(|s| s.percent);
    // An unmetered local reading (the collector's real "no ceiling" sentinel,
    // percent 0.0 under source `ollama-local-unlimited`) must not act as a
    // real 0% floor in the margin comparison — see LOCAL_FIRST_MARGIN docs.
    let local_is_unmetered_sentinel = local.is_some_and(|s| s.source == UNMETERED_LOCAL_SOURCE);
    let suppresses = |provider: &str, pct: f64| -> bool {
        if !cfg.local_first || provider == "ollama-local" || local_is_unmetered_sentinel {
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

/// Parse a reset timestamp out of an observation note and return the RAW
/// (possibly negative) delta in seconds: `parsed_reset - now`. Negative means
/// the window has already rolled over. Shared by the public
/// `reset_in_secs_from_note` (which clamps to 0 for display) and the
/// window-staleness check in `classify` (which needs the negative signal).
fn raw_reset_delta_from_note(note: &str, now_unix: i64) -> Option<i64> {
    let idx = note.find("(resets ")?;
    let rest = &note[idx + "(resets ".len()..];
    let end = rest.find(')')?;
    let body = &rest[..end];

    // "resets in 47m" style (relative) — always future by construction, a
    // collector never emits a negative relative offset.
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

    // Absolute timestamp — reuse the collectors' ISO parser. This CAN be
    // negative: the window already rolled over and the reading predates it.
    let ts = crate::collectors::claude::parse_iso_to_unix(body)?;
    Some((ts as i64) - now_unix)
}

/// Parse a reset timestamp out of an observation note and return seconds
/// from `now` until the reset. Understands the shapes the collectors emit:
///   "(resets 2026-09-01T12:00:00+00:00)"  /  "(resets in 47m)"
/// Returns None when the note carries no parseable reset. Clamped to 0 for
/// display — callers needing to detect an already-passed reset (staleness)
/// should use `raw_reset_delta_from_note` instead.
pub fn reset_in_secs_from_note(note: &str, now_unix: i64) -> Option<i64> {
    raw_reset_delta_from_note(note, now_unix).map(|d| d.max(0))
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

    /// Real wall-clock unix seconds — used only by the window-reset staleness
    /// tests, since `classify` reads the actual clock internally (it has no
    /// injectable `now` parameter).
    fn real_now_unix() -> i64 {
        now_unix()
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
        // A METERED local reading (source "manual", NOT the unmetered
        // sentinel) is what the local-first margin comparison must fire
        // against — this is the genuine suppression case.
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
    fn unmetered_local_sentinel_never_suppresses_a_metered_provider_at_low_usage() {
        // THE bug: ollama-local reports percent 0.0 under source
        // "ollama-local-unlimited" when it is genuinely unmetered — that 0.0
        // is a sentinel ("no ceiling"), not a real reading. The old
        // comparison (pct + 25 > 0) was true for every provider at every
        // usage level, structurally suppressing all cloud providers whenever
        // local Ollama was reachable. A metered provider at 1% must stay
        // eligible.
        let mut states = HashMap::new();
        states.insert(
            "ollama-local".to_string(),
            obs(Some(0.0), UNMETERED_LOCAL_SOURCE),
        );
        states.insert("claude-pro".to_string(), obs(Some(1.0), "manual"));

        let out = classify_all(&cfg(), &states, &HashSet::new());
        let claude = out.iter().find(|c| c.provider == "claude-pro").unwrap();
        assert_eq!(
            claude.verdict,
            Verdict::Eligible,
            "unmetered-local sentinel must never suppress a real metered reading"
        );
        assert!(claude.has_headroom);
    }

    #[test]
    fn local_first_false_gives_pure_headroom_ordering_even_with_metered_local() {
        // Toggle off: no suppression at all, regardless of whether the local
        // reading is metered or the unmetered sentinel.
        let mut cfg = cfg();
        cfg.local_first = false;
        let mut states = HashMap::new();
        states.insert("ollama-local".to_string(), obs(Some(40.0), "manual"));
        states.insert("claude-pro".to_string(), obs(Some(20.0), "manual"));

        let out = classify_all(&cfg, &states, &HashSet::new());
        let claude = out.iter().find(|c| c.provider == "claude-pro").unwrap();
        assert_eq!(claude.verdict, Verdict::Eligible);
        assert!(claude.has_headroom);
    }

    #[test]
    fn empty_local_state_leaves_all_metered_providers_eligible() {
        // No ollama-local observation at all → the policy has nothing to
        // compare against and must never suppress.
        let mut states = HashMap::new();
        states.insert("claude-pro".to_string(), obs(Some(20.0), "manual"));
        states.insert("zai-codeplus".to_string(), obs(Some(30.0), "manual"));

        let out = classify_all(&cfg(), &states, &HashSet::new());
        for provider in ["claude-pro", "zai-codeplus"] {
            let c = out.iter().find(|c| c.provider == provider).unwrap();
            assert_eq!(c.verdict, Verdict::Eligible, "{provider}: {c:?}");
            assert!(c.has_headroom, "{provider}: {c:?}");
        }
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

    // --- sticky limit-hit / stale window-reset regression --------------------
    //
    // Wesley observed a 43-point staleness gap: the CLI reported 44% used on
    // a provider whose window had already reset (the portal showed 1%). The
    // stored percent was measured against a window that has since rolled
    // over; carrying it forward as still-current usage is the sticky bug.

    #[test]
    fn a_reading_whose_window_has_already_reset_becomes_unknown_not_stale_percent() {
        // Simulates the exact incident: a 44%-used observation whose parsed
        // reset timestamp is 10 minutes in the PAST (the window already
        // rolled over — the real post-reset percent, per the provider's own
        // portal, was ~1%). The stale 44% must never be reported as current.
        // `classify` reads the real wall clock internally, so the reset must
        // be anchored to it, not to a synthetic epoch.
        let now = real_now_unix();
        let past_reset = now - 600; // 10 minutes ago
        let note = format!("5h 44% (resets {})", iso_from_unix_for_test(past_reset));
        let stale = Observation {
            percent: Some(44.0),
            source: "server-cache".to_string(),
            note,
            at: "2026-09-01T00:00:00Z".to_string(),
        };
        let c = classify(&cfg(), Some(&stale), "claude-pro", false);
        assert_eq!(
            c.verdict,
            Verdict::Unknown,
            "a reading past its own window reset must not report stale usage"
        );
        assert_eq!(
            c.percent, None,
            "the stale percent must not leak through as current"
        );
    }

    #[test]
    fn a_reading_whose_window_has_not_reset_yet_stays_eligible() {
        // Same shape, but the reset is still 10 minutes in the FUTURE — the
        // reading is current and must classify normally.
        let now = real_now_unix();
        let future_reset = now + 600;
        let note = format!("5h 44% (resets {})", iso_from_unix_for_test(future_reset));
        let fresh = Observation {
            percent: Some(44.0),
            source: "server-cache".to_string(),
            note,
            at: "2026-09-01T00:00:00Z".to_string(),
        };
        let c = classify(&cfg(), Some(&fresh), "claude-pro", false);
        assert_eq!(c.verdict, Verdict::Eligible);
        assert_eq!(c.percent, Some(44.0));
    }

    #[test]
    fn a_fresh_post_reset_reading_recovers_eligibility_after_the_window_rolls() {
        // Full sequence: limit-hit (backoff) → window reset → a fresh
        // collect reports the real low post-reset percent → eligible again.
        // This proves recovery is driven by a NEW observation, not by
        // continuing to trust the pre-reset one.
        let now = real_now_unix();

        // 1. Stale 44% reading from before the reset — must not stick.
        let stale = Observation {
            percent: Some(44.0),
            source: "server-cache".to_string(),
            note: format!("5h 44% (resets {})", iso_from_unix_for_test(now - 600)),
            at: "2026-09-01T00:00:00Z".to_string(),
        };
        let stale_c = classify(&cfg(), Some(&stale), "claude-pro", false);
        assert_eq!(stale_c.verdict, Verdict::Unknown);

        // 2. A fresh collect after the reset reports the real, low percent
        // with a NEW future reset — this must classify as eligible.
        let fresh = Observation {
            percent: Some(1.0),
            source: "server-cache".to_string(),
            note: format!("5h 1% (resets {})", iso_from_unix_for_test(now + 5 * 3600)),
            at: "2026-09-01T00:10:00Z".to_string(),
        };
        let fresh_c = classify(&cfg(), Some(&fresh), "claude-pro", false);
        assert_eq!(fresh_c.verdict, Verdict::Eligible);
        assert_eq!(fresh_c.percent, Some(1.0));
        assert!(fresh_c.has_headroom);
    }

    #[test]
    fn readings_without_a_parseable_reset_note_are_unaffected_by_staleness_check() {
        // manual/credit observations carry no reset note at all — the
        // staleness check must be a no-op for them, not an accidental
        // Unknown downgrade.
        let c = classify(
            &cfg(),
            Some(&obs(Some(44.0), "manual")),
            "claude-pro",
            false,
        );
        assert_eq!(c.verdict, Verdict::Eligible);
        assert_eq!(c.percent, Some(44.0));
    }

    /// Local RFC3339-ish ISO formatter for test notes (mirrors db::iso_from_unix).
    fn iso_from_unix_for_test(unix: i64) -> String {
        let secs = unix.max(0);
        let days = secs / 86400;
        let rem = secs % 86400;
        let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
        let z = days + 719468;
        let era = if z >= 0 { z } else { z - 146096 } / 146097;
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let mth = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = if mth <= 2 { y + 1 } else { y };
        format!("{year:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}+00:00")
    }
}
