//! NVIDIA NIM over its OpenAI-compatible HTTP API.
//!
//! The hosted catalog does not mark models as free, and `/v1/models` also
//! contains embeddings, reward models and other non-chat endpoints. Discovery
//! therefore intersects NVIDIA's featured chat catalog with live inventory;
//! it never invents a `plan` field or treats inventory membership as proof of
//! chat availability. Runs use `POST /v1/chat/completions` with the selected
//! account's `NVIDIA_API_KEY`, stream text/reasoning/usage over SSE, and keep
//! a private local transcript so a resumed runtime retains the conversation.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
};

use base64::Engine as _;
use futures::{StreamExt, stream::BoxStream};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode,
    providers::NVIDIA_NIM_FEATURED_FALLBACK,
};

use crate::{
    CancellationToken, Harness, HarnessError, ModelCatalog, ModelContext, RevertTarget, RunControls,
    SteerMessage, TurnControl,
};

const NIM_BASE_URL: &str = "https://integrate.api.nvidia.com";
const NIM_FEATURED_URL: &str =
    "https://assets.ngc.nvidia.com/products/api-catalog/featured-models.json";
const NIM_USER_AGENT: &str = concat!("zeron/", env!("CARGO_PKG_VERSION"), " (nvidia-nim)");
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(12);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_SSE_BUFFER: usize = 1024 * 1024;
const MAX_ERROR_BODY: usize = 16 * 1024;
const MAX_INLINE_IMAGE: u64 = 8 * 1024 * 1024;
const MAX_INLINE_IMAGES: u64 = 20 * 1024 * 1024;

/// Match the engine's per-device data root on every platform; transcripts
/// must survive a restart beside the rest of Zeron's private state.
fn data_root() -> PathBuf {
    if let Some(root) = std::env::var_os("ZERON_DATA_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        return root;
    }
    #[cfg(windows)]
    {
        if let Some(root) = std::env::var_os("LOCALAPPDATA")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
        {
            return root.join("Zeron");
        }
        if let Some(home) = std::env::var_os("USERPROFILE")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
        {
            return home.join("AppData").join("Local").join("Zeron");
        }
    }
    crate::executable::home_or_current_dir().join(".zeron")
}

pub struct NvidiaNimHarness {
    client: reqwest::Client,
    base_url: String,
    featured_url: String,
    models_cache: crate::catalog::Catalog,
    store_root: Option<PathBuf>,
}

impl Default for NvidiaNimHarness {
    fn default() -> Self {
        let base_url = std::env::var("NVIDIA_NIM_BASE_URL")
            .ok()
            .filter(|url| !url.trim().is_empty())
            .unwrap_or_else(|| NIM_BASE_URL.into());
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let base_url = base_url.trim().trim_end_matches('/').to_owned();
        let featured_url = if base_url == NIM_BASE_URL {
            NIM_FEATURED_URL.into()
        } else {
            format!("{base_url}/featured-models.json")
        };
        Self {
            client,
            base_url,
            featured_url,
            models_cache: Default::default(),
            store_root: None,
        }
    }
}

impl NvidiaNimHarness {
    pub fn new() -> Self {
        Self::default()
    }

    /// Point both discovery and inference at another OpenAI-compatible NIM.
    /// Primarily a self-hosting and local-fixture seam; production uses the
    /// hosted endpoints above.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim().trim_end_matches('/').to_owned();
        self.featured_url = format!("{}/featured-models.json", self.base_url);
        self
    }

    /// Keep resumed conversations below `root` instead of the device default.
    #[doc(hidden)]
    pub fn with_store_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.store_root = Some(root.into());
        self
    }

    fn context(&self) -> ModelContext {
        ModelContext {
            hash: format!(
                "{:x}",
                Sha256::digest(format!("{}\0{}", self.base_url, self.featured_url).as_bytes())
            ),
            binary_path: PathBuf::from(self.base_url.as_str()),
            binary_version: Some("openai-chat-completions-v1".into()),
        }
    }

    fn sessions(&self) -> HistoryStore {
        let root = self
            .store_root
            .clone()
            .unwrap_or_else(|| data_root().join("nvidia-nim").join("sessions"));
        HistoryStore { root }
    }

    async fn discover_models(&self) -> Result<Vec<Model>, HarnessError> {
        // A process-level key is optional for the public hosted inventory.
        // Account-slot keys are deliberately run-scoped and never global.
        let key = std::env::var("NVIDIA_API_KEY").ok();
        let inventory_url = format!("{}/v1/models", self.base_url);
        let (inventory, featured) = tokio::join!(
            get_json(
                &self.client,
                &inventory_url,
                key.as_deref(),
                "model inventory"
            ),
            get_json(&self.client, &self.featured_url, None, "featured catalog")
        );

        let inventory_ids = inventory.as_ref().ok().and_then(|body| {
            serde_json::from_value::<NimModelsResponse>(body.clone())
                .ok()
                .map(|response| {
                    response
                        .data
                        .into_iter()
                        .map(|model| model.id)
                        .filter(|id| !id.is_empty())
                        .collect::<HashSet<_>>()
                })
        });
        let featured_rows = featured.as_ref().ok().and_then(|body| {
            serde_json::from_value::<NimFeaturedResponse>(body.clone())
                .ok()
                .map(|response| response.featured)
        });

        if let Some(rows) = featured_rows {
            let mut seen = HashSet::new();
            let models: Vec<Model> = rows
                .into_iter()
                .filter(|row| !row.model.is_empty())
                // With inventory available, a featured row must still exist
                // there. Without it, NVIDIA's current featured feed is the
                // best available source and is still safer than guessing
                // capabilities from arbitrary inventory ids.
                .filter(|row| {
                    inventory_ids
                        .as_ref()
                        .is_none_or(|ids| ids.contains(&row.model))
                })
                .filter(|row| seen.insert(row.model.clone()))
                .map(featured_model)
                .collect();
            if !models.is_empty() {
                return Ok(models);
            }
        }

        // The featured feed is an enhancement, not a second single point of
        // failure: known chat metadata can still be matched against inventory.
        if let Some(ids) = inventory_ids {
            let models = fallback_models()
                .into_iter()
                .filter(|model| ids.contains(&model.id))
                .collect::<Vec<_>>();
            if !models.is_empty() {
                return Ok(models);
            }
        }

        let detail = match (inventory, featured) {
            (Err(inventory), Err(featured)) => format!("{inventory}; {featured}"),
            (Err(inventory), _) => inventory.to_string(),
            (_, Err(featured)) => featured.to_string(),
            _ => "the responses contained no confirmed chat models".into(),
        };
        Err(HarnessError::Protocol(format!(
            "NVIDIA NIM returned no usable chat catalog: {detail}"
        )))
    }
}

