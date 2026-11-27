# TANOVA ROADMAP - PHASE 1: THE IMMORTAL CHASSIS
*AI DIRECTIVE: Execute one milestone at a time. Do not skip or combine milestones.*

**Milestone 1: Workspace Initialization**
Generate the root `Cargo.toml` workspace defining 8 crates: `tanova-kernel`, `tanova-nodes`, `tanova-ui`, `tanova-render`, `tanova-parse`, `tanova-io`, `tanova-cli`, and `tanova-app`. Initialize all as empty crates.

**Milestone 2: Hardware & Blackboard Types (tanova-kernel & tanova-app)**
Initialize the shared `Arc<wgpu::Device>` in `tanova-app`. In `tanova-kernel`, implement `Packet` and `EvalContext` using a type-map pattern (`anymap`), and define the `NodeTrait` with `evaluate` and `serialize_to_tcl`.

**Milestone 3: The Message Bus, Threading & Governor (tanova-kernel)**
Implement the MPSC command channel and the Render-to-UI texture channel. Set up the Microkernel to run on a background thread. Implement the `evaluate_with_airbag` panic-catching function.

**Milestone 4: The DAG Topology & Cache Hashing (tanova-kernel)**
Implement the DAG using a Lazy Pull architecture. It tracks generic NodeIds and edges. Add the `blake3` OpHash generation and a basic RAM Cache map.

**Milestone 5: The First Cartridge (tanova-nodes)**
Create `dummy.rs`. Implement `NodeTrait`, `serialize_to_tcl`, and use the `inventory` crate to self-register it into a global node registry.

**Milestone 6: The Custom Nuke Canvas (tanova-ui)**
Create a custom `egui::Painter` widget drawing two rectangles with a Top-to-Bottom directional arrow connection. Wire it to `tanova-app` so it sends commands to the background kernel and safely reads off-screen `wgpu::TextureView`s.

# TANOVA ROADMAP - PHASE 2: THE RENDER CORE
*AI DIRECTIVE: Execute one milestone at a time. Do not skip or combine milestones.*

**Milestone 7: Nuke Spatial Types, Governed Planar Tiles & Apron Stitching (`tanova-kernel` & `tanova-render`)**
Inject the shared `MemoryGovernor` into `EvalContext` so cartridges can reserve bytes during evaluation. In `tanova-render`, implement Bottom-Left `(0,0)` spatial types (`TileCoord`, `TileRange`, `BBox`, `Format`, `RegionOfInterest`, `EdgeMode`) with Euclidean negative-coordinate tile math; a governed `TileBuffer` with collapsed uniform and dense representations; `ChannelPlane` / `Raster2D` planes with zero-copy channel pass-through and the canonical Bottom-Left to Top-Left `RGBA8` display conversion; and `extract_padded_tile` apron stitching that samples neighbouring tiles without grid seams.

**Milestone 8: GPU Compute Shader Engine & Core VFX Cartridges (`Constant`, `Grade`, `Merge2`)**
Align sparse input ports so port indices always match the Nuke `B` / `A` / `mask` order. Build a cached `wgpu` compute engine that compiles `Grade` and `Merge2` pipelines once, injects it through `EvalContext`, and runs 16-byte-aligned WGSL kernels over dense tiles with an identical CPU fallback and a zero-allocation uniform-tile fast path. Implement the three core cartridges (`Constant`, `Grade`, `Merge2`) with exact Nuke math, AOV preservation, and self-registration.

**Milestone 9: The Live Nuke Viewer, Off-Screen Texture Bridge & Properties Bin (`tanova-ui` & `tanova-app`)**
Stream `Arc<wgpu::TextureView>` frames from the kernel thread to an egui viewer panel without CPU copies, and add a properties bin that edits the selected node's knobs through the message bus.

**Milestone 10: The `.nk` TCL Stack-Machine Parser & Headless Render CLI (`tanova-parse` & `tanova-cli`)**
Implement the `.nk` TCL stack-machine parser, round-trip graph serialization, and a headless `tanova-cli` render path that loads a script, evaluates the DAG, and writes a display-transformed image.

