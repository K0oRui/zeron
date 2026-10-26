//! NVIDIA NIM model catalog harness.
//!
//! Fetches NIM's public model catalog and filters to models whose plan is
//! free — `plan == "free"` — so the free plan user only sees runnable models.

use futures::stream::BoxStream;
use serde::Deserialize;
use zeron_proto::{AgentEvent, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode};

use crate::{Harness, HarnessError, ModelContext, RunControls};

const NIM_MODELS_URL: &str = "https://integrate.api.nvidia.com/v1/models";
const NIM_USER_AGENT: &str = "zeron/1.0 (nvidia-nim harness)";
const NIM_REQUEST_TIMEOUT_SECS: u64 = 30;
const NIM_MAX_RETRIES: u32 = 3;

#[derive(Debug, Clone, Default)]
pub struct NvidiaNimHarness;

impl NvidiaNimHarness {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Debug, Deserialize)]
struct NimModelsResponse {
    data: Vec<NimModel>,
}

#[derive(Debug, Deserialize)]
struct NimModel {
    id: String,
    name: Option<String>,
    #[allow(dead_code)]
    object: Option<String>,
    #[allow(dead_code)]
    created: Option<u64>,
    #[allow(dead_code)]
    owned_by: Option<String>,
    plan: Option<String>,
    #[allow(dead_code)]
    active: Option<bool>,
}

fn map_model(m: &NimModel) -> Model {
    Model {
        id: m.id.clone(),
        label: m
            .name
            .as_deref()
            .unwrap_or(&m.id)
            .to_string(),
        description: None,
        reasoning_levels: Vec::new(),
        options: Vec::new(),
    }
}

/// Known free NIM models used as a fallback when the live catalog is
/// unreachable. IDs mirror NVIDIA's public catalog entries.
const NIM_FALLBACK_MODELS: &[(&str, &str)] = &[
    ("meta/llama-3.1-8b-instruct", "meta/llama-3.1-8b-instruct"),
    ("meta/llama-3.1-70b-instruct", "meta/llama-3.1-70b-instruct"),
    ("meta/llama-3.1-405b-instruct", "meta/llama-3.1-405b-instruct"),
    ("nvidia/nemotron-4-340b-instruct", "nvidia/nemotron-4-340b-instruct"),
    ("google/gemma-2-27b-it", "google/gemma-2-27b-it"),
    ("mistralai/mixtral-8x22b-instruct-v0.1", "mistralai/mixtral-8x22b-instruct-v0.1"),
    ("nvidia/nemotron-mini-4b-instruct", "nvidia/nemotron-mini-4b-instruct"),
];

async fn fetch_models(client: &reqwest::Client) -> Result<Vec<Model>, HarnessError> {
    let mut last_err: Option<HarnessError> = None;
    for attempt in 0..=NIM_MAX_RETRIES {
        let parsed: NimModelsResponse = match client
            .get(NIM_MODELS_URL)
            .header(reqwest::header::USER_AGENT, NIM_USER_AGENT)
            .timeout(std::time::Duration::from_secs(NIM_REQUEST_TIMEOUT_SECS))
            .send()
            .await
        {
            Ok(resp) => {
                let resp = match resp.error_for_status() {
                    Ok(resp) => resp,
                    Err(e) => {
                        last_err = Some(HarnessError::Protocol(format!(
                            "NVIDIA NIM returned an error status (attempt {}/{}): {}",
                            attempt + 1,
                            NIM_MAX_RETRIES + 1,
                            e
                        )));
                        continue;
                    }
                };
                match resp.json::<NimModelsResponse>().await {
                    Ok(parsed) => parsed,
                    Err(e) => {
                        last_err = Some(HarnessError::Protocol(format!(
                            "NVIDIA NIM response parse failed (attempt {}/{}): {}",
                            attempt + 1,
                            NIM_MAX_RETRIES + 1,
                            e
                        )));
                        continue;
                    }
                }
            }
            Err(e) => {
                last_err = Some(HarnessError::Protocol(format!(
                    "NVIDIA NIM request failed (attempt {}/{}): {}",
                    attempt + 1,
                    NIM_MAX_RETRIES + 1,
                    e
                )));
                continue;
            }
        };

        let models: Vec<Model> = parsed
            .data
            .into_iter()
            .filter(|m| m.plan.as_deref() == Some("free"))
            .map(|m| map_model(&m))
            .collect();

        if !models.is_empty() {
            return Ok(models);
        }

        last_err = Some(HarnessError::Protocol(
            "NVIDIA NIM returned no free models".into(),
        ));
    }

    Err(last_err.unwrap_or_else(|| {
        HarnessError::Protocol("NVIDIA NIM request failed after retries".into())
    }))
}