#[derive(Debug, Deserialize)]
struct NimModelsResponse {
    data: Vec<NimModel>,
}

#[derive(Debug, Deserialize)]
struct NimModel {
    id: String,
}

#[derive(Debug, Deserialize)]
struct NimFeaturedResponse {
    #[serde(rename = "featured-models")]
    featured: Vec<NimFeaturedModel>,
}

#[derive(Debug, Deserialize)]
struct NimFeaturedModel {
    model: String,
    #[serde(rename = "model-name", default)]
    name: String,
    #[serde(default)]
    context: u64,
    #[serde(rename = "max-output", default)]
    max_output: u64,
}

fn featured_model(row: NimFeaturedModel) -> Model {
    let NimFeaturedModel {
        model,
        name,
        context,
        max_output,
    } = row;
    let description = match (context, max_output) {
        (0, 0) => None,
        (context, 0) => Some(format!("{context} token context window")),
        (0, output) => Some(format!("Up to {output} output tokens")),
        (context, output) => Some(format!(
            "{context} token context window · up to {output} output tokens"
        )),
    };
    let label = if name.trim().is_empty() {
        model.clone()
    } else {
        name
    };
    Model {
        id: model,
        label,
        description,
        reasoning_levels: Vec::new(),
        options: Vec::new(),
    }
}

fn fallback_models() -> Vec<Model> {
    NVIDIA_NIM_FEATURED_FALLBACK
        .iter()
        .map(|(id, label, context, output)| Model {
            id: (*id).into(),
            label: (*label).into(),
            description: Some(format!(
                "{context} token context window · up to {output} output tokens"
            )),
            reasoning_levels: Vec::new(),
            options: Vec::new(),
        })
        .collect()
}

async fn get_json(
    client: &reqwest::Client,
    url: &str,
    api_key: Option<&str>,
    what: &str,
) -> Result<Value, HarnessError> {
    let request = client
        .get(url)
        .header(reqwest::header::USER_AGENT, NIM_USER_AGENT)
        .timeout(DISCOVERY_TIMEOUT);
    let request = match api_key {
        Some(key) if !key.is_empty() => request.bearer_auth(key),
        _ => request,
    };
    let response = request.send().await.map_err(|error| {
        HarnessError::Protocol(format!("NVIDIA NIM {what} request failed: {error}"))
    })?;
    let status = response.status();
    if !status.is_success() {
        return Err(HarnessError::Protocol(format!(
            "NVIDIA NIM {what} returned {status}"
        )));
    }
    response.json::<Value>().await.map_err(|error| {
        HarnessError::Protocol(format!("NVIDIA NIM {what} returned invalid JSON: {error}"))
    })
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
        true
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

    fn deterministic_turn_end(&self) -> bool {
        true
    }

    fn stops_turn_in_place(&self) -> bool {
        true
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        match self.model_catalog(false).await {
            Ok(catalog) => Ok(catalog.models),
            Err(error) => {
                // The registry's direct discovery API historically had a
                // static NIM fallback. Keep that contract here; the richer
                // model_catalog path still reports the live/cache failure so
                // its persistent fallback logic can make the same choice.
                let models = fallback_models();
                if models.is_empty() {
                    Err(error)
                } else {
                    tracing::warn!(%error, "NVIDIA NIM live catalog unavailable; using static models");
                    Ok(models)
                }
            }
        }
    }

    fn model_context(&self) -> Result<Option<ModelContext>, HarnessError> {
        Ok(Some(self.context()))
    }

    async fn model_catalog(&self, force: bool) -> Result<ModelCatalog, HarnessError> {
        self.models_cache
            .get_with_timeout(
                force,
                Duration::from_secs(25),
                || Ok(self.context().key()),
                || self.discover_models(),
            )
            .await
    }

    fn fallback_models(&self) -> Vec<Model> {
        fallback_models()
    }

    async fn run_title(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        // NIM is asked for a title directly; there are no tools or shell to
        // restrict, so the ordinary stateless request is already read-only.
        self.run(request, controls).await
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let api_key = request
            .env
            .get("NVIDIA_API_KEY")
            .cloned()
            .or_else(|| std::env::var("NVIDIA_API_KEY").ok())
            .map(|key| key.trim().to_owned())
            .filter(|key| !key.is_empty())
            .ok_or_else(|| {
                HarnessError::NotSignedIn(
                    "NVIDIA NIM needs an API key. Open Settings → Providers → NVIDIA NIM and \
                     connect an `nvapi-…` key, or set NVIDIA_API_KEY on this device."
                        .into(),
                )
            })?;
        let model = request
            .model
            .clone()
            .filter(|model| !model.is_empty())
            .unwrap_or_else(|| NVIDIA_NIM_FEATURED_FALLBACK[0].0.into());
        let session_id = request
            .resume
            .clone()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let (event_tx, event_rx) = mpsc::channel(256);
        let interrupt = controls.interrupt.clone();
        let task = tokio::spawn(run_session(Session {
            client: self.client.clone(),
            base_url: self.base_url.clone(),
            api_key,
            model,
            model_options: request.model_options.clone(),
            session_id,
            resumed: request
                .resume
                .as_deref()
                .is_some_and(|id| !id.is_empty()),
            prompt: request.prompt,
            attachments: request.attachments,
            cwd: request.cwd,
            store: self.sessions(),
            controls,
            event_tx,
        }));
        // Dropping the stream must not leave an unobserved HTTP request alive.
        let guard = RunStreamGuard { interrupt, task };
        Ok(futures::stream::unfold((event_rx, guard), |(mut rx, guard)| async move {
            rx.recv().await.map(|event| (event, (rx, guard)))
        })
        .boxed())
    }

    async fn revert_preview(
        &self,
        request: RunRequest,
        target: RevertTarget,
    ) -> Result<Vec<String>, HarnessError> {
        self.sessions().preview(&request, &target)
    }

    async fn revert_commit(
        &self,
        request: RunRequest,
        target: RevertTarget,
        _files: bool,
    ) -> Result<(), HarnessError> {
        self.sessions().commit(&request, &target)
    }
}

