# Architecture And Core Runtime

This document describes the runtime boundary that belongs to Arknights Super.
It is the reference for new backend services and for upgrades of
ArknightsCostBarRuler.

## Goals

- Record input against capture observations in the backend, not in the GUI.
- Keep the desktop application as one process, while isolating blocking work
  on dedicated threads.
- Treat CostBarRuler as a replaceable implementation dependency rather than
  an application-wide API.
- Keep WebView traffic small and tolerant of reconnects or out-of-order
  delivery.

## Module Boundaries

| Area | Location | Owns | Must not own |
| --- | --- | --- | --- |
| Desktop shell | `apps/desktop` | Tauri commands, persistent UI state, runtime lifecycle, UI events | Capture or input attribution rules |
| CLI | `apps/cli` | Debug and capture entry points | UI state |
| UI contract | `crates/app-contract` | Serializable WebView/backend event types | Capture or Windows APIs |
| Business runtime | `crates/app-core` | Recorder, Windows hooks, operation state machine, OCR, storage, frame/input correlation | Tauri or React types |
| Upstream boundary | `crates/ruler-bridge` | Application-facing ruler types, target configuration and configuration migration | Application business policy |
| Upstream implementation | `third_party/cost-bar-ruler` | Capture backends, battle-state detection, cost-bar analysis and calibration | Our UI and record format |

Only `ruler-bridge` may directly depend on `ruler-core`. The boundary check in
`scripts/check-boundaries.ps1` enforces this at the Cargo dependency level.
`ruler-bridge::legacy` is a temporary recorder migration adapter, not an API
for new application code. New code must use the stable bridge types such as
`CaptureTarget`, `TargetConfiguration`, `FrameObservation` and `BattleState`.

## Runtime Topology

The desktop program is a single process. It does not host the upstream Slint
application or run CostBarRuler as a second executable.

```text
Tauri AppController
  |- recorder capture worker
  |    -> CostBarRuler capture + analysis
  |    -> SharedClockState + FrameTimeline
  |- Windows low-level input-hook thread
  |    -> raw input + operation candidates
  |- event pump / OCR worker
  |    -> operation events with optional operator name
  `- recorder receiver loop
       -> active battle, record storage, status/timeline events

React WebView
  <- bootstrap snapshot and small status/timeline projections
```

Thread separation is a latency and correctness boundary, not a second-product
boundary. The capture worker, hook thread and event/OCR work can progress when
the WebView is busy, minimized or reconnecting.

## Record Pipeline

### 1. Startup And Target Selection

`AppController` examines the saved application settings, discovers supported
targets, and starts a recorder only after setup and calibration are ready. The
application owns target discovery policy; the bridge converts the selected
`TargetConfiguration` into the upstream configuration file. A calibration is
kept only while its target fingerprint remains unchanged.

The current automatic discovery covers the Windows client and the supported
MuMu 12 layout. MuMu 15 is manual/best-effort support inherited from upstream;
this repository intentionally does not carry the former local MuMu 15 patch.

### 2. Capture And Time Inference

The capture worker opens the upstream capture backend and loads its calibration
profile. For every completed frame it:

1. Detects the battle UI state and analyzes the cost bar through CostBarRuler.
2. Publishes the elapsed frame count, logical frame, cycle length, window
   geometry and battle state into `SharedClockState`.
3. Adds an `ObservationStamp` to the fixed-capacity `FrameTimeline` with the
   monotonic capture timestamp.
4. Retains only the latest frame/crop needed for optional OCR and settings
   previews; it does not forward raw frames to the WebView.

This loop is the authoritative time source. It records at capture cadence so
the business record remains accurate even when the game slows down or pauses.
The GUI's approximately 20 Hz time display is intentionally a lower-frequency
projection and must never be used to timestamp a player action.

### 3. Input Attribution And Operations

The Windows low-level keyboard/mouse hook obtains an input timestamp and uses
`FrameTimeline::correlate` to select the latest completed observation at or
before that input. If no earlier observation exists, the oldest retained one
is selected and its age is explicit. This is why record correctness does not
depend on IPC scheduling or React rendering.

Mouse input is converted from screen to target-client coordinates and feeds a
small operation state machine:

- A click during a running or paused battle may select a battlefield tile.
- A drag while the upstream detector reports operator deployment creates a
  pending deployment candidate.
- The following facing-adjustment state finalizes that deployment.
- Configured skill and retreat keys use the last selected tile and capture an
  optional operator-name crop for OCR.

Raw input and normalized operation events are logged. OCR is intentionally
asynchronous: an operation keeps its original correlated timestamp while its
name is attached later by the event pump.

### 4. Storage And Business Services

`app-core` exposes recorder events; the desktop controller turns them into the
current battle and persistent records. Future services such as prompts,
strategy validation or analytics should consume recorder events and
`FrameTimeline`-style observations inside Rust. They must not infer state from
the visible UI or call the WebView to obtain a clock value.

## Backend To GUI Synchronization

The backend is the source of truth. The UI receives two projections.

| Channel | Payload | Delivery rule |
| --- | --- | --- |
| `get_bootstrap_snapshot` | Full recoverable state plus protocol version, revision and session ID | Used at startup and explicit refresh/reconnect |
| `app://status` | Full state after meaningful state changes | Each event has an increasing `revision` |
| `app://timeline` | Tiny live clock projection | Emitted at 20 Hz with `session_id` and increasing `sequence` |

