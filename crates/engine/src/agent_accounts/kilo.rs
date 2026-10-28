//! Balance-aware Kilo Gateway catalogs. The raw inventory is cached by the
//! harness; apply the CURRENT balance after every ListModels (including disk
//! and static fallbacks). A top-up therefore restores paid rows without a
//! login, and a depleted balance cannot resurrect paid rows from disk.
//!
//! Only gateway rows spend this balance. ChatGPT/Copilot/custom providers in
//! the same Kilo catalog keep their own authentication and entitlements.

use serde_json::{Map, Value};
use zeron_proto::{Model, kilo::models_for_balance};

use super::{AgentAccounts, EngineError, read_json, str_field};

pub(super) const KILO_BALANCE_URL: &str = "https://api.kilo.ai/api/profile/balance";
const BALANCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

// No Debug: this holds a live token, not something to include in diagnostics.
#[derive(PartialEq, Eq)]
struct GatewayAccount {
    token: Option<String>,
    organization: Option<String>,
    /// False for custom endpoints or unreadable/interpolated credentials: do
    /// not send their tokens to Kilo or mistake them for an anonymous account.
    known: bool,
}

impl AgentAccounts {
    pub(crate) async fn filter_kilo_models(
        &self,
        models: Vec<Model>,
    ) -> Result<Vec<Model>, EngineError> {
        if !models.iter().any(|model| model.id.starts_with("kilo/")) {
            return Ok(models);
        }
        let account = self.kilo_gateway_account();
        let balance = if !account.known {
            None
        } else if let Some(token) = account.token.as_deref() {
            self.kilo_balance(token, account.organization.as_deref())
                .await
        } else {
            // The CLI's anonymous gateway account cannot spend paid credits.
            Some(0.0)
        };
        if self.kilo_gateway_account() != account {
            return Err(EngineError::Other(
                "Kilo account changed while loading models; retry.".into(),
            ));
        }
        Ok(models_for_balance(models, balance, |model| {
            !model.id.starts_with("kilo/")
                || zeron_proto::kilo::gateway_model_is_free(&model.id, &model.label)
        }))
    }

    fn kilo_gateway_account(&self) -> GatewayAccount {
        let auth_file = &self.inner.config.kilo_auth_file;
        let auth = read_json(auth_file);
        let options = gateway_options();
        let config_known = options.is_ok();
        let options = options.unwrap_or_default();
        let mut account = gateway_account(
            auth.as_ref().and_then(|auth| auth.get("kilo")),
            &options,
            std::env::var("KILO_API_KEY").ok().as_deref(),
            std::env::var("KILO_ORG_ID").ok().as_deref(),
        );
        // Missing is anonymous; corrupt/unreadable is UNKNOWN, not zero.
        if !config_known
            || (auth_file.exists() && auth.is_none())
            || auth.as_ref().is_some_and(|auth| !auth.is_object())
        {
            account.known = false;
        }
        if let Ok(base) = std::env::var("KILO_API_URL")
            && !is_kilo_endpoint(&base)
        {
            account.known = false;
        }
        account
    }

    async fn kilo_balance(&self, token: &str, organization: Option<&str>) -> Option<f64> {
        let mut request = self
            .inner
            .http
            .get(&self.inner.endpoints.kilo_balance)
            .timeout(BALANCE_TIMEOUT)
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(organization) = organization {
            request = request.header("x-kilocode-organizationid", organization);
        }
        let result = async {
            let response = request.send().await?.error_for_status()?;
            response.json::<Value>().await
        }
        .await;
        match result {
            Ok(body) => balance_value(&body),
            Err(_) => {
                // A timeout, 401 or a changed response schema is not evidence
                // that the account has no money. Never log the request/token.
                tracing::debug!("Kilo balance unavailable; preserving advertised models");
                None
            }
        }
    }
}

fn balance_value(body: &Value) -> Option<f64> {
    body.get("balance")
        .and_then(Value::as_f64)
        .filter(|balance| balance.is_finite())
}

fn gateway_account(
    auth: Option<&Value>,
    options: &Map<String, Value>,
    env_key: Option<&str>,
    env_org: Option<&str>,
) -> GatewayAccount {
    let stored_token = auth.and_then(|auth| match auth.get("type").and_then(Value::as_str) {
        Some("api") => str_field(auth, "key"),
        Some("oauth") => str_field(auth, "access"),
        _ => None,
    });
    // Mirrors Kilo's auth plugin: kilocodeToken wins over apiKey. A plugin
    // supplies the stored token; explicit kilocodeToken config can override it.
    let token = config_string(options, "kilocodeToken")
        .or(stored_token)
        .or_else(|| config_string(options, "apiKey"))
        .or_else(|| clean(env_key));
    let organization = config_string(options, "kilocodeOrganizationId")
        .or_else(|| clean(env_org))
        .or_else(|| auth.and_then(|auth| str_field(auth, "accountId")));
    let unresolved = [token.as_deref(), organization.as_deref()]
        .into_iter()
        .flatten()
        .any(|value| value.contains("{env:") || value.contains("{file:"));
    let custom_endpoint = options
        .get("baseURL")
        .and_then(Value::as_str)
        .is_some_and(|url| !is_kilo_endpoint(url));
    let malformed_auth = auth.is_some() && token.is_none();
    let malformed_options = [
        "apiKey",
        "kilocodeToken",
        "kilocodeOrganizationId",
        "baseURL",
    ]
    .iter()
    .filter_map(|key| options.get(*key))
    .any(|value| !value.is_string() && !value.is_null());
    GatewayAccount {
        token: token.filter(|token| !token.eq_ignore_ascii_case("anonymous")),
        organization,
        known: !unresolved && !custom_endpoint && !malformed_auth && !malformed_options,
    }
}

