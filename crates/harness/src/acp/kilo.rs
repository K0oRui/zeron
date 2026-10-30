//! Kilo Code (`kilo acp`) — the Kilo CLI, a fork of opencode that ships its
//! own ACP server. It keeps its own data dir (`$XDG_DATA_HOME/kilo`), so its
//! `auth.json` never collides with opencode's, and signs in through
//! `kilo auth login` (Kilo's own account store, or any configured provider —
//! the `auth` command, aliased `providers`, carries a `login` subcommand).
//!
//! Everything Kilo-specific about the provider lives here: the ACP spec, the
//! install/skill paths, the discovery budget, the gateway-catalog curation
//! (prefix stripped; balance filtering happens at the engine's RPC boundary)
//! and the Windows stdin-nudge workaround.

use std::path::PathBuf;
use std::time::Duration;

use zeron_proto::kilo::strip_gateway_label;
use zeron_proto::{HarnessId, Model, ReasoningLevel, SteeringMode};

use super::{AcpAgentSpec, default_effort_values, identity_transform};

/// Kilo's catalog comes from its gateway, which it fetches with a 10s HTTP
/// timeout the first time `session/new` asks for providers. Matching that
/// with the generic 10s probe budget killed the child just as Kilo would have
/// answered, so a slow gateway fell back to the one-row static catalog
/// ("Auto (Free)"). Leave room for process boot, directory snapshot, and
/// Kilo's own fetch/retry.
pub(super) const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);

/// Kilo runs `git config --get remote.origin.url` through Bun Shell on every
/// LLM request with stdin inherited. Under `kilo acp` that stdin is the
/// JSON-RPC pipe, so on Windows the git child blocks until the client writes
/// another line and the turn produces nothing at all (Kilo issue #10768;
/// `kilo run` is unaffected). Kilo ignores blank lines — the ACP ndjson
/// stream trims and skips them — so a periodic newline releases the child
/// without touching the turn.
///
/// The blocking mechanism exists only on Windows, so the nudge compiles to
/// `None` everywhere else instead of writing a line a second into the pipe
/// for no benefit. Retire once the upstream fix (PR #15374) ships in a `kilo`
/// release.
#[cfg(windows)]
pub(super) const STDIN_NUDGE_PERIOD: Option<Duration> = Some(Duration::from_secs(1));
/// See the `windows` variant: the turn stall it works around is Windows-only.
#[cfg(not(windows))]
pub(super) const STDIN_NUDGE_PERIOD: Option<Duration> = None;

pub(super) fn spec() -> AcpAgentSpec {
    AcpAgentSpec {
        id: HarnessId::Kilo,
        display_name: "Kilo Code",
        executable: "kilo",
        env_override: "KILO_EXECUTABLE",
        args: &["acp"],
        // The CLI *is* the ACP server, so a managed adapter copy would be a
        // second full agent install; the user's own `kilo` is the one that
        // holds their logins and config.
        archive: None,
        extra_paths: install_paths,
        cli_executable: "kilo",
        cli_extra_paths: install_paths,
        install_hint: "kilo (searched PATH, the login shell's PATH, ~/.kilo/bin, \
             ~/.local/bin, ~/.bun/bin, ~/.npm-global/bin, /opt/homebrew/bin, \
             /usr/local/bin, and fnm/nvm/volta/pnpm/bun install dirs; install with \
             `npm install -g @kilocode/cli` or \
             `curl -fsSL https://kilo.ai/cli/install | bash`, then \
             `kilo auth login`; set KILO_EXECUTABLE to override)",
        // Fallback only: discovery reads the real catalog from the ACP
        // session's `model` config option (every provider the user configured).
        // Kilo's own gateway default needs no API key.
        models: fallback_rows,
        // No `_session/steering` extension: a steer waits for the turn end.
        steering_mode: SteeringMode::TurnBoundary,
        // Effort rides the session's `effort` option (category
        // `thought_level`), whose values are the model's reasoning variants.
        reasoning_levels: &[
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
            ReasoningLevel::XHigh,
            ReasoningLevel::Max,
        ],
        prompt_transform: identity_transform,
        effort_values: default_effort_values,
        ladder_extras: &[],
        prompt_complete_extension: false,
        prompt_stall: None,
        stall_hint: "The agent process is likely wedged.",
        effort_in_model_id: false,
        // Kilo's ACP server serves anonymous/free runs and leaves account
        // handling to `kilo auth login`; it never needs a client-side
        // authenticate step.
        auth_method: None,
        skill_dirs,
        hidden_commands: &[],
        drops_unstarted_cancelled_prompt: false,
    }
}

/// The picker's static fallback when the live gateway catalog can't be read,
/// and the engine's answer to an empty discovery result. `kilo/kilo-auto/free`
/// is the gateway's own default free router
/// (`DEFAULT_FREE_MODEL = "kilo-auto/free"` behind provider id `kilo`), so an
/// anonymous install still runs.
fn fallback_rows() -> Vec<Model> {
    vec![Model {
        id: "kilo/kilo-auto/free".into(),
        label: "Kilo Auto (Free)".into(),
        description: Some("Kilo's router picks a model for the task".into()),
        reasoning_levels: Vec::new(),
        options: Vec::new(),
    }]
}

