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

The grid feature is application code in `app-core/src/recorder.rs`, not a
CostBarRuler capability. It is currently deliberately narrow:

- `STAGE_MORAN_SHUGE_TILES` is one static 12 by 8 table.
- `project_stage_tile` transforms grid coordinates with the level's camera
  view, a 30 degree pitch, optional 10 degree side-view yaw and a 20 degree
  perspective projection.
- `snap_client_to_stage_tile` chooses the nearest non-forbidden tile. A normal
  battlefield click additionally requires a 72-pixel maximum distance.
- Deploy drops use the side view; normal selection compares normal and side
  projections to tolerate the two in-game views.

The 96 stored tile fields (`buildableType`, `heightType` and `tileKey`) and the
two camera vectors were verified against this MAA resource at commit
`bad264489337d78882b3f93e0fb2cce4a3296f29`:

```text
resource/Arknights-Tile-Pos/
  level_crisis_v2_05-01-obt-crisis-v2-level_crisis_v2_05-01.json
```

The source lineage is also verified: `prts-plus/src/logic/calc_view.py` uses
the same projection model and attributes map-tile recognition to
[`yuanyan3060/Arknights-Tile-Pos`](https://github.com/yuanyan3060/Arknights-Tile-Pos).
The original repository is MIT licensed and requests an attribution URL.
Migration erased the pre-existing repository history, so the exact historical
copy step cannot be proven further. `THIRD_PARTY_NOTICES.md` records both the
confirmed MIT attribution and the MAA/PRTS+ license-review obligation.

For more maps, do not add another large Rust constant to `recorder.rs`. Define
a versioned, application-owned map-data format under `resources/maps`, attach
the upstream source path and commit to every imported map, and load it through
a dedicated `app-core` map module. Keep camera calibration and projection in
that module so recorder input handling only asks for a tile match.

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
