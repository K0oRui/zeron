//! Which agents Zeron signs in to and how: the rules every app's provider
//! settings follow (desktop Settings → Providers, the phone's Providers), so
//! the platforms offer the same sign-ins, labels and switching. Pure.

use serde::{Deserialize, de::DeserializeOwned};

use crate::HarnessId;

/// The providers zeron can sign into, in display order: (harness, name, CLI
/// command or login label — named in the empty-state copy, zeron
/// settings.agents.tsx `PROVIDERS`). Every agent with a login of its own is
/// here; what each one supports is documented engine-side (`agent_accounts`
/// module docs).
pub const PROVIDERS: [(HarnessId, &str, &str); 11] = [
    (HarnessId::ClaudeCode, "Claude Code", "claude"),
    (HarnessId::Codex, "Codex", "codex"),
    (HarnessId::Cursor, "Cursor", "cursor-agent"),
    (HarnessId::Antigravity, "Antigravity", "Antigravity"),
    (HarnessId::Grok, "Grok", "grok login"),
    (HarnessId::Devin, "Devin", "devin auth login"),
    (HarnessId::Opencode, "OpenCode", "opencode auth login"),
    (HarnessId::Kilo, "Kilo Code", "kilo auth login"),
    (HarnessId::Pi, "Pi", "pi"),
    (HarnessId::Hermes, "Hermes", "hermes auth add"),
    (HarnessId::Muse, "Muse Code", "muse login"),
];

/// Whether `harness` has an Accounts section (and sign-in flow). Pure.
pub fn signs_in(harness: HarnessId) -> bool {
    PROVIDERS
        .iter()
        .any(|(provider, _, _)| *provider == harness)
}

pub fn provider_name(harness: HarnessId) -> &'static str {
    if harness == HarnessId::Fx {
        return "fx";
    }
    PROVIDERS
        .iter()
        .find(|(provider, _, _)| *provider == harness)
        .map_or("this provider", |(_, name, _)| name)
}

/// Whether the provider reports plan usage. One that doesn't shows no meters
/// and no "usage unavailable" note — there is nothing missing. Pure.
pub fn reports_usage(harness: HarnessId) -> bool {
    !matches!(harness, HarnessId::Antigravity | HarnessId::Fx)
}

/// Providers whose agent holds exactly ONE login (Antigravity): once it is
/// connected there is nothing to add — signing in again only re-confirms it.
pub fn keeps_one_login(harness: HarnessId) -> bool {
    harness == HarnessId::Antigravity
}

/// Whether zeron switches this agent's logins. Hermes rotates through its
/// own credential pool (listed, never reordered); Antigravity keeps one.
pub fn switches_accounts(harness: HarnessId) -> bool {
    !matches!(harness, HarnessId::Hermes | HarnessId::Antigravity | HarnessId::Fx)
}

/// A standing note under a provider's Accounts label, for an agent whose
/// accounts work differently. Hermes owns its credential pool: zeron lists
/// it and adds to it through Hermes' own CLI, but never switches or removes
/// its entries. Pure.
pub fn provider_note(harness: HarnessId) -> Option<&'static str> {
    match harness {
        HarnessId::Hermes => Some(
            "Hermes manages its own credential pool and rotates through it. Accounts added \
             here go through `hermes auth add`; remove one with `hermes auth remove`.",
        ),
        _ => None,
    }
}

/// One way to add an account: agents that keep a login PER model provider
/// (OpenCode, Kilo, Pi, Hermes) sign in to a named provider; the rest have one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoginOption {
    /// The engine's `provider` param (`None` = the agent's only login).
    pub provider: Option<&'static str>,
    /// Who the user signs in to.
    pub label: &'static str,
}

/// The sign-ins zeron offers for `harness`, in button order. Pure.
pub fn login_options(harness: HarnessId) -> Vec<LoginOption> {
    let option = |provider, label| LoginOption {
        provider: Some(provider),
        label,
    };
    match harness {
        // Codex signs in twice: the ChatGPT OAuth flow, or a pasted
        // `OPENAI_API_KEY` saved next to it (no browser round-trip).
        HarnessId::Codex => vec![option("oauth", "ChatGPT"), option("api-key", "API key")],
        HarnessId::Opencode | HarnessId::Kilo => vec![
            option("openai", "ChatGPT"),
            option("github-copilot", "GitHub Copilot"),
        ],
        HarnessId::Pi => vec![option("openai-codex", "ChatGPT")],
        HarnessId::Hermes => vec![
            option("openai-codex", "ChatGPT"),
            option("nous", "Nous Portal"),
        ],
        _ => vec![LoginOption {
            provider: None,
            label: provider_name(harness),
        }],
    }
}

