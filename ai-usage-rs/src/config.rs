use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize)]
pub struct Thresholds {
    pub warning: f64,
    pub critical: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub kind: String,
    #[serde(default)]
    pub five_hour_token_budget: Option<u64>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    /// Hard daily token ceiling for this provider's plan (0/absent = no cap).
    /// The budget guard refuses any dispatch whose projected usage would
    /// cross it, so a nominal 10k/day plan cannot silently reach 18k.
    #[serde(default)]
    pub daily_token_budget: Option<u64>,
    /// Hard rolling-7-day ceiling for this provider's plan.
    #[serde(default)]
    pub weekly_token_budget: Option<u64>,
    /// Max concurrent worker lanes allowed on this provider (default 1).
    #[serde(default)]
    pub max_parallel_lanes: Option<u32>,
    // --- credit_balance fields (Ollama Pro & friends) ---
    /// Full monthly dollar pool (credit_balance kind only).
    #[serde(default)]
    pub monthly_pool_dollars: Option<f64>,
    /// Safety net surfaced in status (e.g. auto-reload monthly max $40); the
    /// hard gate is the pool itself.
    #[serde(default)]
    pub reload_monthly_max_dollars: Option<f64>,
    /// When the credit pool resets, RFC3339 UTC (credit_balance kind only).
    #[serde(default)]
    pub reset_at: Option<String>,
    /// Transient 429 backoff TTL in seconds (default 900).
    #[serde(default)]
    pub rate_limit_ttl_secs: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub thresholds: Thresholds,
    pub rotation_order: Vec<String>,
    pub providers: std::collections::HashMap<String, ProviderConfig>,
    /// When true (default), an unmetered local runtime with a verified
    /// reading is preferred over metered providers unless they are far less
    /// used (see recommendation()).
    #[serde(default = "default_true")]
    pub local_first: bool,
}

fn default_true() -> bool {
    true
}

fn ollama_local_config() -> ProviderConfig {
    ProviderConfig {
        kind: "ollama_local".to_string(),
        five_hour_token_budget: None,
        api_key_env: None,
        endpoint: Some("http://localhost:11434/api/tags".to_string()),
        note: Some("Local Ollama models are unmetered.".to_string()),
        daily_token_budget: None,
        weekly_token_budget: None,
        max_parallel_lanes: None,
        monthly_pool_dollars: None,
        reload_monthly_max_dollars: None,
        reset_at: None,
        rate_limit_ttl_secs: None,
    }
}

/// Ollama Pro's real model (2026-09): a $60/month credit balance with a reset
/// date — NOT a rate limit. Transient 429s get a 15-minute backoff TTL that
/// never touches the balance figure.
fn ollama_pro_credit_config() -> ProviderConfig {
    ProviderConfig {
        kind: "credit_balance".to_string(),
        five_hour_token_budget: None,
        api_key_env: None,
        endpoint: None,
        note: Some(
            "Ollama Pro: $60/month credit pool. Observe with `ai-usage credit observe`."
                .to_string(),
        ),
        daily_token_budget: None,
        weekly_token_budget: None,
        max_parallel_lanes: None,
        monthly_pool_dollars: Some(60.0),
        reload_monthly_max_dollars: Some(40.0),
        reset_at: None,
        rate_limit_ttl_secs: None,
    }
}

impl Config {
    pub fn default_config() -> Self {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "claude-pro".to_string(),
            ProviderConfig {
                kind: "claude_local".to_string(),
                // PLACEHOLDER — used only when Hermes OAuth and cache calibration
                // are both unavailable.
                five_hour_token_budget: Some(225_000),
                api_key_env: None,
                endpoint: None,
                note: Some("Exact OAuth quota via Hermes when available.".to_string()),
                daily_token_budget: None,
                weekly_token_budget: None,
                max_parallel_lanes: None,
                monthly_pool_dollars: None,
                reload_monthly_max_dollars: None,
                reset_at: None,
                rate_limit_ttl_secs: None,
            },
        );
        providers.insert(
            "zai-codeplus".to_string(),
            ProviderConfig {
                kind: "zai_quota".to_string(),
                five_hour_token_budget: None,
                api_key_env: Some("ZAI_API_KEY".to_string()),
                endpoint: Some("https://api.z.ai/api/monitor/usage/quota/limit".to_string()),
                note: None,
                daily_token_budget: None,
                weekly_token_budget: None,
                max_parallel_lanes: None,
                monthly_pool_dollars: None,
                reload_monthly_max_dollars: None,
                reset_at: None,
                rate_limit_ttl_secs: None,
            },
        );
        providers.insert(
            "chatgpt-plus".to_string(),
            ProviderConfig {
                kind: "hermes_account_quota".to_string(),
                five_hour_token_budget: None,
                api_key_env: None,
                endpoint: None,
                note: Some("Exact Codex quota via Hermes OAuth when available.".to_string()),
                daily_token_budget: None,
                weekly_token_budget: None,
                max_parallel_lanes: None,
                monthly_pool_dollars: None,
                reload_monthly_max_dollars: None,
                reset_at: None,
                rate_limit_ttl_secs: None,
            },
        );
        providers.insert("ollama-pro".to_string(), ollama_pro_credit_config());
        providers.insert("ollama-local".to_string(), ollama_local_config());

