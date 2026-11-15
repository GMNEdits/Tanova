# Changelog

All notable changes to Tanova are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased] - Phase 5: UI/UX Polish & Artist Muscle Memory

### Added

- **Milestone 17 Part 1 — Nuke Pin Geometry & Viewer A/B Pinning (`tanova-ui`, `tanova-app`):**
  - Exported `PortSide` (`Top`, `Left`, `Right`) and `input_port_side()` from `tanova-ui` so the canvas can place `B` on top, `A` on the left, and the mask on the right.
  - Side-aware input pin anchors, stems, and labels: `Merge2` renders `B` at `rect.center_top()`, `A` at `rect.left_center()`, and `M` at `rect.right_center()`; two-input filters such as `Grade` keep `in` on top and their mask on the right.
  - Side-aware cubic Bezier wire tangents and arrowheads, so wires enter left/right mask pins horizontally instead of from above. Input port hit-testing now uses the same side-aware pin positions.
  - `NodeCanvasState::viewer_slots: [Option<NodeId>; 2]` and `active_viewer_slot` for Nuke-style `1` / `2` viewer input pinning, including deletion pruning and first-frame seeding from the current viewer binding.
  - `NodeCanvasState::select_viewer_slot()` with an A/B comparison guard: pressing the other slot's key while a node stays selected flips the viewer back without overwriting the bookmarked slot.
  - `1` / `2` shortcuts wired in `TanovaApp::handle_shortcuts` and the standalone canvas, with `VIEWER 1` / `VIEWER 2` badges on the active viewer card and a `1/2: view` canvas hint.
- **Milestone 17 Part 2 — Single-Key Node Spawning & Vertical Auto-Chaining (`tanova-ui`, `tanova-app`):**
  - Default Nuke single-key node spawning: `R` Read, `W` Write, `C` Constant, `G` Grade, `B` Blur, `T` Transform, and `M` Merge2. Keys use strict `Modifiers::NONE` so `Ctrl+C` / `Ctrl+V` / `Ctrl+D` keep their clipboard meanings.
  - Spawn guards require the pointer over the canvas with no floating layer capturing it, no focused text/widget keyboard input, no Tab search, no active wire drag, and no open modal. Canvas shortcut messages are dropped while a modal is open.
  - Automatic vertical B-pipe chaining: spawning a processor or combiner with a node selected emits an atomic `UndoableCommand::Batch` of `AddNode` + `ConnectEdge` at `y + 90.0`, nudging `x += 140.0` when branching, and advances selection so `G` -> `B` -> `T` builds a wired column in one undo step.
  - `GraphMirror` command application is idempotent: repeated `AddNode` events update `kind` / `x` / `y` in place while preserving knobs and project-format stamping, and `ConnectEdge` replaces the existing incoming edge on `(to_node, to_port)` instead of duplicating it.
  - `Tab` quick-search and context-menu spawns now share the `spawn_node_message` helper, and the canvas hint reads `R/W/C/G/B/T/M: add node | Tab: search | 1/2: view | D: bypass | F: frame`.
- **Milestone 18 Part 1 — Studio Theme Engine & Node Category Colors (`tanova-ui`, `tanova-app`):**
  - New `tanova-ui::theme` module with `StudioTheme` (`Nuke`, `Natron`, `Fusion`), `NodeCategory` (`Io`, `Color`, `Merge`, `Filter`, `Transform`, `Viewer`, `Other`), `ThemePalette`, `node_category()`, `palette()`, and cache-safe `apply_theme_visuals()`.
  - Two-tone node cards: category-colored header band with top-only rounding, dark crimson crashed fill, dimmed disabled state, drop shadow, and `card_corner_radius` per theme (`2` Nuke, `3` Natron, `6` Fusion).
  - Themed canvas background and grid: line grid for Nuke/Natron, dot grid for Fusion; spacing clamped to `.max(18.0)` at low zoom.
  - Role-colored input pins (`pin_b` / `pin_a` / `pin_m` by `PortSide`) and matching wire colors (`wire_default` for top/B, `pin_a` for left/A, `pin_m` for right/mask).
  - `NodeCanvasState.theme` persists across `execute_new_script()` / script reloads.
  - Top-bar segmented theme selector: `Theme: [Nuke] [Natron] [Fusion]`.

