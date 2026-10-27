//! NVIDIA NIM API-key accounts owned by Zeron.
//!
//! NIM has no CLI credential file to rewrite. Each pasted key is therefore a
//! normal private account slot, with a separate active-selection file. Runs
//! receive only the selected key in `RunRequest::env`; the parked-runtime
//! fingerprint makes an account switch take effect on the next turn. With no
//! selected slot, the harness may still use the device's `NVIDIA_API_KEY`.

use std::path::PathBuf;

use sha2::{Digest, Sha256};
use zeron_proto::{AgentAccountsSnapshot, AgentAuthKind, HarnessId};

use super::{
    AgentAccounts, EngineError, Slot, SlotProfile, now_ms, remove_if_exists, slot_id_for,
    write_file_atomic,
};

const ACTIVE_FILE: &str = "nvidia-nim-active";
const ENVIRONMENT_CREDENTIAL: &str = "nvidia-nim:environment";

impl AgentAccounts {
    fn nvidia_nim_active_file(&self) -> Result<PathBuf, EngineError> {
        Ok(self.inner.config.private_root()?.join(ACTIVE_FILE))
    }

    fn nvidia_nim_active_id(&self) -> Option<String> {
        std::fs::read_to_string(self.nvidia_nim_active_file().ok()?)
            .ok()
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty())
    }

    /// The selected saved key, if its slot still exists and parses.
    pub(super) fn nvidia_nim_active(&self) -> Option<Slot> {
        self.read_slot(HarnessId::NvidiaNim, &self.nvidia_nim_active_id()?)
    }

    pub(super) fn set_nvidia_nim_active(&self, id: Option<&str>) -> Result<(), EngineError> {
        let file = self.nvidia_nim_active_file()?;
        match id {
            Some(id) => write_file_atomic(&file, id.as_bytes(), true),
            None => remove_if_exists(&file),
        }
    }

    /// Save a key without making a second account replace the selected one.
    /// The first saved key is selected automatically, matching the other
    /// Zeron-owned account stores.
    pub(super) async fn save_nvidia_nim_api_key(
        &self,
        raw_key: &str,
    ) -> Result<AgentAccountsSnapshot, EngineError> {
        let key = validate_nvidia_nim_api_key(raw_key)?;
        let digest = Sha256::digest(key.as_bytes());
        let account_key = format!("api-key:{}", &crate::repos::hex(&digest)[..12]);
        let id = slot_id_for(HarnessId::NvidiaNim, &account_key);
        let tail: String = key
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let _ops = self.inner.ops.lock().await;
        self.write_slot(&Slot {
            id: id.clone(),
            harness: HarnessId::NvidiaNim,
            account_key,
            profile: SlotProfile {
                email: format!("NVIDIA API key ·…{tail}"),
                display_name: None,
                organization: None,
                plan: Some("API key".into()),
                auth_kind: AgentAuthKind::ApiKey,
            },
            credentials: serde_json::json!({ "NVIDIA_API_KEY": key }),
            claude_config: None,
            saved_at: now_ms(),
            created_at: None,
            store_key: None,
        })?;
        if self.nvidia_nim_active().is_none() {
            self.set_nvidia_nim_active(Some(&id))?;
        }
        self.list_locked(false).await
    }

    /// Remove only NIM's own slot and selection. NVIDIA keys are revoked in
    /// NVIDIA's console; deleting a local copy cannot revoke it server-side.
    pub(super) fn forget_nvidia_nim(&self, id: &str) -> Result<(), EngineError> {
        if self.nvidia_nim_active_id().as_deref() == Some(id) {
            self.set_nvidia_nim_active(None)?;
        }
        remove_if_exists(&self.slots_dir(HarnessId::NvidiaNim)?.join(format!("{id}.json")))
    }

    /// The selected run key and an opaque fingerprint for parked-run routing.
    pub fn nvidia_nim_run_env(&self) -> Result<(Option<String>, String), String> {
        let Some(slot) = self.nvidia_nim_active() else {
            let fingerprint = std::env::var("NVIDIA_API_KEY")
                .ok()
                .map(|key| key.trim().to_owned())
                .filter(|key| !key.is_empty())
                .map(|key| {
                    format!(
                        "nvidia-nim:environment:{:x}",
                        Sha256::digest(key.as_bytes())
                    )
                })
                .unwrap_or_else(|| ENVIRONMENT_CREDENTIAL.into());
            return Ok((None, fingerprint));
        };
        let key = slot
            .credentials
            .get("NVIDIA_API_KEY")
            .and_then(serde_json::Value::as_str)
            .filter(|key| !key.is_empty())
            .ok_or_else(|| "The selected NVIDIA NIM account has no readable API key.".to_owned())?;
        Ok((Some(key.to_owned()), format!("nvidia-nim:{}", slot.id)))
    }
}