struct RunStreamGuard {
    interrupt: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for RunStreamGuard {
    fn drop(&mut self) {
        self.interrupt.cancel();
        self.task.abort();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct HistoryMessage {
    role: String,
    content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    attachments: Vec<String>,
    /// Zeron's user-entry id deduplicates an interrupted turn revived with
    /// the same provider session and user message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message_id: Option<String>,
}

impl HistoryMessage {
    fn user(prompt: String, attachments: Vec<String>, message_id: Option<String>) -> Self {
        Self {
            role: "user".into(),
            content: prompt,
            attachments,
            message_id,
        }
    }

    fn assistant(content: String) -> Self {
        Self {
            role: "assistant".into(),
            content,
            attachments: Vec::new(),
            message_id: None,
        }
    }
}

#[derive(Serialize)]
struct HistoryFile<'a> {
    version: u32,
    messages: &'a [HistoryMessage],
}

#[derive(Deserialize)]
struct SavedHistory {
    version: u32,
    messages: Vec<HistoryMessage>,
}

struct HistoryStore {
    root: PathBuf,
}

impl HistoryStore {
    fn path(&self, session_id: &str) -> PathBuf {
        self.root
            .join(format!("{:x}.json", Sha256::digest(session_id.as_bytes())))
    }

    fn load(&self, session_id: &str) -> Result<Option<Vec<HistoryMessage>>, HarnessError> {
        let path = self.path(session_id);
        let raw = match std::fs::read(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let saved: SavedHistory = serde_json::from_slice(&raw).map_err(|error| {
            HarnessError::Protocol(format!(
                "NVIDIA NIM could not restore its saved conversation: {error}"
            ))
        })?;
        if saved.version != 1
            || saved
                .messages
                .iter()
                .any(|message| !matches!(message.role.as_str(), "user" | "assistant"))
        {
            return Err(HarnessError::Protocol(
                "NVIDIA NIM found an unsupported saved conversation.".into(),
            ));
        }
        Ok(Some(saved.messages))
    }

    fn user_turns(messages: &[HistoryMessage]) -> Vec<crate::revert::UserTurn> {
        messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message.role == "user")
            .map(|(index, message)| crate::revert::UserTurn {
                key: index as u64,
                text: message.content.clone(),
                id: message.message_id.clone(),
            })
            .collect()
    }

    fn preview(
        &self,
        request: &RunRequest,
        target: &crate::RevertTarget,
    ) -> Result<Vec<String>, HarnessError> {
        let Some(session_id) = crate::revert::resume_id(request) else {
            return Ok(Vec::new());
        };
        let Some(messages) = self.load(session_id)? else {
            return Ok(Vec::new());
        };
        let turns = Self::user_turns(&messages);
        if turns.is_empty() {
            return Ok(Vec::new());
        }
        crate::revert::pick_turn(&turns, target)?;
        Ok(Vec::new())
    }

    fn commit(
        &self,
        request: &RunRequest,
        target: &crate::RevertTarget,
    ) -> Result<(), HarnessError> {
        let Some(session_id) = crate::revert::resume_id(request) else {
            return Ok(());
        };
        let Some(mut messages) = self.load(session_id)? else {
            return Ok(());
        };
        let turns = Self::user_turns(&messages);
        if turns.is_empty() {
            return Ok(());
        }
        let cut = crate::revert::pick_turn(&turns, target)?.key as usize;
        messages.truncate(cut);
        self.save(session_id, &messages)
    }

    fn save(&self, session_id: &str, messages: &[HistoryMessage]) -> Result<(), HarnessError> {
        private_dir(&self.root)?;
        let path = self.path(session_id);
        let bytes = serde_json::to_vec(&HistoryFile { version: 1, messages }).map_err(|error| {
            HarnessError::Protocol(format!(
                "NVIDIA NIM could not encode the saved conversation: {error}"
            ))
        })?;
        let staging = self.root.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&staging)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            // Unix replaces atomically. Windows requires removing the old
            // target first; the sibling temp still prevents torn JSON.
            #[cfg(windows)]
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
            std::fs::rename(&staging, &path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&staging);
        }
        result.map_err(Into::into)
    }
}

fn private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

struct Session {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    model_options: serde_json::Map<String, Value>,
    session_id: String,
    resumed: bool,
    prompt: String,
    attachments: Vec<String>,
    cwd: String,
    store: HistoryStore,
    controls: RunControls,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
}