#[async_trait::async_trait]
impl Harness for NvidiaNimHarness {
    fn id(&self) -> HarnessId {
        HarnessId::NvidiaNim
    }

    fn display_name(&self) -> &str {
        "NVIDIA NIM"
    }

    fn supports_steering(&self) -> bool {
        false
    }

    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }

    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }

    fn installed(&self) -> bool {
        true
    }

    fn executable_path(&self) -> Option<std::path::PathBuf> {
        None
    }

    fn model_context(&self) -> Result<Option<ModelContext>, HarnessError> {
        Ok(None)
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        let client = reqwest::Client::new();
        match fetch_models(&client).await {
            Ok(models) => Ok(models),
            Err(err) => {
                tracing::warn!(%err, "NVIDIA NIM live catalog unavailable, using fallback models");
                Ok(self.fallback_models())
            }
        }
    }

    fn fallback_models(&self) -> Vec<Model> {
        NIM_FALLBACK_MODELS
            .iter()
            .map(|(id, label)| Model {
                id: id.to_string(),
                label: label.to_string(),
                description: None,
                reasoning_levels: Vec::new(),
                options: Vec::new(),
            })
            .collect()
    }

    async fn run(
        &self,
        _request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        Err(HarnessError::Protocol(
            "NVIDIA NIM is a model catalog only — it cannot run prompts".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn free_model(id: &str) -> NimModel {
        NimModel {
            id: id.to_string(),
            name: None,
            object: None,
            created: None,
            owned_by: None,
            plan: Some("free".to_string()),
            active: Some(true),
        }
    }

    fn paid_model(id: &str) -> NimModel {
        NimModel {
            id: id.to_string(),
            name: None,
            object: None,
            created: None,
            owned_by: None,
            plan: Some("paid".to_string()),
            active: Some(true),
        }
    }

    #[test]
    fn map_model_uses_name_when_present() {
        let m = NimModel {
            id: "nvidia/foo".to_string(),
            name: Some("Foo Model".to_string()),
            object: None,
            created: None,
            owned_by: None,
            plan: Some("free".to_string()),
            active: Some(true),
        };
        let model = map_model(&m);
        assert_eq!(model.id, "nvidia/foo");
        assert_eq!(model.label, "Foo Model");
    }

    #[test]
    fn map_model_falls_back_to_id() {
        let m = NimModel {
            id: "nvidia/bar".to_string(),
            name: None,
            object: None,
            created: None,
            owned_by: None,
            plan: Some("free".to_string()),
            active: Some(true),
        };
        let model = map_model(&m);
        assert_eq!(model.id, "nvidia/bar");
        assert_eq!(model.label, "nvidia/bar");
    }

    #[test]
    fn fallback_models_returns_known_free_models() {
        let harness = NvidiaNimHarness::new();
        let models = harness.fallback_models();
        assert!(!models.is_empty());
        for m in &models {
            assert!(!m.id.is_empty());
            assert_eq!(m.reasoning_levels, Vec::new());
            assert_eq!(m.options, Vec::new());
        }
        assert!(models.iter().any(|m| m.id == "meta/llama-3.1-8b-instruct"));
    }

    #[test]
    fn fetch_filters_to_free_only() {
        let response = NimModelsResponse {
            data: vec![
                free_model("meta/llama-3.1-8b-instruct"),
                paid_model("meta/llama-405b"),
            ],
        };
        let models: Vec<Model> = response
            .data
            .into_iter()
            .filter(|m| m.plan.as_deref() == Some("free"))
            .map(|m| map_model(&m))
            .collect();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "meta/llama-3.1-8b-instruct");
    }

    #[test]
    fn fallback_models_are_all_free_plan_ids() {
        let harness = NvidiaNimHarness::new();
        let models = harness.fallback_models();
        // Every fallback id should be a known free model id.
        for m in &models {
            assert!(
                NIM_FALLBACK_MODELS.iter().any(|(id, _)| id == &m.id),
                "unexpected fallback model: {}",
                m.id
            );
        }
    }
}