/// Injects the selected NIM key without interfering with any other provider.
pub struct NvidiaNimRunEnvironment(pub AgentAccounts);

#[async_trait::async_trait]
impl crate::registry::RunEnvironment for NvidiaNimRunEnvironment {
    async fn prepare(
        &self,
        harness: HarnessId,
        request: &mut zeron_proto::RunRequest,
    ) -> Result<(), String> {
        if harness != HarnessId::NvidiaNim {
            return Ok(());
        }
        if let (Some(key), _) = self.0.nvidia_nim_run_env()? {
            // The Cloud broker runs first and owns a key it explicitly set.
            request.env.entry("NVIDIA_API_KEY".into()).or_insert(key);
        }
        Ok(())
    }

    fn model_api_key(&self, harness: HarnessId) -> Result<Option<String>, String> {
        if harness != HarnessId::NvidiaNim {
            return Ok(None);
        }
        self.0.nvidia_nim_run_env().map(|(key, _)| key)
    }

    async fn credential(&self, harness: HarnessId) -> Option<String> {
        if harness != HarnessId::NvidiaNim {
            return None;
        }
        self.0
            .nvidia_nim_run_env()
            .ok()
            .map(|(_, fingerprint)| fingerprint)
    }
}

/// NVIDIA-hosted keys currently use the `nvapi-` prefix. Keep the check
/// strict enough to catch truncated pastes, but do not echo any key material.
fn validate_nvidia_nim_api_key(raw: &str) -> Result<String, EngineError> {
    let key = raw.trim().to_owned();
    if key.is_empty() {
        return Err(EngineError::Other(
            "That key looks empty — paste the whole NVIDIA API key.".into(),
        ));
    }
    if !key.is_ascii() || key.chars().any(char::is_whitespace) {
        return Err(EngineError::Other(
            "That key contains whitespace or non-ASCII characters — paste only the key.".into(),
        ));
    }
    if !key.starts_with("nvapi-") || key.len() < 20 || key.len() > 512 {
        return Err(EngineError::Other(
            "That doesn't look like an NVIDIA API key — it should start with `nvapi-`.".into(),
        ));
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_accounts::AgentAccountsConfig;
    use crate::registry::RunEnvironment;
    use zeron_proto::{AgentLoginMode, AgentLoginStatus};

    fn request() -> zeron_proto::RunRequest {
        serde_json::from_str(
            r#"{"prompt":"p","model":null,"reasoning":null,"cwd":".","sandbox":"workspace-write","resume":null}"#,
        )
        .unwrap()
    }

    #[test]
    fn key_validation_never_accepts_a_truncated_or_foreign_key() {
        assert!(validate_nvidia_nim_api_key("nvapi-1234567890abcdef").is_ok());
        for bad in ["", "sk-1234567890abcdef", "nvapi-short", "nvapi-has space-123456"] {
            assert!(validate_nvidia_nim_api_key(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[tokio::test]
    async fn codex_api_keys_keep_the_legacy_flow_on_generic_completion() {
        let tmp = tempfile::tempdir().unwrap();
        let accounts = AgentAccounts::new(AgentAccountsConfig::isolated(tmp.path()));
        let start = accounts
            .start_login_with(HarnessId::Codex, Some("api-key"), None)
            .await
            .unwrap();
        assert_eq!(start.mode, AgentLoginMode::ApiKey);
        let snapshot = accounts
            .complete_login(&start.login_id, "sk-test-12345678abcd")
            .await
            .unwrap();
        assert!(snapshot.accounts.iter().any(|account| {
            account.harness == HarnessId::Codex
                && account.auth_kind == Some(AgentAuthKind::ApiKey)
        }));
    }

    #[tokio::test]
    async fn api_keys_connect_switch_inject_and_forget() {
        let tmp = tempfile::tempdir().unwrap();
        let accounts = AgentAccounts::new(AgentAccountsConfig::isolated(tmp.path()));
        let env = NvidiaNimRunEnvironment(accounts.clone());
        let first = "nvapi-first-account-1234567890";
        let second = "nvapi-second-account-abcdefghij";

        let start = accounts.start_login(HarnessId::NvidiaNim).await.unwrap();
        assert_eq!(start.mode, AgentLoginMode::ApiKey);
        assert!(start.url.is_empty());
        assert_eq!(
            accounts.poll_login(&start.login_id).await.unwrap().status,
            AgentLoginStatus::Pending
        );
        let snapshot = accounts.complete_login(&start.login_id, first).await.unwrap();
        let rows: Vec<_> = snapshot
            .accounts
            .iter()
            .filter(|row| row.harness == HarnessId::NvidiaNim)
            .collect();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].active);
        assert_eq!(rows[0].auth_kind, Some(AgentAuthKind::ApiKey));

        let start = accounts.start_login(HarnessId::NvidiaNim).await.unwrap();
        let snapshot = accounts.complete_login(&start.login_id, second).await.unwrap();
        let rows: Vec<_> = snapshot
            .accounts
            .iter()
            .filter(|row| row.harness == HarnessId::NvidiaNim)
            .collect();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].active && !rows[1].active, "adding is not switching");

        let mut run = request();
        env.prepare(HarnessId::NvidiaNim, &mut run).await.unwrap();
        assert_eq!(run.env["NVIDIA_API_KEY"], first);
        assert_eq!(
            env.model_api_key(HarnessId::NvidiaNim).unwrap().as_deref(),
            Some(first)
        );
        assert_eq!(env.model_api_key(HarnessId::Codex).unwrap(), None);
        let before = env.credential(HarnessId::NvidiaNim).await.unwrap();
        assert!(!before.contains(first));

        let snapshot = accounts
            .activate(HarnessId::NvidiaNim, &rows[1].id)
            .await
            .unwrap();
        let rows: Vec<_> = snapshot
            .accounts
            .iter()
            .filter(|row| row.harness == HarnessId::NvidiaNim)
            .collect();
        assert!(!rows[0].active && rows[1].active);
        assert_ne!(env.credential(HarnessId::NvidiaNim).await.unwrap(), before);
        let mut run = request();
        env.prepare(HarnessId::NvidiaNim, &mut run).await.unwrap();
        assert_eq!(run.env["NVIDIA_API_KEY"], second);
        assert_eq!(
            env.model_api_key(HarnessId::NvidiaNim).unwrap().as_deref(),
            Some(second)
        );

        let snapshot = accounts
            .forget(HarnessId::NvidiaNim, &rows[1].id)
            .await
            .unwrap();
        let remaining: Vec<_> = snapshot
            .accounts
            .iter()
            .filter(|row| row.harness == HarnessId::NvidiaNim)
            .collect();
        assert_eq!(remaining.len(), 1);
        assert!(!remaining[0].active);
        let mut run = request();
        env.prepare(HarnessId::NvidiaNim, &mut run).await.unwrap();
        assert!(!run.env.contains_key("NVIDIA_API_KEY"));

        let mut other = request();
        env.prepare(HarnessId::Codex, &mut other).await.unwrap();
        assert!(other.env.is_empty());
    }
}