# TANOVA ROADMAP - PHASE 3: THE SPATIAL ENGINE
*AI DIRECTIVE: Execute one milestone at a time. Do not skip or combine milestones.*

**Milestone 11: The Spatial Engine (Gaussian Blur, BBox Expansion & Storage Buffers)**
Add a `blur.wgsl` compute pipeline backed by `var<storage>` pixel buffers so padded apron tiles larger than 3 MB still dispatch, and implement the `Blur` cartridge with BBox expansion, non-RGBA channel pass-through, and a matching CPU fallback.

**Milestone 12: Transform Concatenation (Matrix Math & Filter Sampling) + Interactive Viewer Overlays**
Implement a `Transform` cartridge with 3x3 matrix concatenation and bilinear filter sampling, and add interactive viewer overlays (bounding boxes, handles) driven by the same transform data.

**Milestone 13: Multi-Channel EXR I/O & Timeline Playback (`tanova-io`)**
Read and write multi-channel OpenEXR frames and drive timeline playback through the message bus.

# TANOVA ROADMAP - PHASE 4: APPLICATION LIFECYCLE & STUDIO PIPELINE
*AI DIRECTIVE: Execute one milestone at a time. Do not skip or combine milestones.*

**Milestone 14: Application File Menu & Script Lifecycle (`tanova-app`, `tanova-kernel`, `tanova-parse`, `tanova-ui`)**
Application File Menu (`New`, `Open .nk`, `Save`, `Save As`), `xpos`/`ypos` Layout Persistence, Revision Dirty-State Tracking & Unsaved-Changes Close Guard

**Milestone 15: Node Bypass, Copy/Paste & Duplicate (`tanova-app`, `tanova-kernel`)**
Node Bypass (`D` Key), Copy/Paste (`Ctrl+C`/`Ctrl+V` via `.nk` TCL Clipboard) & Duplicate

**Milestone 16: Multi-Channel EXR `Write` Cartridge & Sequence Render Execution (`tanova-io`, `tanova-app`)**
Multi-Channel EXR `Write` Cartridge & Sequence Render Execution

# TANOVA ROADMAP - PHASE 5: UI/UX POLISH & ARTIST MUSCLE MEMORY
*AI DIRECTIVE: Execute one milestone at a time. Do not skip or combine milestones.*

**Milestone 17: Nuke Pin Geometry, `1`/`2` Viewer Input Pinning, Single-Key Node Spawning & Vertical Auto-Chaining (`tanova-ui` & `tanova-app`)**
Nuke Pin Geometry (`B` Top, `A` Left, `M` Right), `1`/`2` Viewer Input Pinning, Single-Key Node Spawning & Vertical Auto-Chaining

**Milestone 18: Node Category Colors, High-Contrast Dark Theme + Studio Palettes & Two-Column Inspector Grid (`tanova-ui` & `tanova-app`)**
Node Category Colors, High-Contrast Dark Theme + Studio Palettes (`Classic`, `Slate`, `Midnight`) & Two-Column Inspector Grid

**Milestone 19: Graph Organization Cartridges & Canvas Containers (`Dot` Routing Elbows & `Backdrop` Boxes) (`tanova-kernel`, `tanova-parse`, `tanova-ui`, `tanova-app`)**
Graph Organization Cartridges & Canvas Containers (`Dot` Routing Elbows & `Backdrop` Boxes)

# TANOVA ROADMAP - PHASE 6: PRODUCTION COMPOSITING ENGINE

*AI DIRECTIVE: Execute one milestone at a time. Do not skip or combine milestones.*

**Milestone 20: Bounded GreedyDual-Size DAG Cache, Viewer Display Transforms & Channel Soloing**
Bounded GreedyDual-Size DAG cache (cost- & recency-aware eviction), Viewer display transforms (`Linear`, `sRGB`, `Rec.709`, `ACES`) and `R`/`G`/`B`/`A` channel soloing, plus core color/alpha cartridges (`Premult`, `Unpremult`, `ColorCorrect`, `Invert`).

**Milestone 21: Demand-Driven Input & Temporal Routing (`Switch`, `TimeOffset`, `FrameHold`) & Pipe Splicing on Drop**
Nodes that selectively evaluate upstream inputs or shift evaluation time (`Switch`, `TimeOffset`, `FrameHold`), and pipe splicing that reconnects upstream/downstream wires when a node is dropped.

