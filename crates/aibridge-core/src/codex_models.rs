//! v0.29 (O1): read codex's OWN live model cache to populate the review-model
//! selector with codex-usable models — no hardcoded list, no provider API, no auth
//! reading. codex refreshes `$CODEX_HOME/models_cache.json` (else
//! `~/.codex/models_cache.json`) itself; we extract only the few fields the selector
//! needs and ignore the large embedded prompt / `base_instructions` blobs.

use serde_json::Value;
use std::path::{Path, PathBuf};

/// One selectable model from codex's cache (only the fields the selector needs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub slug: String,
    pub display_name: String,
    pub context_window: Option<u64>,
    pub priority: i64,
}

/// Only surface slugs we could actually PERSIST + emit as `-c model="<slug>"` (matches
/// `review_mcp`'s validation charset). A future/malformed cache entry with an unusual
/// slug is dropped from the selector rather than shown-then-rejected on save.
fn is_listable_slug(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// `$CODEX_HOME/models_cache.json` else `~/.codex/models_cache.json`.
pub fn models_cache_path() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("CODEX_HOME") {
        return Some(Path::new(&h).join("models_cache.json"));
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    Some(Path::new(&home).join(".codex").join("models_cache.json"))
}

/// Parse codex's models_cache.json: keep only `visibility == "list"` models, extract
/// `{slug, display_name, context_window, priority}` (ignoring the huge
/// `base_instructions`/personality blobs), sorted by `priority` DESC then `slug` ASC.
/// Malformed / missing `models` → empty.
pub fn parse_models_cache(json: &str) -> Vec<ModelInfo> {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let Some(models) = v.get("models").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out: Vec<ModelInfo> = models
        .iter()
        .filter(|m| m.get("visibility").and_then(Value::as_str) == Some("list"))
        .filter_map(|m| {
            let slug = m
                .get("slug")
                .and_then(Value::as_str)
                .filter(|s| is_listable_slug(s))?
                .to_string();
            let display_name = m
                .get("display_name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(&slug)
                .to_string();
            let context_window = m.get("context_window").and_then(Value::as_u64);
            let priority = m.get("priority").and_then(Value::as_i64).unwrap_or(0);
            Some(ModelInfo {
                slug,
                display_name,
                context_window,
                priority,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| a.slug.cmp(&b.slug))
    });
    out
}

/// Read + parse the live model cache. Empty when missing / unreadable / malformed —
/// the selector then falls back to a custom free-text model id (O1c).
pub fn list_models() -> Vec<ModelInfo> {
    models_cache_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| parse_models_cache(&s))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors the real cache shape: list + hidden models, a huge blob to ignore, a
    // model missing context_window, and a priority tie to exercise the slug tiebreak.
    const SAMPLE: &str = r#"{
      "fetched_at": "2026-05-29T01:27:46Z",
      "models": [
        {"slug":"gpt-5.5","display_name":"GPT-5.5","visibility":"list","priority":9,"context_window":272000,"base_instructions":"...HUGE PROMPT BLOB..."},
        {"slug":"gpt-5.4","display_name":"gpt-5.4","visibility":"list","priority":7,"context_window":272000},
        {"slug":"internal-x","display_name":"Internal","visibility":"hidden","priority":99},
        {"slug":"gpt-4.x","display_name":"GPT-4.x","visibility":"list","priority":9}
      ]
    }"#;

    #[test]
    fn parse_keeps_list_only_sorts_priority_then_slug_and_ignores_blob() {
        let m = parse_models_cache(SAMPLE);
        // hidden model filtered out
        assert!(m.iter().all(|x| x.slug != "internal-x"), "hidden excluded");
        // priority 9 first (gpt-4.x, gpt-5.5 tie → slug ASC), then priority 7 gpt-5.4
        let slugs: Vec<&str> = m.iter().map(|x| x.slug.as_str()).collect();
        assert_eq!(slugs, vec!["gpt-4.x", "gpt-5.5", "gpt-5.4"]);
        // extracted fields; context_window optional
        let g55 = m.iter().find(|x| x.slug == "gpt-5.5").unwrap();
        assert_eq!(g55.display_name, "GPT-5.5");
        assert_eq!(g55.context_window, Some(272_000));
        let g4 = m.iter().find(|x| x.slug == "gpt-4.x").unwrap();
        assert_eq!(g4.context_window, None, "missing context_window → None");
    }

    #[test]
    fn parse_malformed_or_empty_is_empty() {
        assert!(parse_models_cache("not json").is_empty());
        assert!(parse_models_cache("{}").is_empty());
        assert!(parse_models_cache(r#"{"models":[]}"#).is_empty());
        // a model with no slug is skipped, not a panic
        assert!(parse_models_cache(r#"{"models":[{"visibility":"list"}]}"#).is_empty());
    }

    #[test]
    fn parse_drops_unpersistable_slugs() {
        // a listed model whose slug we couldn't persist/emit (`-c model="…"`) is dropped
        // from the selector rather than shown-then-rejected on save.
        let json = r#"{"models":[
            {"slug":"weird slug!","display_name":"Weird","visibility":"list","priority":5},
            {"slug":"gpt-5.5","display_name":"GPT-5.5","visibility":"list","priority":9}
        ]}"#;
        let m = parse_models_cache(json);
        let slugs: Vec<&str> = m.iter().map(|x| x.slug.as_str()).collect();
        assert_eq!(slugs, vec!["gpt-5.5"], "unpersistable slug filtered out");
    }
}
