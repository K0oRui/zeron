# Measuring project and session loading

Two loads decide how long the app feels like it is "loading": the workspace
registry at engine boot, which populates the projects and the session list, and
a chat's transcript when it opens. Both are now timed in the logs, and the
session open builds its rows in parallel.

## What is logged

Long-running processes log at `info` by default and mirror to
`{data_dir}/logs/zeron-<mode>.log` (`zeron-headed.log` for the desktop app,
`zeron-engine.log` for the background engine). `RUST_LOG` overrides the level,
for example `RUST_LOG=info,zeron=debug` to also see the per-chat open line.

| Event | Level | Fields | Source |
| --- | --- | --- | --- |
| `workspace registry opened` | info | `load_ms`, `persist_ms`, `total_ms`, `chats`, `spaces`, `devices`, `migrated` | `crates/engine/src/workspace_host.rs` |
| `opened chat transcript` | debug | `chat`, `opening_tail`, `open_ms` | `crates/engine/src/rpc.rs` |
| `prepared transcript rows` | info | `entries`, `rebuilt`, `parallel`, `elapsed_ms` | `crates/ui/src/transcript.rs` |

`prepared transcript rows` fires only when a build rebuilds several rows, which
is a cold open. A streaming delta rebuilds one entry and stays quiet.

Read the numbers after one launch and one large-chat open:

```sh
grep -E "registry opened|opened chat transcript|prepared transcript rows" \
  ~/.zeron/logs/zeron-headed.log
```

## Session open: rows build in parallel

`TranscriptPreparation::prepare` used to build every entry's rows serially,
parsing each part's Markdown on one thread. A cold open of a long chat pays
that once, and it was the dominant cost of the open.

Settled entries are independent: each parses only its own parts, and the parser
caches are keyed by part id, so the work partitions cleanly. When a build
rebuilds at least `PARALLEL_PREPARE_MIN` (8) entries, `prepare` now fans them
across up to eight threads and merges each worker's parser caches back. The
streaming tail is excluded and stays on the calling thread, because its
incremental parser advances across deltas.

The primary transcript watch also installs the freshly prepared rows before the
frame emits, so the synchronous `sync` reuses them instead of rebuilding every
entry on the UI thread (documented in `crates/ui/src/state.rs`).

The fan-out is per entry, so it helps a conversation with many turns and many
tool calls. It does not help a single entry holding thousands of parts, which
is what the synthetic whale fixture is (`tests` build one entry with 5,000
parts, so it stays on the serial path). Parallelizing within one entry's parts
is the next step if that shape shows up in real transcripts.

## Project open: the boot rewrite stays on the path

`WorkspaceHost::open` loads the registry snapshot and rewrites it before
returning. Deferring that rewrite to a blocking task looked like a free win,
but the two `workspace_host::tests` cases that own the behavior
(`local_profile_compacts_a_bloated_registry_and_never_queues` and
`boot_compaction_reclaims_the_freed_store_pages`) fail: the boot save is also
the compaction step, and the free-page reclaim right after depends on it having
run. It stays synchronous, and the `persist_ms` field in the log is what tells
you its cost. On an ordinary (non-bloated) registry that cost is small.

## The sidebar waits on a boot journal scan

Loaded chats and projects appear only once the engine finishes assembling its
core, because the chats and spaces watches are served by the assembled RPC
service. The registry itself is cheap (single-digit to tens of milliseconds for
hundreds of chats). The time goes to crash recovery.

`SessionsEngine::recover_stale` calls `RunJournal::stale_sessions`, which opens
every journal under `profiles/<scope>/journals` and reads its tail to find runs
that died mid-stream (their last event is not `Done`). On a real install that
is hundreds of files and hundreds of megabytes. Timed directly:

```
workspace registry opened  load_ms=1 persist_ms=34 total_ms=58 chats=197 spaces=28
stale-session journal scan scan_ms=8715 stale=2
recovered interrupted session ... doc_ms=8
recovered interrupted session ... doc_ms=29
engine core assembly phases store_ms=49 workspace_ms=58 services_ms=8801 accounts_ms=323 sync_ms=0 total_ms=9233
```

The scan is 8.7 s of a 9.2 s boot. Opening the two crashed chats' documents is
8 ms and 29 ms, so the cost is the per-file tail reads, not the recovery. The
scan is real work and cannot be removed: without it a crashed turn stays
"working" forever in the transcript, and auto-resume never fires.

It also stopped being the common path. A run now writes a `{chat_id}.open`
marker beside its journal on the first event after a `Done`, and removes it when
the next `Done` lands, so `stale_sessions` reads a directory listing plus the
tails of only the marked journals instead of every journal on disk. The first
boot after the markers existed (no `.open-index` sentinel yet) still runs the
full scan once to seed a marker for any run already in flight, and the scan is
also fanned across cores in that one pass; boots after that are a directory
listing and a handful of reads. Recovery still confirms each marker against its
journal, so a torn marker cannot revive a finished turn, and `discard` clears it
with the journal.

This is a hint and the journal stays the truth, so the failure mode is narrow: a
marker that never got written leaves a crashed run unrecovered until the chat is
touched again. The write happens on the run's open edge (before the first event)
and the clear on its close edge (after the `Done`), so only a disk error widens
that window.

## Reproducing a measurement

For CPU and frame cadence rather than the log lines, use the existing tooling
described in `docs/performance-resource-usage.md`: `ZERON_FRAME_STATS=1` and
`scripts/perf/resource-profile.mjs`. The log timings above are the quick check;
the profiler is the matched before/after.