**Milestone 22: Keyframe Animation Engine (`{curve ...}`), Animated Knob Indicators & Timeline Keyframe Ticks**
Per-knob keyframe curves (`{curve ...}` syntax), animated knob value indicators in the properties bin, and keyframe ticks on the timeline playhead.

# TANOVA ROADMAP - PHASE 7: STUDIO COMPOSITING, SPATIAL WARPING & CURVE EDITOR

*AI DIRECTIVE: Execute one milestone at a time. Do not skip or combine milestones.*

**Milestone 23: Interactive Curve Editor tab & Node Graph Minimap** — COMPLETE (v0.6.0)
Interactive curve editor with draggable keyframes and interpolation modes (`tanova-ui`), Node Graph Minimap for navigating large graphs (`tanova-ui`), Projective Homography & Optical Defocus engine (`tanova-render`), and `Mirror` / `Ramp` / `Glow` cartridges (`tanova-nodes`).

**Milestone 24: CornerPin2D & Defocus cartridges + UI integration** — COMPLETE (v0.6.0)
`CornerPin2D` with interactive 4-corner Viewer overlay handles, `Defocus` cartridge, and full UI integration for all Phase 7 nodes.

# TANOVA ROADMAP - PHASE 8: PROJECT FORMAT VERSIONING, ATOMIC SAVE & CRASH RECOVERY
*AI DIRECTIVE: Execute one milestone at a time. Do not skip or combine milestones.*

