# Sidebar paint cost

"The sidebar loads slowly" is a per-paint cost, not a startup cost: the sessions
list is rebuilt from `AppState` on **every** sidebar render, and the sidebar
re-renders whenever `Shell` notifies. `Shell` observes `AppState`
(`crates/ui/src/shell.rs:2634`) and `SidebarPane` observes `Shell`
(`shell.rs:2800`), so any state change — a new chat, a status flip, a pin, a
settled tool call, a resize, a scroll step, a hover swap — rebuilds the whole
row model (and, today, clones every chat) before a single pixel is painted.
`docs/performance-macos-stability.md` already records one attempt at caching
these rows (v0.2.41) that had to be reverted because stale geometry broke row
widths and truncation; this pass only removes work that cannot change what is
drawn.

Everything below is a pure-work reduction. No visual, ordering, or animation
behaviour changes.

## The path

```
AppState::notify
  → Shell::on_state_changed → cx.notify                     shell.rs:2634
    → SidebarPane::render                                   shell.rs:2138
      → Shell::render_chat_sidebar                          shell.rs:10133
        → Shell::render_active_rows                         spaces.rs:6260
          → AppState::sidebar_chats  → overview_chats + project_filter
          → per-chat row model (clone + label/group resolution)
          → per-row elements → Shell::render_chat_row        shell.rs:9053
```

`render_active_rows` is the expensive half: it builds the model for **all**
chats, while only the visible band gets elements (the virtualization decision
happens later, in the row loop, `spaces.rs:6737`).

## Fixed here

| Where | Before (per paint) | Now |
| --- | --- | --- |
| `state.rs:2901` `overview_chats` | `space_row` linear scan per chat (O(chats·spaces)) | one `HashSet` of space ids, O(1) per chat |
| `state.rs:2954` `project_filter` | `space_row` + `project_key` per chat | space → project-key table built once |
| `spaces.rs:6323` row labels | chat → `(project_key, label)` for **every chat** (~3 allocations each) | space → `(project_key, label)` once; rows look up by space id |
| `spaces.rs:6343` re-sort | second full sort on every paint | skipped for `LastUpdated`, whose order `sidebar_chats` already returns |
| `spaces.rs:6351` jump slots | `order_sidebar_ids` built a full ordered id vector + two maps on every paint | built only while the jump-hint modifier is held |
| `spaces.rs:6391` pin order | `base_ids` + `ordered_ids` + `rank` (2·chats `String` clones, 2 `HashSet`s) | one pin→index map (pins only), stable sort on a computed key |
| `spaces.rs:6447` section grouping | per row: scan every section's session list | chat → section map built once |
| `spaces.rs:6700` drag payload | `chat_id`, `filter`, `profile_key` cloned for **every** row, off-screen included | built only for rows that draw |
| `spaces.rs:6219` row metadata | hidden branch/PR copied (and the PR snapshot list walked) for every chat, then gated by the setting | settings checked before the work |
| `shell.rs:9102` row device | the chat table scanned twice per row for the same `device_id` | scanned once |
| `shell.rs:9053` title | title `String` cloned per row before `single_line` | borrowed |
| `shell.rs:2137` hidden column | full list built behind a zero-width clip | collapse / width-fit skip, resort baselines cleared so no glide replays |
| `shell.rs:1126` marquee map | `String` key allocated per row per paint | probe first, allocate only on insert |

## Left alone, and why

Ordered by what it would buy.

**Row model clones every chat (~10 allocations per chat per paint).**
`spaces.rs:6339` clones each `Chat` so the row model outlives the
`Read<AppState>` guard; the guard cannot be held across the render loop because
the loop calls `&mut self` (`render_sidebar_gap_row`, `render_custom_sidebar_section`).
The rows only ever read `id`, `device_id`, `title`, `created_at`,
`last_message_at` and `config.harness` (`chat.source_context`/`checkout_id` are
consumed inside `sidebar_chat_data`). A slim owned row struct — built inside the
guard's scope, with `sidebar_chat_data` taking `&Chat` — would cut this to a few
small allocations per row. It is a mechanical rename across one function and
touches ordering, pinned counts and drag payloads, so it wants a compiler and
the existing sidebar tests in the same change.

**Caching the model across paints.** The obvious fix — keep the built rows and
invalidate on a revision — is what v0.2.41 reverted
(`docs/performance-macos-stability.md`): a cached row kept geometry that no
longer matched the row's own text, and the cached/fresh pixel comparison did not
catch it. A safe retry has to cache *data* (not elements), key it on a
`AppState` revision plus every sidebar-affecting setting (compact, labels,
branch, pull request, organization, sort, pins, sections, filter), and assert
cached-vs-fresh equality in the UI tests. Worth doing only after the row struct
above, which removes most of what would be cached.

**Per-row lookups inside `render_chat_row`.** The remaining `chats.iter().find`
(`shell.rs:9102`, now one per drawn row instead of two) and
`render_project_icon`'s two scans plus a `Space` clone
(`shell/project_icon.rs:282`, `:323`) are O(chats) per drawn row. Both are
removable by handing the row's own chat (or its resolved space) down the
signature instead of re-finding it by id, but that is a signature change across
four call sites (sessions list, archived list, command palette, hover-card
preview — the last has no real chat) and one shared icon cache. Left for a
change that can be compiled and run against the UI tests.

**`change_request_for_chat` per row** (`state.rs:3169`) walks the snapshot list
per chat. Snapshots are few; an index would only matter for someone with many
open worktrees.

**`overview_chats` rebuilds a `HashMap` of sessions per paint** to avoid
`session_for`'s linear scan. Cheap relative to the row model, but it belongs to
the same cache as the rest.

## Measuring

Cadence, not wall time, is what exhausts a battery here — the sidebar can be
rebuilt by an unrelated animation. `ZERON_FRAME_STATS=1` logs per-view render
cadence (`crates/ui/src/transcript.rs:1938`, `record_view_frame("sidebar")`), and
`ZERON_NO_RENDER_CACHE=1` is the existing A/B knob for the transcript flatten
cache. For a before/after on this change, replay the same session list and
compare:

- sidebar renders/s while idle, while scrolling, and during a streaming turn;
- `sample`/`perf` main-thread time in `render_active_rows` for a workspace with
  several hundred sessions (the numbers that used to hurt are all proportional
  to the chat count, so a small workspace will not show them).

## Verification

Changed: `crates/ui/src/state.rs`, `crates/ui/src/shell.rs`,
`crates/ui/src/shell/spaces.rs`. `stable_pin_key_matches_pinned_first_order`
(`spaces.rs`, `pinned_session_tests`) pins the computed sort key to
`project_pinned_first`, the definition the pinned order had before.

This environment has no Rust toolchain and no crates.io access, so nothing here
was compiled or run. The three edited files parse cleanly under tree-sitter's
Rust grammar; run the real suite before merging:

```sh
cargo nextest run --config-file scripts/ci/nextest.toml --locked --no-fail-fast -p zeron-ui --lib
```

That is what `.github/workflows/ui-tests.yml` runs; the sidebar tests
(`active_sessions_virtualize_rows_outside_the_viewport`, the pinned-order and
section-reorder cases) live in `crates/ui/src/shell/spaces.rs`.