async fn run_session(session: Session) {
    let Session {
        client,
        base_url,
        api_key,
        mut model,
        mut model_options,
        session_id,
        resumed,
        prompt,
        attachments,
        cwd,
        store,
        mut controls,
        event_tx,
    } = session;
    let history = match store.load(&session_id) {
        Ok(history) => history,
        Err(error) => {
            let message = error.to_string();
            let _ = send(&event_tx, AgentEvent::Error {
                message: message.clone(),
            })
            .await;
            let _ = send(&event_tx, AgentEvent::Done {
                status: DoneStatus::Errored,
                result: None,
                error: Some(message),
                session_id: Some(session_id),
            })
            .await;
            return;
        }
    };
    let missing_resume = resumed && history.is_none();
    let mut history = history.unwrap_or_default();
    let mut assistant_message_id = uuid::Uuid::new_v4().to_string();
    if !send(&event_tx, AgentEvent::SessionStarted {
        harness: HarnessId::NvidiaNim,
        model: model.clone(),
        tools: Vec::new(),
        cwd,
        session_id: session_id.clone(),
        assistant_message_id: assistant_message_id.clone(),
    })
    .await
    {
        return;
    }
    if missing_resume
        && !send(&event_tx, AgentEvent::Error {
            message: "NVIDIA NIM could not find the saved conversation on this device; starting \
                      this session with the current message."
                .into(),
        })
        .await
    {
        return;
    }

    let mut initial = SteerMessage::text(prompt);
    initial.attachments = attachments;
    initial.message_id = controls.user_message_id.clone();
    let mut next = Some(initial);
    let mut first_turn = true;
    let mut steering_open = true;
    loop {
        if next.is_none() {
            if !steering_open {
                break;
            }
            tokio::select! {
                steer = controls.steering.recv() => match steer {
                    Some(steer) => next = Some(steer),
                    None => steering_open = false,
                },
                _ = controls.interrupt.cancelled() => {
                    let _ = send(&event_tx, AgentEvent::Done {
                        status: DoneStatus::Interrupted,
                        result: None,
                        error: None,
                        session_id: Some(session_id.clone()),
                    }).await;
                    break;
                }
            }
            continue;
        }
        let Some(message) = next.take() else {
            continue;
        };
        if !first_turn {
            let previous = std::mem::replace(
                &mut assistant_message_id,
                uuid::Uuid::new_v4().to_string(),
            );
            if !send(&event_tx, AgentEvent::Steered {
                assistant_message_id: Some(previous),
                next_assistant_message_id: Some(assistant_message_id.clone()),
            })
            .await
            {
                break;
            }
        }
        first_turn = false;
        if let Some(config) = message.config {
            let config = *config;
            model = config.model.unwrap_or(model);
            model_options = config.model_options;
        }

        // A crash/restart can revive the same user entry after we already
        // persisted it but before a response arrived. Do not send it twice.
        let retry_same_turn = message.message_id.as_ref().is_some_and(|id| {
            history.last().is_some_and(|last| {
                last.role == "user" && last.message_id.as_ref() == Some(id)
            })
        });
        if !retry_same_turn {
            history.push(HistoryMessage::user(
                message.prompt,
                message.attachments,
                message.message_id,
            ));
            if let Err(error) = store.save(&session_id, &history) {
                let message = format!("NVIDIA NIM could not save the conversation: {error}");
                let _ = send(&event_tx, AgentEvent::Error {
                    message: message.clone(),
                })
                .await;
                let _ = send(&event_tx, AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(message),
                    session_id: Some(session_id.clone()),
                })
                .await;
                break;
            }
        }

        let outcome = execute_turn(
            &client,
            &base_url,
            &api_key,
            &model,
            &model_options,
            &history,
            &event_tx,
            &controls.interrupt,
            &controls.turn,
        )
        .await;
        match outcome {
            Ok(answer) => {
                history.push(HistoryMessage::assistant(answer));
                if let Err(error) = store.save(&session_id, &history) {
                    let message = format!("NVIDIA NIM could not save the conversation: {error}");
                    let _ = send(&event_tx, AgentEvent::Error {
                        message: message.clone(),
                    })
                    .await;
                    let _ = send(&event_tx, AgentEvent::Done {
                        status: DoneStatus::Errored,
                        result: None,
                        error: Some(message),
                        session_id: Some(session_id.clone()),
                    })
                    .await;
                    break;
                }
                if !send(&event_tx, AgentEvent::Done {
                    status: DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: Some(session_id.clone()),
                })
                .await
                {
                    break;
                }
            }
            Err(TurnError::Failed(message)) => {
                let _ = send(&event_tx, AgentEvent::Error {
                    message: message.clone(),
                })
                .await;
                let _ = send(&event_tx, AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(message),
                    session_id: Some(session_id.clone()),
                })
                .await;
                break;
            }
            Err(TurnError::Interrupted) => {
                let _ = send(&event_tx, AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: Some(session_id.clone()),
                })
                .await;
                break;
            }
            Err(TurnError::Stopped) => {
                if !send(&event_tx, AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: Some(session_id.clone()),
                })
                .await
                {
                    break;
                }
            }
            Err(TurnError::Closed) => break,
        }
    }
}

async fn send(
    tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
    event: AgentEvent,
) -> bool {
    tx.send(Ok(event)).await.is_ok()
}

#[derive(Debug)]
enum TurnError {
    Failed(String),
    Interrupted,
    Stopped,
    Closed,
}

#[derive(Default)]
struct TurnState {
    answer: String,
    saw_done: bool,
    finish_reason: Option<String>,
}

