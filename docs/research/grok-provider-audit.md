# Grok provider audit (2026-10-28)

Scope: `zeron_harness::AcpHarness::grok()` (`crates/harness/src/acp/`), the Grok rows of
`crates/engine` (registry, accounts, usage, harness updates) and the client-side fallback
catalog.

Method: code review plus live probes against **grok 1.0.70**, the current `latest` of
`@xai-official/grok` (registry check: `dist-tags.latest = 1.0.70`; the pinned `1.0.4`
package and the CLI ship their own `docs/user-guide/*.md`, which were read as the vendor
specification). The probes spoke ACP to `grok --no-auto-update agent --no-leader stdio`
and pointed `GROK_MODELS_BASE_URL` at a local OpenAI-compatible gateway, so full turns
(text, tool calls, cancel, usage) could be driven without an xAI account.

---

## What checks out (so the list below is calibrated)

Verified working against 1.0.70:

- Launch: `--no-auto-update agent --no-leader stdio` is accepted (the first flag is hidden
  but still parsed).
- Handshake: `initialize` + `session/new` work; the session advertises
  `configOptions[model]` and `configOptions[reasoning_effort]` (category `thought_level`,
  values `xhigh|high|medium|low`) — exactly what `models_from_session` /
  `config_option_sets` read.
- `session/set_config_option` is sent with the *flattened* bare value
  (`{"configId":"model","value":"grok-4.6"}`); that is the shape 1.0.70 accepts. Sending an
  object (`{"value":{"value":…}}`) is rejected with `-32602 … untagged enum
  SessionConfigOptionValue`, so the flattening in `select_session_config` is load-bearing
  and correct.
- `session/cancel` is a **notification** in ACP; sent as a notification (as zeron does) it
  cancels the live turn with `cancelTrigger: "ctrl_c"`. (Sent as a *request* it answers
  `-32601 Method not found`, which is a trap for hand-written probes, not a driver bug.)
- `_x.ai/session/prompt_complete` arrives with a top-level `params.promptId`, matching the
  echo zeron mints in `prompt_turn`; the authoritative-turn-end path matches.
- Per-turn usage is on the prompt response `_meta` (`inputTokens`, `outputTokens`, plus a
  nested `usage` object), so `usage_from_response` reports tokens.
- `session/load` of a deleted session still fails with `Path not found. (code -32603) …
  FS_NOT_FOUND`, which `session_unrestorable` recognises → resume falls back to a fresh
  session.
- `grok login --device-auth` is a real alias of `--device-code`, and the printed
  verification URL is on `x.ai`, which `scan_grok_url` accepts.
- `GROK_MODELS_BASE_URL` is the right Headroom hook: the CLI fetches `{base}/models` and
  sends inference to `{base}/chat/completions`.

---

## Issues

### 1. The managed npm install pins Grok **1.0.4** — 66 releases behind — and installs it into the user's own `~/.grok/bin` (high)

`crates/harness/src/acp/mod.rs:245`

```rust
npm_package: Some("@xai-official/grok@1.0.4"),
```

- `latest` is 1.0.70 (checked today); the driver's own comments were live-verified on
  1.0.4 *and later re-verified on 1.0.46* (`session_unrestorable`, the FS_NOT_FOUND case),
  so a managed install now runs an agent **older than the behaviour the adapter is written
  against**.
- The npm package is not an adapter, it is the CLI. Its `postinstall.js` writes
  `$GROK_HOME/bin/grok-<version>` and swaps the canonical `$GROK_HOME/bin/grok` symlink.
  `install_into` (`crates/harness/src/adapter_install.rs:518`) does not pass
  `--ignore-scripts`, so *zeron's private adapters dir mutates the CLI's canonical install
  location*. Afterwards `find_server()` finds `~/.grok/bin/grok` (it is in
  `grok_install_paths`), so every later launch runs the stale 1.0.4 copy.