React discards stale revisions, changes session on a new runtime and ignores
older timeline sequences. Timeline rendering runs in `startTransition`, so an
editor interaction or command response is not delayed by live-clock painting.
Raw images remain backend-local; the settings page uses an on-demand detail
preview command instead.

The design target is below 100 ms UI freshness. This is a presentation target,
not the recorder's timestamp budget: the recorder always associates input with
backend observations at the capture side.

## CostBarRuler Upgrade Procedure

The dependency is the official `rust-rewrite` Git submodule pinned in
`.gitmodules`; this project does not maintain a fork.

1. Update the submodule SHA in a clean working tree.
2. Adapt only `ruler-bridge` first. Do not spread upstream types or config
   details into `app-core`, Tauri or React.
3. Run `cargo test --workspace`, `pwsh scripts/check-boundaries.ps1`,
   `npm run build` from `apps/desktop`, and a Windows capture/calibration smoke
   test.
4. Treat changed capture semantics or calibration formats as a migration:
   require recalibration rather than silently reusing an incompatible profile.

`app-core/src/recorder.rs` still uses the bridge's temporary `legacy` adapter.
Replacing it with a bridge-owned `RulerSession` facade is the next decoupling
task; it should be completed before expanding direct use of upstream features.

## Map Tile Projection

The grid feature belongs to `app-core/src/maps.rs`, not CostBarRuler. Version 1
map files live under `resources/maps` and are embedded with `include_str!`, so
development and packaged builds use identical data without runtime downloads.
Each file records its source repository, commit, path and original SHA-256.
The loader validates the schema, stage ID, dimensions, tile order and cameras.

- `StageId::MoranShuge` keeps the original 12 by 8 map. All 96 tile fields were
  compared with the previous Rust table before migration.
- `StageId::Jt8_3` selects ordinary JT8-3 (`main_08-17`), a 15 by 9 map with
  its own normal and deployment camera vectors.
- `StageMap::project` retains the existing 30 degree pitch, optional 10 degree
  side-view yaw and 20 degree perspective model.
- `StageMap::snap` selects the nearest non-forbidden tile. Normal battlefield
  selection retains the existing 72-pixel limit; deployment uses the side view.

Both maps were imported from `resource/Arknights-Tile-Pos` in MAA commit
`bad264489337d78882b3f93e0fb2cce4a3296f29`. Their file names are recorded in
each map's `source` object. The display name for the original crisis map remains
墨染书阁, while JT8-3 retains its source name 昂首，足践烈焰.

`AppSettings.stage_id` is persisted and copied into immutable `RecorderOptions`
and the input hook runtime. Changing it restarts the recorder only while idle;
the backend rejects changes while a battle, pending OCR or runtime restart is
in progress. The UI also disables map selection during a battle or calibration.
Each battle captures its stage ID at creation, and record metadata retains it.
Missing stage IDs in old settings and records default to the original map;
unknown IDs are rejected. History remains readable across maps, but timing
highlights and operation prompts require matching stage IDs.

The source lineage is also verified: `prts-plus/src/logic/calc_view.py` uses
the same projection model and attributes map-tile recognition to
[`yuanyan3060/Arknights-Tile-Pos`](https://github.com/yuanyan3060/Arknights-Tile-Pos).
The original repository is MIT licensed and requests an attribution URL.
Migration erased the pre-existing repository history, so the exact historical
copy step cannot be proven further. `THIRD_PARTY_NOTICES.md` records both the
confirmed MIT attribution and the MAA/PRTS+ license-review obligation.

To add another map, import its data into the versioned format, retain provenance,
register a `StageId`, and extend projection and persistence checks. The backend
provides the available-stage list to the UI. Recorder input handling only passes
its selected stage to the shared projection module.

## Verification

```powershell
pwsh scripts/check-boundaries.ps1
cargo test --workspace
cd apps/desktop
npm run build
npm run tauri -- build --no-bundle
```

The final command is a Windows smoke build. A real capture target and a fresh
calibration are still required to validate a release against the current game
client.