### Changed

- Canvas bottom hint now uses printable ASCII separators instead of Unicode bullets, keeping the Linux `egui` glyph atlas clean.

## [v0.4.0-studio-pipeline] - 2026-11-14

### Added

- **Milestone 14 — Project File Lifecycle, `.tnv` Format & Unsaved-Changes Guard:**
  - Top-bar `File` menu (`New Script`, `Open Script...`, `Save`, `Save As...`, `Quit`) and global shortcuts (`Ctrl+N`, `Ctrl+O`, `Ctrl+S`, `Ctrl+Shift+S`, `Ctrl+Q`).
  - Native `.tnv` project file extension (`Untitled.tnv`) with version-safe extension normalization (`ensure_script_extension`, preserving version dots such as `shot_010.v02.tnv`) alongside full `.nk` Nuke script compatibility (`is_supported_script_file`).
  - Node `(x, y)` coordinate persistence via `xpos` and `ypos` in `Dag::export_to_tcl()` and `tanova-parse` (`ParsedNode.xpos` / `ypos`) without polluting node `KnobTable` sliders.
  - Monotonic `op_id` revision dirty-state tracking on `UndoStack` (`current_op_id`, `mark_saved`, `clear_and_mark_clean`, `is_dirty`) with `KernelMessage::{MarkClean, NewScript, ReplaceDag, SaveScript}` and `KernelEvent::{DirtyStateChanged, ScriptSaved, ScriptError}`.
  - `GraphMirror::clear()`, `GraphMirror::from_dag()`, and `NodeCanvasState::reset_for_script()` for full UI/canvas synchronization on script load and reset.
  - High-contrast top-bar project pill badge (slate `Untitled.tnv` when clean, amber `Untitled.tnv *` when dirty) with hover tooltip displaying full path and `epoch`.
  - In-app `FileModalState` (`Open`, `SaveAs`) with directory navigation, `[Paste]` filename button, and `arboard` cross-platform clipboard bridge (`bridge_clipboard_paste`).
  - `PendingUnsavedAction` state machine and `"Unsaved Changes"` confirmation dialog (`Save`, `Discard Changes`, `Cancel`) intercepting OS window close requests (`ViewportCommand::CancelClose`).
  - Added Section 8 to `ARCHITECTURE.md` documenting zero-copy GPU residency (`TileStorage::GpuResident`), demand-driven temporal input hooks, and Blackboard state sharing.
- **Milestone 15 — Node Bypass (`D`), Sub-Graph TCL Clipboard & Canvas Framing:**
  - Node bypass state (`disabled: bool`) across `DagNode`, `NodeSnapshot`, `CanvasNode`, and `RemoveNode`/`RestoreNode` with `UndoableCommand::SetDisabled { node, disabled }`, `OpHash` differentiation, and zero-copy `PortId(0)` passthrough in `Dag::pull`.
  - `.tnv` / `.nk` persistence for `disable true` in `Dag::export_to_tcl()` and `tanova-parse`.
  - Nuke-style dimmed node card and diagonal red `X` overlay on bypassed nodes, plain `D` shortcut (supporting multi-node `Batch` undo), and `"Toggle Bypass (D)"` context menu item.
  - Canvas `F` shortcut (`NodeCanvasState::frame_nodes`) to center and fit selected or all nodes in the viewport (`zoom` clamped to `0.25..=1.5`).
  - Sub-graph TCL clipboard serialization (`export_selection_to_tcl`) and remapped atomic paste (`paste_tcl_snippet`) supporting `Ctrl+C` (Copy), cascading `(+40, +40)` `Ctrl+V` (Paste), and `Ctrl+D` (Duplicate without modifying clipboard) in a single `UndoableCommand::Batch` undo step.
