# Changelog

All notable changes to Tanova are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [v0.3.1-ux-hardening] - 2026-11-13

### Added

- Explicit `ViewerWorkflow` toggle (`Nuke Node` vs `Fusion Target`) in the top bar with atomic `RouteNukeViewer` routing.
- Physical `Tab` quick-search popup on the Node Canvas (intercepting `egui` focus cycling via rising-edge `key_down` + `consume_key`).
- Bidirectional wire dragging (`FromOutput`, `FromInput`, `Detaching` rip-off) with `Escape` / right-click zero-side-effect cancellation.
- World-space box selection, Shift multi-selection, and multi-node group dragging.
- Nuke-style interactive red/green X/Y Transform handle in `ViewerPanel` with axis constraints (`X`, `Y`, free 2D) and in-flight drag accumulation.
- Global `Ctrl+Z` (Undo) and `Ctrl+Shift+Z` / `Ctrl+Y` (Redo) keyboard shortcuts with focus guards.
- Transactional `UndoableCommand::Batch` with step-by-step enrichment and automatic all-or-nothing rollback on cycle detection.
- Time-bounded (400 ms) gesture coalescing on `UndoStack` for `SetKnob`, `MoveNode`, and group `Batch` moves.
- `ProjectFormat` top-down propagation in `EvalContext` and automatic stamping onto newly spawned `Constant` nodes.

### Fixed

- Unconnected `Viewer` node and empty-packet viewport clearing (`frame --`).
- Node card port label collisions (`B`, `A`, `M` vs `in`, `M` positioned above stem tips).
- `Transform` + `Merge2` format canvas alignment and transparent out-of-bounds sampling.
- Downstream-only DAG cache invalidation (`invalidate_downstream`) on `SetKnob` and warm pixel cache preservation on layout-only `MoveNode`.
- Optimistic `GraphMirror` updates and `awaiting_kernel` repaint wakeups eliminating drag and undo visual lag.

## [v0.3.0-studio-io]

### Added

- **Milestone 11:** Spatial `Blur` cartridge with dynamic `BBox` expansion, `extract_padded_tile` aprons, WGSL storage buffers (`var<storage, read>`), and zero-radius `Arc` bypass.
- **Milestone 12:** `Mat3x3` 2D affine matrix engine, inverse bilinear sampling (`transform.wgsl` + CPU fallback), and interactive viewer overlay foundation.
- **Milestone 13:** Safe OpenEXR reader (`exrs`) with `f16` conversion, Top-Left to Bottom-Left Y-inversion, `displayWindow`/`dataWindow` intersection, single-worker 32-frame LRU `DiskCache` with thundering-herd suppression, `Read` cartridge (`%04d`/`####` sequence expansion, `error`/`black`/`nearest` missing-frame policies), and persistent timeline playback UI.

### Fixed

- Integrated GPU queue starvation via background worker thread frame yielding.
- `egui` 0.37 resizable panel infinite growth feedback loop (`TIMELINE_HEIGHT + item_spacing.y` reservation).

## [v0.2.0-render-core]

### Added

- **Milestone 7:** Nuke Bottom-Left `(0,0)` spatial coordinate system (`div_euclid`/`rem_euclid`), `BBox`, `Format`, `EdgeMode`, governed planar `TileBuffer` (`Uniform` 4-byte fast-path + `Dense` with `MemoryPermit`), and tile apron stitcher.
- **Milestone 8:** Cached `GpuComputeEngine` (`wgpu`) with 16-byte aligned WGSL shaders, exact Nuke `Grade` (with `unpremult` and `NaN`-safe inverse gamma) and `Merge2` (`over`, `under`, `plus`, `from`, `minus`, `multiply`, `stencil`, `mask` with B-pipe AOV preservation), and `Constant` cartridge.
- **Milestone 9:** Asynchronous background render loop with queue draining, live `ViewerPanel` (`NEAREST` texture upload, gain/gamma display transform, Bottom-Left float RGBA sampler HUD), and auto-generated `PropertiesPanel`.
- **Milestone 10:** `.nk` TCL stack-machine parser (`set`, `push`, `push 0`, `inputs N+M`, `addUserKnob`), `PassthroughNode` for unknown Foundry classes, `Dag::export_to_tcl()`, TCL math/expression evaluator (`expr_tcl.rs`), and headless `tanova-cli` executable.

## [v0.1.0-chassis]

### Added

- **Milestones 1–6:** 8-crate Cargo workspace, `Packet` and `EvalContext` `TypeMap` blackboards, MPSC `KernelBus` & `UndoStack`, `MemoryGovernor` & `catch_unwind` panic airbag, Lazy-Pull `Dag` with cycle detection and `blake3` `OpHash` caching, compile-time `inventory` cartridge registry, and custom Top-to-Bottom `egui::Painter` Nuke node canvas.
