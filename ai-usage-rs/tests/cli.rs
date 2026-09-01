//! End-to-end CLI tests. Every test runs the real binary against isolated
//! AI_USAGE_CONFIG_PATH / AI_USAGE_DB_PATH locations so tests never touch the
//! user's real config or usage database.

#[path = "sandbox_guard.rs"]
mod tempfile_guard;

use std::process::Command;
use tempfile_guard::TempDir;

const EXE: &str = env!("CARGO_BIN_EXE_ai-usage");

struct Sandbox {
    _dir: TempDir,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        Self {
            _dir: TempDir::create(tag),
        }
    }
}

fn run(args: &[&str], config: &std::path::Path, db: &std::path::Path) -> (i32, String, String) {
    let out = Command::new(EXE)
        .args(args)
        .env("AI_USAGE_CONFIG_PATH", config)
        .env("AI_USAGE_DB_PATH", db)
        .env_remove("ZAI_API_KEY")
        .env_remove("GLM_API_KEY")
        .output()
        .expect("binary runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn sandbox_paths(tag: &str) -> (Sandbox, std::path::PathBuf, std::path::PathBuf) {
    let sandbox = Sandbox::new(tag);
    let root = sandbox._dir.path.clone();
    (
        sandbox,
        root.join("config.json"),
        root.join("usage.sqlite3"),
    )
}

fn recommend_json(cfg: &std::path::Path, db: &std::path::Path) -> serde_json::Value {
    let (code, stdout, stderr) = run(&["recommend", "--json"], cfg, db);
    assert_eq!(code, 0, "recommend --json failed: {stderr}");
    serde_json::from_str(&stdout).expect("recommend --json emits JSON")
}

#[test]
fn fresh_database_never_recommends_an_unknown_provider() {
    let (_s, cfg, db) = sandbox_paths("fresh");

    let (code, _, _) = run(&["status"], &cfg, &db);
    assert_eq!(code, 0);

    let parsed = recommend_json(&cfg, &db);
    assert_eq!(parsed["recommended"], serde_json::Value::Null);
}

#[test]
fn observe_and_recommend_round_trip_reports_verified_headroom() {
    let (_s, cfg, db) = sandbox_paths("roundtrip");

    run(&["status"], &cfg, &db);

    let (code, stdout, _) = run(
        &["observe", "chatgpt-plus", "30", "--note", "cli test"],
        &cfg,
        &db,
    );
    assert_eq!(code, 0);
    assert!(stdout.contains("recorded"));

    let parsed = recommend_json(&cfg, &db);
    assert_eq!(parsed["recommended"], "chatgpt-plus");
    let chatgpt = parsed["candidates"]
        .as_array()
        .expect("candidates array")
        .iter()
        .find(|c| c["provider"] == "chatgpt-plus")
        .expect("chatgpt-plus among candidates")
        .clone();
    assert_eq!(chatgpt["has_headroom"], true);
    assert_eq!(chatgpt["verdict"], "eligible");
}

#[test]
fn limit_hit_marks_provider_and_recommendation_routes_around_it() {
    let (_s, cfg, db) = sandbox_paths("limithit");

    run(&["status"], &cfg, &db);
    run(
        &["observe", "claude-pro", "20", "--note", "cli test"],
        &cfg,
        &db,
    );

    let parsed = recommend_json(&cfg, &db);
    assert_eq!(parsed["recommended"], "claude-pro");

    let (code, _, _) = run(
        &["limit-hit", "claude-pro", "--note", "cli test 429"],
        &cfg,
        &db,
    );
    assert_eq!(code, 0);

    let parsed = recommend_json(&cfg, &db);
    assert_eq!(parsed["recommended"], serde_json::Value::Null);

    let status = run(&["status"], &cfg, &db).1;
    assert!(
        status.contains("claude-pro"),
        "status lists claude-pro: {status}"
    );
    assert!(
        status.contains("limit-hit"),
        "status shows source: {status}"
    );
}

#[test]
fn recommend_json_exposes_every_provider_with_explicit_verdicts() {
    let (_s, cfg, db) = sandbox_paths("verdicts");

    // Sandbox config FIRST so `collect` is hermetic for the local collector:
    // a dead ollama endpoint must produce an explicit `unavailable` marker
    // instead of silently skipping the provider. (Hermes/zai collectors may
    // hit real local services; the observes below override those readings.)
    std::fs::write(
        &cfg,
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

    let (_, stdout, _) = run(&["collect"], &cfg, &db);
    assert!(
        stdout.contains("unavailable"),
        "status after collect mentions unavailable state: {stdout}"
    );
    run(
        &["observe", "claude-pro", "20", "--note", "cli test"],
        &cfg,
        &db,
    );
    run(
        &["observe", "chatgpt-plus", "25", "--note", "cli test"],
        &cfg,
        &db,
    );
    run(
        &["observe", "zai-codeplus", "97", "--note", "cli test"],
        &cfg,
        &db,
    );

    let parsed = recommend_json(&cfg, &db);
    let candidates = parsed["candidates"].as_array().expect("candidates array");

    // EVERY rotation-order provider appears — unknown/unavailable states are
    // explicit, never silently dropped.
    assert_eq!(candidates.len(), 5, "all providers listed: {parsed}");
    let verdict_of = |provider: &str| -> String {
        candidates
            .iter()
            .find(|c| c["provider"] == provider)
            .map(|c| c["verdict"].as_str().unwrap_or("<missing>").to_string())
            .unwrap_or_else(|| "<missing>".to_string())
    };

    assert_eq!(verdict_of("claude-pro"), "eligible");
    assert_eq!(verdict_of("chatgpt-plus"), "eligible");
    assert_eq!(verdict_of("zai-codeplus"), "exhausted");
    assert_eq!(verdict_of("ollama-local"), "unavailable");
    assert_eq!(verdict_of("ollama-pro"), "unknown");

    // Only eligible-with-headroom providers can be recommended; claude-pro
    // (20% used → 80% headroom) beats chatgpt-plus (25% used).
    assert_eq!(parsed["recommended"], "claude-pro");
    assert_eq!(parsed["excluded"]["zai-codeplus"], "exhausted");
    assert_eq!(parsed["excluded"]["ollama-local"], "unavailable");
    assert_eq!(parsed["excluded"]["ollama-pro"], "unknown");
}

#[test]
fn recommend_json_excludes_providers_suppressed_by_local_first_policy() {
    let (_s, cfg, db) = sandbox_paths("localfirst-excluded");

    run(&["status"], &cfg, &db);
    run(
        &["observe", "ollama-local", "40", "--note", "cli test local"],
        &cfg,
        &db,
    );
    run(
        &["observe", "claude-pro", "20", "--note", "cli test"],
        &cfg,
        &db,
    );

    // Policy: 20 + 25 = 45 > 40 → claude-pro suppressed; ollama-local wins.
    let parsed = recommend_json(&cfg, &db);
    assert_eq!(parsed["recommended"], "ollama-local");

    // A machine consumer must never see a policy-refused provider as
    // dispatchable: the suppressed provider's verdict is `local-first` with
    // no headroom, and it appears in `excluded`.
    let claude = parsed["candidates"]
        .as_array()
        .expect("candidates array")
        .iter()
        .find(|c| c["provider"] == "claude-pro")
        .expect("claude-pro among candidates")
        .clone();
    assert_eq!(
        claude["verdict"], "local-first",
        "verdict surface: {parsed}"
    );
    assert_eq!(claude["has_headroom"], false);
    assert_eq!(parsed["excluded"]["claude-pro"], "local-first");
}

#[test]
fn local_first_prefers_unmetered_runtime_when_metered_options_are_not_fresher() {
    let (_s, cfg, db) = sandbox_paths("localfirst");

    let (_, stdout, _) = run(&["collect"], &cfg, &db);
    assert!(stdout.contains("ollama-local"), "collect ran: {stdout}");
    run(
        &["observe", "ollama-local", "0", "--note", "cli test local"],
        &cfg,
        &db,
    );
    run(
        &["observe", "claude-pro", "45", "--note", "cli test"],
        &cfg,
        &db,
    );

    // claude-pro at 45% has headroom, but ollama-local (0%, unmetered,
    // local-first policy) must win the plain recommend.
    let (_, stdout, _) = run(&["recommend"], &cfg, &db);
    assert!(
        stdout.contains("ollama-local"),
        "local-first routes to the unmetered runtime: {stdout}"
    );
    assert!(
        !stdout.contains("claude-pro ("),
        "metered provider suppressed by local-first: {stdout}"
    );
}

#[test]
fn local_first_can_be_disabled_in_config() {
    let (_s, cfg, db) = sandbox_paths("localfirst-off");

    std::fs::write(
        &cfg,
        r#"{
          "thresholds": {"warning": 90, "critical": 95},
          "local_first": false,
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

    // Discriminating scenario for the toggle: local at 20% (used, not fresh),
    // claude at 18% (less used, but within the suppression window — a plain
    // local-first policy would still suppress claude and pick local... no:
    // with the toggle OFF, pure percent ranking must pick claude-pro at 18%).
    run(
        &["observe", "ollama-local", "20", "--note", "cli test"],
        &cfg,
        &db,
    );
    run(
        &["observe", "claude-pro", "18", "--note", "cli test"],
        &cfg,
        &db,
    );

    // local_first=false → pure percent ranking: claude-pro (18%) wins.
    let parsed = recommend_json(&cfg, &db);
    assert_eq!(parsed["recommended"], "claude-pro");
}
