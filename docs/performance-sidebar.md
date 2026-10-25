# Sidebar paint cost

"The sidebar loads slowly" is a per-paint cost, not a startup cost: the sessions
list is rebuilt from `AppState` on **every** sidebar render, and the sidebar
re-renders whenever `Shell` notifies. `Shell` observes `AppState`
(`crates/ui/src/shell.rs:2634`) and `SidebarPane` observes `Shell`
(`shell.rs:2800`), so any state change — a new chat, a status flip, a pin, a
settled tool call, a resize, a scroll step, a hover swap — rebuilds the whole
row model before a single pixel is painted. Everything in this pass removes work
that cannot change what is drawn: no visual, ordering, or animation behaviour
changes.

## The path

```
AppState::notify
  → Shell::on_state_changed → cx.notify                     shell.rs:2634
    → SidebarPane::render                                   shell.rs:2138
      → Shell::render_chat_sidebar                          shell.rs:10133
        → Shell::render_active_rows                         spaces.rs:6352
          → AppState::sidebar_chats  → overview_chats + project_filter
          → per-chat row model (ActiveChatRow)
          → per-row elements → Shell::render_chat_row        shell.rs:9053
```

`render_active_rows` builds the model for **all** chats; only the visible band
gets elements (the virtualization decision comes later, in the row loop,
`spaces.rs:6821`). So the model build is the part that must be cheap.

## Fixed

| Where | Before (per paint) | Now |
| --- | --- | --- |
| `spaces.rs:2653` `ActiveChatRow` | a whole `Chat` cloned per chat (~10–14 allocations: id, device, title, cwd, branch, checkout, source context, config, preview, ids) | a slim owned row: id, device, title, two timestamps, harness, folder, branch, PR, group — built from a **borrow** |
| `spaces.rs:7380` archived shelf | the same whole-`Chat` clone per archived chat, plus a second sort of a cloned `Vec<Chat>` | the same slim row from a borrow; sorts `Vec<&Chat>` |
| `shell.rs:9053` row device | the chat table scanned **twice** per drawn row (remote glyph + PR badge device) | passed in by the caller that already has the chat |
| `shell.rs:9105` PR device | `String` allocated for every drawn row | only when a badge is drawn |
| `project_icon.rs:414` cache sweep | `retain` reading **every cached icon entity** per icon per paint | swept at most once a second |
| `state.rs:2901` `overview_chats` | `space_row` linear scan per chat (O(chats·spaces)) | one `HashSet` of space ids, O(1) per chat |
| `state.rs:2954` `project_filter` | `space_row` + `project_key` per chat | space → project-key table built once |
| `spaces.rs:6357` row labels | chat → `(project_key, label)` for **every chat** (~3 allocations each) | space → `(project_key, label)` once; rows look up by space id |
| `spaces.rs:6432` re-sort | second full sort on every paint | skipped for `LastUpdated`, whose order `sidebar_chats` already returns |
| `spaces.rs:6440` jump slots | `order_sidebar_ids` built a full ordered id vector + two maps on every paint | built only while the jump-hint modifier is held |
| `spaces.rs:6475` pin order | `base_ids` + `ordered_ids` + `rank` (2·chats `String` clones, 2 `HashSet`s) | one pin→index map (pins only), stable sort on a computed key |
| `spaces.rs:6531` section grouping | per row: scan every section's session list | chat → section map built once |
| `spaces.rs:6784` drag payload | `chat_id`, `filter`, `profile_key` cloned for **every** row, off-screen included | built only for rows that draw |
| `spaces.rs:6262` row metadata | hidden branch/PR copied (and the PR snapshot list walked) for every chat, then gated by the setting | settings checked before the work |
| `shell.rs:9053` title | title `String` cloned per row before `single_line` | borrowed |
| `shell.rs:2137` hidden column | full list built behind a zero-width clip | collapse / width-fit skip, resort baselines cleared so no glide replays |
| `shell.rs:1126` marquee map | `String` key allocated per row per paint | probe first, allocate only on insert |

## Why the model is not cached across paints

