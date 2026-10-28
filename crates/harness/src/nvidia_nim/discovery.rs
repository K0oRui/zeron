//! Classify only declared capabilities and known non-chat families. Unknown
//! chat models remain discoverable; featured membership is not an allow-list.

use serde_json::Value;

/// Explicit wire metadata takes precedence over ID heuristics. `None` means
/// the provider supplied no usable type information, not that chat is verified.
pub(super) fn chat_support(row: &Value) -> Option<bool> {
    if ["available", "active", "enabled"]
        .iter()
        .any(|key| row.get(*key).and_then(Value::as_bool) == Some(false))
    {
        return Some(false);
    }
    if let Some(status) = row.get("status").and_then(Value::as_str)
        && matches!(
            status.to_ascii_lowercase().as_str(),
            "retired" | "disabled" | "unavailable" | "deleted" | "inactive" | "offline"
        )
    {
        return Some(false);
    }
    let chat_capability = ["chat", "chat_completions", "chat-completions"]
        .iter()
        .find_map(|key| {
            row.get("capabilities")
                .and_then(|caps| caps.get(*key))
                .and_then(Value::as_bool)
        });
    if chat_capability == Some(false) {
        return Some(false);
    }
    if let Some(endpoint) = row.get("endpoint").and_then(Value::as_str)
        && let Some(chat) = endpoint_support(endpoint)
    {
        return Some(chat);
    }
    if let Some(endpoints) = row
        .get("supported_endpoints")
        .or_else(|| row.get("endpoints"))
        .and_then(Value::as_array)
    {
        let support: Vec<_> = endpoints
            .iter()
            .filter_map(Value::as_str)
            .filter_map(endpoint_support)
            .collect();
        if support.contains(&true) {
            return Some(true);
        }
        if !support.is_empty() && support.len() == endpoints.len() {
            return Some(false);
        }
    }
    if chat_capability == Some(true) {
        return Some(true);
    }
    for key in ["task", "pipeline_tag", "model_type", "model-type"] {
        let Some(task) = row.get(key).and_then(Value::as_str) else {
            continue;
        };
        let task = task.trim().to_ascii_lowercase().replace('_', "-");
        match task.as_str() {
            "chat"
            | "chat-completion"
            | "chat-completions"
            | "conversational"
            | "text-generation"
            | "text2text-generation"
            | "image-text-to-text" => return Some(true),
            "embedding"
            | "embeddings"
            | "text-embedding"
            | "feature-extraction"
            | "sentence-similarity"
            | "ranking"
            | "ranking-vlm"
            | "rerank"
            | "reranking"
            | "reward"
            | "image-generation"
            | "text-to-image"
            | "image-to-image"
            | "text-to-video"
            | "image-to-video"
            | "text-to-speech"
            | "text-to-audio"
            | "automatic-speech-recognition"
            | "speech-to-text"
            | "audio-to-audio"
            | "audio-classification"
            | "voice-conversion"
            | "asr"
            | "tts"
            | "image-classification"
            | "zero-shot-image-classification"
            | "object-detection"
            | "image-segmentation"
            | "depth-estimation"
            | "protein-folding"
            | "document-parsing"
            | "ocr"
            | "completions"
            | "text-completion" => return Some(false),
            _ => {} // Architecture names (e.g. llama) are not task declarations.
        }
    }
    None
}

fn endpoint_support(endpoint: &str) -> Option<bool> {
    let endpoint = endpoint
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .trim_end_matches('/');
    if endpoint.ends_with("/chat/completions") || endpoint == "chat/completions" {
        return Some(true);
    }
    if [
        "/embeddings",
        "/ranking",
        "/rerank",
        "/images",
        "/audio",
        "/genai/",
        "/vlm/",
    ]
    .iter()
    .any(|path| endpoint.contains(path))
        || endpoint.ends_with("/completions")
    {
        return Some(false);
    }
    None
}

pub(super) fn is_chat_model(id: &str, row: &Value) -> bool {
    if let Some(chat) = chat_support(row) {
        return chat;
    }
    let id = id.to_ascii_lowercase();
    ![
        "embed",
        "rerank",
        "ranking",
        "reward",
        "nvclip",
        "parakeet",
        "riva-",
        "stable-diffusion",
        "black-forest-labs/flux",
        "stabilityai/",
    ]
    .iter()
    .any(|family| id.contains(family))
}

