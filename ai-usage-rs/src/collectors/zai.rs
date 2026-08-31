//! Z.ai CodePlus quota collector — polls the documented quota endpoint when
//! ZAI_API_KEY is set. Returns None if the key is absent (not an error).

use crate::config::Config;
use serde_json::Value;
use std::env;

pub fn collect(cfg: &Config) -> Result<Option<(f64, String)>, String> {
    let provider = cfg
        .providers
        .get("zai-codeplus")
        .ok_or_else(|| "zai-codeplus not in config".to_string())?;

    let key_env = provider.api_key_env.as_deref().unwrap_or("ZAI_API_KEY");
    let Ok(key) = env::var(key_env) else {
        return Ok(None);
    };

    let endpoint = provider
        .endpoint
        .as_deref()
        .ok_or_else(|| "zai-codeplus endpoint missing from config".to_string())?;

    let resp: Value = ureq::get(endpoint)
        .set("Authorization", &key)
        .set("Accept-Language", "en-US,en;q=0.9")
        .call()
        .map_err(|e| e.to_string())?
        .into_json()
        .map_err(|e| e.to_string())?;

    let raw = resp.get("data").unwrap_or(&resp);
    let mut values = Vec::new();
    for key_name in ["tokenUsage5Hour", "mcpUsage1Month"] {
        if let Some(v) = raw.get(key_name).and_then(|v| v.as_f64()) {
            values.push(v);
        }
    }

    match values.iter().cloned().fold(None, |acc: Option<f64>, v| {
        Some(acc.map_or(v, |a| a.max(v)))
    }) {
        Some(max) => Ok(Some((max, "Z.ai direct quota endpoint".to_string()))),
        None => Err("Z.ai response did not contain a recognized quota percentage".to_string()),
    }
}
