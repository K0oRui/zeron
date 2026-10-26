//! Headroom: the optional context-compression proxy agent traffic can ride.
//!
//! Headroom is a per-device runtime: it is installed beside the agent CLIs and
//! listens on loopback, so its preference file, install, and process all live
//! in the engine data dir rather than the synced workspace. When enabled, runs
//! for [`PROVIDERS`] get the proxy in their spawn environment (the same seam
//! the credential broker uses): a base-URL override, or for OpenCode a value
//! its harness turns into provider config. Agents named by [`UNROUTED`] have
//! no such seam, and Settings says so rather than implying full coverage.
//!
//! Everything here fails open: an agent run never waits on Headroom, and a
//! proxy that cannot start only records an error in [`HeadroomStatus`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use zeron_proto::agent::HarnessId;
use zeron_proto::headroom::HeadroomStatus;

/// The loopback port Headroom's proxy listens on.
pub const PORT: u16 = 8787;

/// OpenCode has no base-URL variable. Its harness reads this one and merges
/// Headroom's provider overrides into the config it builds for the child.
const OPENCODE_PROXY_ENV: &str = "HEADROOM_PROXY_URL";
/// Names the transport plugin directory OpenCode should load. Kept in the run
/// environment only, like every other credential-broker value.
const OPENCODE_PLUGIN_ENV: &str = "ZERON_HEADROOM_OPENCODE_PLUGIN";

/// Providers Zeron can route through the proxy. Each one either honors a
/// documented base-URL variable, or (OpenCode) takes the same routing as
/// inline config its harness merges into the `OPENCODE_CONFIG_CONTENT` it
/// already builds. `routes` is the other half of this list: a provider here
/// must produce an override, and [`UNROUTED`] must not.
pub const PROVIDERS: [HarnessId; 6] = [
    HarnessId::ClaudeCode,
    HarnessId::Codex,
    HarnessId::Cursor,
    HarnessId::Grok,
    HarnessId::Opencode,
    HarnessId::Antigravity,
];

/// Agents Zeron runs that Headroom does not currently route. Their drivers
/// are not wired to the proxy's per-provider environment, so traffic bypasses
/// it whatever the preference says. Settings names them instead of letting
/// the routed list imply full coverage (`Mock` is test-only and stays out of
/// both lists).
pub const UNROUTED: [HarnessId; 6] = [
    HarnessId::Devin,
    HarnessId::Hermes,
    HarnessId::Pi,
    HarnessId::Kilo,
    HarnessId::Muse,
    HarnessId::NvidiaNim,
];

const INSTALL_DEADLINE: Duration = Duration::from_secs(15 * 60);
const VERSION_PROBE_DEADLINE: Duration = Duration::from_secs(5);
const PLUGIN_PROBE_DEADLINE: Duration = Duration::from_secs(15);
const PROXY_START_DEADLINE: Duration = Duration::from_secs(25);
const RUNTIME_TTL: Duration = Duration::from_secs(30);
/// How long a release check is trusted before Settings asks PyPI again.
const LATEST_TTL: Duration = Duration::from_secs(60 * 60);
/// A failed check retries sooner, so an offline launch doesn't hide updates
/// for a whole hour, without hammering PyPI on every status call.
const LATEST_RETRY: Duration = Duration::from_secs(5 * 60);
const LATEST_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
enum Runtime {
    Cli(PathBuf),
    Python(PathBuf),
}

impl Runtime {
    fn command(&self) -> tokio::process::Command {
        let mut command = match self {
            Runtime::Cli(path) => tokio::process::Command::new(path),
            // The CLI has no top-level `__main__`; the module entry point is
            // `headroom.cli`, which forwards to the same argument parser.
            Runtime::Python(path) => {
                let mut command = tokio::process::Command::new(path);
                command.args(["-m", "headroom.cli"]);
                command
            }
        };
        zeron_harness::compose_login_shell_path(&mut command);
        hide_console(&mut command);
        command
    }
}

