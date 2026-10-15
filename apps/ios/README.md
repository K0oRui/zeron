# Zeron for iOS

A native SwiftUI viewport onto the zeron mesh. The phone is a **peer
device**: it joins the same Loro CRDT rooms as every other device (workspace
doc + per-chat session docs over the edge's Durable Objects), renders the
mirrors, and drives remote engines through the durable command queue. No
engine runs on the phone.

## Build & run

Requires Xcode 26+ (iOS 26 SDK — Liquid Glass APIs).

```sh
cd apps/ios
xcodebuild -project Zeron.xcodeproj -scheme Zeron \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' build
```

Or open `Zeron.xcodeproj` in Xcode and run. Dependencies (SPM, resolved
automatically): [loro-swift 1.13.x](https://github.com/loro-dev/loro-swift)
(matches the engine's loro 1.13), [swift-markdown](https://github.com/swiftlang/swift-markdown)
(cmark-gfm: tables/strikethrough/tasklists — the same feature set as the
desktop's pulldown-cmark config).

The Zeron target's **Rust core** build phase runs `scripts/ios/build-core.sh`,
which builds `crates/mobile` for the active platform (always optimized — the
`mobile` cargo profile) and refreshes the committed UniFFI bindings in
`Zeron/Core/Generated/`. `ZERON_SKIP_CORE=1` reuses the last built library
when iterating on Swift only.

## Layout

```
Zeron/
  App/         AppDelegate, SceneDelegate, AppModel (owns CoreClient; maps
               workspace snapshots to view models; Keychain credentials)
  Core/        Generated UniFFI bindings; Fonts (the exact bytes Rust measures,
               registered with CoreText) + CoreText fallback measurer
  Design/      Palette (Zeron color roles → light/dark), Glass helpers,
               cell-grid status glyph, harness brand marks
  Transcript/  TranscriptListView (virtualized scroll host over LayoutFrame),
               RowView/RowModel (CoreText painter for Rust display lists,
               streaming veil), FrameRelay
  Session/     SessionViewController, SessionSource (Core/Fixture), new-session
               canvas
  Composer/    ComposerBar (glass capsule ⇄ card with inline context chips;
               send/queue/steer/stop), question and queue panels, attachments
  Threads/     Sessions (foldable sidebar sections)/folder/search lists,
               cells, new-project folder browser
  Shell/       Tab bar (Sessions, Settings, search; "New session" accessory
               with live summary), sign-in
  Debug/       Transcript lab + hitch meter
ZeronLiveActivity/  Widget extension: the voice call's Live Activity (Dynamic
               Island + Lock Screen), with the orb's states exported from
               zeron-orb as vector assets
ZeronShared/   Compiled into the app and the extension: the activity's
               attributes and its mute/end App Intents
```

### Transcript pipeline

1. `zeron-client` applies session-doc updates incrementally (O(changed
   entries)) and publishes immutable snapshots.
2. `TranscriptView.attach(client, chatId)` subscribes **in Rust**; the layout
   thread turns entries into rows (one per markdown block / user message / tool
   group), reparses streaming markdown incrementally, and measures every row
   with `zeron-text` at the viewport width.
3. Each pass publishes a `LayoutFrame`: exact heights + prefix-sum offsets.
   `rowsIn(y0, y1)` is a binary search; `display(i)` builds a display list
   (text runs at exact positions, boxes, links, scrollers, widgets).
4. `TranscriptListView` positions reusable `RowView`s at those offsets and
   paints runs with CoreText at the Rust coordinates — measurement and
   rendering can't disagree. Display models for rows beyond the viewport are
   prefetched off the main thread.
5. Agent visuals (`show_visual` Luau scripts) get a worker thread each
   (`crates/mobile/src/visual`): it runs the script, lays the node tree out,
   draws canvases and charts as vector paint, renders scenes and shaders with
   wgpu, and reports its height back to the row. `VisualView` paints what
   arrives (rasterized on the worker thread) and forwards touches; the demo's
   "Agent visuals" chat (`-demo -route chat:chat-visuals`) has one of each.

## Launch arguments

| Arg | Effect |
| --- | --- |
| `-demo` | Offline demo workspace (Rust `DemoHost`: registry, docs, streaming replies) |
| `-fast` / `-longreply` | Demo stream speed / reply length |
| `-big` / `-huge` | Demo transcripts with 120 / 600 turns |
| `-route chat:<id>` / `new` / `more` / `search` | Open a screen at launch |
| `-signedout` | Clear stored credentials |
| `-dev <userId> <orgId> [-edge <url>]` | Dev bearer against an `AUTH_MODE=dev` edge (e.g. `wrangler dev`); not persisted |
| `-harness <id>` | Default harness for new sessions (`mock` for live-stack tests) |
| `-bench` (with `-lab`) | Display-link fling benchmark; writes `Documents/bench.json` |
| `-measureopen` | Session open latency; writes `Documents/open.json` |
| `-lab [-turns N] [-autostream] [-autoscroll] [-top] [-meter]` | Transcript lab over fixture markdown, with an on-screen hitch meter |

## Tests

```sh
# Rust
cargo test -p zeron-text -p zeron-markdown -p zeron-client -p zeron-mobile
cargo test --release -p zeron-mobile --lib bench_layout -- --ignored --nocapture

# Line-break accuracy vs CoreText (same font bytes), flows, hitch benchmarks
xcodebuild … -only-testing:ZeronTests/LineBreakAccuracyTests test
xcodebuild … -only-testing:ZeronUITests/SessionFlowTests test
xcodebuild … -only-testing:ZeronUITests/ScrollPerformanceTests test
```

Live stack (real edge + headless engine; see `ZeronUITests/LiveStackTests.swift`):

```sh
(cd edge && npx wrangler dev --port 27650 --var AUTH_MODE:dev) &
ZERON_DATA_DIR=/tmp/e2e ZERON_IPC_PORT=27811 ZERON_EDGE_URL=http://localhost:27650 \
  ZERON_EDGE_TOKEN=alice@org1 ZERON_ORG_ID=org1 ZERON_HARNESS=mock target/debug/zeron headless &
TEST_RUNNER_ZERON_LIVE_EDGE=http://localhost:27650 xcodebuild … -only-testing:ZeronUITests/LiveStackTests test
```

The test launches the app with `-harness mock`: a chat's configured harness
wins over the engine's `ZERON_HARNESS` default, so without it a real agent
would run.

## Session notifications

The edge sends them (`edge/src/push-notify.ts`, `registry-room.ts`): when a
session's registry row changes it applies the desktop's rule (run finished,
waiting on your input, run failed) and pushes to every phone that registered
with `POST /registry/:org/push-target`. The app asks for permission the first
time a session is started from the phone (or from Settings → Notifications),
registers its APNs token with the choices from Settings, stays quiet while
open, and opens the session a notification is tapped for.

Setup once per deployment: an APNs auth key (Apple Developer → Keys →
Apple Push Notifications service), then `wrangler secret put APNS_KEY_P8` (the
`.p8` contents) and `wrangler secret put APNS_KEY_ID` in `edge/`. The
TestFlight workflow enables the Push capability on the App ID itself.
`/registry/:org/stats` shows registered phones and recent deliveries.

## TestFlight release

Run the **TestFlight** workflow from GitHub Actions on `main`. It compiles the
iOS app and tests, selects the next numeric build number from App Store
Connect, archives with automatic signing, uploads an internal-only TestFlight
build, and waits for Apple processing to report `VALID`.

The workflow uses the `AC_API_KEY_P8`, `AC_API_KEY_ID`, and
`AC_API_ISSUER_ID` repository secrets.

### Connecting

- **WorkOS**: enter the edge URL, open the sign-in page on any device, paste
  the code it shows (`/auth/exchange`), pick an org (`/auth/refresh` re-scopes
  the token with the `org_id` claim).
- **Dev**: against an `AUTH_MODE=dev` edge (e.g. `wrangler dev`), enter a user
  id + org id; the bearer is `userId@orgId`.
- **Demo mode**: fully offline dataset with a scripted streaming reply —
  explore the UI with no infrastructure. Launch args for screenshot rigs:
  `-demo [-route chat:<id>|space:<id>] [-stream]`.

### Sessions without a project

Home's **+ → Session without a project…** opens the execution-device picker,
including when the account has no projects. Choose a desktop host (offline
hosts are allowed); the draft shows **No project** and the host name. Tap the
host above the composer to change it before sending. Catalogs and attachment
delivery use that host, and Git/ref/worktree controls are hidden.

Creation uses the same local registry outbox and durable session command
ledger as project sessions: `spaceId` is omitted, `deviceId` names the selected
host, `cwd` is `"~"`, and `roomGen` is `2`. Home's **All** list includes these
sessions, including ones synchronized from desktop. A project filter excludes
them. Active sessions pointing to a deleted/missing project remain hidden;
the archived shelf retains its existing behavior.

Regression coverage lives in `ProjectlessSessionTests` (registry persistence,
incoming desktop rows, destination selection, configuration and first command)
and `ProjectlessSessionUITests` (create/send/reopen, empty accounts, host changes,
project filters and the existing project flow). Run on an Xcode 26+ Mac:

```sh
cd apps/ios
xcodebuild -project Zeron.xcodeproj -scheme Zeron \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' \
  -only-testing:ZeronTests/ProjectlessSessionTests \
  -only-testing:ZeronUITests/ProjectlessSessionUITests test
```

Demo fixtures accept `-no-projects` (empty spaces/chats), `-ios-only` (no
execution hosts), and `-sethomefilter <spaceId>` (an empty value selects All).

## Architecture

```
Sync/
  LoroProtocol.swift    loro-protocol 0.3 wire codec (byte-compatible port of
                        the crate's encoding.rs: magic/varBytes/type/payload)
  RoomClient.swift      room.rs port: join with oplog VV, snapshot backfill,
                        resubmit-from-server-VV, DocUpdate+Ack, fragments,
                        %EPH presence sub-room, ping/pong lease, backoff
  WorkspaceStore.swift  ws3/{org}/{user} mirror: devices/spaces/chats/sessions
                        rows, presence heartbeats, viewer-side writes
                        (createChat, archive, lastSeenAt, own device row)
  SessionStore.swift    session doc mirror: entries/parts (continuations
                        joined), command ledger appends (rule 1), host nudge
Markdown/
  MarkdownModel.swift   block model + incremental tail re-parser (re-parse
                        from the 2nd-to-last top-level block; link-defs force
                        full parses) — parser.rs port
  Highlight.swift       line tokenizer with carry state, paint-only
  MarkdownBlockView.swift  mobile metrics: body 17/26, headings 23/31…17/26,
                        code 14/21 (Dynamic Type scaled line rows), violet inline code,
                        accent blockquotes, hairline tables
Transcript/
  TranscriptRows.swift  rows_for_entry port: block-granularity rows, stable
                        ids ({msg}#{part}.{block}, {msg}#g{n}), fingerprint
                        versions, consecutive-tool grouping
  NativeTranscriptTable.swift  UIKit row reuse, gesture anchoring and
                        animated local-send runway retained across navigation
  TranscriptView.swift  SwiftUI message content, user-message folding, follow
                        (70pt re-engage, 140pt jump), tool activity rail
  Veil.swift            paint-only streaming fade (EMA-tracked duration,
                        1−(1−p)^1.6 curve)
Composer/               glass pill, Send→Steer→Stop morph, QuestionPanel
                        (paged, numbered options, 220ms auto-advance)
Theme/                  theme.rs port: oklch→sRGB converter, exact palette,
                        Geist/Geist Mono, motion timings + flavour words
```

### Parity notes (desktop ⇄ mobile translations)

| Desktop | iOS |
| --- | --- |
| Sidebar: Spaces + attention-sorted Sessions | Home screen sections (same sort ranks: awaiting > errored > working > completed > idle) |
| Horizontal session tabs per space | Space detail: vertical session list (creation order) |
| Tab close = archive | Swipe-to-archive |
| Archived shelf under the sidebar list (open by default, Show-more paging, hover-swap Unarchive) | Same shelf under Home/space lists; unarchive is swipe-to-unarchive |
| Status word in the row corner (muted dots; Done keeps its pop; spinner rides bottom-right) | Same, same colors |
| Composer `white_alpha(0.03)` pill + hairline | Liquid Glass pill (`glassEffect`) + hairline |
| Harness brand SVG marks (icons.rs) | Same path data via a native SVG path parser (`BrandMarks.swift`) |
| Harness/model picker popover + curated catalogs | Brand-mark cards + catalog menu + reasoning-ladder chips (`HarnessCatalog.swift`, ported from crates/harness) |
| Add-space palette (device + folder browser) | New-space sheet: device tabs + remote folder browser (ListFolders over the device-room relay, git repos badged) |
| ControlRpc over device-room relay | `DeviceRelayClient` — binary `uleb128(len)+header+payload` frames, `{"s","k","to","from"}` header, ndjson ControlRpc; used for ListFolders + direct-to-host `Mutate {createSpace}` (local doc-write fallback when the host is offline) |
| Hover timestamps / copy | Context menus |
| Long user messages: Show more / Show less | Five-line preview, 44pt disclosure target, expansion retained per session |
| gpui `list()` sum-tree virtualization | Native table row reuse with SwiftUI content, stable row ids and version fingerprints |
| Stick-to-bottom spring, wheel-up breaks pin | Gesture-owned follow, composer-sized viewport with glass underlap, retained local prompt runway |

Status colors, font families, veil timing, command-ledger shapes, and wire
protocol follow desktop. Text sizes and touch targets are adapted for phones;
see [mobile polish and simulator coverage](../../docs/mobile-polish.md).

### Writer discipline (what the phone writes)

- Workspace registry: chat creates (host = the project's owning device or
  the explicitly selected projectless host), `archived`/`title`/`lastSeenAt`
  LWW sets, presence heartbeats. The phone owns no engine device row.
- Session docs: append commands (`run`/`steer`/`interrupt`/`respondInput`)
  with client-minted message ids for optimistic echo; add and reorder shared
  queue rows. Queue edits and removals require host acknowledgement. The host
  writes all transcript entries and command outcomes.
- After queuing a command it POSTs `/device/{host}/nudge` so a cold host
  opens the doc and drains — delivery stays durable in the doc regardless.

### Shared message queue

Messages submitted during an active turn use the host's shared queue when it
advertises support. The composer toolbar's **Queue / Steer** menu persists the
phone's preference; **Queue** remains the default. Steer allows automatic
mid-turn delivery when the provider supports it. Attachments wait for a turn
that can accept files.

Rows offer **Steer** for a host-advertised mid-turn provider and text-only
messages, or **Send now** (interrupt) otherwise. Unknown provider capabilities
leave delivery actions unavailable. Edits require a host lease; removal waits
for the host's acknowledgement and keeps the row inert while pending. Failed
or unconfirmed actions show an error above the composer and trigger sync repair.
Deleting an actively edited row discards it without first releasing its lease.

With an external keyboard, **Command+Return** submits the draft (including
attachment-only drafts), saves an active queue edit, or activates the first
queued row when the composer is empty. It never skips a blocked head or stops
the agent merely because the draft is empty.

Queue editing on iOS changes text only and preserves queued attachments, including when the text is cleared. Draft photos are hidden and the attachment picker is unavailable during editing. If the row disappears or its lease is superseded, **Copy edit and stop editing** saves the edited text to the clipboard and restores the original draft and photos.
