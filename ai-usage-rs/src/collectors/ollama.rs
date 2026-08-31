//! Ollama local-capacity collector.
//!
//! Ollama exposes no aggregate subscription quota. Local models are unmetered,
//! though, so a reachable loopback daemon with at least one non-cloud model is
//! a verified fallback with full headroom. Cloud-tagged models remain separate
//! because they can have subscription limits.

use serde_json::Value;
use std::time::Duration;

const OLLAMA_TAGS_URL: &str = "http://localhost:11434/api/tags";

#[derive(Debug, PartialEq)]
pub struct LocalSnapshot {
    pub percent: f64,
    pub source: String,
    pub note: String,
}

pub(crate) fn parse_tags(body: &str) -> Result<LocalSnapshot, String> {
    let payload: Value = serde_json::from_str(body)
        .map_err(|_| "Ollama returned an invalid tags response".to_string())?;
    let models = payload
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| "Ollama tags response did not include models".to_string())?;

    let mut local_count = 0usize;
    let mut cloud_count = 0usize;
    for model in models {
        let Some(name) = model.get("name").and_then(Value::as_str) else {
            continue;
        };
        if name.ends_with(":cloud") {
            cloud_count += 1;
        } else {
            local_count += 1;
        }
    }
    if local_count == 0 {
        return Err("Ollama is reachable but has no local models available".to_string());
    }

    let local_label = if local_count == 1 {
        "local model"
    } else {
        "local models"
    };
    let cloud_label = if cloud_count == 1 {
        "cloud model"
    } else {
        "cloud models"
    };
    Ok(LocalSnapshot {
        percent: 0.0,
        source: "ollama-local-unlimited".to_string(),
        note: format!(
            "local runtime reachable; {local_count} {local_label} (unmetered), {cloud_count} {cloud_label}"
        ),
    })
}

pub fn collect() -> Result<LocalSnapshot, String> {
    let body = ureq::get(OLLAMA_TAGS_URL)
        .timeout(Duration::from_secs(5))
        .call()
        .map_err(|_| "Ollama local runtime is unavailable on localhost:11434".to_string())?
        .into_string()
        .map_err(|_| "Ollama returned an unreadable tags response".to_string())?;
    parse_tags(&body)
}

#[cfg(test)]
mod tests {
    use super::parse_tags;

    #[test]
    fn reports_local_ollama_as_unlimited_when_models_are_available() {
        let payload = r#"{
          "models": [
            {"name":"qwen3-30b-64k:latest"},
            {"name":"glm-5.2:cloud"},
            {"name":"nomic-embed-text:latest"}
          ]
        }"#;

        let snapshot = parse_tags(payload).expect("valid Ollama tags response");
        assert_eq!(snapshot.percent, 0.0);
        assert_eq!(snapshot.source, "ollama-local-unlimited");
        assert!(snapshot.note.contains("2 local models"));
        assert!(snapshot.note.contains("1 cloud model"));
        assert!(snapshot.note.contains("local runtime reachable"));
    }

    #[test]
    fn rejects_responses_without_a_models_array() {
        assert!(parse_tags(r#"{"error":"not found"}"#).is_err());
    }
}