**Milestone 25: `.tnv` Schema Versioning, Atomic Save, Confirmed Dirty-State Save, Idle Auto-Save & Crash Recovery** — COMPLETE (v0.6.5)
Stamps every exported `.tnv` with `# tanova_schema 1` on line 2 (line 1 stays Nuke's `version 15.0 v5`). Parser validates the header: `v == 0 || v > TANOVA_SCHEMA_VERSION` is rejected; a missing header loads normally (legacy `.tnv` / external `.nk`). Adds `# tanova_recovery_source <path>` metadata parsed into `ParsedScript.recovery_source`. Save becomes atomic: hidden temp file in the same directory, `create_new`, `write_all`, `flush`, `sync_all`, drop, `rename`, best-effort temp removal on failure. A failed `SaveScript` emits `ScriptError` plus `DirtyStateChanged { is_dirty: undo_stack.is_dirty() }` and must NOT call `mark_saved()`. Save is confirmed-only: `execute_save_to_path` no longer clears dirty or binds `script_path`; only `ScriptSaved` does, and follow-up actions (`pending_post_save`) run only after confirmation. `WriteAutoSave` never touches `mark_saved()`, `is_dirty`, or `render_viewer()`. Auto-save gating: idle ≥ 3 s, min interval 30 s, not dragging, no modal/pending recovery, not already saving. `recovery_dir` is `Some(...)` only in live desktop startup; `None` in headless/tests. Recovery restore uses `parsed.recovery_source` for `script_path` (never the hidden `autosave.tnv`) and marks `is_dirty = true`. Centered ASCII "Crash Recovery" modal (Restore / Discard). Clean-exit cleanup via `on_exit` removes any pending snapshot.

**Milestone 26: Real GUI RAM Budget, Cooperative Cache Eviction & 1080p Speed Gate** — COMPLETE (v0.6.5)
Real 1–2 GiB auto-scaled GUI `MemoryGovernor` cap; cooperative `Dag` cache eviction (evicts lowest-priority entry on `MemoryExceeded`, retries once); `DiskCache::clear()` releases `MemoryPermit`s; `PurgeCache` + live top-bar `RAM` meter & `Purge` button; safe `N - 2` `std::thread::scope` tile multi-threading; per-node timing heat bars + non-blocking hover tooltips; `tanova-cli --bench-1080p` speed gate.

**Milestone 27: Full-Res-Safe Viewer Proxy & Remaining Cleanup** — COMPLETE (v0.6.5)
`1:1` / `1:2` / `1:4` Viewer Proxy (`ProxyScale`, `Ctrl+P`, amber `PROXY` badge, full-res logical handles & full-res `Write` lock); direct-slice + parallel `to_top_left_rgba8_with_view`; layout-only `MoveNode` render skip & `Arc<Packet>` upload dedupe; surgical downstream cache invalidation on `AddNode`/`ConnectEdge`/`Batch`; `1920×1080` knob fix across `Constant`/`CheckerBoard2`/`Ramp`/`Reformat`/`Crop`; `CornerPin2D` upstream format inheritance + `[Set to Input]`; top-bar `1280px` fit + bottom-dock export pill; unused `RamCache` removal; `Cargo.toml` repo URL fix; `LICENSE-MIT`/`LICENSE-APACHE`, `deny.toml`, `.github/workflows/ci.yml`; plain-English `README.md` rewrite.

# TANOVA ROADMAP - PHASE 9 (v0.7.0): MASKING, KEYING & ERROR UX
*AI DIRECTIVE: Execute one milestone at a time. Do not skip or combine milestones.*

**Milestone 28: Shared Viewer Overlay Module, `Ramp` Handles & Node Error UX** (this milestone)
A shared, extensible Viewer overlay module (`tanova-ui/src/overlay.rs`) so every 2D handle — `Transform` jack, `CornerPin2D` corners, and new `Ramp` endpoints — lives in one input path. `OverlayViewport` reproduces the Viewer coordinate helpers proxy-safely so all handles write full-resolution logical coordinates even at 1:2/1:4 proxy. `emit_2d_knob_command` preserves the untouched alpha channels (`[x, y, prev[2], prev[3]]`) and auto-keys via `Batch([SetKnob, SetKnobCurve])` when the knob has an active curve. `OverlayState` with `is_dragging()` and `OverlayResponse { consumed_pointer, commands }`; per-kind handlers for `Transform` (exact red X/green Y/crosshair/dashed box/smooth drag), `CornerPin2D` (orange quad + numbered handles clamped to `[-fw..=2*fw, -fh..=2*fh]`, auto-key) and `Ramp` (`p0`/`p1` from effective `KnobValue::Color`, `Color32::from_rgb(100,200,255)` line 1.5px, 7.0px `"P0"`/`"P1"` handles highlighted on hover/drag, full-res logical coords, suppress pan, `request_repaint`). `ViewerPanel::show` signature and `is_dragging_overlay()` stay 100% backward-compatible; the overlay runs strictly before pan/drag handling. Exact-origin node error reporting: `NodeErrorInfo { node, message, crashed }` and `KernelEvent::NodeErrors(Vec<NodeErrorInfo>)` emitted on every `render_viewer()` pass (empty vec clears), reporting the failing origin node (e.g. `Read #4` → `NodeId(4)`, never the Viewer `NodeId(3)`). `GraphMirror::node_errors` updated on `NodeErrors`, pruned on `RemoveNode`, cleared on `clear()`/`NewScript`/`ReplaceDag`; canvas draws a 2.0px `Color32::from_rgb(235,75,70)` border plus `"CRASH"`/`"ERR"` pill, and the `Order::Tooltip` hover area shows the truncated error reason. `ViewerPanel::active_error` paints a non-blocking dark-red banner pinned to the top interior edge of the Viewer viewport.

**Milestone 29: Extensible Node Data Model & Interactive `Roto`**
Extensible per-node data model; interactive `Roto` shape drawing with Viewer overlay handles.

**Milestone 30: `ChromaKey`**
`ChromaKey` cartridge with screen color selection, spill suppression, and Viewer integration.

## Release & Tagging Policy

- Every milestone/version tag must first record all Added, Changed, and Fixed items in `CHANGELOG.md`.
- Every release must use an annotated Git tag (`git tag -a vX.Y.Z-... -m "..."`) and push both the branch and `--tags` to `origin`.

## Release History

- **[v0.6.0] - 2026-11-22:** Phase 7 Studio Compositing, Spatial Warping & Curve Editor (Milestones 23–24). See `CHANGELOG.md`.
- **[v0.5.0] - 2026-11-19:** Phase 6 Production Compositing Engine (Milestones 20–22).