- 1.0.4 also speaks less ACP than the code expects: probed live, its `session/new` returns
  **no `configOptions`** (only the legacy `models` block), so on a managed install there is
  no `reasoning_effort` option at all — effort selection silently cannot be applied, and
  the default model's reasoning ladder comes out empty.
- The path is reachable even though Grok is `can_install: false` and `installed()` only
  checks the CLI: `run()` spawns with `block_on_install = true`
  (`crates/harness/src/acp/mod.rs:2487`), so the first Grok chat on a machine without the
  CLI blocks on a ~55 MB npm install (600 s timeout) before any output.
- Recommendation: drop `npm_package` for Grok (its "adapter" is the binary; the CLI install
  hints in `install.rs` already cover it), or pin `@xai-official/grok@latest` *and* stop the
  package's postinstall from touching the user's `GROK_HOME`.

### 2. `prewarm_managed_adapters` can never prewarm Grok (medium)

`crates/harness/src/acp/mod.rs:1265`. Grok is the only spec left in the loop, and for Grok
`executable == cli_executable == "grok"`, so the guard

```rust
if find_on_paths(spec.executable, …).is_some()
    || … || find_on_paths(spec.cli_executable, …).is_none() || …
{ continue; }
```

is always true: "grok not on PATH" implies "cli not on PATH". The function is dead code, so
the download it exists to hide still lands inside the first chat (issue 1).

### 3. `GROK_HOME` is honoured for credentials but not for the binary or the transcripts (medium)

- Honoured: `stores.rs:76` (`default_grok_home`), `model_context.rs:51`
  (`$GROK_HOME/auth.json`, `config.toml`).
- Not honoured: `grok_install_paths()` (`acp/mod.rs:210`) probes `~/.grok/bin`, and
  `SubagentTracker::default_sessions_root()` (`acp/subagent.rs:40`) tails `~/.grok/sessions`.
- Grok's own docs (`docs/user-guide/17-sessions.md:18`, `02-authentication.md`) and its npm
  bootstrap/postinstall resolve `GROK_HOME` first: binaries at `$GROK_HOME/bin`, sessions at
  `$GROK_HOME/sessions`.

With `GROK_HOME` set (documented for fleets and slow NFS homes) zeron reads the login from
`$GROK_HOME/auth.json` but reports Grok as not installed unless `grok` is also on `PATH`,
and subagent tabs stay empty even though the child transcripts are being written.

### 4. The static fallback catalogs are stale, and they disagree with each other (medium)

| Source | Rows (first = default) | Ladders |
| --- | --- | --- |
| `acp/mod.rs:255-270` (harness static) | `grok-4.5` only | low, medium, high |
| `client/src/catalog.rs:193-212` (UI fallback) | `grok-4.7`, `grok-4.6`, `grok-4.5` | 4.7/4.6: low…xhigh; 4.5: low…high |
| grok 1.0.70 (probed) | `grok-4.6` (**default**), `grok-4.5` | 4.6: xhigh/high/medium/low; 4.5: high/medium/low |

Consequences:

- When discovery is unavailable, the harness falls back to a session default of
  `grok-4.5` — a downgrade from the CLI's own 4.6 default — and the registry test
  (`engine/src/registry.rs:1139`) hardwires that "grok-4.5's ladder stops at High".
