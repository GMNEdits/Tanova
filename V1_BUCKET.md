# # Tanova — Living V1 Bucket \& Architecture Plan

\> \*\*Purpose:\*\* A flexible master checklist of features, architectural upgrades, performance budgets, and known traps between \*\*\`v0.6.0\`\*\* and a rock-solid \*\*\`v1.0.0\`\*\*.

\>

\> \*\*How to use this file:\*\* Do \*\*not\*\* lock future version numbers in advance. After each release (\`v0.6.5\`, \`v0.7.0\`, \`v0.7.5\`, ...), review this file, log new discoveries in the \*\*Parking Lot\*\*, and pull the next highest-value slice into \`ROADMAP.md\`.

\---

\## 1. Product Identity \& Release Philosophy

\- \*\*92–93% Nuke Familiarity + 7–8% Modern Improvements:\*\* Keep node names, keyboard shortcuts, workflow habits, and linear math familiar to Nuke artists, while smoothing out friction points where a cleaner design makes daily compositing faster.

\- \*\*Built for Solo Artists, Freelancers \& Students:\*\* Runs smoothly on modest laptops without studio GPUs, high RAM, or gigabytes of bundled LUT/plugin bloat.

\- \*\*Rolling \`.5\` Release Cadence:\*\* Ship small, fully verified increments (\`v0.6.0\` -\> \`v0.6.5\` -\> \`v0.7.0\` -\> \`v0.7.5\` -\> ... -\> \`v1.0.0\`). Every \`.5\` release must pass automated gates and live GUI verification before tagging.

\- \*\*Lean Core Principle:\*\* Core app stays pure compiled Rust (\~20–40 MB binary, \<1s startup). Heavy studio assets (external \`.ocio\` configs, third-party \`.ofx\` bundles) remain optional external files on the user's disk.

\---

\## 2. Baseline Hardware \& "Speed Contract"

All budgets are measured on our daily reference laptop with the OS and browser tabs open beside Tanova:

\- \*\*Reference Machine:\*\* Intel Core i5-10310U (4 physical cores / 8 threads), 16 GiB RAM, SSD, integrated graphics.

\| Metric \| Target Budget \| Guardrail \|

\| :--- \| :--- \| :--- \|

\| \*\*Core Binary Size\*\* \| \~20 MB – 40 MB \| No bundled Qt, Python runtime, browser engine, or giant LUT packs \|

\| \*\*RAM (Idle / Small Comp)\*\* \| \~80 MB – 150 MB \| Lightweight startup with no heavy background frameworks \|

\| \*\*RAM Ceiling (1080p Comp)\*\* \| 2.0 GiB default (auto-lowered on 8 GiB PCs) \| Enforced by \`MemoryGovernor\`; zero SSD swap thrashing \|

\| \*\*GPU / VRAM Usage\*\* \| \~30 MB – 80 MB (integrated graphics) \| Standard display + UI only; no dedicated studio GPU required \|

\| \*\*CPU Worker Cap\*\* \| Max 4–6 threads (\`total - 2\`) \| Always leave \*\*2 threads free\*\* so OS, mouse, and browser stay smooth \|

\| \*\*Idle CPU Usage\*\* \| \~0% when not dragging/playing \| Quiet laptop fans and low battery draw while idle \|

\| \*\*Interactive Drag (1080p, \`1:2\` Proxy)\*\* \| \< 25–33 ms / frame \| Smooth live knob, handle, and shape editing on HD footage \|

\| \*\*Interactive Drag (1080p, \`1:1\` Full)\*\* \| \< 50–80 ms / frame \| Usable full-res tweaking on typical stacks \|

\| \*\*Auto-Save Write Time\*\* \| \< 20 ms (background SSD write) \| Never stutter the UI thread during edits \|

\| \*\*\`Write\` Sequence RAM\*\* \| Flat across 500+ frames \| Free each frame immediately after atomic write to disk \|

\---

\## 3. Workflow \& Quality Gates (Every Ticket \& Release)

1\. \*\*Challenge Before Coding:\*\* Review every plan for architectural walls, undo traps, and memory spikes before writing code.

2\. \*\*Standard 5 Local/CI Gates (Zero Failures Allowed):\*\*

   \- \`cargo fmt --check\`

   \- \`cargo clippy --workspace --all-targets -- -D warnings\`

   \- \`cargo test --workspace\`

   \- \`cargo run -p tanova-cli -- --check-headless\`

   \- \`cargo run -p tanova-app -- --check-headless\`

3\. \*\*Measure, Don't Guess (Profiler + 1080p Benchmark Gate):\*\* Add automated 1080p render timing benchmarks and profile slow hotspots directly whenever performance slips, catching O(R²) loops or hidden allocations before live testing.

4\. \*\*Golden Pixel-Diff Checks:\*\* Maintain reference-image pixel-diff regression tests so speed optimizations and refactors never silently alter rendered output.

5\. \*\*Fresh-Eyes Review (Heavy Milestones):\*\* On high-risk changes (multi-threading, \`Roto\`, \`OCIO\`, \`OpenFX\`), run a read-only audit from a second worktree/agent hunting for hangs, memory leaks, and Undo bugs.

6\. \*\*Periodic Architecture Cleanup:\*\* Before major new layers (like \`Roto\` or \`OpenFX\`), run a focused cleanup/refactor pass so core modules (\`viewer.rs\`, \`canvas.rs\`, \`bus.rs\`, \`ops.rs\`) stay clean and modular.

7\. \*\*CI \& Supply-Chain Hygiene:\*\* Set up and keep GitHub Actions CI + \`deny.toml\` (license \& advisory checks) green alongside local build and test jobs.

8\. \*\*Live GUI Check:\*\* Never commit or tag a release without real human interaction testing on the reference laptop.

\---

\## 4. Next Candidate Slice — \`v0.6.5\` (Safety, Speed \& Diagnostics) — SHIPPED (v0.6.5)

\*Goal: Make Tanova safe for real projects and fast on 1080p footage before adding heavier tools.\*

\- \[x\] \*\*Audit Previous Phases (\`v0.1.0\`–\`v0.6.0\`):\*\* Completed 2026-11-22 on \`ec6c171\`; all deferred items, save/memory gaps, and doc/repo drift folded into this file.

\### Milestone 25 — Bulletproof Save, \`.tnv\` Versioning \& Crash Recovery

\- \[x\] \*\*Nuke-Safe \`.tnv\` Schema Version:\*\* Emit a version comment header (e.g., \`# tanova\_schema 1\`) alongside TCL output so Nuke still opens \`.nk\`/\`.tnv\` files cleanly while Tanova validates and migrates its own schema version.

\- \[x\] \*\*Fix Optimistic Clean Save Bug:\*\* Keep \`is\_dirty = true\` until the kernel emits a confirmed \`ScriptSaved\` event; if saving fails, stay dirty and surface the error clearly.

\- \[x\] \*\*Same-Folder Atomic Save:\*\* Write to a hidden \`.tmp\` file in the \*\*same directory\*\* as the target \`.tnv\` and atomically rename it (avoids cross-filesystem rename errors and prevents 0-byte corruption on power loss).

\- \[x\] \*\*Idle Background Auto-Save \& Crash Recovery:\*\* Save snapshots into a dedicated OS app-data recovery folder (\`$XDG\_DATA\_HOME/tanova/recovery\` or \`\~/.local/share/tanova/recovery\`) only when idle + dirty (never mid-drag, never overwriting the user's real saved \`.tnv\`); delete recovery snapshots cleanly on normal exit; prompt \*\*"Restore Session / Discard"\*\* after an unclean exit.

\### Milestone 26 — Real RAM Budget, Purge Button, Safe Multi-Threading \& 1080p Speed Gate

\- \[x\] \*\*Real GUI \`MemoryGovernor\` Cap + Cooperative Eviction:\*\* Replace \`usize::MAX\` in the GUI with a safe \`2.0 GiB\` default cap (auto-scaled lower on machines with less RAM) and wire \`MemoryGovernor\` pressure to automatic \`Dag\` cache eviction so cached entries drop cleanly instead of failing allocations.

\- \[x\] \*\*Live RAM Indicator \& \`Purge RAM\` Button:\*\* Expose \`allocated\_bytes / max\_bytes\` from the kernel to the UI top bar with a one-click \*\*\`Purge RAM\`\*\* button that calls \`Dag::clear\_cache()\`.

\- \[x\] \*\*Safe Tile Multi-Threading (\`N - 2\` Threads):\*\* Parallelize heavy ops (\`Blur\`, \`Defocus\`, \`Glow\`, \`CornerPin2D\`) using a thread pool capped to leave \*\*2 threads free\*\* for OS/browser while staying deterministic and \`MemoryGovernor\`-safe.

\- \[x\] \*\*Per-Node Render Timings + 1080p Benchmark Gate:\*\* Record last evaluation ms per node (optional slow-node indicator/tooltip) and add automated \`1920×1080\` speed benchmarks.

\### Milestone 27 — Full-Res-Safe Viewer Proxy (\`1:1\` / \`1:2\` / \`1:4\`) \& Repo Hygiene

\- \[x\] \*\*Viewer Proxy Mode (\`1:1\` / \`1:2\` / \`1:4\`):\*\* Viewer toolbar selector + clear \`PROXY 1:2\` / \`PROXY 1:4\` badge; \`Write\` always exports full-res; all spatial knobs, keyframes, radii, and Viewer handles remain in \*\*full-resolution logical units\*\* while the engine scales math via \`proxy\_scale\`. Implemented and live-GUI verified on `v0.6.5-testing`.

\- \[x\] \*\*Repo Hygiene:\*\* Fix \`Cargo.toml\` repository URL (\`tanova/tanova\` -\> \`GMNEdits/Tanova\`) and add root \`LICENSE-MIT\` / \`LICENSE-APACHE\` files (merged from `codex-nodes` on `v0.6.5-testing`).

\- \[x\] \*\*Remaining Cleanup:\*\* Removed the unused duplicate \`RamCache\` and synced the \`README.md\` / \`CHANGELOG.md\` limitations.

\---

\## 5. Flexible V1 Feature Buckets (Unassigned — Pick Per Release)

\### Bucket A — Export, Caching, Architecture \& Reliability UX

\- \[ \] \*\*\`Write\` Render Progress Dialog \& Interruptible Export:\*\* Refactor \`Write\` (currently blocks the kernel loop synchronously in \`bus.rs\`, making cancel impossible) to be interruptible per frame, adding a Render Dialog with frame range (\`start..end\`, step), live frame counter, elapsed/ETA, instant \*\*Cancel\*\*, auto-create output folders, free-disk-space error checks, and atomic \`.tmp\`-then-rename per frame.

\- \[ \] \*\*\`Write\` Sequence Resume / Checkpoint:\*\* Allow interrupted batch renders to skip already-completed frames and resume cleanly instead of restarting from frame 1.

\- \[ \] \*\*File Browser \& Workspace UX:\*\* Add an \*\*"All files"\*\* toggle (beyond \`.tnv\`/\`.nk\`/\`.exr\`) and a \*\*Recent Projects\*\* list.

\- \[ \] \*\*Smart Duplicate / Paste Placement:\*\* Place duplicated or pasted nodes near the cursor (or with a configurable offset) instead of the hardcoded \`(40, 40)\` cascade offset.

\- \[ \] \*\*Upstream (Mid-Graph) Node Cache Tuning:\*\* Strengthen existing Blake3 \`OpHash\` mid-graph caching (currently entry-count bounded at \`64\`) with byte-aware eviction and verify unchanged upstream nodes (\`Roto\`, \`Defocus\`, \`Read\`) are reused reliably when tweaking downstream nodes (and without nuking reusable static upstream nodes needlessly during \`Write\`).

\- \[ \] \*\*Bounded Viewer Playback RAM Cache:\*\* Cache rendered timeline frames under \`MemoryGovernor\` with a green cached-frame bar on the timeline for smooth real-time playback.

\- \[ \] \*\*Region of Interest (ROI) Optimization:\*\* Avoid evaluating off-screen or cropped upstream pixels when zoomed in or cropped.

\- \[ \] \*\*GPU-Ready Buffer Discipline:\*\* Keep tiled raster/buffer abstractions clean and decoupled from CPU-only assumptions so optional GPU acceleration can be added later without rewriting all 29+ nodes.

\- \[ \] \*\*Node Error Badges \& Missing-Media UX:\*\* Clear red error highlight + hover reason on broken nodes (missing file, bad LUT, degenerate warp, plugin error) instead of silent black frames.

\- \[ \] \*\*Project-Relative Media Paths:\*\* Support paths relative to the \`.tnv\` file alongside absolute paths so moving a project folder doesn't break \`Read\` nodes.

\### Bucket B — Shared Viewer Tools, Masking \& Keying

\- \[ \] \*\*Shared Viewer Tool / Overlay Layer:\*\* Refactor \`CornerPin2D\` handles out of \`viewer.rs\` into a reusable gizmo/overlay architecture (enabling \`Roto\` plus optional \`Transform\` TRS/pivot, \`Crop\` box, and \`Ramp\` endpoints).

\- \[ \] \*\*Alpha \& Premultiplication Audit:\*\* Verify \`Merge\`, \`Grade\`, \`Blur\`, \`Defocus\`, \`Premult\`, and \`Unpremult\` against standard linear compositing rules before shipping \`Roto\` / \`ChromaKey\` (eliminate dark fringes and white edge halos).

\- \[ \] \*\*Extensible Node Data Model:\*\* Extend knob/parameter storage beyond fixed primitives to cleanly hold animated Bezier shapes (\`Roto\`) and dynamic runtime parameters (\`OpenFX\`).

\- \[ \] \*\*Interactive \`Roto\` Node:\*\* Draw closed Bezier/polygon masks directly in the Viewer, drag points \& tangents, animate over time, feather/opacity/invert, and wire into any node's \`M\` mask port.

\- \[ \] \*\*\`ChromaKey\` Node:\*\* Screen-color picker, screen balance, clip black/white matte cleanup, edge softness, and green/blue spill suppression.

\- \[ \] \*\*Curve Editor Depth \*(Optional for V1)\*:\*\* Draggable Bezier tangent handles, multi-key box selection, and Dope Sheet multi-node re-timing.

\- \[ \] \*\*2D \`Tracker\` / Knob Linking \*(Optional / Stretch)\*:\*\* Basic 2D patch tracking or expression links driving \`Transform\` / \`CornerPin2D\`.

\### Bucket C — Lightweight Color Management \& Optional OCIO

\- \[ \] \*\*Built-In Pure-Rust Color Pipeline:\*\* Fast built-in transforms (Linear, sRGB, Rec.709, common camera Log curves) + explicit color-space tags on \`Read\`, \`Viewer\`, and \`Write\`.

\- \[ \] \*\*Pure-Rust \`LUT\` Node (\`.cube\`):\*\* Full 1D (\`LUT\_1D\_SIZE\`) and 3D (\`LUT\_3D\_SIZE\`, e.g., 33³ / 65³) \`.cube\` parser \& tetrahedral/trilinear interpolator supporting \`DOMAIN\_MIN\`, \`DOMAIN\_MAX\`, and \`TITLE\`.

\- \[ \] \*\*Optional External OpenColorIO (\`.ocio\`):\*\* Opt-in external \`.ocio\` config loading (\`OCIOColorSpace\` / \`OCIODisplay\`) without bundling multi-GB studio LUTs or forcing heavy C++ builds on every contributor.

\### Bucket D — Safe OpenFX (OFX) Plugins \& Presets

\- \[ \] \*\*Isolated Plugin Discovery \& Startup Probe:\*\* Scan standard system OFX paths using a separate probe check + blocklist so broken or oversized \`.ofx\` bundles never crash Tanova on startup.

\- \[ \] \*\*Thin C-ABI OFX 1.x Host Layer:\*\* Map OFX parameters to dynamic Tanova knobs (guarding against 2D point-parameter coordinate quirks), support plugin presets (save/load), allocate safe worker stack sizes (e.g., 16 MB for stack-heavy C++ plugins), and evaluate \*\*out-of-process / helper-process crash isolation\*\* since Rust \`catch\_unwind\` cannot stop a C++ segfault.

\### Bucket E — Pre-\`v1.0.0\` Hardening \& Release Readiness

\- \[ \] \*\*Full \`.tnv\` \& \`.nk\` Round-Trip Audit:\*\* Verify every cartridge, animated curve, interpolation mode, Viewer state, and format setting saves and reloads cleanly with zero silent data loss or migration panics.

\- \[ \] \*\*Public \`.nk\` Learning Script \& Pixel-Diff Suite:\*\* Test parsing, wiring, rendering, and golden pixel-diff output against publicly available Nuke Non-Commercial learning/sample \`.nk\` scripts (no local Nuke install required).

\- \[ \] \*\*Large-Plate \& Multi-Layer EXR Stress Testing:\*\* Audit \`Read\` + \`DiskCache\` + \`MemoryGovernor\` on 4K plates and multi-layer scanline EXRs so RAM stays capped globally and bounding boxes never explode silently.

\- \[ \] \*\*Undo/Redo \& Nuke Keyboard Shortcuts Audit:\*\* Confirm every user action (\`Roto\` points, gizmos, curve keys, knobs, node/wire changes) supports predictable \`Ctrl+Z\` / \`Ctrl+Shift+Z\`, and verify 92–93% Nuke shortcut consistency.

\- \[ \] \*\*CLI / Headless Render Parity \& Packaging:\*\* Verify batch CLI rendering matches GUI output identically, set up GitHub Actions CI + \`deny.toml\`, and package lean, clean release binaries.

\---

\## 6. Known Architectural Walls \& Traps ("Do Not Forget")

\| Wall / Trap \| What Goes Wrong If Ignored \| Prevention Rule \|

\| :--- \| :--- \| :--- \|

\| \*\*Optimistic Clean Save State\*\* \| UI clears \`is\_dirty\` before kernel finishes writing; a failed save tricks the user into thinking changes are safe. \| Clear \`is\_dirty\` only after confirmed \`ScriptSaved\`; stay dirty and surface error on write failure. \|

\| \*\*Count-Only Cache vs. Byte RAM Cap\*\* \| Enabling a real \`2.0 GiB\` GUI cap while \`Dag\` caches 64 heavy frames fills permits and causes allocation errors on the next node. \| Wire \`MemoryGovernor\` pressure directly into \`Dag\` cache eviction so old entries free permits automatically. \|

\| \*\*Proxy Scale Mismatch\*\* \| \`Blur\`/\`Defocus\` look 2× stronger at \`1:2\` proxy; \`CornerPin2D\`/\`Roto\` handles shift when switching back to \`1:1\`. \| Store \*\*all\*\* coordinates \& radii in full-res logical units; pass \`proxy\_scale\` to render ops only. \|

\| \*\*Mid-Drag Auto-Save \& Stale Locks\*\* \| Auto-saving mid-drag causes stutters/partial states; stale lock files trigger false crash popups on every launch. \| Auto-save only when idle + dirty via same-folder temp-file swap; delete recovery snapshot cleanly on normal exit; never overwrite the user's real \`.tnv\`. \|

\| \*\*Upstream Full-Tree Recompute\*\* \| Adjusting a fast downstream \`Grade\` re-runs heavy upstream \`Roto\`/\`Defocus\` nodes every frame. \| Keep upstream node caching intact and invalidate only downstream of edited nodes. \|

\| \*\*Bounding-Box Explosion\*\* \| Panning/scaling/warping far off-screen balloons canvas to 15k×15k, stalling every downstream node. \| Clamp runaway bounding boxes, preserve \`Uniform\` tile fast paths, and warn on abnormally large bboxes. \|

\| \*\*Partial Frame Corruption / Disk Full\*\* \| Canceling \`Write\` or running out of disk leaves corrupted frames or cryptic test failures. \| Write each frame to \`.tmp\` then rename; handle I/O errors cleanly per frame; clean sibling \`target/\` caches when disk gets tight. \|

\| \*\*Alpha / Premult Halos\*\* \| Dark fringes or white glowing edges appear around every \`Roto\` mask and \`ChromaKey\` cutout. \| Complete alpha/premult audit before shipping \`Roto\` and \`ChromaKey\`. \|

\| \*\*UI Thread Blocking \& Starvation\*\* \| Heavy disk/render ops on UI thread trigger \*"Not Responding"\*; continuous drags starve heavy renders. \| Keep UI strictly non-blocking, preserve kernel anti-starvation guard, and bump \`render\_epoch\` before \`Shutdown\`. \|

\| \*\*C++ OFX Segfaults \& OCIO Bloat\*\* \| A single buggy \`.ofx\` crashes the entire process (cannot be caught by \`catch\_unwind\`); mandatory C++ OCIO bloats builds and binary size. \| Ship Auto-Save before OFX, probe/blocklist plugins safely, design for process isolation, keep core color/LUTs pure Rust, make \`.ocio\` optional. \|

\---

\## 7. Living Parking Lot (New Discoveries \& Ideas)

\*Add bugs, user feedback, missing nodes, or improvements discovered during development here:\*

\- \*(2026-11-22)\* Set up \`.github/workflows/\` CI (fmt, clippy, test, headless checks) + \`deny.toml\` (currently absent from repo).

\- \*(2026-11-22; resolved in Milestone 27)\* Annotate stale \`v0.4.0\` Known Limitations in \`CHANGELOG.md\` and align \`README.md\` limitations.

\- \*(2026-11-22)\* Watch disk space across the 3 git worktrees (\`tanova\`, \`tanova-codex\`, \`tanova-kilo\`); purge unused \`target/\` caches if disk gets tight.

\- \*(2026-11-26)* Optimize small-format Defocus/Blur border-tile spill (expanding 0..256 to -4..260 currently allocates 9 tiles / ~9.75 MiB instead of clipping or sparse-skipping empty border regions).

### Milestone 28 — Shared Viewer Overlay, `Ramp` Handles & Node Error UX

- [x] **Shared Viewer overlay module (`tanova-ui/src/overlay.rs`):** `OverlayViewport` reproduces the Viewer coordinate helpers proxy-safely so all 2D handles write full-resolution logical coordinates at 1:1/1:2/1:4 proxy; `OverlayState` with `is_dragging()` and `OverlayResponse { consumed_pointer, commands }`; `emit_2d_knob_command` preserves `[x, y, prev[2], prev[3]]` and auto-keys via `Batch([SetKnob, SetKnobCurve])`.

- [x] **Overlay handlers:** `Transform` (red X/green Y/crosshair/dashed box/smooth drag), `CornerPin2D` (orange quad + numbered handles clamped to `[-fw..=2*fw, -fh..=2*fh]`, auto-key) and new `Ramp` (`p0`/`p1` from effective `KnobValue::Color`, `Color32::from_rgb(100,200,255)` line 1.5 px, 7.0 px `"P0"`/`"P1"` handles highlighted on hover/drag, full-res logical coords, pan suppressed, `request_repaint`).

- [x] **Viewer backward compatibility:** `ViewerPanel::show` signature and `is_dragging_overlay()` unchanged; overlay runs strictly before pan/drag handling; `TanovaApp::fold_event` still protects `self.mirror` while `is_dragging_overlay()`.

- [x] **Exact-origin node error reporting:** `NodeErrorInfo { node, message, crashed }` and `KernelEvent::NodeErrors(Vec<NodeErrorInfo>)` emitted on every `render_viewer()` pass (empty vec clears), reporting the failing origin node (e.g. `Read #4` → `NodeId(4)`, never the Viewer `NodeId(3)`); panic/airbag tests green.

- [x] **`GraphMirror::node_errors`:** updated on `NodeErrors`, pruned on `RemoveNode`, cleared on `clear()`/`NewScript`/`ReplaceDag`.

- [x] **Canvas error badges:** 2.0 px `Color32::from_rgb(235,75,70)` border plus `"CRASH"`/`"ERR"` pill; `Order::Tooltip` hover area shows the truncated error reason even without timing data.

- [x] **Viewer error banner:** `ViewerPanel::active_error`, non-blocking dark-red banner pinned to the top interior edge of the Viewer viewport.