#[allow(clippy::too_many_arguments)]
async fn execute_turn(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    model_options: &serde_json::Map<String, Value>,
    history: &[HistoryMessage],
    event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
    interrupt: &CancellationToken,
    turn: &TurnControl,
) -> Result<String, TurnError> {
    let mut body = json!({
        "model": model,
        "messages": request_messages(history).await,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    // Keep the wire options deliberately small and OpenAI-compatible. NIM
    // model catalogs currently expose no extra choices, but older clients can
    // still carry these standard selections in a saved run request.
    for (wire, names) in [
        ("temperature", &["temperature"][..]),
        ("top_p", &["top_p", "topP"][..]),
        ("max_tokens", &["max_tokens", "maxTokens"][..]),
        ("seed", &["seed"][..]),
    ] {
        if let Some(value) = names
            .iter()
            .find_map(|name| model_options.get(*name))
        {
            body[wire] = value.clone();
        }
    }

    let request = client
        .post(format!("{base_url}/v1/chat/completions"))
        .header(reqwest::header::USER_AGENT, NIM_USER_AGENT)
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .bearer_auth(api_key)
        .json(&body)
        .send();
    let mut response = tokio::select! {
        response = request => response.map_err(|error| {
            TurnError::Failed(format!("NVIDIA NIM request failed: {error}"))
        })?,
        _ = interrupt.cancelled() => return Err(TurnError::Interrupted),
        _ = turn.stop_requested() => return Err(TurnError::Stopped),
    };
    let status = response.status();
    if !status.is_success() {
        let snippet = read_error_snippet(&mut response, interrupt, turn).await?;
        return Err(TurnError::Failed(http_error(status, &snippet)));
    }

    let mut decoder = SseDecoder::default();
    let mut state = TurnState::default();
    loop {
        let chunk = tokio::select! {
            chunk = response.chunk() => chunk,
            _ = interrupt.cancelled() => return Err(TurnError::Interrupted),
            _ = turn.stop_requested() => return Err(TurnError::Stopped),
        };
        match chunk {
            Ok(Some(chunk)) => {
                let events = decoder.push(&chunk).map_err(TurnError::Failed)?;
                for data in events {
                    handle_stream_data(&data, &mut state, event_tx).await?;
                }
                if state.saw_done {
                    break;
                }
            }
            Ok(None) => {
                for data in decoder.finish().map_err(TurnError::Failed)? {
                    handle_stream_data(&data, &mut state, event_tx).await?;
                }
                break;
            }
            Err(error) => {
                return Err(TurnError::Failed(format!(
                    "NVIDIA NIM's response stream failed: {error}"
                )));
            }
        }
    }
    if !state.saw_done && state.finish_reason.is_none() {
        return Err(TurnError::Failed(
            "NVIDIA NIM ended the response before a completion marker.".into(),
        ));
    }
    match state.finish_reason.as_deref() {
        Some("content_filter") => {
            return Err(TurnError::Failed(
                "NVIDIA NIM stopped the answer because its content filter fired.".into(),
            ));
        }
        Some("tool_calls") => {
            return Err(TurnError::Failed(
                "NVIDIA NIM requested a tool call, but this chat did not offer tools.".into(),
            ));
        }
        Some("length") => {
            if !send(event_tx, AgentEvent::Error {
                message: "NVIDIA NIM reached the output-token limit; the answer may be truncated."
                    .into(),
            })
            .await
            {
                return Err(TurnError::Closed);
            }
        }
        _ => {}
    }
    Ok(state.answer)
}

async fn read_error_snippet(
    response: &mut reqwest::Response,
    interrupt: &CancellationToken,
    turn: &TurnControl,
) -> Result<String, TurnError> {
    let mut bytes = Vec::new();
    while bytes.len() < MAX_ERROR_BODY {
        let chunk = tokio::select! {
            chunk = response.chunk() => chunk,
            _ = interrupt.cancelled() => return Err(TurnError::Interrupted),
            _ = turn.stop_requested() => return Err(TurnError::Stopped),
            _ = tokio::time::sleep(Duration::from_secs(10)) => break,
        };
        match chunk {
            Ok(Some(chunk)) => {
                let remaining = MAX_ERROR_BODY - bytes.len();
                bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn http_error(status: reqwest::StatusCode, snippet: &str) -> String {
    match status.as_u16() {
        401 | 403 => format!(
            "NVIDIA NIM rejected the API key ({status}). Connect a valid `nvapi-…` key in \
             Settings → Providers → NVIDIA NIM."
        ),
        404 => format!(
            "NVIDIA NIM could not run that model ({status}). Refresh the model list or choose \
             another model.{}",
            error_detail(snippet)
        ),
        429 => format!(
            "NVIDIA NIM rate-limited the request ({status}). Wait a moment and try again.{}",
            error_detail(snippet)
        ),
        _ => format!("NVIDIA NIM returned {status}.{}", error_detail(snippet)),
    }
}

fn error_detail(snippet: &str) -> String {
    let parsed = serde_json::from_str::<Value>(snippet).ok();
    let message = parsed
        .as_ref()
        .and_then(provider_error)
        .unwrap_or_else(|| snippet.trim().to_owned());
    let message = crate::redact::redact_output(message.trim());
    if message.is_empty() {
        return String::new();
    }
    let detail: String = message.chars().take(600).collect();
    format!(" {detail}")
}

async fn handle_stream_data(
    data: &str,
    state: &mut TurnState,
    event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
) -> Result<(), TurnError> {
    if data.trim() == "[DONE]" {
        state.saw_done = true;
        return Ok(());
    }
    let value: Value = serde_json::from_str(data)
        .map_err(|_| TurnError::Failed("NVIDIA NIM sent an invalid SSE JSON frame.".into()))?;
    if let Some(message) = provider_error(&value) {
        return Err(TurnError::Failed(format!(
            "NVIDIA NIM failed the request: {}",
            crate::redact::redact_output(&message)
        )));
    }
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first());
    if let Some(choice) = choice {
        let delta = choice.get("delta").unwrap_or(&Value::Null);
        let reasoning = ["reasoning_content", "reasoning", "thinking"]
            .iter()
            .find_map(|key| delta.get(*key).and_then(text_content))
            .filter(|text| !text.is_empty());
        if let Some(text) = reasoning
            && !send(event_tx, AgentEvent::ReasoningDelta { text }).await
        {
            return Err(TurnError::Closed);
        }
        if let Some(text) = delta.get("content").and_then(text_content)
            && !text.is_empty()
        {
            state.answer.push_str(&text);
            if !send(event_tx, AgentEvent::TextDelta { text }).await {
                return Err(TurnError::Closed);
            }
        }
        if choice.get("tool_calls").is_some_and(|calls| !calls.is_null())
            || delta.get("tool_calls").is_some_and(|calls| !calls.is_null())
        {
            return Err(TurnError::Failed(
                "NVIDIA NIM returned a tool call, but this chat did not offer tools.".into(),
            ));
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            state.finish_reason = Some(reason.to_owned());
        }
    }
    if let Some(usage) = value.get("usage").filter(|usage| usage.is_object()) {
        let input = usage.get("prompt_tokens").and_then(Value::as_u64);
        let output = usage.get("completion_tokens").and_then(Value::as_u64);
        let total = usage.get("total_tokens").and_then(Value::as_u64);
        if input.is_some() || output.is_some() {
            if !send(event_tx, AgentEvent::Usage {
                input_tokens: input.unwrap_or(0),
                output_tokens: output.unwrap_or(0),
            })
            .await
            {
                return Err(TurnError::Closed);
            }
        }
        if let Some(tokens) = total.or_else(|| input.zip(output).map(|(a, b)| a + b))
            && !send(event_tx, AgentEvent::ContextUsage {
                tokens: Some(tokens),
                window: None,
            })
            .await
        {
            return Err(TurnError::Closed);
        }
    }
    Ok(())
}

fn provider_error(value: &Value) -> Option<String> {
    let error = value.get("error")?;
    error
        .as_str()
        .map(str::to_owned)
        .or_else(|| error.get("message")?.as_str().map(str::to_owned))
}

fn text_content(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter_map(|part| {
                    part.as_str().map(str::to_owned).or_else(|| {
                        part.get("text")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                })
                .collect::<Vec<_>>()
                .join(""),
        ),
        _ => None,
    }
}

async fn request_messages(history: &[HistoryMessage]) -> Vec<Value> {
    let mut messages = Vec::with_capacity(history.len());
    for message in history {
        if message.role == "assistant" || message.attachments.is_empty() {
            messages.push(json!({
                "role": message.role.as_str(),
                "content": message.content.as_str(),
            }));
            continue;
        }
        let images = load_image_parts(&message.attachments).await;
        if images.is_empty() {
            // Do not make an unreadable attachment change an otherwise plain
            // text turn into a multimodal request.
            messages.push(json!({ "role": "user", "content": message.content.as_str() }));
        } else {
            let mut parts = vec![json!({ "type": "text", "text": message.content.as_str() })];
            parts.extend(images);
            messages.push(json!({ "role": "user", "content": parts }));
        }
    }
    messages
}

/// Inline ordinary raster images for NIM's VL models. The prompt already
/// carries local path refs, so unsupported or unreadable files are best-effort
/// omissions rather than a failed turn.
async fn load_image_parts(paths: &[String]) -> Vec<Value> {
    let mut parts = Vec::new();
    let mut total = 0u64;
    for path in paths {
        let Ok(metadata) = tokio::fs::metadata(path).await else {
            continue;
        };
        if metadata.len() > MAX_INLINE_IMAGE || metadata.len() > MAX_INLINE_IMAGES - total {
            continue;
        }
        let Ok(bytes) = tokio::fs::read(path).await else {
            continue;
        };
        let mime = if bytes.starts_with(b"\x89PNG") {
            "image/png"
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            "image/jpeg"
        } else if bytes.starts_with(b"GIF8") {
            "image/gif"
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(&b"WEBP"[..]) {
            "image/webp"
        } else {
            continue;
        };
        if bytes.len() as u64 > MAX_INLINE_IMAGE
            || bytes.len() as u64 > MAX_INLINE_IMAGES - total
        {
            continue;
        }
        total += bytes.len() as u64;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        parts.push(json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{mime};base64,{encoded}") },
        }));
    }
    parts
}

/// Incremental SSE framing: network chunks may split a UTF-8 codepoint, a
/// line, or an event; CRLF and multi-line `data:` fields are legal.
#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
    data: Vec<String>,
}

impl SseDecoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>, String> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=newline).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.line(&line, &mut events)?;
        }
        if self.buffer.len() > MAX_SSE_BUFFER {
            return Err("NVIDIA NIM sent an oversized SSE line.".into());
        }
        Ok(events)
    }

    fn finish(&mut self) -> Result<Vec<String>, String> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            self.line(&line, &mut events)?;
        }
        self.dispatch(&mut events);
        Ok(events)
    }

    fn line(&mut self, line: &[u8], events: &mut Vec<String>) -> Result<(), String> {
        if line.is_empty() {
            self.dispatch(events);
            return Ok(());
        }
        if line.starts_with(b":") {
            return Ok(());
        }
        if let Some(data) = line.strip_prefix(b"data:") {
            let data = data.strip_prefix(b" ").unwrap_or(data);
            self.data.push(String::from_utf8_lossy(data).into_owned());
            if self.data.iter().map(|line| line.len()).sum::<usize>() > MAX_SSE_BUFFER {
                return Err("NVIDIA NIM sent an oversized SSE event.".into());
            }
        }
        Ok(())
    }

    fn dispatch(&mut self, events: &mut Vec<String>) {
        if !self.data.is_empty() {
            events.push(self.data.join("\n"));
            self.data.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn run_request() -> RunRequest {
        serde_json::from_str(
            r#"{"prompt":"first","model":"fixture/chat","reasoning":null,"cwd":".","sandbox":"workspace-write","resume":null}"#,
        )
        .unwrap()
    }

    fn run_controls() -> (
        RunControls,
        mpsc::Sender<SteerMessage>,
        CancellationToken,
        TurnControl,
    ) {
        let (steering_tx, steering) = mpsc::channel(8);
        let interrupt = CancellationToken::new();
        let turn = TurnControl::default();
        (
            RunControls {
                realtime: None,
                execution_lease: None,
                request_input: Box::new(|_| {
                    let (_tx, rx) = tokio::sync::oneshot::channel::<
                        Vec<zeron_proto::UserInputAnswer>,
                    >();
                    rx
                }),
                steering,
                user_message_id: Some("m1".into()),
                interrupt: interrupt.clone(),
                turn: turn.clone(),
            },
            steering_tx,
            interrupt,
            turn,
        )
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> (String, String, Value) {
        let mut bytes = Vec::new();
        let header_end;
        loop {
            let mut chunk = [0u8; 4096];
            let read = socket.read(&mut chunk).await.unwrap();
            assert!(read > 0, "connection ended before request headers");
            bytes.extend_from_slice(&chunk[..read]);
            if let Some(position) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                header_end = position + 4;
                break;
            }
        }
        let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        while bytes.len() < header_end + length {
            let mut chunk = [0u8; 4096];
            let read = socket.read(&mut chunk).await.unwrap();
            assert!(read > 0, "connection ended before request body");
            bytes.extend_from_slice(&chunk[..read]);
        }
        let path = headers
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or_default()
            .to_owned();
        let body = serde_json::from_slice(&bytes[header_end..header_end + length])
            .unwrap_or(Value::Null);
        (path, headers, body)
    }

    async fn respond(socket: &mut tokio::net::TcpStream, status: &str, content_type: &str, body: &str) {
        socket
            .write_all(
                format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
        .unwrap();
        socket.shutdown().await.unwrap();
    }

    #[test]
    fn sse_decoder_preserves_split_lines_and_utf8() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"data: {\"text\":\"\xF0\x9F").unwrap().is_empty());
        let events = decoder
            .push(b"\xA6\x80\",\"more\":true}\r\ndata: second\r\n\r\n")
            .unwrap();
        assert_eq!(events, vec!["{\"text\":\"🦀\",\"more\":true}\nsecond"]);
        assert_eq!(decoder.finish().unwrap(), Vec::<String>::new());
    }

    #[tokio::test]
    async fn discovery_uses_chat_metadata_not_a_nonexistent_plan() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (path, _, _) = read_request(&mut socket).await;
                match path.as_str() {
                    "/v1/models" => respond(&mut socket, "200 OK", "application/json", r#"{"data":[{"id":"fixture/chat"},{"id":"fixture/embed"}]}"#).await,
                    "/featured-models.json" => respond(&mut socket, "200 OK", "application/json", r#"{"featured-models":[{"model":"fixture/chat","model-name":"Fixture Chat","context":4096,"max-output":512},{"model":"fixture/not-inventory","model-name":"Gone","context":1,"max-output":1}]}"#).await,
                    other => panic!("unexpected path {other}"),
                }
            }
        });
        let models = NvidiaNimHarness::new()
            .with_base_url(base)
            .models()
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "fixture/chat");
        assert_eq!(models[0].label, "Fixture Chat");
        assert!(models[0].description.as_deref().unwrap().contains("4096"));
    }

    #[tokio::test]
    async fn streams_reasoning_text_usage_and_resumes_history() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (path, headers, body) = read_request(&mut socket).await;
            assert_eq!(path, "/v1/chat/completions");
            assert!(headers.to_ascii_lowercase().contains("authorization: bearer nvapi-fixture-key-123456"));
            assert_eq!(body["stream"], true);
            assert_eq!(body["messages"].as_array().unwrap().len(), 1);
            respond(&mut socket, "200 OK", "text/event-stream", concat!(
                "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking\"}}]}\r\n\r\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1,\"total_tokens\":4}}\n\n",
                "data: [DONE]\n\n",
            )).await;

            let (mut socket, _) = listener.accept().await.unwrap();
            let (_, headers, body) = read_request(&mut socket).await;
            assert!(headers.to_ascii_lowercase().contains("authorization: bearer nvapi-fixture-key-123456"));
            let messages = body["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 3, "user, assistant, next user: {messages:?}");
            assert_eq!(messages[1]["role"], "assistant");
            assert_eq!(messages[1]["content"], "Hello");
            assert_eq!(messages[2]["content"], "second");
            respond(&mut socket, "200 OK", "text/event-stream", concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Again\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            )).await;

            // A fresh runtime with the same provider session id must reload
            // the durable transcript, not start with only the latest prompt.
            let (mut socket, _) = listener.accept().await.unwrap();
            let (_, _, body) = read_request(&mut socket).await;
            let messages = body["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 5, "resumed history: {messages:?}");
            assert_eq!(messages[3]["content"], "Again");
            assert_eq!(messages[4]["content"], "third");
            respond(&mut socket, "200 OK", "text/event-stream", concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Third\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            )).await;
        });

        let tmp = tempfile::tempdir().unwrap();
        let harness = NvidiaNimHarness::new()
            .with_base_url(base.clone())
            .with_store_root(tmp.path());
        let mut request = run_request();
        request.env.insert("NVIDIA_API_KEY".into(), "nvapi-fixture-key-123456".into());
        let (controls, steering, _interrupt, _turn) = run_controls();
        let mut stream = harness.run(request, controls).await.unwrap();
        let mut first = Vec::new();
        let mut session_id = None;
        loop {
            let event = stream.next().await.unwrap().unwrap();
            if let AgentEvent::SessionStarted { session_id: id, .. } = &event {
                session_id = Some(id.clone());
            }
            let done = matches!(event, AgentEvent::Done { status: DoneStatus::Completed, .. });
            first.push(event);
            if done { break; }
        }
        assert!(first.iter().any(|event| matches!(event, AgentEvent::ReasoningDelta { text } if text == "thinking")));
        assert!(first.iter().any(|event| matches!(event, AgentEvent::TextDelta { text } if text == "Hello")));
        assert!(first.iter().any(|event| matches!(event, AgentEvent::Usage { input_tokens: 3, output_tokens: 1 })));

        steering.send(SteerMessage {
            prompt: "second".into(),
            message_id: Some("m2".into()),
            attachments: Vec::new(),
            config: None,
        }).await.unwrap();
        let mut second = Vec::new();
        loop {
            let event = stream.next().await.unwrap().unwrap();
            let done = matches!(event, AgentEvent::Done { status: DoneStatus::Completed, .. });
            second.push(event);
            if done { break; }
        }
        assert!(matches!(second.first(), Some(AgentEvent::Steered { .. })));
        assert!(second.iter().any(|event| matches!(event, AgentEvent::TextDelta { text } if text == "Again")));
        let session_id = session_id.unwrap();
        let saved = harness.sessions().load(&session_id).unwrap().unwrap();
        assert_eq!(saved.len(), 4);
        assert_eq!(saved[3].content, "Again");
        drop(steering);
        drop(stream);

        let resumed_harness = NvidiaNimHarness::new()
            .with_base_url(base)
            .with_store_root(tmp.path());
        let mut resumed = run_request();
        resumed.prompt = "third".into();
        resumed.resume = Some(session_id.clone());
        resumed
            .env
            .insert("NVIDIA_API_KEY".into(), "nvapi-fixture-key-123456".into());
        let (mut resumed_controls, _steering, _interrupt, _turn) = run_controls();
        resumed_controls.user_message_id = Some("m3".into());
        let mut stream = resumed_harness.run(resumed, resumed_controls).await.unwrap();
        let mut saw_third = false;
        loop {
            let event = stream.next().await.unwrap().unwrap();
            saw_third |= matches!(&event, AgentEvent::TextDelta { text } if text == "Third");
            if matches!(event, AgentEvent::Done { status: DoneStatus::Completed, .. }) {
                break;
            }
        }
        server.await.unwrap();
        let saved = resumed_harness
            .sessions()
            .load(&session_id)
            .unwrap()
            .unwrap();
        assert_eq!(saved.len(), 6);
        assert_eq!(saved[5].content, "Third");
        assert!(saw_third);
        drop(stream);
    }

    #[tokio::test]
    async fn cancellation_ends_the_http_turn_as_interrupted() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").await.unwrap();
            let _ = accepted_tx.send(());
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let harness = NvidiaNimHarness::new()
            .with_base_url(base)
            .with_store_root(tempfile::tempdir().unwrap().path());
        let mut request = run_request();
        request.env.insert("NVIDIA_API_KEY".into(), "nvapi-fixture-key-123456".into());
        let (controls, _steering, interrupt, _turn) = run_controls();
        let mut stream = harness.run(request, controls).await.unwrap();
        assert!(matches!(stream.next().await.unwrap().unwrap(), AgentEvent::SessionStarted { .. }));
        accepted_rx.await.unwrap();
        interrupt.cancel();
        let done = tokio::time::timeout(Duration::from_secs(2), async move {
            while let Some(event) = stream.next().await {
                if let AgentEvent::Done { status, .. } = event.unwrap() {
                    return status;
                }
            }
            panic!("stream ended without Done")
        }).await.unwrap();
        assert_eq!(done, DoneStatus::Interrupted);
        server.abort();
    }

    #[tokio::test]
    async fn rejected_keys_are_visible_errors_not_a_stub() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            respond(&mut socket, "401 Unauthorized", "application/json", r#"{"error":{"message":"bad key"}}"#).await;
        });
        let harness = NvidiaNimHarness::new()
            .with_base_url(base)
            .with_store_root(tempfile::tempdir().unwrap().path());
        let mut request = run_request();
        request.env.insert("NVIDIA_API_KEY".into(), "nvapi-fixture-key-123456".into());
        let (controls, _steering, _interrupt, _turn) = run_controls();
        let mut stream = harness.run(request, controls).await.unwrap();
        let mut saw_error = false;
        let status = loop {
            let event = stream.next().await.unwrap().unwrap();
            if let AgentEvent::Error { message } = &event {
                saw_error |= message.contains("rejected the API key");
                assert!(!message.contains("nvapi-fixture-key-123456"));
            }
            if let AgentEvent::Done { status, .. } = event {
                break status;
            }
        };
        assert!(saw_error);
        assert_eq!(status, DoneStatus::Errored);
        server.await.unwrap();
    }

    #[test]
    fn revert_truncates_saved_history_at_the_user_message() {
        let tmp = tempfile::tempdir().unwrap();
        let store = HistoryStore {
            root: tmp.path().to_path_buf(),
        };
        store
            .save(
                "s1",
                &[
                    HistoryMessage::user("first".into(), Vec::new(), Some("m1".into())),
                    HistoryMessage::assistant("a".into()),
                    HistoryMessage::user("second".into(), Vec::new(), Some("m2".into())),
                    HistoryMessage::assistant("b".into()),
                ],
            )
            .unwrap();
        let mut request = run_request();
        request.resume = Some("s1".into());
        let target = crate::RevertTarget {
            message_id: "m2".into(),
            user_index: 1,
            text: "second".into(),
        };
        assert!(store.preview(&request, &target).unwrap().is_empty());
        store.commit(&request, &target).unwrap();
        let kept = store.load("s1").unwrap().unwrap();
        assert_eq!(
            kept.iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "a"]
        );
    }
}