fn clean(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn config_string(options: &Map<String, Value>, key: &str) -> Option<String> {
    let value = clean(options.get(key).and_then(Value::as_str))?;
    if let Some(variable) = value
        .strip_prefix("{env:")
        .and_then(|value| value.strip_suffix('}'))
    {
        // Preserve an unresolved reference as unknown, never anonymous.
        return std::env::var(variable)
            .ok()
            .and_then(|value| clean(Some(&value)))
            .or(Some(value));
    }
    Some(value)
}

fn is_kilo_endpoint(raw: &str) -> bool {
    reqwest::Url::parse(raw).ok().is_some_and(|url| {
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port_or_known_default() == Some(443)
            && matches!(url.host_str(), Some("api.kilo.ai" | "kilo.ai"))
    })
}

/// Kilo also accepts a key/org in provider options. Read its global configs
/// and explicit config overrides rather than hiding a funded config-only
/// account. HJSON accepts JSONC comments/trailing commas used by the CLI.
fn gateway_options() -> Result<Map<String, Value>, ()> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| crate::repos::home_dir().join(".config"));
    let mut files = Vec::new();
    for provider in ["opencode", "kilo"] {
        for extension in ["json", "jsonc"] {
            files.push(
                config
                    .join(provider)
                    .join(format!("{provider}.{extension}")),
            );
        }
    }
    if let Some(file) = std::env::var_os("KILO_CONFIG") {
        files.push(file.into());
    }
    if let Some(dir) = std::env::var_os("KILO_CONFIG_DIR") {
        for extension in ["json", "jsonc"] {
            files.push(std::path::PathBuf::from(&dir).join(format!("kilo.{extension}")));
        }
    }
    let mut options = Map::new();
    for path in files {
        match std::fs::read_to_string(path) {
            Ok(raw) => merge_options(&mut options, &raw)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(()),
        }
    }
    if let Ok(raw) = std::env::var("KILO_CONFIG_CONTENT") {
        merge_options(&mut options, &raw)?;
    }
    Ok(options)
}

fn merge_options(options: &mut Map<String, Value>, raw: &str) -> Result<(), ()> {
    let config = deser_hjson::from_str::<Value>(raw).map_err(|_| ())?;
    if !config.is_object()
        || config
            .pointer("/provider/kilo/options")
            .is_some_and(|options| !options.is_object())
    {
        return Err(());
    }
    if let Some(configured) = config
        .pointer("/provider/kilo/options")
        .and_then(Value::as_object)
    {
        options.extend(configured.clone());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_balance_is_not_zero() {
        for body in [
            json!({}),
            json!({"balance":null}),
            json!({"balance":"invalid"}),
            json!({"other":0}),
        ] {
            assert_eq!(balance_value(&body), None);
        }
        assert_eq!(balance_value(&json!({"balance":0})), Some(0.0));
        assert_eq!(balance_value(&json!({"balance":-0.1})), Some(-0.1));
        assert_eq!(balance_value(&json!({"balance":12.5})), Some(12.5));
    }

    #[test]
    fn gateway_uses_its_own_login_and_selected_organization() {
        let auth = json!({"type":"oauth", "access":"fixture-token", "accountId":"team-a"});
        let account = gateway_account(Some(&auth), &Map::new(), None, None);
        assert_eq!(account.token.as_deref(), Some("fixture-token"));
        assert_eq!(account.organization.as_deref(), Some("team-a"));
        assert!(account.known);
        let account = gateway_account(Some(&auth), &Map::new(), Some("env-key"), Some("team-b"));
        assert_eq!(account.token.as_deref(), Some("fixture-token"));
        assert_eq!(account.organization.as_deref(), Some("team-b"));
    }

    #[test]
    fn anonymous_and_custom_accounts_are_distinct() {
        let anonymous = gateway_account(None, &Map::new(), None, None);
        assert!(anonymous.known && anonymous.token.is_none());
        let custom = json!({"apiKey":"private-key", "baseURL":"https://other.example/v1"});
        let custom = gateway_account(None, custom.as_object().unwrap(), None, None);
        assert!(!custom.known);
        let malformed = gateway_account(Some(&json!({"type":"oauth"})), &Map::new(), None, None);
        assert!(!malformed.known);
        for url in [
            "http://api.kilo.ai",
            "https://api.kilo.ai.evil.example",
            "https://evil@api.kilo.ai",
            "https://api.kilo.ai:8443",
        ] {
            assert!(!is_kilo_endpoint(url));
        }
    }

    #[test]
    fn config_only_keys_and_organization_are_supported() {
        let mut options = Map::new();
        merge_options(
            &mut options,
            r#"{
            // Kilo's JSONC provider config
            "provider":{"kilo":{"options":{"apiKey":"configured-key","kilocodeOrganizationId":"team-c",}}},
        }"#,
        ).unwrap();
        let account = gateway_account(None, &options, None, None);
        assert_eq!(account.token.as_deref(), Some("configured-key"));
        assert_eq!(account.organization.as_deref(), Some("team-c"));
        assert!(account.known);
        assert!(merge_options(&mut options, "null").is_err());
        assert!(merge_options(&mut options, r#"{"provider":{"kilo":{"options":42}}}"#).is_err());
    }
}
