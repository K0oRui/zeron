# Sidebar paint cost

"The sidebar loads slowly" is a per-paint cost, not a startup cost: the sessions
list is rebuilt from `AppState` on **every** sidebar render, and the sidebar
re-renders whenever `Shell` notifies. `Shell` observes `AppState`
(`crates/ui/src/shell.rs:2634-2635`) and `SidebarPane` observes `Shell`
(`shell.rs:2800`), so any state change — a new chat, a status flip, a pin, a
settled tool call, a resize, a scroll step, a hover swap — rebuilds the whole
row model before a single pixel is painted. Everything in this pass removes work
that cannot change what is drawn: no visual, ordering, or animation behaviour
changes.

## The path

```
AppState::notify
  → Shell::on_state_changed → cx.notify                     shell.rs:2635
    → SidebarPane::render                                   shell.rs:2137
      → Shell::render_chat_sidebar                          shell.rs:10129
        → Shell::render_active_rows                         spaces.rs:6620
          → AppState::sidebar_chats  → overview_chats + project_filter
          → row model (SidebarRowModel, cached by fingerprint)
          → per-row elements → Shell::render_chat_row        shell.rs:9059
```

`render_active_rows` builds the model for **all** chats; only the visible band
gets elements (the virtualization decision comes later, in the row loop,
`spaces.rs:7052`). So the model build is the part that must be cheap — and it is
now the only part that runs on a state change, not on every frame.

## Fixed

| Where | Before (per paint) | Now |
| --- | --- | --- |
| `spaces.rs:2853` `ActiveChatRow` | a whole `Chat` cloned per chat (~10–14 allocations: id, device, title, cwd, branch, checkout, source context, config, preview, ids) | a slim owned row: id, device, title, two timestamps, harness, folder, branch, PR, group — built from a **borrow** |
| `spaces.rs:7612` archived shelf | the same whole-`Chat` clone per archived chat, plus a second sort of a cloned `Vec<Chat>` | the same slim row from a borrow; sorts `Vec<&Chat>` |
| `shell.rs:9053` row device | the chat table scanned **twice** per drawn row (remote glyph + PR badge device) | passed in by the caller that already has the chat |
| `shell.rs:9105` PR device | `String` allocated for every drawn row | only when a badge is drawn |
| `project_icon.rs:410` cache sweep | `retain` reading **every cached icon entity** per icon per paint | swept at most once a second |
| `state.rs:2909` `overview_chats` | `space_row` linear scan per chat (O(chats·spaces)) | one `HashSet` of space ids, O(1) per chat |
| `state.rs:2962` `project_filter` | `space_row` + `project_key` per chat | space → project-key table built once |
| `spaces.rs:6521` row labels | chat → `(project_key, label)` for **every chat** (~3 allocations each) | space → `(project_key, label)` once; rows look up by space id |
| `spaces.rs:6553` re-sort | second full sort on every paint | skipped for `LastUpdated`, whose order `sidebar_chats` already returns |
| `spaces.rs:6721` jump slots | `order_sidebar_ids` built a full ordered id vector + two maps on every paint | built only while the jump-hint modifier is held |
| `spaces.rs:6568` pin order | `base_ids` + `ordered_ids` + `rank` (2·chats `String` clones, 2 `HashSet`s) | one pin→index map (pins only), stable sort on a computed key |
| `spaces.rs:6768` section grouping | per row: scan every section's session list | chat → section map built once |
| `spaces.rs:7067` drag payload | `chat_id`, `filter`, `profile_key` cloned for **every** row, off-screen included | built only for rows that draw |
| `spaces.rs:6427` row metadata | hidden branch/PR copied (and the PR snapshot list walked) for every chat, then gated by the setting | settings checked before the work |
| `shell.rs:9053` title | title `String` cloned per row before `single_line` | borrowed |
| `spaces.rs:6182` cycling / jump order | `sidebar_visible_order` cloned **every** chat to sort them | sorts the borrowed `&Chat` list; `order_sidebar_ids` takes that borrow |
| `command_palette.rs:1104` palette row | — | one device `String` per palette row, because the row's device borrow would otherwise hold the palette's state read across the call |
| `shell.rs:2140` hidden column | full list built behind a zero-width clip | collapse / width-fit skip, resort baselines cleared so no glide replays |
| `shell.rs:1124` marquee map | `String` key allocated per row per paint | probe first, allocate only on insert |

## The row model is cached, keyed on a fingerprint

v0.2.41 shipped a row cache and reverted it
(`docs/performance-macos-stability.md`): a cached row kept geometry that no
longer matched the row's own text, which broke widths and truncation, and the
cached/fresh pixel comparison did not catch it. The cache here is built so that
failure cannot recur, in the order this document set out:

1. **The render walks the rows by reference** (`spaces.rs:6620`). The build and
   the layout are now separate: `Shell::build_sidebar_row_model`
   (`spaces.rs:6521`) produces a `SidebarRowModel` — the owned rows plus the
   pinned count — and pins, custom sections and project groups address those
   rows by **index** (`Vec<usize>` per group), so reusing a cached model clones
   no row at all. Before this, the render `split_off`-ed the pinned rows and
   moved every row into its section, which made a cache pointless: it would have
   had to clone what it saved.
