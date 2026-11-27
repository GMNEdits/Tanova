# Changelog

All notable changes to Tanova are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased] - v0.7.0

**Phase 9 — Masking, Keying & Error UX (Milestone 28).**

### Added

- **Shared Viewer overlay module (`tanova-ui/src/overlay.rs`):** every 2D handle (`Transform` jack, `CornerPin2D` corners, `Ramp` endpoints) lives in one input path that runs strictly before pan/drag handling. `OverlayViewport` reproduces the Viewer coordinate helpers proxy-safely so all handles write full-resolution logical coordinates even at 1:2/1:4 proxy.
- **`OverlayState` / `OverlayResponse`:** `is_dragging()`, `consumed_pointer`, and `commands` shared across overlay kinds.
- **`emit_2d_knob_command`:** preserves untouched channels (`[x, y, prev[2], prev[3]]`) and auto-keys via `Batch([SetKnob, SetKnobCurve])` when the knob has an active curve.
- **`Ramp` Viewer handles:** `p0`/`p1` read from effective `KnobValue::Color`, `Color32::from_rgb(100,200,255)` 1.5 px guide line, 7.0 px `"P0"`/`"P1"` handles highlighted on hover/drag, full-resolution logical coords, pan suppressed, `request_repaint`.
- **`ViewerPanel`:** `show` signature and `is_dragging_overlay()` remain 100% backward-compatible; the overlay refactor keeps `Transform` (red X/green Y/crosshair/dashed box/smooth drag) and `CornerPin2D` (orange quad + numbered handles clamped to `[-fw..=2*fw, -fh..=2*fh]`, auto-key) behavior identical.

### Changed

- **Node Error Badges & Viewer banner:** `NodeErrorInfo { node, message, crashed }` and `KernelEvent::NodeErrors(Vec<NodeErrorInfo>)` emitted on every `render_viewer()` pass (empty vec clears), reporting the exact failing origin node (e.g. `Read #4` → `NodeId(4)`, never the Viewer `NodeId(3)`).
- **`GraphMirror::node_errors`:** updated on `NodeErrors`, pruned on `RemoveNode`, cleared on `clear()`/`NewScript`/`ReplaceDag`.
- **Canvas:** 2.0 px `Color32::from_rgb(235,75,70)` border plus `"CRASH"`/`"ERR"` pill on erroring nodes; the `Order::Tooltip` hover area shows the truncated error reason even without timing data.
- **`ViewerPanel::active_error`:** non-blocking dark-red banner (`Color32::from_rgba_unmultiplied(120,24,24,225)`, 1 px bright-red border, one truncated line, full text on hover) pinned to the top interior edge of the Viewer viewport.

### Fixed

- **`Blur` now filters alpha:** `BlurParams` carries `blur_alpha` (`u32` for 16-byte WGSL alignment), and `Blur` convolves all four premultiplied RGBA channels so antialiased cutouts stay halo-free; `Glow` sets it to `0` and keeps RGB-only bloom with untouched base alpha.
- **`Glow` zero-highlight `effect_only`:** no longer emits an all-zero RGBA tile before reading the mask input; it routes through the mask-aware composite with `bloom = [0,0,0]`, so mask `0` returns the input, mask `0.5` blends halfway toward black, and base alpha survives.
- **Grade-family unpremult policy:** `Grade`, `ColorCorrect` and `Saturation` use exact `effective_mix <= 0`/`>= 1` endpoints, a consistent `unpremult -> process -> premult` order, and force processed RGB to `[0,0,0]` at `alpha == 0` or non-finite alpha so `offset`/`lift`/`gain` can never leak RGB onto transparent pixels. `Saturation` no longer produces `inf`/`NaN` from `rgb / tiny_alpha`.
- **`Merge2` Multiply:** two super-blacks stay negative per component (`if a < 0.0 && b < 0.0 { a } else { a * b }`) on both CPU and WGSL, with exact mix `0`/`1` endpoints.
- **`Unpremult` tiny alpha:** removed the `|alpha| <= 1e-5` passthrough; only `alpha == 0` or non-finite alpha is a passthrough, and tiny signed factors (`±1e-6`) round-trip cleanly with `Premult`.
- **Missing-alpha consistency:** an absent `RgbaAlpha` plane is implicit `1.0` inside `raster.bbox` and `0.0` outside in `BlackOutside` (uniform, gathered, warped, defocused, premult and blur paths, plus the fast display converter); a present-but-sparse alpha tile stays `0.0`.
- **Resampler boundary taps:** `Transform`, `Reformat`, `CornerPin2D` and downsampling resolve each bilinear tap independently against the bbox/edge mode, so the outer `[-0.5..0.5]` half-pixel strip blends toward transparent black instead of being rejected or hard-clamped.
- **Mask uniformity:** `uniform_mask_value` checks an empty region / empty bbox intersection (`Some(0.0)`) and a region crossing the bbox boundary (`None`) before the `Some(1.0)` fallback, matching `mask_value`.
- **Viewer error banner text:** drawn at `banner_rect.left() + 10.0` with `Align2::LEFT_CENTER`, vertically centered against the banner.

## [0.6.5] - 2026-11-26

**Phase 8 — Project Format Versioning, Atomic Save & Crash Recovery (Milestone 25).**

### Added

