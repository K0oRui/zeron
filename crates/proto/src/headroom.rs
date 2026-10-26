//! Headroom context-compression proxy: per-device install and routing state.
//!
//! Headroom runs on the device that spawns the agent, so enablement, the
//! installation, and the proxy's health all live beside the engine rather than
//! in the synced workspace doc. This type is the Settings page's whole view of
//! that runtime.

use serde::{Deserialize, Serialize};

use crate::agent::HarnessId;

/// The device-local Headroom runtime as Settings sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadroomStatus {
    /// The user wants agent traffic routed through the proxy.
    pub enabled: bool,
    /// A `headroom` CLI (or importable `headroom` module) answers on this
    /// device.
    pub installed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The installed build is older than the newest release PyPI advertises.
    /// An unknown or failed check reports `false`, so the Update button only
    /// appears when it is earned.
    #[serde(default)]
    pub update_available: bool,
    /// The proxy is answering health checks right now.
    pub running: bool,
    /// The loopback port the proxy listens on.
    pub port: u16,
    /// An install or update is running.
    #[serde(default)]
    pub installing: bool,
    /// The last routing/startup failure, cleared by the next successful action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Providers this build routes through the proxy.
    #[serde(default)]
    pub providers: Vec<HarnessId>,
    /// Agents Zeron can run that Headroom cannot route: their CLIs read no
    /// base-URL variable and take no inline proxy config. Settings names them
    /// so the card never reads as "every provider is compressed".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unrouted: Vec<HarnessId>,
    /// The command Install/Update runs, for display when automation is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_command: Option<String>,
}

impl HeadroomStatus {
    /// A status for engines that know nothing about Headroom.
    pub fn unavailable() -> Self {
        Self {
            enabled: false,
            installed: false,
            version: None,
            update_available: false,
            running: false,
            port: 8787,
            installing: false,
            error: None,
            providers: Vec::new(),
            unrouted: Vec::new(),
            manual_command: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_uses_stable_camel_case_fields() {
        let status = HeadroomStatus {
            enabled: true,
            installed: true,
            version: Some("0.4.1".into()),
            update_available: true,
            running: true,
            port: 8787,
            installing: false,
            error: None,
            providers: vec![HarnessId::ClaudeCode, HarnessId::Codex],
            unrouted: vec![HarnessId::Hermes],
            manual_command: None,
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["enabled"], true);
        assert_eq!(json["installed"], true);
        assert_eq!(json["updateAvailable"], true);
        assert_eq!(json["running"], true);
        assert_eq!(json["providers"][0], "claude-code");
        assert_eq!(json["unrouted"][0], "hermes");
        assert_eq!(
            serde_json::from_value::<HeadroomStatus>(json).unwrap(),
            status
        );
    }

    #[test]
    fn older_engines_default_the_additive_fields() {
        let status: HeadroomStatus = serde_json::from_value(serde_json::json!({
            "enabled": false,
            "installed": false,
            "running": false,
            "port": 8787
        }))
        .unwrap();
        assert!(!status.installing);
        assert!(status.providers.is_empty());
        assert!(status.unrouted.is_empty());
        assert!(status.error.is_none());
        assert!(!status.update_available);
    }
}