        Config {
            thresholds: Thresholds {
                warning: 90.0,
                critical: 95.0,
            },
            rotation_order: vec![
                "claude-pro".to_string(),
                "zai-codeplus".to_string(),
                "chatgpt-plus".to_string(),
                "ollama-pro".to_string(),
                "ollama-local".to_string(),
            ],
            providers,
            local_first: true,
        }
    }
}

pub fn load_or_init(path: &Path) -> io::Result<Config> {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let cfg = Config::default_config();
        let json = serde_json::to_string_pretty(&cfg).map_err(io::Error::other)?;
        fs::write(path, json + "\n")?;
        return Ok(cfg);
    }
    let text = fs::read_to_string(path)?;
    let mut config: Config = serde_json::from_str(&text).map_err(io::Error::other)?;
    config
        .providers
        .entry("ollama-local".to_string())
        .or_insert_with(ollama_local_config);
    // Credit-model migration: ollama-pro was modeled as percent-of-a-limit
    // ("manual"); it is a $60/month credit balance. Upgrade in place,
    // preserving any explicit note the user set.
    if let Some(p) = config.providers.get_mut("ollama-pro") {
        if p.kind == "manual" {
            let note = p.note.clone();
            *p = ollama_pro_credit_config();
            p.note = note;
        }
    }
    if !config
        .rotation_order
        .iter()
        .any(|provider| provider == "ollama-local")
    {
        config.rotation_order.push("ollama-local".to_string());
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::load_or_init;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn migrates_existing_config_to_include_ollama_local() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ai-usage-config-{unique}"));
        fs::create_dir_all(&root).expect("temp directory");
        let path = root.join("config.json");
        fs::write(
            &path,
            r#"{
              "thresholds":{"warning":90,"critical":95},
              "rotation_order":["claude-pro","ollama-pro"],
              "providers":{
                "claude-pro":{"kind":"claude_local"},
                "ollama-pro":{"kind":"manual"}
              }
            }"#,
        )
        .expect("old config");

        let config = load_or_init(&path).expect("migrated config");
        assert_eq!(
            config
                .providers
                .get("ollama-local")
                .map(|provider| provider.kind.as_str()),
            Some("ollama_local")
        );
        assert!(config
            .rotation_order
            .iter()
            .any(|provider| provider == "ollama-local"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn migrates_ollama_pro_from_manual_to_credit_balance() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ai-usage-credit-{unique}"));
        fs::create_dir_all(&root).expect("temp directory");
        let path = root.join("config.json");
        fs::write(
            &path,
            r#"{
              "thresholds":{"warning":90,"critical":95},
              "rotation_order":["ollama-pro"],
              "providers":{"ollama-pro":{"kind":"manual"}}
            }"#,
        )
        .expect("old config");

        let config = load_or_init(&path).expect("migrated config");
        let p = config.providers.get("ollama-pro").expect("ollama-pro");
        assert_eq!(p.kind, "credit_balance");
        assert_eq!(p.monthly_pool_dollars, Some(60.0));
        assert_eq!(p.reload_monthly_max_dollars, Some(40.0));

        // Migration is idempotent: a credit_balance config loads unchanged.
        let json = serde_json::to_string_pretty(&config).unwrap();
        fs::write(&path, json).unwrap();
        let again = load_or_init(&path).expect("reload");
        assert_eq!(
            again.providers.get("ollama-pro").unwrap().kind,
            "credit_balance"
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn credit_fields_default_absent_and_config_parses_without_them() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ai-usage-creditttl-{unique}"));
        fs::create_dir_all(&root).expect("temp directory");
        let path = root.join("config.json");
        fs::write(
            &path,
            r#"{
              "thresholds":{"warning":90,"critical":95},
              "rotation_order":["ollama-pro"],
              "providers":{"ollama-pro":{"kind":"manual"}}
            }"#,
        )
        .expect("old config");
        let config = load_or_init(&path).expect("config");
        let p = config.providers.get("ollama-pro").expect("provider");
        assert_eq!(p.kind, "credit_balance");
        assert_eq!(p.rate_limit_ttl_secs, None, "absent → CLI default 900");
        fs::remove_dir_all(root).expect("cleanup");
    }
}
