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

fn max_limit_percentage(resp: &Value) -> Option<f64> {
    let limits = resp.get("data")?.get("limits")?.as_array()?;
    limits
        .iter()
        .filter_map(|l| l.get("percentage").and_then(|p| p.as_f64()))
        .fold(None, |acc: Option<f64>, v| {
            Some(acc.map_or(v, |a| a.max(v)))
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

    match max_limit_percentage(&resp) {
        Some(pct) => Ok(Some((
            pct,
            "Z.ai direct quota endpoint (max across windows)".to_string(),
        ))),
        None => Err("Z.ai response did not contain a recognized quota percentage".to_string()),
    }
}