/// Exact, dated exclusions for NVIDIA's public hosted route, not a model
/// allow-list or a claim that the weights/other deployments are retired.
/// 2026-10-28: the Palmyra hosted page below returns 404, matching the reported
/// function/account-not-found response. Do not apply this to self-hosted NIM.
/// https://build.nvidia.com/writer/palmyra-fin-70b-32k
/// An actual successful completion may override this exclusion for its key.
pub(super) fn known_unavailable_on_hosted(base_url: &str, model: &str) -> bool {
    is_public_hosted(base_url) && model == "writer/palmyra-fin-70b-32k"
}

pub(super) fn is_public_hosted(base_url: &str) -> bool {
    let base_url = base_url.trim().trim_end_matches('/');
    let base_url = base_url.strip_suffix("/v1").unwrap_or(base_url);
    base_url.eq_ignore_ascii_case("https://integrate.api.nvidia.com")
        || base_url.eq_ignore_ascii_case("https://integrate.api.nvidia.com:443")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn palmyra_exclusion_is_exact_and_public_hosted_only() {
        let model = "writer/palmyra-fin-70b-32k";
        for base in [
            "https://integrate.api.nvidia.com",
            "https://integrate.api.nvidia.com/v1/",
            "https://integrate.api.nvidia.com:443",
            "https://INTEGRATE.API.NVIDIA.COM/",
        ] {
            assert!(known_unavailable_on_hosted(base, model), "{base}");
            assert!(!known_unavailable_on_hosted(
                base,
                "writer/new-finance-chat"
            ));
        }
        for base in [
            "http://localhost:8000",
            "https://private.nim.example/v1",
            "https://integrate.api.nvidia.com:8443",
            "https://integrate.api.nvidia.com/private",
            "https://integrate.api.nvidia.com.evil.example",
            "https://integrate.api.nvidia.com?tenant=x",
        ] {
            assert!(!known_unavailable_on_hosted(base, model), "{base}");
        }
    }

    #[test]
    fn new_models_do_not_need_to_be_featured_or_statically_known() {
        assert!(is_chat_model("fixture/new-chat", &json!({})));
        assert!(is_chat_model(
            "fixture/architecture",
            &json!({"model_type":"llama"})
        ));
        assert!(is_chat_model(
            "fixture/embed-aware-chat",
            &json!({"capabilities":{"chat":true}})
        ));
        assert!(!is_chat_model("fixture/embed", &json!({})));
    }

    #[test]
    fn declared_non_chat_tasks_are_excluded_without_guessing_from_the_name() {
        for task in [
            "ranking",
            "ranking-vlm",
            "feature_extraction",
            "text-to-video",
            "image-to-image",
            "object-detection",
            "audio-classification",
            "protein-folding",
            "completions",
            "ocr",
        ] {
            assert!(
                !is_chat_model("fixture/arbitrary-name", &json!({"task":task})),
                "{task}"
            );
        }
        assert!(!is_chat_model(
            "fixture/chat",
            &json!({"capabilities":{"chat":false}})
        ));
        assert!(is_chat_model(
            "fixture/vision-chat",
            &json!({"pipeline_tag":"image-text-to-text"})
        ));
    }

    #[test]
    fn explicit_unavailability_is_not_overridden_by_chat_capability() {
        assert!(!is_chat_model(
            "fixture/chat",
            &json!({"available":false,"capabilities":{"chat":true}})
        ));
        for status in ["retired", "UNAVAILABLE", "deleted", "inactive"] {
            assert!(!is_chat_model("fixture/chat", &json!({"status":status})));
        }
        // Deprecation alone is not evidence that a still-live model cannot run.
        assert!(is_chat_model(
            "fixture/chat",
            &json!({"deprecated":true,"available":true})
        ));
    }

    #[test]
    fn endpoints_requiring_another_wire_protocol_are_not_chat_completions() {
        for endpoint in [
            "/v1/embeddings",
            "/v1/ranking",
            "/v1/vlm/vendor/model",
            "/v1/completions",
            "/v1/audio/transcriptions",
        ] {
            let row = json!({"endpoint":endpoint});
            assert!(!is_chat_model("fixture/model", &row), "{endpoint}");
        }
        assert!(!is_chat_model(
            "fixture/chat",
            &json!({"capabilities":{"chat":true},"endpoint":"/v1/embeddings"})
        ));
        assert!(is_chat_model(
            "fixture/chat",
            &json!({"endpoint":"https://example.test/v1/chat/completions"})
        ));
        assert_eq!(
            chat_support(&json!({"supported_endpoints":["/v1/embeddings"]})),
            Some(false)
        );
        assert_eq!(
            chat_support(&json!({"supported_endpoints":["/v1/embeddings","/v1/chat/completions"]})),
            Some(true)
        );
    }
}