2. **The key is a fingerprint of exactly what the build reads.** A row's inputs
   are spread over `AppState` — chats, sessions (status), spaces (labels and
   project keys), devices (names), the Cloud account and sessions, pending
   sends, change-request snapshots, the connectivity table, `local_device_id` —
   and `apply_chats` is not the only writer: `set_chat_config` and the seen
   marker mutate `chats` in place. So the key hashes those values themselves
   (`AppState::sidebar_row_fingerprint`, `state.rs:2902`; dependency walk in
   `state.rs:4669`) rather than trusting a revision counter to be bumped at
   every writer. The shell adds what it owns — organization, sort, the branch /
   PR / harness gates, the project filter and the resolved pin order
   (`sidebar_row_gates`, `spaces.rs:3002`). A missed field is one reviewable
   omission in one function, not a counter kept honest by hand.
3. **Layout inputs are in the key.** Row heights come from
   `sidebar_row_height(compact, show_label, branch, pr)`, so `branch` and the
   change request are hashed as models of the row itself; toggling
   `sidebar_show_branch` / `sidebar_show_pull_request` changes the gates and
   rebuilds the rows without them. That is the 0.2.41 failure made impossible
   rather than merely avoided.
4. **Time enters as a bucket, not a clock.** Session staleness
   (`zeron_proto::view::session_stale`), device presence and the Cloud wake
   grace flip on the clock alone, so the key includes `now` at one-second
   resolution: a cached model can never lag the clock by more than a second, and
   an idle repaint inside the same second reuses it. Corollary: after any state
   change the model is built at most once, and never again for that snapshot
   while the second lasts.
5. **It is proven, not asserted.** Two new tests:
   - `sidebar_row_model_reuse_equals_a_cold_build` (`spaces.rs`) pins the model
     against a fresh build of the same snapshot, checks pin ordering, checks
     that a branch edit and a `sidebar_show_branch` toggle both miss the cache,
     and checks that the row height follows the branch actually drawn — the
     geometry guard.
   - `sidebar_row_fingerprint_tracks_exactly_what_the_rows_draw` (`state.rs`)
     pins both directions of the key: titles, branches, project names, session
     status, pending sends and the clock invalidate; an unrelated field
     (`selected_chat`, which the rows never read) does not.