- The UI fallback's default row, `grok-4.7`, is not in the CLI's catalog at all (it looks
  copied from Cursor's model list). A rejected model set is logged at `debug` and the agent
  default runs (`acp/mod.rs:4163`), so the selection would fail silently.
- Hazard: for an id the static catalog *does* match, `models_from_session`
  (`acp/mod.rs:2054`) prefers the static ladder over the wire's
  (`known.filter(|m| !m.reasoning_levels.is_empty())`). Today only `grok-4.5` matches and
  its static ladder happens to equal the wire's; if the static row is moved to `grok-4.6`
  with the current three-level ladder, the wire's `xhigh` disappears.

### 5. The Accounts usage meter's primary branch and period label are dead against 1.0.70 (medium)

`crates/engine/src/agent_accounts/usage.rs:355`.

Zeron reads:

```rust
config.productUsage[]            // { product: "GrokBuild", usagePercent }
config.currentPeriod.type        // "USAGE_PERIOD_TYPE_WEEKLY" → "Week"
```

The 1.0.70 `BillingConfig` (11 fields, from the binary and `billing.rs`) is:

```
creditUsagePercent, currentPeriod { start, end }, monthlyLimit,
onDemandCap, onDemandUsed, prepaidBalance, isUnifiedBillingUser,
billingPeriodStart, history[{ billingCycle { month }, includedUsed, totalUsed }]
```

- `productUsage` / `usagePercent` no longer exist, so the "Grok Build share" arm (and the
  `USAGE_PERIOD_TYPE_*` labels) can never fire: every Grok meter falls back to the blended
  `creditUsagePercent` and is always labelled **"Period"** rather than Day/Week/Month.
- Worth confirming live that `creditUsagePercent` is on a 0–100 scale: the code divides by
  100 after `clamp(0.0, 100.0)`, so a 0–1 fraction (as `onDemandUsed/onDemandCap` produces)
  would render the bar ~100× too low.
- The window length can be derived from `currentPeriod.start`/`end` or
  `history[].billingCycle.month`.

### 6. Every Grok `_x.ai/session_notification` payload is dropped (medium)

`crates/harness/src/acp/mod.rs:3093` routes the method to the subagent tracker only and
returns no events. In 1.0.70 that channel carries:

- `retry_state` — `{type: "retrying", attempt, max_retries: 15, reason, error_type}` and
  `{type: "failed", …}`. Probed live: a failing upstream produced 15 silent retries over
  minutes while zeron showed a bare "Working" — and the 30 s stall chip is cleared by these
  very notifications, so nothing ever surfaces.
- `session_notice`-style payloads such as `Auto-compaction failed` / `Auto-compact
  cancelled` (strings in the binary; `x.ai/session/notice` is the documented channel).
- `turn_completed`, `response_completed` (usage), `model_changed`, `last_turn_summary`,
  `session_summary_generated`.

Surfacing retry/failure and compaction notices would remove the main "Grok looks stuck"
report class.

### 7. A Grok tool chip starts as a generic `Unknown` (low)

Grok's opening `tool_call` frame (probed) carries **no top-level `kind`**:

```json
{"sessionUpdate":"tool_call","toolCallId":"call_1","title":"run_terminal_command",
 "rawInput":{"command":"echo hello-from-probe"},
 "_meta":{"x.ai/tool":{"name":"run_terminal_command","kind":"execute",
                       "label":"Run Command","read_only":false}}}
```

`typed_call` reads only `update["kind"]` (`acp/normalize.rs:169`), so the chip is created as
`Unknown { name: "run_terminal_command" }` (snake_case tool name, no Exec icon) and is only
re-typed when the first `tool_call_update` — which does carry `kind`, `title` and a human
`rawInput` — arrives. Falling back to `_meta["x.ai/tool"].kind` (zeron already reads
`…["name"]` at `normalize.rs:96`) would type the opening chip correctly.

### 8. Mid-turn steering: what exists, and the descriptor drift it exposed (corrected)

*Corrected during the fix pass.* The first reading of this issue was wrong on two counts,
and the probes that settled it are worth recording:

- `x.ai/session/interjection` is **agent → client**. Sending it as a request answers
  `-32601 Method not found`; sending it as a notification is what the CLI does to its own
  pager (`interjectionId`, `forward_fire_and_forget`). There is no client-callable
  interjection, so zeron cannot inject text into a live grok turn beyond what it already
  does: preempt (`session/cancel` + re-prompt with the steer appended).
- Sending `session/prompt` while a turn runs does **not** steer — grok queues it
  (`_x.ai/queue/changed`, `_x.ai/queue/removed`) and only starts it after the current turn
  ends, so "steer by prompt" would be strictly worse than the current preempt.

What *was* broken is the descriptor:

- `grok_spec` says `SteeringMode::StepBoundary` (preempt: the steer interrupts the live
  turn), but the engine's static lazy descriptor said `TurnBoundary`. `descriptors()` serves
  the static copy until the harness is first resolved (`registry.rs:681`), and the composer
  only offers mid-turn steering when the descriptor says `StepBoundary`
  (`ui/src/pickers.rs:1627`) — so Grok silently had no steer affordance until something
  else resolved the harness, then flipped to having one.

### 9. `initialize._meta.modelState` is ignored (low)

1.0.70 advertises the whole catalog — `currentModelId`, `availableModels`, and per-model
`reasoningEfforts` — in `initialize._meta.modelState`, *before* `session/new`, i.e. also
when the session cannot be opened because the user is signed out. `models_from_session`
(`acp/mod.rs:2054`) only looks at `session/new`'s `configOptions` and legacy `models`, so a
signed-out or failed-probe user is left with the stale static rows from issue 4.

### 10. Smaller notes

- `run_auth_method`'s doc (`acp/mod.rs:3643`) cites Grok's `cached_token` default method.
  1.0.70 advertises only `[grok.com]` and sends `_meta.defaultAuthMethodId: null`, so the
  self-sign-in path it describes no longer exists; the user still gets the correct
  "Grok isn't signed in: … (code -32000)" message, and with `XAI_API_KEY` set the default
  becomes `xai.api_key` as intended.
- Update checking: `grok update --check` on 1.0.70 prints a current-version banner first and
  exits 0 even when the check fails (`Update check failed: GCS channel pointer fetch
  failed…`). `extract_latest_version` takes the last version token, so a failed check reads
  as "up to date". `grok update --check --json` exists and would be unambiguous.
- `docs`/`README` say `header`/`GROK_ISSUERS` etc. are fine; no other drifts found in
  `install.rs` (`~/.grok/bin`, npm global), `headroom.rs` (`GROK_MODELS_BASE_URL`) or the
  accounts swap/refresh paths — the latter still cannot refresh a web login (no
  `oidc_client_id`), which matches its "sign in again" copy.

---

## Reproduction recipe

The probes used here are cheap to repeat:

1. `npm pack @xai-official/grok-linux-x64@1.0.70`, then
   `node -e "zlib.brotliDecompressSync(fs.readFileSync('bin/grok.br'))"` → a runnable binary.
2. Spawn it with `GROK_HOME=<tmp> XAI_API_KEY=dummy GROK_MODELS_BASE_URL=http://127.0.0.1:<p>/v1
   --no-auto-update agent --no-leader stdio`.
3. Serve `GET /v1/models` and stream `/v1/chat/completions` chunks (any OpenAI-shaped
   `chat.completion.chunk` with `id`), including a `tool_calls` delta to drive the tool
   frames, and hold the stream open to test cancel.

This makes `session/new` succeed without an xAI login and exercises the real wire shapes
(tool calls, `prompt_complete`, usage `_meta`, cancel, retry notifications).

---

## Fixes applied (2026-10-28)

Every issue above is addressed in code. Verification in this pass was static (no Rust
toolchain in the audit sandbox) plus the live wire probes recorded above; the checks that
were re-run after the change are named per issue.

| # | Change | Where |
| --- | --- | --- |
| 1 | **Dropped the managed npm install for Grok.** `AcpAgentSpec::npm_package` and the whole bin-entry install path went with it (`Launch::Managed`, `ensure_installed`, `installed_entry`/`bin_entry`, and the two `managed_install*` E2E tests). The package is the CLI, its `postinstall` **and** its `bin/grok` launcher both write `$GROK_HOME/bin` and swap the canonical symlink (read from `postinstall.js`/`grok-bootstrap.js` at 1.0.70), and the `.zeron-install-ok` marker froze zeron's copy at the first-installed version — so "pin a newer release" cannot fix the class. `install.rs`'s Grok methods (installer script / `npm i -g`) are the install path, and `descriptors()` computes `can_install` per device from them (`registry.rs:700`, so the static `false` in the lazy slot is only a placeholder). The npm machinery stays for its remaining consumer, Cursor's SDK shim. | `acp/mod.rs`, `adapter_install.rs`, `crates/harness/tests/`, `docs/research/acp.md` |
| 2 | Deleted `prewarm_managed_adapters` and its boot call — it could never fire. | `acp/mod.rs`, `engine/src/lib.rs` |
| 3 | `grok_install_paths` probes `$GROK_HOME/bin/grok` first; `default_sessions_root()` is `$GROK_HOME/sessions` (else `~/.grok`). | `acp/mod.rs`, `acp/subagent.rs` |
| 4 | Static catalogs rebuilt from the 1.0.70 wire: harness rows `grok-4.6` (default, low…xhigh) + `grok-4.5` (low…high); client fallback dropped `grok-4.7` and matches. Registry ladder gained `XHigh` and its descriptor now says `StepBoundary`. Per-model ladders are read from `availableModels[]._meta.reasoningEfforts` and **beat** both the session-wide `thought_level` and the static row, which removes the "static shadowing a narrower wire ladder" hazard. | `acp/mod.rs`, `client/src/catalog.rs`, `engine/src/registry.rs` |
| 5 | `grok_usage_snapshot` reads `creditUsagePercent` (0–100 → 0–1, on-demand spend as fallback) and names the window from `currentPeriod.start`/`end` (Day/Week/Month/Period). `productUsage` and the `USAGE_PERIOD_TYPE_*` labels — dead on 1.0.70 — are gone. | `engine/src/agent_accounts/usage.rs` |
| 6 | `_x.ai/session_notification` now maps `retry_state` and `auto_compact_failed`/`_cancelled` into notice chips (`notice_events`, also used by `map_update`'s unknown-kind catch-all): one chip when a retry streak starts, none for attempts 2..n, one when it gives up. | `acp/normalize.rs`, `acp/mod.rs` |
| 7 | `typed_call` falls back to `_meta["x.ai/tool"].kind`, so grok's opening `tool_call` frame types correctly (Exec/Read/Edit) instead of `Unknown { name: "run_terminal_command" }`. | `acp/normalize.rs` |
| 8 | Corrected (see the issue): interjection is agent→client and prompt-while-busy only queues, so preempt stays; the real bug — the `TurnBoundary` descriptor that hid the steer affordance — is fixed. | `acp/mod.rs`, `engine/src/registry.rs` |
| 9 | Discovery keeps `initialize._meta.modelState` and uses it when `session/new` advertises nothing (signed out, rejected cwd) or fails, instead of falling back to the stale static rows. | `acp/mod.rs` |
| 10 | `run_auth_method`'s doc now describes 1.0.70's `defaultAuthMethodId: null` / `xai.api_key` reality. Update checks use `grok update --check --json` and treat a non-empty `error` as a failed check (the plain form exits 0 with a banner even when the fetch failed, which used to read as "up to date" — reproduced live with the GCS pointer blocked). The generic `LatestSource::Command`/`extract_latest_version` banner parser went away with its last user. | `engine/src/harness_updates.rs`, `acp/mod.rs` |

Tests updated/added with the code: `grok_handshake_model_state_carries_the_real_catalog_and_ladders`,
`grok_retry_states_surface_one_chip_per_streak`,
`grok_tool_call_typing_falls_back_to_the_xai_meta_kind`,
`grok_lazy_descriptor_matches_resolved_harness`,
`grok_update_check_reads_the_json_record_not_the_banner`, and the reworked Grok usage /
client-catalog / descriptor-surface tests.

### Still open (needs a signed-in account or a build)

- `creditUsagePercent`'s scale could not be probed without a login; 0–100 is what the name
  and the money-valued caps imply, and a 0–1 fraction would show ~100× low. Worth one look
  on a real account.
- No `cargo test` ran in this sandbox: the harness tests above are written against the
  probed shapes but not executed here.
- The compatibility feed (`releases/harness-compatibility.json`) still publishes no Grok
  listing, so a known-broken Grok build is only caught by version comparison.