/// Console programs spawned from the GUI app get a console window of their
/// own. The flag also covers children a launcher starts without it (uv's
/// trampoline runs Python with plain flags, and still ends up windowless).
fn hide_console(command: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        command.as_std_mut().creation_flags(CREATE_NO_WINDOW);
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Prefs {
    #[serde(default)]
    enabled: bool,
}

struct Inner {
    enabled: bool,
    installing: bool,
    error: Option<String>,
    runtime: Option<Runtime>,
    version: Option<String>,
    probed_at: Option<Instant>,
    /// The newest release PyPI advertised at the last check.
    latest: Option<String>,
    latest_at: Option<Instant>,
    /// OpenCode's transport plugin, resolved from the installed package.
    plugin: Option<PathBuf>,
    plugin_probed: bool,
    proxy: Option<tokio::process::Child>,
    running: bool,
}

pub struct Headroom {
    prefs_path: PathBuf,
    inner: Mutex<Inner>,
    /// Serializes runtime probes so concurrent status calls spawn one CLI.
    detect: tokio::sync::Mutex<()>,
    /// Serializes release checks so concurrent status calls make one request.
    latest: tokio::sync::Mutex<()>,
    /// Serializes the OpenCode plugin probe, which imports the package.
    plugin_probe: tokio::sync::Mutex<()>,
    /// A background plugin probe is already in flight.
    plugin_probing: std::sync::atomic::AtomicBool,
    /// Serializes proxy start/stop so two runs cannot spawn two proxies.
    start: tokio::sync::Mutex<()>,
    /// A background start is already in flight, so runs don't queue more.
    starting: std::sync::atomic::AtomicBool,
}

impl Headroom {
    /// Load this device's preference. Missing or corrupt files mean "off".
    pub fn open(data_dir: &Path) -> Arc<Self> {
        let prefs_path = data_dir.join("headroom.json");
        let prefs = std::fs::read_to_string(&prefs_path)
            .ok()
            .and_then(|text| serde_json::from_str::<Prefs>(&text).ok())
            .unwrap_or_default();
        Arc::new(Self {
            prefs_path,
            inner: Mutex::new(Inner {
                enabled: prefs.enabled,
                installing: false,
                error: None,
                runtime: None,
                version: None,
                probed_at: None,
                latest: None,
                latest_at: None,
                plugin: None,
                plugin_probed: false,
                proxy: None,
                running: false,
            }),
            detect: tokio::sync::Mutex::new(()),
            latest: tokio::sync::Mutex::new(()),
            plugin_probe: tokio::sync::Mutex::new(()),
            plugin_probing: std::sync::atomic::AtomicBool::new(false),
            start: tokio::sync::Mutex::new(()),
            starting: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn enabled(&self) -> bool {
        self.lock().enabled
    }

    /// Start the proxy in the background when this runtime boots enabled.
    pub fn start_if_enabled(self: &Arc<Self>) {
        if !self.enabled() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        // One starter at a time: runs arriving mid-start fail open rather
        // than queueing another spawn.
        if self
            .starting
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let this = self.clone();
        handle.spawn(async move {
            this.ensure_proxy().await;
            // Warm the OpenCode plugin path so the first run after boot can
            // route every provider, not just the two built-ins.
            this.resolve_plugin().await;
            this.starting
                .store(false, std::sync::atomic::Ordering::SeqCst);
        });
    }

    /// The full Settings view: install state, proxy health, and last error.
    pub async fn status(self: &Arc<Self>) -> HeadroomStatus {
        self.refresh_runtime(false).await;
        self.refresh_latest().await;
        let (enabled, installing, error, version, latest, installed) = {
            let inner = self.lock();
            (
                inner.enabled,
                inner.installing,
                inner.error.clone(),
                inner.version.clone(),
                inner.latest.clone(),
                inner.runtime.is_some(),
            )
        };
        // Warm the OpenCode plugin path while Settings is open, so the next
        // run doesn't start with the provider overrides alone.
        if installed && !self.lock().plugin_probed {
            self.probe_plugin();
        }
        let update_available = newer_than_installed(version.as_deref(), latest.as_deref());
        let running = health().await;
        if running {
            self.lock().running = true;
        }
        HeadroomStatus {
            enabled,
            installed,
            version,
            update_available,
            running,
            port: PORT,
            installing,
            error,
            providers: PROVIDERS.to_vec(),
            unrouted: UNROUTED.to_vec(),
            manual_command: Some(manual_install_command()),
        }
    }

    /// Flip the preference, persisting it. Enabling starts the proxy (and
    /// reports, without failing the call, when the runtime is missing).
    pub async fn set_enabled(self: &Arc<Self>, enabled: bool) -> HeadroomStatus {
        self.lock().enabled = enabled;
        self.save();
        if enabled {
            self.ensure_proxy().await;
            if !self.lock().plugin_probed {
                self.probe_plugin();
            }
        } else {
            self.stop_proxy().await;
        }
        self.status().await
    }

    /// Install when missing, update when present, then restart the proxy so it
    /// runs the new build. Returns the settled status either way; failures land
    /// in `error` rather than as an RPC error.
    pub async fn install(self: &Arc<Self>) -> HeadroomStatus {
        if self.lock().installing {
            return self.status().await;
        }
        {
            let mut inner = self.lock();
            inner.installing = true;
            inner.error = None;
        }
        let result = self.install_inner().await;
        self.lock().installing = false;
        match result {
            Ok(()) => {
                {
                    let mut inner = self.lock();
                    inner.probed_at = None;
                    inner.runtime = None;
                    inner.version = None;
                    inner.plugin = None;
                    inner.plugin_probed = false;
                }
                self.refresh_runtime(true).await;
                if self.enabled() {
                    self.stop_proxy().await;
                    self.ensure_proxy().await;
                }
            }
            Err(error) => self.lock().error = Some(error),
        }
        self.status().await
    }

    /// Fill the spawn environment for a supported provider. Called from
    /// [`crate::registry::HarnessRegistry::prepare_run`]; never fails a run.
    pub async fn prepare(self: &Arc<Self>, harness: HarnessId, env: &mut BTreeMap<String, String>) {
        if !self.enabled() || !PROVIDERS.contains(&harness) {
            return;
        }
        // Fail open without stalling the run: route through a proxy that is
        // already answering, otherwise start one for the next run.
        if !health().await {
            self.start_if_enabled();
            return;
        }
        apply_routes(harness, PORT, env);
        if harness == HarnessId::Opencode
            && let Some(plugin) = self.opencode_plugin()
        {
            env.insert(OPENCODE_PLUGIN_ENV.into(), plugin.display().to_string());
        }
    }

    /// Headroom's OpenCode transport plugin, which reroutes every provider
    /// (not just the two with a base-URL override). Never waits: a cold probe
    /// runs in the background, and this run gets the provider overrides alone.
    fn opencode_plugin(self: &Arc<Self>) -> Option<PathBuf> {
        let inner = self.lock();
        if inner.plugin_probed {
            return inner.plugin.clone();
        }
        drop(inner);
        self.probe_plugin();
        None
    }

    /// Start (or join) the one background plugin probe.
    fn probe_plugin(self: &Arc<Self>) {
        if self
            .plugin_probing
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.plugin_probing
                .store(false, std::sync::atomic::Ordering::SeqCst);
            return;
        };
        let this = self.clone();
        handle.spawn(async move {
            this.resolve_plugin().await;
            this.plugin_probing
                .store(false, std::sync::atomic::Ordering::SeqCst);
        });
    }

    async fn resolve_plugin(&self) {
        let _guard = self.plugin_probe.lock().await;
        if self.lock().plugin_probed {
            return;
        }
        self.refresh_runtime(false).await;
        let runtime = self.lock().runtime.clone();
        let plugin = match runtime {
            Some(runtime) => find_opencode_plugin(&runtime).await,
            None => None,
        };
        let mut inner = self.lock();
        inner.plugin = plugin;
        inner.plugin_probed = true;
    }

    pub async fn shutdown(&self) {
        self.stop_proxy().await;
    }

    async fn install_inner(&self) -> Result<(), String> {
        self.refresh_runtime(true).await;
        // One budget for every attempt: the RPC deadline is sized to it, and a
        // retry must not restart the clock.
        let deadline = Instant::now() + INSTALL_DEADLINE;
        let runtime = self.lock().runtime.clone();
        if let Some(runtime) = runtime {
            // The CLI's own updater understands where it was installed
            // (pip, pipx, uv tool, `--user`).
            let mut command = runtime.command();
            command.arg("update");
            match run(command, remaining(deadline)).await {
                Ok(_) => return Ok(()),
                Err(error) => {
                    tracing::warn!(%error, "headroom update failed; reinstalling");
                }
            }
        }
        let mut last = None;
        for command in install_commands()? {
            let left = remaining(deadline);
            if left.is_zero() {
                break;
            }
            match run(command, left).await {
                Ok(_) => return Ok(()),
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or_else(|| "Headroom install ran out of time.".into()))
    }

    /// Probe the runtime at most once per [`RUNTIME_TTL`] (or when `force`).
    async fn refresh_runtime(&self, force: bool) {
        if !force
            && self
                .lock()
                .probed_at
                .is_some_and(|at| at.elapsed() < RUNTIME_TTL)
        {
            return;
        }
        let _guard = self.detect.lock().await;
        if !force
            && self
                .lock()
                .probed_at
                .is_some_and(|at| at.elapsed() < RUNTIME_TTL)
        {
            return;
        }
        let detected = detect_runtime().await;
        let mut inner = self.lock();
        inner.probed_at = Some(Instant::now());
        match detected {
            Some((runtime, version)) => {
                inner.runtime = Some(runtime);
                inner.version = version;
            }
            None => {
                inner.runtime = None;
                inner.version = None;
                inner.plugin = None;
                inner.plugin_probed = false;
            }
        }
    }

    /// Ask PyPI for the newest release, at most once per [`LATEST_TTL`]. A
    /// failed check keeps the previous answer and is never a status error: an
    /// unreachable index is not a broken install.
    async fn refresh_latest(&self) {
        // Nothing installed means nothing to compare; skip the request.
        if self.lock().runtime.is_none() {
            return;
        }
        if self.latest_fresh() {
            return;
        }
        let _guard = self.latest.lock().await;
        // Another status call may have answered while this one waited.
        if self.latest_fresh() {
            return;
        }
        let latest = fetch_latest().await;
        let mut inner = self.lock();
        inner.latest_at = Some(Instant::now());
        if latest.is_some() {
            inner.latest = latest;
        }
    }

    /// Whether the cached release answer is still inside its window. A missing
    /// answer ages out faster, so a failed check retries soon.
    fn latest_fresh(&self) -> bool {
        let inner = self.lock();
        let Some(at) = inner.latest_at else {
            return false;
        };
        let ttl = if inner.latest.is_some() {
            LATEST_TTL
        } else {
            LATEST_RETRY
        };
        at.elapsed() < ttl
    }

    /// Ensure a healthy proxy on [`PORT`]. Reuses any Headroom already
    /// listening there (including one the user started), otherwise spawns one.
    async fn ensure_proxy(&self) -> Option<u16> {
        let _guard = self.start.lock().await;
        if !self.enabled() {
            return None;
        }
        if health().await {
            let mut inner = self.lock();
            inner.running = true;
            inner.error = None;
            return Some(PORT);
        }
        self.reap_dead_child();
        self.refresh_runtime(false).await;
        let runtime = self.lock().runtime.clone();
        let Some(runtime) = runtime else {
            let mut inner = self.lock();
            inner.running = false;
            inner.error =
                Some("Headroom isn't installed. Use Install in Settings → Providers.".into());
            return None;
        };
        let mut command = runtime.command();
        command
            .arg("proxy")
            .arg("--port")
            .arg(PORT.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let mut inner = self.lock();
                inner.running = false;
                inner.error = Some(format!("Couldn't start Headroom's proxy: {error}"));
                return None;
            }
        };
        self.lock().proxy = Some(child);
        let deadline = Instant::now() + PROXY_START_DEADLINE;
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if health().await {
                let mut inner = self.lock();
                inner.running = true;
                inner.error = None;
                return Some(PORT);
            }
            if self.child_exited() {
                break;
            }
        }
        self.stop_proxy().await;
        let mut inner = self.lock();
        inner.running = false;
        inner.error = Some(format!(
            "Headroom's proxy didn't answer on 127.0.0.1:{PORT}. Check that the port is free, then retry."
        ));
        None
    }

    async fn stop_proxy(&self) {
        let child = { self.lock().proxy.take() };
        if let Some(mut child) = child {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        }
        self.lock().running = false;
    }

    fn reap_dead_child(&self) {
        let mut inner = self.lock();
        if let Some(child) = inner.proxy.as_mut()
            && matches!(child.try_wait(), Ok(Some(_)) | Err(_))
        {
            inner.proxy = None;
            inner.running = false;
        }
    }

    fn child_exited(&self) -> bool {
        let mut inner = self.lock();
        let Some(child) = inner.proxy.as_mut() else {
            return true;
        };
        match child.try_wait() {
            Ok(Some(_)) => {
                inner.proxy = None;
                inner.running = false;
                true
            }
            Ok(None) => false,
            Err(_) => true,
        }
    }

    fn save(&self) {
        let prefs = Prefs {
            enabled: self.lock().enabled,
        };
        let Ok(text) = serde_json::to_vec_pretty(&prefs) else {
            return;
        };
        let tmp = self.prefs_path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &self.prefs_path);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The environment overrides one provider needs to reach `port`, in the order
/// they are applied. A provider [`PROVIDERS`] lists must return at least one
/// entry here; anything else returns none, so an unrouted agent's run is left
/// exactly as it would be with Headroom off.
///
/// Anthropic-family clients append `/v1/messages` themselves, so they get the
/// bare origin; OpenAI-family clients append `/chat/completions` to a base
/// that already ends in `/v1`.
fn routes(harness: HarnessId, port: u16) -> Vec<(&'static str, String)> {
    let base = format!("http://127.0.0.1:{port}");
    let openai = format!("{base}/v1");
    match harness {
        // GH #746: with a custom ANTHROPIC_BASE_URL and ENABLE_TOOL_SEARCH
        // unset, Claude Code stops searching tools and loses its MCP tools.
        HarnessId::ClaudeCode => vec![
            ("ANTHROPIC_BASE_URL", base),
            ("ENABLE_TOOL_SEARCH", "true".into()),
        ],
        HarnessId::Codex => vec![("OPENAI_BASE_URL", openai)],
        // Cursor's agent picks its endpoints from these two, the same pair
        // Headroom's own Cursor install writes into the app's environment.
        HarnessId::Cursor => vec![
            ("OPENAI_BASE_URL", openai),
            ("ANTHROPIC_BASE_URL", base),
        ],
        HarnessId::Grok => vec![("GROK_MODELS_BASE_URL", openai)],
        // Antigravity's ACP server is Gemini-CLI lineage and reads the CLI's
        // endpoint variable; Headroom has no variable of its own for it.
        HarnessId::Antigravity => vec![("GOOGLE_GEMINI_BASE_URL", base)],
        // OpenCode has no base-URL variable: its harness turns this into the
        // provider overrides and plugin entry inside its inline config.
        HarnessId::Opencode => vec![(OPENCODE_PROXY_ENV, base)],
        _ => Vec::new(),
    }
}

fn apply_routes(harness: HarnessId, port: u16, env: &mut BTreeMap<String, String>) {
    for (key, value) in routes(harness, port) {
        env.insert(key.to_string(), value);
    }
}

async fn detect_runtime() -> Option<(Runtime, Option<String>)> {
    if let Some(path) = zeron_harness::find_executable_in("headroom", install_dirs()) {
        let runtime = Runtime::Cli(path);
        if let Some(version) = probe_version(&runtime).await {
            return Some((runtime, Some(version)));
        }
    }
    for name in ["python3", "python"] {
        if let Some(path) = zeron_harness::find_executable(name) {
            let runtime = Runtime::Python(path);
            if let Some(version) = probe_version(&runtime).await {
                return Some((runtime, Some(version)));
            }
        }
    }
    None
}

/// Where uv tool and pipx put their shims. The login-shell PATH was captured
/// when the engine booted, so a CLI installed from Settings is only found
/// through these directories until the next launch.
fn install_dirs() -> Vec<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|home| vec![PathBuf::from(home).join(".local").join("bin")])
        .unwrap_or_default()
}

/// The transport plugin directory Headroom ships inside its wheel. OpenCode
/// loads a local plugin from a directory, and that copy is self-contained, so
/// routing needs no npm install and matches the installed runtime.
async fn find_opencode_plugin(runtime: &Runtime) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("HEADROOM_OPENCODE_PLUGIN_PATH") {
        let path = PathBuf::from(path);
        if path.is_dir() {
            return Some(path);
        }
    }
    let python = python_for(runtime)?;
    let mut command = tokio::process::Command::new(python);
    command.args([
        "-c",
        "import headroom.providers.opencode.runtime as r; print(r.headroom_opencode_plugin_path() or '')",
    ]);
    zeron_harness::compose_login_shell_path(&mut command);
    let output = run(command, PLUGIN_PROBE_DEADLINE).await.ok()?;
    let path = PathBuf::from(output.lines().last()?.trim());
    path.is_dir().then_some(path)
}

