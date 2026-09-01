//! Unit tests for the `unavailable` observation semantics: collectors that
//! hard-fail must leave an explicit NULL-percent marker so "quota source
//! down" never masquerades as "never checked" or "0% used".

#[path = "sandbox_guard.rs"]
mod tempfile_guard;

use std::process::Command;

const EXE: &str = env!("CARGO_BIN_EXE_ai-usage");

#[test]
fn collect_records_explicit_unavailable_for_dead_local_collector() {
    let dir = tempfile_guard::TempDir::create("unavailable");
    let root = dir.path.clone();
    let config = root.join("config.json");
    let db = root.join("usage.sqlite3");

    // Point the ollama-local collector at a dead loopback port via the
    // sandbox config so the whole run is hermetic (no live services).
    std::fs::write(
        &config,
        r#"{
          "thresholds": {"warning": 90, "critical": 95},
          "rotation_order": ["claude-pro", "zai-codeplus", "chatgpt-plus", "ollama-pro", "ollama-local"],
          "providers": {
            "claude-pro": {"kind": "claude_local", "five_hour_token_budget": 225000},
            "zai-codeplus": {"kind": "zai_quota", "api_key_env": "ZAI_API_KEY", "endpoint": "https://api.z.ai/api/monitor/usage/quota/limit"},
            "chatgpt-plus": {"kind": "hermes_account_quota"},
            "ollama-pro": {"kind": "manual"},
            "ollama-local": {"kind": "ollama_local", "endpoint": "http://localhost:9/api/tags"}
          }
        }"#,
    )
    .expect("sandbox config");

    let out = Command::new(EXE)
        .args(["collect"])
        .env("AI_USAGE_CONFIG_PATH", &config)
        .env("AI_USAGE_DB_PATH", &db)
        .env_remove("ZAI_API_KEY")
        .env_remove("GLM_API_KEY")
        .output()
        .expect("binary runs");
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();

    // Dead ollama-local must surface as an explicit unavailable reading.
    assert!(
        stdout.contains("unavailable"),
        "status should show unavailable marker: {stdout}"
    );
    assert!(
        stdout.contains("Ollama local runtime is unavailable"),
        "unavailable marker carries the collector error: {stdout}"
    );
}