The obvious next step — keep the built rows and skip the rebuild — is what
v0.2.41 shipped and reverted (`docs/performance-macos-stability.md`): a cached
row kept geometry that no longer matched the row's own text, which broke widths
and truncation, and the cached/fresh pixel comparison did not catch it.

Two properties make a naive cache unsafe here, and both are still true:

- **The model feeds layout.** Row heights come from `sidebar_row_height(compact,
  show_label, branch, pr)`, so a stale `branch`/`change_request` in a cached row
  silently changes the list's total height and every scroll extent — exactly the
  0.2.41 failure, and it is invisible in a single frame.
- **The inputs are spread over `AppState`.** The model reads chats, spaces,
  devices, the cloud account, sessions (status), change-request snapshots and
  pending sends, plus the sidebar settings. `apply_chats` is not the only writer:
  `set_chat_config` and the seen-marker path mutate `chats` in place, so a
  revision counter has to be bumped at each of those or it goes stale. A missed
  bump is the same bug with a longer fuse.

A safe version needs three things, in this order:

1. **Split the build from the render.** `render_active_rows` currently consumes
   its rows (`split_off` for pins, then grouping moves them into sections), so a
   cache would have to be cloned to be reused — which allocates as much as the
   build it avoids. The cache is only worth having once the render walks the
   rows by reference.
2. **Key it on a fingerprint, not a promise.** Hash exactly what the model reads
   (each chat's id/title/device/space/timestamps/harness/branch/archived,
   status, space display names, device names, and the sidebar settings), and
   rebuild when the hash changes. That is the same dependency enumeration a
   revision needs, but the key sits next to the code that reads the fields, so
   drift is reviewable in one diff.
3. **Prove it.** A test that builds the model twice — once cold, once from the
   cache after a mutation — and asserts row-for-row equality, plus the existing
   layout checks (short/long titles, selected and hovered rows) on a cached
   frame.

The slim row above is the prerequisite for all three, and it already removes
most of what a cache would have saved: the remaining per-chat allocations are
`id`, `device_id`, `folder` and (for grouping) the group key.

## Left alone, and why

- **Per-row lookups inside `render_project_icon`** (`shell/project_icon.rs:282`,
  `:323`): two scans of the chat table plus a `Space` clone per icon-bearing row.
  It cannot take the chat's space: `ProjectIcons::render` needs `&mut Context`
  while the `AppState` read guard is alive. Passing the project key (already in
  the row's `group` in by-project mode) needs a second cache keyed by key rather
  than by `&Space`. Its per-call cache sweep is throttled here in the meantime.
- **`overview_chats` rebuilds a `HashMap` of sessions per paint** to avoid
  `session_for`'s linear scan. Cheap next to the model build, and it belongs to
  the same cache as the rest.
- **`change_request_for_chat` per row** (`state.rs:3169`) walks the snapshot list
  per chat. Snapshots are few; an index only matters with many open worktrees.
- **`reveal_sidebar_chat`** still clones one chat (`spaces.rs:6136`), to hold it
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

Changed: `crates/ui/src/state.rs`, `crates/ui/src/shell.rs`,
`crates/ui/src/shell/spaces.rs`, `crates/ui/src/shell/command_palette.rs`,
`crates/ui/src/shell/session_hover_card.rs`,
`crates/ui/src/shell/project_icon.rs`.

New tests, both in `crates/ui/src/shell/spaces.rs`:

- `stable_pin_key_matches_pinned_first_order` — the computed pin sort key
  reproduces `project_pinned_first`, the definition the pinned order had before.
- `row_model_carries_the_fields_the_row_draws` — the slim row still carries the
  identity, timestamps, device and harness the row draws, and drops branch and
  harness when those settings are off. This is the contract that replaced the
  whole-`Chat` clone.

This environment has no Rust toolchain and no crates.io access, so nothing here
was compiled or run. The edited files parse cleanly under tree-sitter's Rust
grammar; run the real suite before merging:

```sh
cargo nextest run --config-file scripts/ci/nextest.toml --locked --no-fail-fast -p zeron-ui --lib
```

That is what `.github/workflows/ui-tests.yml` runs.