/// An interpreter that can import the package behind `runtime`.
fn python_for(runtime: &Runtime) -> Option<PathBuf> {
    match runtime {
        Runtime::Python(path) => Some(path.clone()),
        Runtime::Cli(path) => launcher_python(path)
            .or_else(|| zeron_harness::find_executable("python3"))
            .or_else(|| zeron_harness::find_executable("python")),
    }
}

/// The interpreter behind a console-script launcher. uv and pipx both store
/// the target Python as plain text inside their launcher, so the owning venv
/// is found without knowing which installer the user chose.
fn launcher_python(exe: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    for name in ["python.exe", "python3.exe", "python", "python3"] {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let bytes = std::fs::read(exe).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut best: Option<PathBuf> = None;
    for (pos, _) in text.match_indices("python") {
        let starts = text[..pos]
            .char_indices()
            .map(|(index, _)| index)
            .rev()
            .take(260);
        for start in starts {
            let Some(base) = text.get(start..pos + "python".len()) else {
                continue;
            };
            for suffix in ["", ".exe", "3", "3.exe"] {
                let candidate = PathBuf::from(format!("{base}{suffix}"));
                if candidate.is_file()
                    && best
                        .as_ref()
                        .is_none_or(|best| candidate.as_os_str().len() > best.as_os_str().len())
                {
                    best = Some(candidate);
                }
            }
        }
    }
    best
}

async fn probe_version(runtime: &Runtime) -> Option<String> {
    let mut command = runtime.command();
    command.arg("--version");
    let output = run(command, VERSION_PROBE_DEADLINE).await.ok()?;
    parse_version(&output)
}

fn parse_version(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .find(|token| token.starts_with(|c: char| c.is_ascii_digit()))
        .map(|token| {
            token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-')
        })
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

/// `uv` first (isolated tool env), then the interpreter's user site, then the
/// interpreter itself for a venv, where `pip --user` is refused.
fn install_commands() -> Result<Vec<tokio::process::Command>, String> {
    if let Some(uv) = zeron_harness::find_executable("uv") {
        let mut command = tokio::process::Command::new(uv);
        command.args([
            "tool",
            "install",
            "--force",
            "--python",
            "3.13",
            "headroom-ai[all]",
        ]);
        zeron_harness::compose_login_shell_path(&mut command);
        return Ok(vec![command]);
    }
    let python = ["python3", "python"]
        .into_iter()
        .find_map(|name| zeron_harness::find_executable(name))
        .ok_or_else(|| {
            "Headroom needs uv or Python 3.10+. Install either, then press Install again."
                .to_string()
        })?;
    Ok([true, false]
        .into_iter()
        .map(|user_site| {
            let mut command = tokio::process::Command::new(&python);
            command.args(["-m", "pip", "install", "--upgrade", "headroom-ai[all]"]);
            if user_site {
                command.arg("--user");
            }
            zeron_harness::compose_login_shell_path(&mut command);
            command
        })
        .collect())
}

fn manual_install_command() -> String {
    if zeron_harness::find_executable("uv").is_some() {
        "uv tool install --python 3.13 \"headroom-ai[all]\"".into()
    } else {
        "python -m pip install --upgrade \"headroom-ai[all]\"".into()
    }
}

/// The newest `headroom-ai` release on PyPI. Serde streams the whole project
/// document and keeps only `info.version`.
async fn fetch_latest() -> Option<String> {
    #[derive(Deserialize)]
    struct Project {
        info: Info,
    }

    #[derive(Deserialize)]
    struct Info {
        version: String,
    }

    let response = reqwest::Client::new()
        .get("https://pypi.org/pypi/headroom-ai/json")
        .timeout(LATEST_DEADLINE)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body = response.text().await.ok()?;
    let project: Project = serde_json::from_str(&body).ok()?;
    parse_version(&project.info.version)
}

/// Whether `latest` is a newer release than the installed build. Unknown on
/// either side means no update, so the button only appears when it is earned.
fn newer_than_installed(installed: Option<&str>, latest: Option<&str>) -> bool {
    match (installed, latest) {
        (Some(installed), Some(latest)) => {
            crate::harness_updates::version_is_newer(latest, installed)
        }
        _ => false,
    }
}

async fn health() -> bool {
    reqwest::Client::new()
        .get(format!("http://127.0.0.1:{PORT}/health"))
        .timeout(Duration::from_millis(800))
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

/// Time left before `deadline`, never negative.
fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

async fn run(mut command: tokio::process::Command, deadline: Duration) -> Result<String, String> {
    hide_console(&mut command);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("CI", "1")
        .env("NONINTERACTIVE", "1")
        .env("TERM", "dumb");
    match tokio::time::timeout(deadline, command.output()).await {
        Err(_) => Err(format!(
            "Headroom timed out after {}.",
            if deadline.as_secs() >= 60 {
                format!("{} minutes", deadline.as_secs() / 60)
            } else {
                format!("{} seconds", deadline.as_secs())
            }
        )),
        Ok(Err(error)) => Err(format!("Couldn't run Headroom: {error}")),
        Ok(Ok(output)) if output.status.success() => {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let text = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            Err(if text.is_empty() {
                format!("Headroom exited with {}.", output.status)
            } else {
                tail(text, 1200)
            })
        }
    }
}

/// Keep the end of a provider's output: that is where the actionable line is.
fn tail(text: &str, limit: usize) -> String {
    let mut chars: Vec<char> = text.chars().collect();
    if chars.len() > limit {
        chars = chars.split_off(chars.len() - limit);
    }
    chars.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_supported_provider_gets_its_documented_override() {
        let mut env = BTreeMap::new();
        apply_routes(HarnessId::ClaudeCode, 8787, &mut env);
        assert_eq!(env["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8787");
        assert_eq!(env["ENABLE_TOOL_SEARCH"], "true");

        let mut env = BTreeMap::new();
        apply_routes(HarnessId::Codex, 8787, &mut env);
        assert_eq!(env["OPENAI_BASE_URL"], "http://127.0.0.1:8787/v1");

        // Cursor rides both families: whichever models a chat uses, the agent
        // resolves that base URL through the proxy.
        let mut env = BTreeMap::new();
        apply_routes(HarnessId::Cursor, 8787, &mut env);
        assert_eq!(env["OPENAI_BASE_URL"], "http://127.0.0.1:8787/v1");
        assert_eq!(env["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8787");
        assert_eq!(env.len(), 2);

        let mut env = BTreeMap::new();
        apply_routes(HarnessId::Grok, 8787, &mut env);
        assert_eq!(env["GROK_MODELS_BASE_URL"], "http://127.0.0.1:8787/v1");

        let mut env = BTreeMap::new();
        apply_routes(HarnessId::Antigravity, 8787, &mut env);
        assert_eq!(env["GOOGLE_GEMINI_BASE_URL"], "http://127.0.0.1:8787");

        // OpenCode's harness turns this into provider config, not an env var.
        let mut env = BTreeMap::new();
        apply_routes(HarnessId::Opencode, 8787, &mut env);
        assert_eq!(env[OPENCODE_PROXY_ENV], "http://127.0.0.1:8787");
        assert_eq!(env.len(), 1);
    }

    #[test]
    fn unsupported_providers_are_left_alone() {
        for harness in UNROUTED {
            let mut env = BTreeMap::new();
            apply_routes(harness, 8787, &mut env);
            assert!(env.is_empty(), "{} must not be routed", harness.as_str());
        }
        // ...including the test harness, which is in neither list.
        let mut env = BTreeMap::new();
        apply_routes(HarnessId::Mock, 8787, &mut env);
        assert!(env.is_empty());
    }

    /// The card's two lists are the whole product's provider surface: every
    /// agent either has a route or is named as one Headroom cannot compress.
    #[test]
    fn provider_lists_match_the_route_table() {
        for harness in HarnessId::ALL {
            assert_eq!(
                !routes(harness, 8787).is_empty(),
                PROVIDERS.contains(&harness),
                "{} is listed and routed inconsistently",
                harness.as_str()
            );
            assert!(
                PROVIDERS.contains(&harness)
                    || UNROUTED.contains(&harness)
                    || harness == HarnessId::Mock,
                "{} is neither routed nor listed as unroutable",
                harness.as_str()
            );
        }
        for harness in PROVIDERS {
            assert!(
                !UNROUTED.contains(&harness),
                "{} is listed both ways",
                harness.as_str()
            );
        }
    }

    #[test]
    fn launcher_finds_a_sibling_interpreter() {
        let dir = tempfile::tempdir().unwrap();
        let python = dir.path().join("python.exe");
        std::fs::write(&python, b"interpreter").unwrap();
        let launcher = dir.path().join("headroom.exe");
        std::fs::write(&launcher, b"launcher").unwrap();
        assert_eq!(launcher_python(&launcher), Some(python));
    }

    #[test]
    fn launcher_finds_the_interpreter_it_embeds() {
        let root = tempfile::tempdir().unwrap();
        let venv = root.path().join("venv");
        std::fs::create_dir_all(&venv).unwrap();
        let python = venv.join("python.exe");
        std::fs::write(&python, b"interpreter").unwrap();
        // The shape uv's trampoline and distlib's console script both use:
        // the target interpreter as plain text inside the launcher.
        let launcher = root.path().join("headroom.exe");
        std::fs::write(
            &launcher,
            format!("__main__.py#!{}\0unrelated trailer", python.display()),
        )
        .unwrap();
        assert_eq!(launcher_python(&launcher), Some(python));
    }

    #[tokio::test]
    async fn disabled_prepare_leaves_the_environment_alone() {
        let dir = tempfile::tempdir().unwrap();
        let headroom = Headroom::open(dir.path());
        let mut env = BTreeMap::new();
        headroom.prepare(HarnessId::ClaudeCode, &mut env).await;
        assert!(env.is_empty());
    }

    #[test]
    fn versions_parse_from_common_shapes() {
        assert_eq!(parse_version("headroom 0.4.1\n"), Some("0.4.1".into()));
        assert_eq!(parse_version("0.4.1\n"), Some("0.4.1".into()));
        assert_eq!(
            parse_version("headroom version: 1.2.3"),
            Some("1.2.3".into())
        );
        assert_eq!(parse_version("no version here"), None);
    }

    #[test]
    fn only_a_newer_release_earns_an_update() {
        assert!(newer_than_installed(Some("0.41.0"), Some("0.42.0")));
        assert!(newer_than_installed(Some("0.9.0"), Some("0.10.0")));
        assert!(!newer_than_installed(Some("0.42.0"), Some("0.42.0")));
        assert!(!newer_than_installed(Some("0.42.1"), Some("0.42.0")));
        // Unknown on either side is not an update.
        assert!(!newer_than_installed(None, Some("0.42.0")));
        assert!(!newer_than_installed(Some("0.41.0"), None));
    }

    #[test]
    fn missing_prefs_file_means_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let headroom = Headroom::open(dir.path());
        assert!(!headroom.enabled());
    }

    #[test]
    fn preference_round_trips_through_the_device_file() {
        let dir = tempfile::tempdir().unwrap();
        let headroom = Headroom::open(dir.path());
        headroom.lock().enabled = true;
        headroom.save();
        let reopened = Headroom::open(dir.path());
        assert!(reopened.enabled());
    }

    #[test]
    fn corrupt_prefs_fall_back_to_disabled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("headroom.json"), "{not json").unwrap();
        assert!(!Headroom::open(dir.path()).enabled());
    }
}