- **Schema versioning:** `TANOVA_SCHEMA_VERSION = 1` exported from `tanova-kernel`/`tanova-app`. `export_to_tcl` now writes `# tanova_schema 1` on line 2 of every `.tnv` (line 1 stays Nuke's `version 15.0 v5` so Nuke keeps loading).
- **Parser validation:** `parse_nk_script`/`load_nk_into_dag` read `# tanova_schema N`; `v == 0 || v > TANOVA_SCHEMA_VERSION` is rejected with `Unsupported Tanova schema version {v} (max supported: {})`. Missing header = `None` and loads normally (legacy `.tnv` / external `.nk`).
- **Recovery provenance:** `# tanova_recovery_source <path>` is parsed into `ParsedScript.recovery_source`; a restored crash session re-binds to this path, never to the hidden `autosave.tnv`.
- **`ParsedScript` gains `schema_version: Option<u32>` and `recovery_source: Option<PathBuf>`.**
- **`tanova_kernel::atomic_write_string`**: pure-std atomic write — hidden temp file in the same directory as the target, `create_new`, `write_all`, `flush`, `sync_all`, drop handle, `rename`, best-effort `remove_file` of the temp on failure (avoids `EXDEV` cross-filesystem renames).
- **`KernelMessage::WriteAutoSave`** for idle background crash-recovery snapshots.
- **`tanova-app` recovery fields:** `recovery_dir` (`Some` only in live desktop startup; `None` in headless/tests), `last_user_action`, `last_auto_save`, `recovery_snapshot_path`, `pending_recovery`, `pending_post_save`, `save_in_flight`.
- **Centered ASCII "Crash Recovery" modal** (Restore / Discard) offered before file/unsaved modals.
- **`on_exit` shutdown hook** removes any pending recovery snapshot.

### Changed

- **Save is confirmed-only:** `execute_save_to_path` no longer sets `script_path` or `is_dirty = false`; only `ScriptSaved` applies them. Follow-up actions (`pending_post_save`) run only after `ScriptSaved`.
- **`SaveScript` uses `atomic_write_string`** (same-directory atomic write) instead of `std::fs::write`.
- **Failed `SaveScript`** emits `ScriptError` plus `DirtyStateChanged { is_dirty: undo_stack.is_dirty() }` and does NOT call `mark_saved()`, so a failed save leaves the project dirty.
- **`WriteAutoSave`** never calls `mark_saved()`, never changes `is_dirty`, never triggers `render_viewer()`.
- **Auto-save gating:** idle ≥ 3 s, min interval 30 s, not dragging, no modal/pending recovery, not already saving.
- Existing save tests now pump until `ScriptSaved` confirms clean state.

### Fixed

- `export_to_tcl` line 1 kept as `version 15.0 v5\n`; schema marker inserted on line 2.

**Phase 8 — Real GUI RAM Budget, Cooperative Eviction & 1080p Speed Gate (Milestone 26).**

### Added

- **Real GUI `MemoryGovernor` cap:** `recommended_gui_memory_cap_bytes()` auto-scales to `1.0 GiB` on systems with ≤8.5 GiB RAM (parsed from `/proc/meminfo` on Linux) and `2.0 GiB` otherwise; desktop `KernelHandle` boots with this cap instead of `usize::MAX`.
- **Cooperative cache eviction:** `Dag::pull` evicts the lowest-priority cached entry when a `MemoryExceeded` error occurs, retrying evaluation; the cache also pre-evicts above 75% of the governor cap so heavy nodes can allocate tiles.
- **`DiskCache::clear()`** drops cached `Arc<Raster2D>`s and releases their `MemoryPermit`s cleanly.
- **`KernelMessage::PurgeCache`** purges the DAG cache and any injected external cache, emitting `KernelEvent::MemoryStats`.
- **`KernelEvent::MemoryStats`** (`allocated_bytes`, `max_bytes`) emitted after renders, `NewScript`, `ReplaceDag`, `ExecuteWriteNode`, and `PurgeCache`.
- **`KernelEvent::PerfSnapshot(Vec<NodePerfSample>)`** emitted after each `render_viewer()` frame with per-node `(node, eval_us, from_cache)` timings.
- **`ExternalCachePurger`** lets `tanova-app` inject a disk-cache purge closure without `tanova-kernel` knowing about `tanova-io`.
- **`tanova-render/src/pool.rs`:** `worker_thread_count()` (`N - 2`, clamped `1..=6`) and `parallel_map_ordered()` using pure `std::thread::scope`, zero `unsafe`.
- **Parallel tile ops:** `defocus_raster`, `perspective_warp_raster`, `grade_raster`, `merge_raster` (CPU path), and `blur_padded_rgba` (heavy blurs) parallelize across tiles/rows with pixel-identical output.
- **Top-bar RAM meter:** compact pure-ASCII `RAM {:.0}/{:.0}M ({}%)` readout with hover tooltip and a `[Purge]` button sending `KernelMessage::PurgeCache`.
- **Per-node timing heat bar:** 2.5 px performance heat bar on node cards (green/amber/coral) plus tiny `1.2ms`/`C` text when zoomed ≥ 0.75; exposed via `GraphMirror::node_perf`.
- **`tanova-cli --bench-1080p`:** 1920×1080 speed benchmark gate for Grade, Blur, Defocus, CornerPin2D, Glow on a 40-tile CheckerBoard2 source under the 2.0 GiB cap.

### Changed

- **`MemoryGovernor`** is no longer `usize::MAX` in the GUI; the desktop app uses `recommended_gui_memory_cap_bytes()`.
- **`Dag::evaluate_pull`** now records per-node perf samples internally, retrievable via `Dag::take_perf_samples()`.
- **Repo hygiene & CI:** Fixed `Cargo.toml` repository URL (`tanova/tanova` → `GMNEdits/Tanova`), added root `LICENSE-MIT` and `LICENSE-APACHE`, added `deny.toml` (cargo-deny license/advisory checks), and added `.github/workflows/ci.yml` (fmt, clippy, test, headless checks).

**Phase 8 — Full-Res-Safe Viewer Proxy & Remaining Cleanup (Milestone 27).**

### Added

- **Viewer Proxy:** `1:1`, `1:2`, and `1:4` Viewer Proxy modes from the Viewer toolbar; `Ctrl+P` toggles between full and half resolution; an amber `PROXY 1:2` / `PROXY 1:4` badge identifies proxy mode; all spatial knobs, keyframes, radii, and Viewer handles remain in full-resolution logical units while the engine scales math via `proxy_scale`; `Write` always exports at `1:1` without changing the interactive proxy mode.
- **Proxy-aware evaluation:** Sources and spatial ops render fewer pixels without changing knobs or keyframes; `Read` retains full-resolution EXRs in the disk cache; DAG cache entries are distinct by scale (`ProxyScale` hashed into `OpHash`).
- **Direct-slice + parallel `to_top_left_rgba8_with_view`:** display conversion borrows tile pointers once per row, uses a uniform-run fast-path, and parallelizes across scanline chunks via `std::thread::scope` for frames ≥ 512×512 pixels — byte-for-byte identical to the naive reference across all display transforms and channel soloing modes.
- **`tanova-cli --bench-1080p`:** compares full and half-resolution CPU pulls against the same logical scene, plus a `Display RGBA8 (1920x1080)` row.

### Changed

- **Layout-only `MoveNode` render skip:** canvas node drags no longer trigger a Viewer render or texture upload when no frame changed; `ViewerPanel` dedupes repeated identical `Arc<Packet>` uploads.
- **Surgical downstream cache invalidation:** `AddNode` never invalidates existing nodes; `ConnectEdge`/`DisconnectEdge` invalidate only downstream; `SetKnob`/`SetKnobCurve`/`SetDisabled` invalidate only the edited node's downstream; `Batch` applies recursively — so spawning/wiring `Write` preserves upstream cache entries.
- **`1920×1080` knob fix:** `Constant`, `CheckerBoard2`, `Ramp`, `Reformat`, and `Crop` now honor explicitly-authored `format_width`/`format_height`/`box_max` knobs instead of hijacking them to the project format; project format only applies when callers supply it through `EvalContext` without authoring knobs.
- **`CornerPin2D` upstream format inheritance:** auto-chaining below a node stamps `to1`–`to4` to the upstream format; new `[Set to Input]` button in Properties snaps corners to the upstream format.
- **Top-bar `1280px` fit:** `RAM` and `Purge` restored to the right edge; variable-width project badge truncated with hover tooltip; `"Export status"` removed from the top bar; export confirmation pill (`Exported Nf`) shown on the bottom-left dock tab bar with full path on hover.
- **Unused `RamCache` removed:** `Dag.cache` is the single canonical cache abstraction.
- **Repo hygiene & CI:** Fixed `Cargo.toml` repository URL (`tanova/tanova` → `GMNEdits/Tanova`), added root `LICENSE-MIT` and `LICENSE-APACHE`, added `deny.toml` (cargo-deny license/advisory checks), added `.github/workflows/ci.yml` (fmt, clippy, test, headless checks).
- **`README.md`:** Rewritten as the `v0.6.5` landing page with highlights, 28-node table, architecture summary, Windows/Linux build guides, limitations, and license/trademark notice.

### Fixed

- **Milestone 25:** Nuke-safe `# tanova_schema 1` header on `.tnv` exports; same-folder pure-std atomic save (`create_new` + `rename`); confirmed-only dirty state so save failures keep the project dirty; idle background auto-save (`request_repaint_after`, 0% idle CPU); Crash Recovery modal (Restore / Discard) with clean-exit snapshot removal.
- **Milestone 26:** Real 1–2 GiB auto-scaled GUI `MemoryGovernor` cap; cooperative `Dag` cache eviction (evicts lowest-priority entry on `MemoryExceeded`, retries once); `DiskCache::clear()` releases `MemoryPermit`s; `PurgeCache` + live top-bar `RAM` meter & `Purge` button; safe `N - 2` `std::thread::scope` tile multi-threading; per-node timing heat bars + non-blocking hover tooltips (`egui::Area`, `interactable(false)`); direct-slice `extract_padded_tile` / `gather_grade_inputs` / `gather_merge_inputs`; `defocus_raster` clipped to `tile_bbox ∩ output_bbox` with prefix-sum rows restricted to the active band; `--bench-1080p` speed gate.

## [0.6.0] - 2026-11-22

**Phase 7 — Studio Compositing, Spatial Warping & Curve Editor (Milestones 23–24).** Ships an Interactive Curve Editor tab, Node Graph Minimap, projective 4-point homography `CornerPin2D`, optical circular/anamorphic disk `Defocus`, `Mirror`/`Ramp`/`Glow` cartridges (**29 total**), and live-GUI responsiveness fixes for continuous knob/handle dragging.

### Added

- **Milestone 23 — Interactive Curve Editor tab (`tanova-ui`, `tanova-app`):**
  - New `CurveEditorPanel`: a bottom-left tab (`[Node Graph]` | `[Curve Editor]`) that edits the `{curve ...}` keyframe curves of the selected node.
  - Toolbar with `[Fit]` (auto-scales the view to all keys with safe minimum spans), interpolation buttons `[Smooth]` / `[Linear]` / `[Constant]` for the selected key, and `[Del Key]` to remove it.
  - Left sidebar lists all animated `(knob_name, channel_index)` curves on the selected node, color-coded by channel (Red, Green, Blue, Alpha, Scalar Gold).
  - Interactive spline plot (`egui::Painter`): recessed dark background, labeled frame/value gridlines, orange playhead line, polyline curve traces, and clickable/draggable keyframe markers.
  - In-place keyframe dragging mutates the key's `frame`/`value` directly (no ghost keys left behind), re-sorts keys ascending by frame, emits `UndoableCommand::SetKnobCurve`, and requests a repaint each frame.
  - Middle-mouse drag pans; mouse wheel zooms around the cursor with safe minimum span clamps.
  - `TanovaApp::fold_event` extends the mirror-protection guard with `|| self.curve_editor.is_dragging_key()` so dragging a keyframe never snaps back from stale kernel echoes.

- **Milestone 23 — Node Graph Minimap (`tanova-ui`, `tanova-app`):**
  - `CanvasState.show_minimap` (default `false`) plus a pure-ASCII `[Map]` toggle in the Node Graph header bar next to `zoom XX%`.
  - When enabled, a `170 x 110` semi-transparent dark inset in the bottom-right corner draws category-colored node rectangles (orange outline for selected) and a translucent viewport indicator.
  - Input priority: pointer interaction inside the minimap is checked before canvas panning, node hit-testing, or box selection, so clicking/dragging inside the minimap pans `state.pan` to center the clicked world point without dragging nodes or starting box selection underneath.

- **Milestone 24 — `CornerPin2D` & `Defocus` cartridges + Phase 7 UI integration (`tanova-nodes`, `tanova-kernel`, `tanova-parse`, `tanova-ui`, `tanova-app`):**
  - New `"CornerPin2D"` cartridge: one input, four corner knobs (`to1`–`to4`) stored as `Color([x, y, 0, 0])`, plus an `invert` knob. Solves a `Mat3x3` homography from the input's source quad to the destination quad and warps via `perspective_warp_raster`. Zero-copy identity fast path, OOM-safe clamped output bbox, and `serialize_to_tcl` round-trip.
  - New `"Defocus"` cartridge: two inputs (main + optional `mask` on `PortId(1)`), `defocus` radius, `aspect`, and `mix` knobs. Blurs via `defocus_raster` with an anti-aliased elliptical disk kernel; non-RGBA AOVs pass through by `Arc` clone.
  - New `CornerPin2D` Viewer overlay: a translucent orange wireframe quad connecting four labeled handles, using the same forward/inverse image mapping as the pixel HUD. Handle presses take priority over panning; absolute pointer drags emit `SetKnob`, or an atomic knob/curve auto-key batch for animated corners at the playhead. `TanovaApp::fold_event` protects the mirror with `|| self.viewer.is_dragging_overlay()`.
  - `Ramp` added to the 0-input `GENERATORS` list so `.nk`/`.tnv` loading never auto-connects an upstream pipe.
  - `ProjectFormat` stamping: `Ramp`'s `p1` corner and `CornerPin2D`'s `to2`–`to4` corners are stamped onto the project format when freshly spawned at defaults.
  - Theme categories: `Ramp` → `Color`; `Mirror` / `CornerPin2D` → `Transform`; `Glow` / `Defocus` → `Filter`.
  - Spawn menus and port counts: all five Phase 7 cartridges appear in context menu / Tab search / `+ Add Node`. `Ramp` → 0 inputs, `Mirror` / `CornerPin2D` → 1 input, `Glow` / `Defocus` → 2 inputs.
  - Inspector controls: `p0` / `p1` (Ramp) and `to1`–`to4` (CornerPin2D) render as clean 2D X/Y `DragValue` rows (never RGBA pickers); `ramp_type` renders as a `ComboBox` with `linear` / `smooth` / `plinear`.
  - 29 total registered cartridges (27 + `CornerPin2D` + `Defocus`).

### Changed

- **`defocus_raster` replaced brute-force O(R²) disk convolution with exact O(R) row-prefix-sum convolution (`tanova-render`):** for each kernel row `dy`, the anti-aliased elliptical weight equals `1.0` across a single contiguous interior chord (collapsed to one prefix-sum subtraction per output pixel) plus at most a few feathered edge taps. `f64` prefix sums per padded row guarantee zero catastrophic cancellation on HDR pixels. 15x–20x speedup with mathematically identical bokeh (verified within `1e-5` against a naive O(R²) reference).

- **`perspective_warp_raster` builds a borrowed per-channel tile-pointer grid (`tanova-render`):** eliminates per-tap `HashMap` lookups by precomputing `Vec<Option<&Arc<TileBuffer>>>` indexed by `(ty - min_ty) * grid_w + (tx - min_tx)`. Bilinear sampling is now O(1) slice indexing: `TileBuffer::Uniform(v)` returns `v` immediately; `TileBuffer::Dense(buf)` indexes `buf[ly * TILE_SIZE + lx]` directly. Keeps pixel-identical output, uniform-tile fast path, and zero-copy non-RGBA AOV preservation.

- **`kernel_loop` anti-starvation guard for continuous `SetKnob` streams (`tanova-kernel`):** tracks `consecutive_preemptions: u32` (reset to `0` on `RenderOutcome::Completed` or empty/no viewer). On `Preempted`, drains `cmd_rx.try_recv()` through `coalesce_batch` so the retry renders the newest state instead of stale values. When `consecutive_preemptions >= 1`, runs `render_viewer` with a fresh unshared `RenderEpoch::new()` — guaranteeing at most one render is ever preempted before a live frame completes and emits `FrameReady`. `coalesce_batch` collapses consecutive `SetKnob` commands for the same `(node_id, name)` while preserving `UndoStack` undo behavior (`Ctrl+Z` restores the original pre-drag value).

- **Clean `KernelHandle::drop` / `shutdown` epoch bump (`tanova-kernel`):** bumps `self.render_epoch.bump()` before sending `KernelMessage::Shutdown` so any in-flight preemptible render aborts immediately instead of blocking `join()` on window close.

- **`DefocusNode::evaluate` non-finite knob guard (`tanova-nodes`):** returns the input unchanged when `radius`, `aspect`, or `mix` are non-finite before passing to `defocus_raster`.

- **Defocus UI `DragValue` clamping (`tanova-ui`):** `defocus` clamped to `0.0..=24.0` and `aspect` clamped to `0.25..=4.0` so dragging never produces non-finite or out-of-range values while keeping continuous live emission during `response.dragged()`.

### Fixed

- **CornerPin2D handle drag froze image until cursor paused (`tanova-kernel`, `tanova-render`):** continuous `SetKnob` streams from live handle dragging no longer starve `render_viewer`. The anti-starvation guard guarantees at most one render is abandoned before a live `FrameReady` completes, so the warped image updates smoothly on every drag frame even without releasing the mouse button.

- **Defocus `DragValue` updated only on mouse release (`tanova-render`, `tanova-app`, `tanova-nodes`):** `defocus_raster` previously performed O(TILE_PIXELS) `extract_padded_tile` allocations per tile inside the pixel loop when the alpha channel was non-uniform, hanging the worker thread for hundreds of milliseconds on a `256x256` checkerboard. The alpha padded tile is now pre-extracted once per tile (never per pixel), and the O(R) prefix-sum convolution completes in milliseconds, so continuous `defocus`/`aspect` knob drags produce a fresh blurred frame every 16 ms. The app tracks `in_flight_knob_edits > 0` / active drags and calls `ctx.request_repaint_after(16ms)` so `FrameReady` texture uploads land the instant the worker finishes.

- **Window close hung on `KernelHandle::drop` / `worker.join()` (`tanova-kernel`):** `KernelHandle::drop()` and `shutdown()` now bump `render_epoch` before sending `Shutdown`, so an active render aborts right away and the worker exits cleanly in milliseconds.

- **`defocus_raster` panicked on non-finite knob values (`tanova-render`, `tanova-nodes`):** guards against `!radius.is_finite()`, `!aspect.is_finite()`, `!mix.is_finite()` before clamping, returning the input unchanged for non-finite values.

### Known Limitations

- **No auto-save or crash recovery:** The Milestone 14 unsaved-changes guard prompts on close, but there is no periodic auto-save. A crash or kill loses all work since the last manual `Save`/`Save As`.
- **Sequence render has no resume / checkpoint:** The Milestone 16 `Write` sequence render reports `WriteFrameRendered` (`frame of first..last`) and `WriteCompleted`, but if a render is interrupted there is no checkpoint; re-running always restarts from frame `first` of `first..=last`.
- **Color management lacks OpenColorIO integration:** OpenEXR files are read, processed, and displayed in the Viewer as raw linear data without display transforms (e.g., sRGB/ACES).
- **Graph parallelism is single-threaded:** Branch evaluation in `Dag::pull` is currently single-threaded; large multi-branch graphs cannot parallelize across CPU cores.
- **File browser filters to supported extensions only:** File dialogs only surface `.tnv`, `.nk`, and `.exr` files (via `is_supported_script_file` / `is_supported_exr_file`). There is no "all files" mode and no recently-opened scripts list.

## [v0.5.0] - 2026-11-19

**Phase 6 — Production Compositing Engine (Milestones 20–22).** Ships 13 new cartridges (**24 total**), a bounded GreedyDual-Size DAG cache, viewer display transforms with `R`/`G`/`B`/`A` soloing, demand-driven `upstream_inputs` routing with temporal nodes, `TransformChain` matrix concatenation, and a live `{curve ...}` keyframe animation engine with a full-width timeline scrubber.

### Added

- **Milestone 20 — Viewer Display Transforms & Channel Soloing (`tanova-render`, `tanova-ui`):**
  - New `DisplayTransform` (`Linear`, `sRGB`, `Rec.709`, `ACES`) and `ViewerChannel` (`Rgba`, `R`, `G`, `B`, `A`) enums, re-exported from `tanova-render`.
  - `Raster2D::to_top_left_rgba8_with_view` applies gain/gamma to RGB, the selected display transform (sRGB OETF, BT.709 OETF, or ACES filmic + sRGB), then channel soloing.
  - `ViewerPanel` stores `pub display_transform` (default `Linear`) and `pub channel` (default `Rgba`); both are tracked in the texture dirty flag so changes upload immediately even while playback is paused.
  - Viewer toolbar: channel solo buttons `[RGBA] [R] [G] [B] [A]` (highlighting the active channel) and a `Display transform` combo box (`Linear`, `sRGB`, `Rec.709`, `ACES`).
  - Pointer-scoped `R`/`G`/`B`/`A` hotkeys toggle channel soloing only when the pointer is inside the viewer viewport and no widget owns the keyboard — never conflicting with canvas node spawning. `NodeCanvas::over_canvas` now requires the pointer over the canvas rect, so hovering the viewer never spawns a `Read`/`Grade`/`Blur` node.

- **Milestone 20 — Core Color/Alpha Cartridges & UI Integration (`tanova-nodes`, `tanova-ui`):**
  - New `"Premult"` cartridge: premultiplies RGB by a selected alpha plane (`rgba.alpha` / `depth.Z` / `none`) with a `mix` knob; `PropertiesPanel` renders a `ComboBox` for its `alpha` knob.
  - New `"Unpremult"` cartridge: divides RGB by a selected alpha plane (`rgba.alpha` / `depth.Z` / `none`) with a `mix` knob; `PropertiesPanel` renders a `ComboBox` for its `alpha` knob.
  - New `"Invert"` cartridge: inverts selected channels (`rgb` / `rgba` / `alpha` / `none`) with a `mix` knob; `PropertiesPanel` renders a `ComboBox` for its `channels` knob.
  - New `"ColorCorrect"` cartridge: `saturation`, `contrast`, `gamma`, `gain`, `offset`, `unpremult`, and `mix` knobs.
  - All four cartridges map to `NodeCategory::Color`, so they render the category header band color across `Classic`, `Slate`, and `Midnight` themes; they appear in the `+ Add Node` dropdown, right-click context menu, and `Tab` quick-search.

- **Milestone 21 — Demand-Driven `upstream_inputs` Routing, Temporal Hooks & Pipe Splicing (`tanova-kernel`, `tanova-ui`):**
  - New `NodeTrait::upstream_inputs` default method: a cartridge declares exactly which connected ports to pull and the `EvalContext` for each. The default still pulls every connected port with the caller's context, so existing cartridges are unchanged.
  - `Dag::pull` now honors `upstream_inputs` (and bypassed nodes pull only `PortId(0)`), assembling per-port inputs from just the requested branches, so an unselected `Switch` branch is never evaluated and never invalidates the active branch.
  - `CacheEntry` records its `node_id`, so multi-frame `TimeOffset` entries can be invalidated by owner.
  - `Switch` pin geometry: input `0` on the top edge, input `1` on the left `A` edge, with `0` / `1` labels; the cartridge accepts two inputs.
  - Canvas `Shift+X` swaps `PortId(0)` and `PortId(1)` on every selected multi-input node as one `Batch`, so a single `Ctrl+Z` swaps the pipes back.
  - Dragging a single unconnected node onto a Bezier wire splices it into that pipe: the release emits one `Batch` of `MoveNode` + detach + two connects, so one `Ctrl+Z` unsplices and returns the node.

- **Milestone 21 — `Switch`, `TimeOffset` & `FrameHold` Cartridges (`tanova-nodes`, `tanova-ui`):**
  - New `"Switch"` cartridge: two inputs (`0` / `1`) and a `which` knob; `upstream_inputs` requests only the selected port, so the unselected branch is never pulled, evaluated, or hashed. `evaluate` is a pure pass-through of the selected packet.
  - New `"TimeOffset"` cartridge: `time_offset` rewrites the upstream `EvalContext` time to `t - offset`, so a positive offset delays the clip with zero pixel work in `evaluate`.
  - New `"FrameHold"` cartridge: `first_frame` always requests that single upstream frame, freezing the clip for downstream nodes.
  - `Switch` maps to `NodeCategory::Merge`; `TimeOffset` / `FrameHold` map to `NodeCategory::Filter`; all three appear in the `+ Add Node` menu, right-click context menu, and `Tab` quick-search. `Saturation` and `Clamp` now report their optional mask input.

- **Milestone 21 — `Crop`, `Clamp` & `Saturation` Cartridges (`tanova-nodes`, `tanova-ui`):** merged from `codex-nodes`.
  - New `"Clamp"` cartridge: per-channel `minimum` / `maximum` with independent enables and a masked `mix`.
  - New `"Saturation"` cartridge: `saturation`, luminance `mode` (`Rec709` / `Average` / `Maximum`), `unpremult`, and a masked `mix`.
  - New `"Crop"` cartridge: `box_min` / `box_max`, `reformat`, and `intersect`, sharing interior AOV tiles with the input.
  - `Clamp` and `Saturation` map to `NodeCategory::Color` and render their mask port; `Crop` maps to `NodeCategory::Transform`. The properties bin edits `box_min` / `box_max` as clean `X` / `Y` coordinates (never RGBA swatches) and offers a `mode` combo box for `Saturation`.

- **Milestone 22 — Nuke `{curve ...}` Keyframe Parser (`tanova-parse`):** merged from `kilo-parse`.
  - New `AnimationCurve` / `Keyframe` / `InterpMode` types with Nuke-compatible smooth (Hermite), linear, and constant (hold) segments, plus frame-sorted evaluation and TCL serialization.
  - `NkNode` collects `curves: HashMap<String, Vec<AnimationCurve>>`; an animated knob accepts both a single `{curve ...}` channel and a nested `{{curve ...} {curve ...} 1 1}` multi-channel tuple, coercing the frame-1.0 value into the schema knob type.

- **Milestone 22 — Live Keyframe Animation Engine (`tanova-kernel`, `tanova-parse`, `tanova-ui`, `tanova-app`):**
  - `DagNode` stores `curves: BTreeMap<String, Vec<AnimationCurve>>` (one curve per channel, deterministic order). `DagNode::evaluated_knobs_at(frame)` resolves Float/Int from channel 0, Color per channel with a channel-0/base fallback, and leaves statically stored knobs untouched.
  - `Dag::pull` now injects the frame-resolved knobs into the node's `EvalContext`, and `hash_node` hashes those resolved values, so scrubbing a `{curve}` produces a distinct cache entry per frame.
  - `export_to_tcl` writes `{curve ...}` for animated knobs (nested tuples for multi-channel colors) and the plain static scalar otherwise; `load_nk_into_dag` copies parsed curves onto the `DagNode`, so animation survives a save/load round trip.
  - New `UndoableCommand::SetKnobCurve { node, name, curves, previous }` with an exact inverse; applying it updates the node curves and invalidates downstream cache entries.
  - Properties bin `[Key]` controls: blue `[K]` when a key sits on the current frame, teal `[+K]` when the knob is animated but interpolated, and a neutral `[+K]` when it is static. Clicking toggles a key at the current frame; editing an animated knob auto-keys the new value.
  - Timeline draws gold keyframe ticks on the frame slider and adds `[< Key]` / `[Key >]` buttons that jump the playhead to the previous/next keyframe (selected node's keys, or every node's keys when nothing is selected).
  - `GraphMirror` mirrors curve state from `KernelEvent`s and exposes `keyframe_times(selected)`; the app syncs `PropertiesPanel::current_frame` from the timeline each frame.

- **Milestone 22 — Full-Width Timeline Scrubber, Live Animated Properties & Merged Cartridges (`tanova-kernel`, `tanova-parse`, `tanova-nodes`, `tanova-ui`, `tanova-app`):**
  - New full-width custom scrubber track: recessed rail, 5/20-frame ruler ticks with frame numbers, gold keyframe ticks, and an orange playhead. The playhead and keyframe ticks share one `frame_to_x` mapping, so a key on the current frame lines up exactly.
  - Added a compact frame `DragValue` and kept `[< Key]` / `[Key >]` navigation; clicking or dragging anywhere on the rail scrubs the playhead and emits `SetTime`. The `TIMELINE_HEIGHT` reservation is unchanged, so the viewer panel keeps avoiding the egui 0.37 panel-growth feedback loop.
  - `PropertiesPanel` displays the frame-resolved value of every Float/Int/Color knob via `effective_knob_value`; `mix` renders as a `0.0..=1.0` slider.
  - Editing an animated knob auto-keys the new value and emits one `Batch([SetKnob, SetKnobCurve])`, which the kernel coalesces inside its 400 ms window so a drag stays a single undo step. Consecutive `SetKnobCurve` and knob `Batch` edits also coalesce.
  - `Dag` tracks the last evaluated frame; applying `SetKnobCurve` syncs the knob's static fallback to that frame, and the kernel invalidates downstream caches and re-renders for `SetKnobCurve` and batches containing it.
  - The properties banner stays pinned above a `ScrollArea::vertical().auto_shrink([false, false])` around the knob grid, so the node identity and bypass control never scroll away.
  - Wired the merged `Reformat`, `CheckerBoard2`, and `Shuffle` cartridges into the canvas, category colors, spawn menus, and properties bin (`resize` and per-channel selector combo boxes); `CheckerBoard2` is a zero-input generator.
  - The canvas bottom-left shortcut hint now sits on a framed dark pill so it no longer collides with node labels.

- **Thirteen new cartridges — 24 total (`tanova-nodes`):** `Premult`, `Unpremult`, `Invert`, `ColorCorrect`, `Switch`, `TimeOffset`, `FrameHold`, `Clamp`, `Saturation`, `Crop`, `Reformat`, `CheckerBoard2`, and `Shuffle` join the existing 11 cartridges for **24 self-registering nodes** in total.

- **Two-pass separable GPU/CPU Gaussian `Blur` (`tanova-nodes`, `tanova-render`):** `blur.wgsl` and the CPU fallback run a horizontal 1D pass into an intermediate buffer followed by a vertical 1D pass, so the cost is `2 * (2 * radius + 1)` samples per pixel instead of the full 2D kernel. Both paths stay bit-compatible with the previous 2D Gaussian (`separable_blur_matches_2d_gaussian_and_gpu_parity`), and the storage-buffer path still handles aprons larger than 3 MB.

- **Test isolation and regression coverage:**
  - `GPU_TEST_LOCK` serializes every GPU-backed app test through `test_gpu()`, so parallel `cargo test` runs no longer race on the shared `wgpu` device/adapter.
  - `animated_slider_scrubbing_timeline_emits_zero_phantom_keys` (`tanova-ui`): scrubs frames `1..=30` across an animated `mix` knob with no pointer input and asserts zero `Execute` messages.
  - `mix_slider_drag_emits_continuous_monotonic_updates_before_release` (`tanova-ui`): presses the `mix` rail inside a narrow, scrollable properties column, then walks the pointer right over five frames without releasing the button (drifting above/below the rail) and asserts every intermediate frame emits a strictly newer value that tracks the pointer.
  - `optimistic_batch_updates_both_knob_and_curve_without_snapback` and `stale_knob_echo_cannot_roll_the_mirror_back_while_edits_are_in_flight` (`tanova-app`): cover optimistic `Batch([SetKnob, SetKnobCurve])` mirroring and stale-echo rejection.
  - `batch_drain_coalesces_consecutive_auto_key_batches` (`tanova-kernel`): three auto-key batches collapse to one message carrying the newest value and curves.
  - `set_time_is_only_sent_when_the_whole_frame_changes` and `every_theme_paints_a_visible_slider_rail_and_trailing_fill` (`tanova-ui`): cover `SetTime` deduplication and per-theme rail/trailing-fill contrast.

### Changed

- **Bounded GreedyDual-Size cache replaces the unbounded DAG RAM map (`tanova-kernel`):**
  - The cache is now bounded and cost- and recency-aware, keyed by `OpHash` (`max_cache_entries` defaults to 64, `global_clock` drives recency).
  - Cache entries track `cost_us` (evaluation time measured with `std::time::Instant` around `evaluate_with_airbag`, floored at 1 us) and `priority` (`global_clock + cost_us` on insert and on every hit).
  - `Dag::pull` refreshes a hit entry's priority so actively scrubbed nodes stay hot; on insert, when the cache is full, the lowest-priority entry is evicted and `global_clock` advances to the victim's priority.
  - New `Dag::set_max_cache_entries`, `Dag::cache_len`, and `Dag::clear_cache` (which also resets `global_clock`); `invalidate_downstream` behavior is unchanged.
- **`OpHash` hashes only the pulled inputs (`tanova-kernel`):** the op hash covers only the ports selected by `upstream_inputs` plus the node's frame time, so an unselected `Switch` branch cannot invalidate the active branch.
- **Downstream invalidation is per-node (`tanova-kernel`):** `invalidate_downstream` now drops every cache entry owned by a downstream node (not just the last hash), keeping multi-frame `TimeOffset` entries from serving stale frames.
- **`TransformNode` concatenates through `TransformChain` (`tanova-nodes`):** contiguous transforms fold into one `Mat3x3` chain and resample once from the original source, returning it untouched when the combined matrix is identity, which preserves sharpness across scale-down/scale-up pairs.
- **`tanova-nodes` module surface (`tanova-nodes`):** `tanova-nodes/src/lib.rs` is now a pure module/re-export surface; the shared `map_color_tiles` helper moved into `tanova-nodes/src/util.rs`.
- **Curve types moved into the kernel (`tanova-kernel`, `tanova-parse`):** `AnimationCurve`, `Keyframe`, and `InterpMode` now live in `tanova-kernel::types` and are re-exported from the kernel; `tanova-parse::curve` aliases the kernel types and keeps only the Nuke parser.
- **Display conversion delegates (`tanova-render`):** `to_top_left_rgba8` and `to_top_left_rgba8_with_display` delegate to `to_top_left_rgba8_with_view` with `Linear` / `Rgba` and remain byte-identical.
- **Timeline widget (`tanova-ui`):** the 100 px `egui::Slider` that collapsed the 100-frame timeline into a tiny stub is replaced by the full-width custom scrubber, with `TIMELINE_HEIGHT` unchanged.
- **Animated knob values (`tanova-ui`):** the properties bin now shows the frame-resolved value of animated knobs instead of the static base value, and `mix` uses a `f64` slider end-to-end.

### Fixed

- **Phantom auto-keyframes while scrubbing or playing (`tanova-ui`):** an animated `mix` slider no longer authors a key per frame. The slider now uses `f64` end-to-end (the old `f32` round-trip plus `fixed_decimals(2)` re-rounded the bound value every frame, so `response.changed()` fired on every interpolated value), and knob edits are only emitted when the widget saw real pointer/keyboard input **and** the value actually differs from the frame-resolved value. `[Key]`/`+K` toggles and non-animated knobs are unchanged, and typing into a knob still commits normally.

- **Invisible slider rails and mid-drag snap-back (`tanova-ui`, `tanova-app`):** every theme palette now carries a `widget_bg` track color with clear luminance contrast against `panel_bg` (previously `widgets.inactive.bg_fill` was set to the panel color, so the rail vanished and only a floating thumb showed), hover/press steps brighten from the same track color, and `visuals.slider_trailing_fill` lights the filled part of the rail. The properties grid reserves `140px` of slider travel, requests a repaint while a knob is dragged, and the app drops stale kernel `Applied` echoes for knob commands while a drag is in flight so the thumb never snaps back under the cursor.

- **Laggy timeline scrubbing and playback (`tanova-ui`, `tanova-app`, `tanova-kernel`):** `SetTime` is only sent when the whole frame changed (deduplicated in `ViewerPanel` and again in `TanovaApp::dispatch`), consecutive `SetTime` messages are coalesced by the kernel's batch drain so only the newest requested frame renders, and the UI requests repaints while the timeline is playing or being scrubbed instead of waiting on pointer motion.

- **Mix slider froze mid-drag and jumped to the release position (`tanova-ui`, `tanova-app`, `tanova-kernel`):** the properties bin disables drag-to-scroll on its `ScrollArea` (a horizontal drag could be interpreted as a scroll gesture), pins the `mix` widget to a stable id, and fixes the readout width with `custom_formatter("{value:.2}")` instead of `fixed_decimals` (which mutated the bound value). The kernel now keeps draining and applying messages **until its queue is empty** and then renders once, instead of paying one GPU render per drag frame, and it retires a burst of auto-key `Batch([SetKnob, SetKnobCurve])` messages in the same drain. `TanovaApp::dispatch` bumps the shared `render_epoch` for knob and time updates so a render already running for an older value is abandoned, and `render_viewer` keeps the last good image when a render is preempted instead of blanking the viewer. The app additionally tracks `in_flight_knob_edits` and refuses to fold a knob `Applied` echo back into the mirror while edits are outstanding or a drag is live, so an intermediate drag step can never roll the thumb backward.

- **Viewer `Display transform` combo box showed a blank closed button (`tanova-ui`):** the combo box now sets `.selected_text(...)` from a `DisplayTransform` label helper, so the active transform (`Linear`, `sRGB`, `Rec.709`, or `ACES`) is always visible without opening the dropdown.

- **CornerPin2D handle drag froze the image until cursor paused (`tanova-kernel`, `tanova-render`):** continuous `SetKnob` streams from live handle dragging no longer starve `render_viewer`. `kernel_loop` tracks `consecutive_preemptions`, drains `cmd_rx.try_recv()` through `coalesce_batch` before each retry so the newest state renders first, and switches to a fresh unshared `RenderEpoch` after one preemption — guaranteeing at most one render is abandoned before a live `FrameReady` completes. `perspective_warp_raster` now builds a borrowed per-channel tile-pointer grid (`Vec<Option<&TileBuffer>>`) for O(1) bilinear sampling instead of per-tap `HashMap` lookups, keeping zero governor bytes on identity and zero-copy non-RGBA AOVs.

- **Defocus DragValue updated only on mouse release (`tanova-render`, `tanova-app`, `tanova-nodes`):** `defocus_raster` now uses an exact O(R) `f64` row-prefix-sum disk convolution (interior chord collapsed to one prefix-sum subtraction per row plus feathered edge taps), a 15x–20x speedup over brute-force O(R²) taps, so continuous `defocus`/`aspect` knob drags produce a fresh blurred frame every 16 ms. The kernel tracks `in_flight_knob_edits > 0` / active drags and calls `ctx.request_repaint_after(16ms)` so `FrameReady` uploads land the instant the worker finishes. `KernelHandle::drop()` bumps `render_epoch` before `Shutdown` so the worker aborts and exits cleanly on window close.

## [v0.4.5] - 2026-11-16

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
  - New `tanova-ui::theme` module with `StudioTheme` (`Classic`, `Slate`, `Midnight`), `NodeCategory` (`Io`, `Color`, `Merge`, `Filter`, `Transform`, `Viewer`, `Other`), `ThemePalette`, `node_category()`, `palette()`, and cache-safe `apply_theme_visuals()`.
  - Two-tone node cards: category-colored header band with top-only rounding, dark crimson crashed fill, dimmed disabled state, drop shadow, and `card_corner_radius` per theme (`2` Classic, `3` Slate, `7` Midnight).
  - Themed canvas background and grid: line grid for Classic/Slate, dot grid for Midnight; spacing clamped to `.max(18.0)` at low zoom.
  - Role-colored input pins (`pin_b` / `pin_a` / `pin_m` by `PortSide`) and matching wire colors (`wire_default` for top/B, `pin_a` for left/A, `pin_m` for right/mask).
  - `NodeCanvasState.theme` persists across `execute_new_script()` / script reloads.
  - Top-bar segmented theme selector: `Theme: [Classic] [Slate] [Midnight]`.

- **Milestone 18 Part 2 — Brand-Free Theme Naming, Visual Fixes & Two-Column Inspector (`tanova-ui`, `tanova-app`):**
  - Renamed `StudioTheme` variants to brand-free names: `Classic` (Nuke muscle memory), `Slate` (Natron-inspired), `Midnight` (Fusion-inspired).
  - Distinct visual silhouettes: `Classic` uses full-body category-fill cards with a warm grid; `Slate` uses two-tone header cards on a cool high-contrast grid; `Midnight` uses rounded capsule cards on a deep gunmetal dot grid.
  - Port label wire-clearance fix: `B`/`in` labels shift left of the vertical stem; `A` and `M` labels sit below their horizontal stems so incoming wires no longer slice through them.
  - Theme-coordinated active viewer ring and `VIEWER 1`/`2` badge using `viewer_badge_accent` per theme.
  - Two-Column `PropertiesPanel` Inspector: category header banner with bypass toggle, aligned `egui::Grid` with labeled columns for all knob types (Float, Int, Bool, Text, ComboBox, Color, Vec2).
  - Top-bar labels updated: `Viewer Mode: [Viewer Node] [Direct Target]`, `Theme: [Classic] [Slate] [Midnight]`.
  - Added tests: `classic_slate_and_midnight_palettes_have_distinct_card_and_grid_styles` and `properties_panel_renders_two_column_grid_and_bypass_toggle_across_themes`.

- **Milestone 19 — Graph Organization Cartridges & Canvas Containers (`tanova-nodes`, `tanova-parse`, `tanova-ui`, `tanova-app`):**
  - New `"Dot"` cartridge: a 1-in/1-out routing elbow whose `evaluate()` is a zero-copy passthrough of `PortId(0)`, with an empty `label` knob.
  - New `"BackdropNode"` cartridge: a 0-input canvas container with `label`, `color` (`slate`/`blue`/`teal`/`amber`/`purple`/`crimson`/`green`), `bdwidth` (default `320`), and `bdheight` (default `200`) knobs; its `evaluate()` returns an empty packet.
  - `.nk` / `.tnv` parser normalizes Nuke's plain `Backdrop` to `BackdropNode` and never pops an input from, nor pushes the backdrop onto, the TCL stack, so a saved backdrop can never corrupt downstream wiring on reload.
  - Canvas geometry helpers `DOT_SIZE` (`22x22`), `NODE_SIZE`, `is_dot_node()`, `is_backdrop_node()`, and `node_world_size()`; screen rects, wire endpoints, framing, and box selection all honor the per-node size.
  - `.` single-key shortcut spawns a `Dot`; `Dot` and `BackdropNode` also appear in the `Tab` search, right-click `Add node` menu, and canvas hint (`R/W/C/G/B/T/M/.: add node`).
  - Center-aligned vertical auto-chaining keeps `22px` Dots and `168px` cards on one dead-straight spine, and preserves the historical `90px` pitch for standard cards.
  - `BackdropNode` spawning wraps the current selection in one atomic `Batch` (`AddNode` + `bdwidth`/`bdheight` `SetKnob`s) with `36px`/`44px` padding, and falls back to a default `320x200` backdrop at the cursor when nothing is selected.
  - Three-layer z-order (`Backdrops` -> `Wires` -> `Nodes & Dots`): tinted semi-transparent backdrop bodies, a solid category-colored header bar with the label, and a diagonal bottom-right resize grip.
  - Backdrop header group-drag moves every enclosed non-backdrop node in one coalesced undo step; corner resize commits a single `Batch` of two `SetKnob`s so `Ctrl+Z` restores the previous size in one press. Non-backdrop nodes and ports are hit-tested first, so a card inside a backdrop stays clickable.
  - `PropertiesPanel` renders a `color` preset `ComboBox` for `BackdropNode` and hides the `[Bypass (D)]` toggle for `Dot` and `BackdropNode`.
  - Added tests: `dot_node_spawns_via_period_key_centers_on_spine_and_passes_packet_through`, `backdrop_wraps_selected_nodes_moves_enclosed_nodes_and_round_trips_tcl`, and `backdrop_between_wired_nodes_never_corrupts_the_stack`.

### Fixed

- **Milestone 19 live-GUI interaction fixes (`tanova-ui`, `tanova-app`):**
  - `Dot` grew to `22x22` and its circular body is now a drag handle: a pointer-down inside the body moves the node, and only the top/bottom stem tips can start a wire drag, so a body click can no longer spawn a stray wire.
  - Spawning `.` (`Dot`) or `B` (`Blur`) below an already-wired node now performs a true pipe splice (`up -> new -> downstream`) in one atomic `Batch` and pushes the downstream column down, instead of leaving a dangling wire or nudging the new node sideways.
  - `BackdropNode` spawning wraps the selection with `36px`/`44px` padding and never receives the `x += 140` overlap nudge; right-click and `Tab` spawns preserve the current selection.
- Fixed the live-GUI bug where `BackdropNode` spawned at the cursor instead of wrapping box-selected nodes: all four spawn paths (`Tab` search Enter + mouse-click row, right-click context menu, `+ Add Node` top-bar dropdown, single-key shortcuts) now route through `spawn_node_message`; `TabSearchState` captures `selected_snapshot` / `primary_snapshot` when Tab opens and restores them unconditionally before every popup spawn; canvas pointer press/box-selection/deselect is fully disabled while any spawn UI (Tab popup, context menu, node menu) is open, so a popup click can never wipe `selected_nodes`; right-click on empty canvas never clears the selection; `Shift+B` and plain `Z` spawn `BackdropNode` alongside the existing `R/W/C/G/B/T/M/./Z` keys; `spawn_node_message` Case 0 computes the backdrop bounds from `GraphMirror` world coordinates (`x = min_x - 36`, `y = min_y - 44`, `bdwidth = ((max_x - min_x) + 72).max(200)`, `bdheight = ((max_y - min_y) + 76).max(140)`) using `mirror.node()` directly and keeping all non-backdrop nodes including `Dot` and `Viewer`; `GraphMirror::apply_command` `SetKnob` uses `.insert` so `bdwidth`/`bdheight` are stored even when the `BackdropNode` cartridge isn't linked, and `Batch` applies recursively.
- Added `+ Add Node` top-bar toolbar dropdown (`ui.menu_button`) routing through `spawn_node_message` using `tanova_ui::canvas::spawnable_kinds`.
- Added multi-frame regression tests `tab_search_enter_wraps_box_selected_nodes_in_backdrop_not_at_cursor` and `tab_search_mouse_click_on_backdrop_row_wraps_selected_nodes_without_clearing_selection` driving `TanovaApp::draw` / `NodeCanvas::show` frames (not isolated `spawn_node_message` calls) to catch frame-order selection loss.
  - Backdrop group drag captures its enclosed nodes exactly once at mouse-down, so sweeping an empty backdrop across other cards can no longer capture or pile them together; box selection stays off during node drags and backdrop resizes.
  - Added tests: `dot_body_drag_moves_dot_without_starting_wire_drag`, `spawning_dot_and_blur_on_wired_constant_splices_pipe_and_pushes_downstream_grade`, and `dragging_empty_backdrop_across_nodes_never_captures_or_moves_them`.

### Changed

- Canvas bottom hint now uses printable ASCII separators instead of Unicode bullets, keeping the Linux `egui` glyph atlas clean.

### Known Limitations (Phase 4 carryover)

- **Duplicate / paste use a fixed canvas offset:** `Ctrl+D` duplicate and each `Ctrl+V` paste cascade shift every pasted node by a hard-coded `(40, 40)` offset (`paste_tcl_snippet` adds `offset.0`/`offset.1` to every node's `x`/`y`). There is no way to paste at the cursor, configure the cascade offset or direction, or disable cascading, so repeated pastes always march down-right at a fixed 40pt pitch.
- **No auto-save or crash recovery:** The Milestone 14 unsaved-changes guard prompts on close, but there is no periodic auto-save. A crash or kill loses all work since the last manual `Save`/`Save As`.
- **Sequence render has no resume / checkpoint:** The Milestone 16 `Write` sequence render reports `WriteFrameRendered` (`frame of first..last`) and `WriteCompleted`, but if a render is interrupted there is no checkpoint; re-running always restarts from frame `first` of `first..=last`.
- **File browser filters to supported extensions only:** Milestone 14/16 file dialogs only surface `.tnv`, `.nk`, and `.exr` files (via `is_supported_script_file` / `is_supported_exr_file`). There is no "all files" mode and no recently-opened scripts list.

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

- **Cache Eviction:** No automatic LRU cache eviction during interactive scrubbing; long sessions may accumulate memory until the script is reloaded or a sequence render triggers a cache clear. *(Resolved in v0.5.0/v0.6.5 with bounded GreedyDual-Size caching and governor-driven eviction.)*
- **Color Management:** Lacks OpenColorIO (OCIO) integration. OpenEXR files are read, processed, and displayed in the Viewer as raw linear data without display transforms (e.g., sRGB/ACES).
- **Parameter Animation:** No keyframing or Curve Editor. Node knobs (e.g., `Grade` multiply) remain static across the timeline. *(Resolved in v0.5.0/v0.6.0 with keyframes and the Curve Editor.)*
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
