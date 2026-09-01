//! DeepSeek pay-as-you-go collector — balance + rate-limit visibility when
//! DEEPSEEK_API_KEY is set. Returns Ok(None) without a key (not an error).
//!
//! DeepSeek is not a subscription: it is prepaid credit consumed per token, so
//! "usage percent" is modeled as balance burn against a configured comfort
//! floor (deepseek_floor_usd, default $10): percent = 100 × (1 − balance/floor),
//! clamped to [0, 100]. Floor 0 disables the collector.
//!
//! Verified endpoint shapes (api.deepseek.com, Bearer auth):
//!   GET /user/balance → {"is_available":true,"balance_infos":[
//!     {"currency":"CNY","total_balance":"112.50","granted_balance":"0.00",
//!      "topped_up_balance":"112.50"}]}
//!   GET /v1/models → rate-limit headers:
//!     x-ratlmit-limit-requests / -remaining-requests / -reset-requests
//!     x-ratelimit-limit-tokens  / -remaining-tokens  / -reset-tokens

use crate::config::Config;
use serde_json::Value;
use std::env;
use std::time::Duration;

const BASE: &str = "https://api.deepseek.com";

fn get_json(key: &str, path: &str) -> Result<Value, String> {
    let resp: Value = ureq::get(&format!("{BASE}{path}"))
        .set("Authorization", &format!("Bearer {key}"))
        .set("Accept", "application/json")
        .timeout(Duration::from_secs(15))
        .call()
        .map_err(|e| format!("DeepSeek {path} request failed: {e}"))?
        .into_json()
        .map_err(|e| format!("DeepSeek {path} returned unreadable JSON: {e}"))?;
    Ok(resp)
}

fn ratelimit_note(key: &str) -> String {
    // Rate limits ride as headers on any authenticated GET; /v1/models is free.
    match ureq::get(&format!("{BASE}/v1/models"))
        .set("Authorization", &format!("Bearer {key}"))
        .timeout(Duration::from_secs(15))
        .call()
    {
        Ok(resp) => {
            let h = |name: &str| resp.header(name).unwrap_or("-").to_string();
            format!(
                "RPM {}/{} (resets {}s), TPM {}/{} (resets {}s)",
                h("x-ratelimit-remaining-requests"),
                h("x-ratelimit-limit-requests"),
                h("x-ratelimit-reset-requests"),
                h("x-ratelimit-remaining-tokens"),
                h("x-ratelimit-limit-tokens"),
                h("x-ratelimit-reset-tokens")
            )
        }
        Err(_) => "rate-limit headers unavailable".to_string(),
    }
}

pub fn collect(cfg: &Config) -> Result<Option<(f64, String)>, String> {
    let provider = match cfg.providers.get("deepseek") {
        Some(p) => p,
        None => return Ok(None), // not configured — skip silently
    };
    let key_env = provider
        .api_key_env
        .as_deref()
        .unwrap_or("DEEPSEEK_API_KEY");
    let key = match env::var(key_env) {
        Ok(k) if !k.is_empty() => k,
        _ => return Ok(None), // no key — not an error
    };

    // Floor of 0 disables the burn model.
    let floor = provider.floor_usd.unwrap_or(10.0);
    if floor <= 0.0 {
        return Ok(None);
    }

    let balance = get_json(&key, "/user/balance")?;
    let infos = balance
        .get("balance_infos")
        .and_then(Value::as_array)
        .ok_or_else(|| "DeepSeek balance response missing balance_infos".to_string())?;
    let primary = infos
        .first()
        .ok_or_else(|| "DeepSeek balance_infos is empty".to_string())?;
    let total: f64 = primary
        .get("total_balance")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| "DeepSeek total_balance not parseable".to_string())?;
    let currency = primary
        .get("currency")
        .and_then(Value::as_str)
        .unwrap_or("CNY");

    let available = balance
        .get("is_available")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if !available {
        return Ok(Some((
            100.0,
            format!("account flagged unavailable — balance {total:.2} {currency}").to_string(),
        )));
    }

    let pct = (100.0 * (1.0 - total / floor)).clamp(0.0, 100.0);
    let note = format!(
        "balance {total:.2} {currency} vs ${floor:.0} comfort floor; {}",
        ratelimit_note(&key)
    );
    Ok(Some((pct, note)))
}
