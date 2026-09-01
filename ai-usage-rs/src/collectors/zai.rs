//! Z.ai quota collector — polls the documented quota endpoint when ZAI_API_KEY
//! (or GLM_API_KEY) is set. Returns None if no key is present (not an error).
//!
//! Live response shape (verified 2026-08-31):
//! {"code":200,"data":{"limits":[{"type":"CREDIT_LIMIT","unit":3,"number":5,
//!   "percentage":21,"nextResetTime":1788168033973,...}, ...],"level":"lite"}}
//! unit 3 = 5-hour window, unit 6 = monthly. We report the max percentage across
//! limits — whichever window binds first is what routing must respect.

use crate::config::Config;
use serde_json::Value;
use std::env;

fn window_label(unit: i64) -> &'static str {
    match unit {
        3 => "5h",
        6 => "weekly",
        _ => "window",
    }
}

fn fmt_reset(ms: &Value) -> String {
    ms.as_i64()
        .map(|ms| {
            // chrono is not a dependency here; format UTC ISO manually via seconds math
            let secs = ms / 1000;
            let days = secs / 86400;
            let rem = secs % 86400;
            let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
            // civil-from-days (Howard Hinnant algorithm) — small, no external crate
            let z = days + 719468;
            let era = z.div_euclid(146097);
            let doe = z.rem_euclid(146097);
            let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
            let y = yoe + era * 400;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let d = doy - (153 * mp + 2) / 5 + 1;
            let m = if mp < 10 { mp + 3 } else { mp - 9 };
            let y = if m <= 2 { y + 1 } else { y };
            format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}Z")
        })
        .unwrap_or_else(|| "?".to_string())
}

/// (max_percentage, per-window detail string)
fn summarize_limits(resp: &Value) -> Option<(f64, String)> {
    let limits = resp.get("data")?.get("limits")?.as_array()?;
    let mut max: Option<f64> = None;
    let mut parts: Vec<String> = Vec::new();
    for l in limits {
        let Some(pct) = l.get("percentage").and_then(|p| p.as_f64()) else {
            continue;
        };
        let unit = l.get("unit").and_then(|u| u.as_i64()).unwrap_or(-1);
        let reset = fmt_reset(l.get("nextResetTime").unwrap_or(&Value::Null));
        parts.push(format!(
            "{} {:.1}% (resets {})",
            window_label(unit),
            pct,
            reset
        ));
        max = Some(max.map_or(pct, |m| m.max(pct)));
    }
    max.map(|m| {
        let detail = if parts.is_empty() {
            "Z.ai direct quota endpoint (max across windows)".to_string()
        } else {
            format!("{} — binding window governs routing", parts.join("; "))
        };
        (m, detail)
    })
}

pub fn collect(cfg: &Config) -> Result<Option<(f64, String)>, String> {
    let provider = cfg
        .providers
        .get("zai-codeplus")
        .ok_or_else(|| "zai-codeplus not in config".to_string())?;

    let key_env = provider.api_key_env.as_deref().unwrap_or("ZAI_API_KEY");
    let key = match env::var(key_env).or_else(|_| env::var("GLM_API_KEY")) {
        Ok(k) => k,
        Err(_) => return Ok(None), // no key configured — not an error
    };

    let endpoint = provider
        .endpoint
        .as_deref()
        .ok_or_else(|| "zai-codeplus endpoint missing from config".to_string())?;

    // The API key is sent to this URL — anchor it to the real host so a
    // tampered config can't redirect the key to an attacker-controlled server.
    if !endpoint.starts_with("https://api.z.ai/") {
        return Err(format!(
            "zai-codeplus endpoint must be under https://api.z.ai/, got: {endpoint}"
        ));
    }

    let resp: Value = ureq::get(endpoint)
        .set("Authorization", &key)
        .set("Accept-Language", "en-US,en;q=0.9")
        .timeout(std::time::Duration::from_secs(15))
        .call()
        .map_err(|e| e.to_string())?
        .into_json()
        .map_err(|e| e.to_string())?;

    match summarize_limits(&resp) {
        Some((pct, detail)) => Ok(Some((pct, detail))),
        None => Err("Z.ai response did not contain a recognized quota percentage".to_string()),
    }
}
