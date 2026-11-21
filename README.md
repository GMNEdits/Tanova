# Tanova

**v0.5.0 · Node-based compositing in Rust**

Tanova is a GPU-accelerated, tile-based compositing engine and desktop app written in Rust. Inspired by Nuke’s directed acyclic graph (DAG) workflow, it lets you wire image-processing nodes together, preview the results in a live viewer, and render scripts from the command line. A microkernel evaluates the graph on a background thread while the desktop UI communicates with it through a message bus.

## Key highlights in v0.5.0

- **Microkernel.** The eight-crate workspace separates UI state from kernel execution. `RenderEpoch` preempts stale render work, `MemoryGovernor` caps governed RAM allocations, and a bounded GreedyDual-Size cache evicts DAG results that hold rendered tiles.
- **Tiled rendering.** The RGBA/AOV pipeline uses `wgpu` compute with Vulkan support and CPU fallbacks for GPU-accelerated operations. Gaussian blur runs in two separable passes, and `TransformChain` concatenates adjacent transforms for a single resampling step.
- **Animation and scripts.** Edit and preview live `{curve ...}` keyframes with a full-width timeline scrubber and previous/next keyframe controls. Continuous edits coalesce into a single undo step. Parse and export supported `.nk` and native `.tnv` scripts with keyframe round-tripping.
- **Viewer.** Choose `Linear`, `sRGB`, `Rec.709`, or `ACES` filmic tone mapping, solo the `R`, `G`, `B`, or `A` channel, adjust display gain and gamma, and inspect RGBA values with the live pixel probe.

### Built-in cartridges

Tanova registers 24 cartridges, including the internal `Dummy` test cartridge.

| Category | Cartridges |
| --- | --- |
| I/O & Generators | `Read`, `Write`, `Constant`, `CheckerBoard2` |
| Color | `Grade`, `ColorCorrect`, `Saturation`, `Clamp`, `Invert`, `Premult`, `Unpremult`, `Shuffle` |
| Transform | `Transform`, `Reformat`, `Crop` |
| Filter | `Blur`, `TimeOffset`, `FrameHold` |
| Merge & Utility | `Merge2`, `Switch`, `Viewer`, `Dot`, `BackdropNode`, `Dummy` |

## Workspace architecture

| Crate | Role |
| --- | --- |
| `tanova-kernel` | Evaluates the DAG on demand and manages caching, cartridge registration, messaging, and undo history. |
| `tanova-render` | Provides tiled raster and AOV storage, spatial math, GPU compute, and display conversions. |
| `tanova-nodes` | Implements and registers the built-in node cartridges. |
| `tanova-parse` | Parses Nuke-style script syntax, knob expressions, and animation curves. |
| `tanova-io` | Handles OpenEXR I/O and caches frames read from disk. |
| `tanova-ui` | Provides the egui graph canvas, viewer, properties panel, and timeline. |
| `tanova-app` | Runs the desktop app and connects the UI to the background kernel. |
| `tanova-cli` | Loads scripts and renders frames headlessly to PPM or OpenEXR. |

[Architecture](ARCHITECTURE.md) · [Roadmap](ROADMAP.md) · [Changelog](CHANGELOG.md)

## Quick start

From the workspace root, launch the desktop GUI with a working `wgpu` graphics adapter.

```bash
cargo run --release -p tanova-app
```

Run the CLI and desktop smoke checks headlessly. The desktop check still initializes the GPU.

```bash
cargo run --release -p tanova-cli -- --check-headless
cargo run --release -p tanova-app -- --check-headless
```

Run the workspace test suite.

```bash
cargo test --workspace
```

## Essential shortcuts

| Shortcut | Action |
| --- | --- |
| `Tab` | Open node search with the pointer over the graph canvas. |
| `1` / `2` | Bind or recall viewer comparison slot 1 or 2. |
| `D` | Toggle bypass on selected processing nodes. |
| `F` | Frame the selection, or the whole graph when nothing is selected. |
| `Shift+X` | Swap the first two inputs of selected multi-input nodes. |
| `Ctrl+Z` / `Ctrl+Y` | Undo / redo. `Ctrl+Shift+Z` also redoes. |
| `Ctrl+C` / `Ctrl+V` / `Ctrl+D` | Copy, paste, or duplicate graph nodes. |
| `Delete` / `Backspace` | Delete selected nodes. |
| `R` / `G` / `B` / `A` | Toggle channel soloing with the pointer over the viewer. |
| `Ctrl+S` / `Ctrl+Shift+S` | Save / Save As. |

## License

The workspace declares a dual license of MIT or Apache-2.0.