/// Keep the entire advertised catalog in the credential-scoped cache. A
/// balance can change without changing credentials; the engine applies it
/// after loading live, cached OR fallback rows, never before persistence.
pub(super) fn curate_gateway_rows(mut rows: Vec<Model>) -> Vec<Model> {
    for row in &mut rows {
        row.label = strip_gateway_label(&row.label).to_owned();
    }
    rows
}

fn install_paths() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = crate::executable::home_dir() {
        // `curl -fsSL https://kilo.ai/cli/install | bash` installs here.
        dirs.push(home.join(".kilo").join("bin").join("kilo"));
        dirs.push(home.join(".local").join("bin").join("kilo"));
        dirs.push(home.join(".bun").join("bin").join("kilo"));
        dirs.push(home.join(".npm-global").join("bin").join("kilo"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin/kilo"));
    dirs.push(PathBuf::from("/usr/local/bin/kilo"));
    dirs
}

/// Kilo's own skill folders: `<config>/skill{,s}/**/SKILL.md`, where config is
/// `$KILO_CONFIG_DIR` or `$XDG_CONFIG_HOME/kilo`.
fn skill_dirs() -> Vec<PathBuf> {
    let Some(config) = std::env::var_os("KILO_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| crate::executable::home_dir().map(|home| home.join(".config")))
        })
    else {
        return Vec::new();
    };
    let config = config.join("kilo");
    vec![config.join("skills"), config.join("skill")]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_is_the_registered_descriptor() {
        // Engine's registry test pins the DESCRIPTOR side; this pins the
        // SPEC side, so a drift fails in whichever crate introduced it.
        let spec = spec();
        assert_eq!(spec.id, HarnessId::Kilo);
        assert_eq!(spec.display_name, "Kilo Code");
        assert_eq!(spec.executable, "kilo");
        assert_eq!(spec.env_override, "KILO_EXECUTABLE");
        assert_eq!(spec.args, ["acp"].as_slice());
        assert!(matches!(spec.steering_mode, SteeringMode::TurnBoundary));
        assert_eq!(
            spec.reasoning_levels,
            [
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max,
            ]
            .as_slice()
        );
        // Anonymous free runs never prompt a client-side authenticate.
        assert!(spec.auth_method.is_none());
        let fallback = (spec.models)();
        assert_eq!(fallback.len(), 1);
        assert_eq!(fallback[0].id, "kilo/kilo-auto/free");
    }

    #[test]
    fn gateway_rows_keep_all_models_and_drop_the_prefix() {
        let row = |id: &str, label: &str| Model {
            id: id.into(),
            label: label.into(),
            description: None,
            reasoning_levels: Vec::new(),
            options: Vec::new(),
        };
        let models = curate_gateway_rows(vec![
            row("kilo/kilo-auto/free", "Kilo Gateway/Auto Free"),
            row(
                "kilo/nvidia/nemotron-3-super-120b-a12b:free",
                "Kilo Gateway/NVIDIA: Nemotron 3 Super (free)",
            ),
            row(
                "kilo/anthropic/claude-sonnet-5",
                "Kilo Gateway/Anthropic: Claude Sonnet 5",
            ),
            row("kilo/freeform/model", "Kilo Gateway/Freeform"),
        ]);
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec![
                "kilo/kilo-auto/free",
                "kilo/nvidia/nemotron-3-super-120b-a12b:free",
                "kilo/anthropic/claude-sonnet-5",
                "kilo/freeform/model"
            ]
        );
        assert_eq!(
            models.iter().map(|m| m.label.as_str()).collect::<Vec<_>>(),
            vec![
                "Auto Free",
                "NVIDIA: Nemotron 3 Super (free)",
                "Anthropic: Claude Sonnet 5",
                "Freeform"
            ]
        );
    }

    #[test]
    fn discovery_does_not_make_a_balance_decision() {
        let row = |id: &str, label: &str| Model {
            id: id.into(),
            label: label.into(),
            description: None,
            reasoning_levels: Vec::new(),
            options: Vec::new(),
        };
        let models = curate_gateway_rows(vec![row(
            "kilo/anthropic/claude-sonnet-5",
            "Kilo Gateway/Anthropic: Claude Sonnet 5",
        )]);
        assert_eq!(models.len(), 1);
    }

    #[test]
    fn the_stdin_nudge_is_windows_only() {
        assert_eq!(STDIN_NUDGE_PERIOD.is_some(), cfg!(windows));
    }

    #[test]
    fn install_paths_cover_the_installer_and_homebrew() {
        let paths = install_paths();
        assert!(paths.contains(&PathBuf::from("/opt/homebrew/bin/kilo")));
        assert!(paths.contains(&PathBuf::from("/usr/local/bin/kilo")));
        if let Some(home) = crate::executable::home_dir() {
            // The install script's fixed location.
            assert!(paths.contains(&home.join(".kilo").join("bin").join("kilo")));
        }
    }
}