/// The list's last row: connect the first account, or add another. Pure.
pub fn add_account_label(harness: HarnessId, empty: bool) -> String {
    if empty {
        format!("Connect a {} account", provider_name(harness))
    } else {
        "Add account".to_string()
    }
}

/// [`add_account_label`] for one [`LoginOption`]: a per-provider sign-in
/// names its provider ("Connect ChatGPT", "Add GitHub Copilot account"). Pure.
pub fn add_option_label(harness: HarnessId, option: LoginOption, empty: bool) -> String {
    match option.provider {
        None => add_account_label(harness, empty),
        Some(_) if empty => format!("Connect {}", option.label),
        Some(_) => format!("Add {} account", option.label),
    }
}

/// Decode a provider list from another engine without reviving unsupported
/// providers or losing supported siblings. Malformed supported rows still fail.
pub fn decode_provider_rows<T: DeserializeOwned>(
    mut value: serde_json::Value,
    harness_field: &str,
) -> Result<Vec<T>, serde_json::Error> {
    if let Some(rows) = value.as_array_mut() {
        rows.retain(|row| {
            row.get(harness_field)
                .and_then(serde_json::Value::as_str)
                .is_none_or(|slug| HarnessId::from_slug(slug).is_some())
        });
    }
    serde_json::from_value(value)
}

/// Serde adapter for account/warning lists whose provider field is `harness`.
pub(crate) fn deserialize_provider_rows<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    decode_provider_rows(value, "harness").map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_real_harness_has_one_provider_entry() {
        // fx keeps no account store of its own: it has no row here, and its
        // name comes from the `provider_name` special case.
        assert_eq!(PROVIDERS.len(), HarnessId::ALL.len() - 2);
        for harness in HarnessId::ALL {
            let signs_in = !matches!(harness, HarnessId::Mock | HarnessId::Fx);
            assert_eq!(
                PROVIDERS.iter().filter(|(id, _, _)| *id == harness).count(),
                usize::from(signs_in),
            );
            assert_eq!(super::signs_in(harness), signs_in);
            assert_eq!(HarnessId::from_slug(harness.as_str()), Some(harness));
            assert_eq!(
                serde_json::to_value(harness).unwrap(),
                json!(harness.as_str())
            );
        }
    }

    #[test]
    fn retired_ids_are_not_harnesses_or_login_providers() {
        for slug in ["removed-provider", "nvidia-nim"] {
            assert!(HarnessId::from_slug(slug).is_none());
            assert!(serde_json::from_value::<HarnessId>(json!(slug)).is_err());
            assert!(!PROVIDERS.iter().any(|(id, _, _)| id.as_str() == slug));
        }
    }

    #[derive(Debug, Deserialize)]
    struct Row {
        id: HarnessId,
    }

    #[test]
    fn peer_provider_lists_skip_unsupported_rows_only() {
        let rows: Vec<Row> = decode_provider_rows(
            json!([
                {"id": "codex"},
                {"id": "removed-provider"},
                {"id": "claude-code"}
            ]),
            "id",
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, HarnessId::Codex);
        assert_eq!(rows[1].id, HarnessId::ClaudeCode);
        assert!(decode_provider_rows::<Row>(json!([{}]), "id").is_err());
        assert!(decode_provider_rows::<Row>(json!({}), "id").is_err());
    }

    #[test]
    fn peer_update_status_lists_skip_retired_providers() {
        let rows: Vec<crate::HarnessUpdateStatus> = decode_provider_rows(
            json!([
                {"harness": "removed-provider", "phase": "current"},
                {"harness": "codex", "phase": "current"}
            ]),
            "harness",
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].harness, HarnessId::Codex);
        assert_eq!(rows[0].phase, crate::HarnessUpdatePhase::Current);
    }

    #[test]
    fn peer_account_snapshots_preserve_supported_accounts_and_warnings() {
        let snapshot: crate::AgentAccountsSnapshot = serde_json::from_value(json!({
            "accounts": [
                {"id": "saved", "harness": "codex", "active": true},
                {"harness": "removed-provider"}
            ],
            "warnings": [
                {"harness": "codex", "message": "Keep this warning"},
                {"harness": "removed-provider", "message": "Retired"}
            ]
        }))
        .unwrap();
        assert_eq!(snapshot.accounts.len(), 1);
        assert_eq!(snapshot.accounts[0].harness, HarnessId::Codex);
        assert!(snapshot.accounts[0].active);
        assert_eq!(snapshot.warnings.len(), 1);
        assert_eq!(snapshot.warnings[0].message, "Keep this warning");
    }
}
