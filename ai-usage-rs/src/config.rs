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
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub thresholds: Thresholds,
    pub rotation_order: Vec<String>,
    pub providers: std::collections::HashMap<String, ProviderConfig>,
}

impl Config {
    pub fn default_config() -> Self {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "claude-pro".to_string(),
            ProviderConfig {
                kind: "claude_local".to_string(),
                // PLACEHOLDER — Anthropic publishes no numeric limit for Pro/Max.
                // Calibrate: run `/usage` in Claude Code, compare against
                // `ai-usage collect` raw token count at the same moment, back-solve.
                five_hour_token_budget: Some(225_000),
                api_key_env: None,
                endpoint: None,
                note: None,
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
            },
        );
        providers.insert(
            "chatgpt-plus".to_string(),
            ProviderConfig {
                kind: "manual".to_string(),
                five_hour_token_budget: None,
                api_key_env: None,
                endpoint: None,
                note: Some(
                    "ChatGPT consumer subscriptions have no supported usage API.".to_string(),
                ),
            },
        );
        providers.insert(
            "ollama-pro".to_string(),
            ProviderConfig {
                kind: "manual".to_string(),
                five_hour_token_budget: None,
                api_key_env: None,
                endpoint: None,
                note: Some(
                    "Ollama cloud subscription usage has no documented quota endpoint.".to_string(),
                ),
            },
        );

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
            ],
            providers,
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
    serde_json::from_str(&text).map_err(io::Error::other)
}