- **Milestone 16 — Multi-Channel OpenEXR `Write`, Sequence Renderer & EXR Browser:**
  - 32-bit float OpenEXR file exporter (`tanova_io::write_exr_to_path`) with symmetric Bottom-Left to Top-Left Y-inversion, automatic parent directory creation (`create_dir_all`), and `"rgba"` / `"all"` multi-channel AOV export (`DepthZ` and custom layers sorted lexically).
  - `"Write"` cartridge (`tanova-nodes`) operating as a zero-copy interactive passthrough during viewport scrubbing and exporting frame sequences via `export_packet_with_write_knobs`.
  - Decoupled `FrameExporter` callback in `EvalContext` and `KernelMessage::ExecuteWriteNode` sequence render loop (`first..=last`) with per-frame cache clearing, playhead `Time` restoration, and non-dirtying execution.
  - `"Write"` node `channels` dropdown (`rgba` / `all`), `[Render Sequence to Disk]` button (`pending_write_render`), and pure-ASCII render progress/completion status in the top bar (`WriteFrameRendered`, `WriteCompleted`).
  - Headless `--out-exr <path>` CLI render flag in `tanova-cli`.
  - Nuke/Natron-style `Browse...` button (`FileBrowseRequest`) beside `"file"` knobs on `"Read"` and `"Write"` nodes opening `BrowseReadExr` / `BrowseWriteExr` modals with `[EXR]` file filtering, inline `+ Create Folder` directory creation, and automatic frame-sequence detection (`detect_exr_sequence_in_dir`).

### Changed

- Replaced flat 16 ms worker thread sleep in `kernel_loop` (`tanova-kernel/src/bus.rs`) with an adaptive 2–16 ms GPU yield scaled by render duration.

### Fixed

- **Modal Close State Machine:** Fixed `draw_file_modal` bug where `self.file_modal` remained open after clicking `Save`, `Open`, `Cancel`, or pressing `Escape` by taking ownership via `self.file_modal.take()` and resolving an explicit `ModalOutcome`.
- **Linux Missing-Glyph Tofu Boxes:** Replaced Unicode symbols (`●`, `📁`, `📄`) with 100% printable ASCII (`Untitled.tnv *`, `[..] Parent Directory`, `[DIR]`, `[TNV]`, `[NK]`, `[EXR]`, `->`) and hidden dot-folder filtering (`.git`).
- **Modal Centering & Single-Paste Replacement:** Anchored `FileModalState` and `Unsaved Changes` dialogs to `Align2::CENTER_CENTER`, and added auto-focus + full-text selection (`CCursorRange`) on `file_input` upon modal open so `Ctrl+V` replaces `"Untitled.tnv"` cleanly without double-pasting.
- **Linux X11/Wayland Canvas Copy/Paste:** Fixed `Ctrl+C` and `Ctrl+V` on the Node Canvas by retaining a persistent `arboard::Clipboard` instance on `TanovaApp` (preventing Linux clipboard drop on scope exit), adding `internal_clipboard` fallback, and handling both `egui::Event::Copy`/`Paste` and `Key::C`/`Key::V`.
- **`Write` Plain Filename Sequence Auto-Numbering:** Fixed `Write` sequence rendering overwriting a single file when given a plain filename without `%04d` or `#` (e.g., `my_shot.exr`) by automatically numbering exported frames as `{base_stem}.{:04}.{ext}` (`my_shot.0001.exr`, `my_shot.0002.exr`, etc.) while keeping the clean filename in the knob.
- **`Read` Browser Frame-Range Synchronization:** Fixed `Read` file browser leaving default `first = 1, last = 100` untouched when selecting single frames or plain filenames by extending `detect_exr_sequence_in_dir` (supporting directories, stem-to-sequence lookup, and single-frame ranges) and updating `"file"`, `"first"`, and `"last"` together in an atomic `UndoableCommand::Batch`.

### Known Limitations (v0.4.0)

- **Cache Eviction:** No automatic LRU cache eviction during interactive scrubbing; long sessions may accumulate memory until the script is reloaded or a sequence render triggers a cache clear.
- **Color Management:** Lacks OpenColorIO (OCIO) integration. OpenEXR files are read, processed, and displayed in the Viewer as raw linear data without display transforms (e.g., sRGB/ACES).
- **Parameter Animation:** No keyframing or Curve Editor. Node knobs (e.g., `Grade` multiply) remain static across the timeline.
- **Graph Parallelism:** Branch evaluation in `Dag::pull` is currently single-threaded.

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