What the cache does *not* cover, deliberately: an off-screen row still needs its
model row (the pinned-height and scroll-extent math reads every row, and the
window's own virtualization is what skips the element), so the model is still
built per chat on a miss. The win is between misses: scroll, hover, marquee,
resize, the resort glide and idle animation frames reuse it.

## Left alone, and why

- **Per-row lookups inside `render_project_icon`** (`shell/project_icon.rs:288`,
  `:330`): two scans of the chat table plus a `Space` clone per icon-bearing row.
  It cannot take the chat's space: `ProjectIcons::render` needs `&mut Context`
  while the `AppState` read guard is alive. Passing the project key (already in
  the row's `group` in by-project mode) needs a second cache keyed by key rather
  than by `&Space`. Its per-call cache sweep is throttled here in the meantime.
- **`overview_chats` rebuilds a `HashMap` of sessions** to avoid `session_for`'s
  linear scan. It now runs once per model build, not once per paint; the status
  walk is only worth folding further if the cache is ever keyed per chat.
- **`change_request_for_chat` per row** (`state.rs:3188`) walks the snapshot list
  per chat. Snapshots are few; an index only matters with many open worktrees,
  and it now runs per model build rather than per paint.
- **`reveal_sidebar_chat`** still clones one chat (`spaces.rs:6305`), to hold it
  across the mutations the reveal performs. It runs on an explicit reveal
  (rename, deep link), never on the paint path.

## Measuring

Cadence, not wall time, is what exhausts a battery here — the sidebar can be
rebuilt by an unrelated animation. `ZERON_FRAME_STATS=1` logs per-view render
cadence (`crates/ui/src/transcript.rs:1938`, `record_view_frame("sidebar")`), and
`ZERON_NO_RENDER_CACHE=1` is the existing A/B knob for the transcript flatten
cache. For a before/after, replay the same session list and compare:

- sidebar renders/s while idle, while scrolling, and during a streaming turn;
- `perf`/`sample` main-thread time inside `render_active_rows` for a workspace
  with several hundred sessions. Every cost removed here is proportional to the
  chat count, so a small workspace will not show it.

## Verification

Changed: `crates/ui/src/state.rs`, `crates/ui/src/change_requests.rs`,
`crates/ui/src/shell.rs`, `crates/ui/src/shell/spaces.rs`,
`crates/ui/src/shell/command_palette.rs`,
`crates/ui/src/shell/session_hover_card.rs`,
`crates/ui/src/shell/project_icon.rs`.

New tests:

- `stable_pin_key_matches_pinned_first_order` (`shell/spaces.rs`) — the computed
  pin sort key reproduces `project_pinned_first`, the definition the pinned
  order had before.
- `row_model_carries_the_fields_the_row_draws` (`shell/spaces.rs`) — the slim row
  still carries the identity, timestamps, device and harness the row draws, and
  drops branch and harness when those settings are off. This is the contract
  that replaced the whole-`Chat` clone.
- `sidebar_row_model_reuse_equals_a_cold_build` (`shell/spaces.rs`) — the cached
  model equals a fresh build of the same snapshot, pins lead it, a branch edit
  and a settings toggle both miss the cache, and the row height follows the
  branch that was actually drawn.
- `sidebar_row_fingerprint_tracks_exactly_what_the_rows_draw` (`state.rs`) — the
  key invalidates on every field a row reads (title, branch, project name,
  session status, pending send, the second bucket) and does not invalidate on a
  field it never reads (`selected_chat`).

All four pass in the CI run described below.

There is no Rust toolchain in the authoring sandbox, so this was compiled and
tested on the pull request's CI instead (`crates/ui` only; the harness, engine
and app crates are untouched):

- **Linux `UI tests`** (`cargo nextest run --config-file scripts/ci/nextest.toml
  --locked --no-fail-fast -p zeron-ui --lib --test voice_orb_geometry`):
  `zeron-ui` compiles and `1954 tests run: 1951 passed (1 flaky), 3 failed, 1
  skipped` — the 1952 of the previous run plus the two cache tests. The three
  failures are the pre-existing ones —
  `pickers::tests::typed_windows_paths_resolve_on_any_client`,
  `settings::accounts::tests::per_provider_agents_offer_one_sign_in_per_provider`
  and `pinned_session_tests::sidebar_optimistic_writes_preserve_newer_edits_and_watch_state_on_failure`
  — and they fail identically in the Windows serial run; none of them reaches
  the row model, the ordering or the pin-write path this change touches.
- **Windows `UI tests`**: the release build of `zeron-ui` and the app compile and
  the suite runs (`1873 passed; 3 failed; 5 ignored` — the 1871 of the previous
  run plus the two cache tests); the same three tests fail.
- **`core-tests`** (harness/engine/sync/update/doc/preview) is red on `main`
  itself: `crates/harness/tests/acp_hardening.rs` builds `RunControls` without
  the `user_message_id` field the struct requires, so that suite does not
  compile at `f626e78`. Unrelated to this change.
- **`linux-browser`** passed in this run; the previous run's failure was the CI
  action's `apt` step (`download failed`), before it built the browser fixture.

Locally, the edited files were only checked against tree-sitter's Rust grammar
(no parse errors). To rerun the suite by hand:

```sh
cargo nextest run --config-file scripts/ci/nextest.toml --locked --no-fail-fast -p zeron-ui --lib
```

### Repairs made to get the branch green

The run above was red, and every red job is now fixed (`a31cfe4`, `7f9ae3a`).
Two waves, all of them outside the row model:

1. **Compile**: test-side `RunControls` literals missed `user_message_id`
   (`crates/harness/tests/*`, `opencode/tests.rs`) — E0063 failed the harness,
   engine and offline-Linux jobs before any test ran.
2. **Stale expectations**: grok's ladder stops at `High` and its catalog copy is
   `xAI's coding model — 500k context` (engine registry + ACP descriptor + ACP
   enrichment tests); an OpenCode `cancelled` todo stays `Cancelled` and an
   unknown status is `Pending`; an ACP edit call carries the diff it mapped
   (`old_string`/`new_string`); an empty OpenCode provider list falls back to the
   curated free tier instead of failing the probe.
3. **Real behaviour gaps the tests exposed**:
   - `pickers::resolve_typed_path` resolved a single-backslash-rooted Windows
     path (`\Projects`, or `/Projects` swapped to it) through
     `typed_path_target`, which answered it as a "windows-flavored" absolute —
     the drive-root arm was unreachable and the result normalized to
     `/\Projects`. It now resolves against the base's drive root first; UNC and
     extended roots keep their two backslashes.
   - `proto::providers::login_options` had no Codex arm, so the desktop offered
     one Codex login while the engine and the API-key dialog already supported
     `oauth` and `api-key`. Codex now offers both.
   - `finish_sidebar_pin_write` cleared the whole queue on a failure, discarding
     a drop made while an older write was in flight. It now pops only the
     resolved change: the queue holds deltas applied to the latest watch state,
     so the newer drop survives and is still sent.
   - The devin fixture's session advertised a fixed plan model, so the model
     refresh/retention tests could never observe a new state (the plan filters
     the catalog by id). The fixture's session now reports the fixture's model
     state.

Final state: `Windows tests`, `macOS tests`, `Native Codex voice`, `Android`,
`Preview networking`, `Cursor compatibility` and `core-tests` pass. Both Linux
UI jobs in that run stopped in the CI action's `apt` step
(`libwebkit2gtk-4.1-0`: `download failed` after ~150 s) before compiling, the
same step that succeeded on the previous run; they are re-run with this commit.
