# Tanova

**v0.5.0 · Node-based compositing in Rust**

Tanova is a GPU-accelerated, tile-based compositing engine and desktop app written in Rust. Inspired by Nuke’s directed acyclic graph (DAG) workflow, it lets you wire image-processing nodes together, preview the results in a live viewer, and render scripts from the command line. A microkernel evaluates the graph on a background thread while the desktop UI communicates with it through a message bus.

[Quick start](#quick-start) · [What you can do](#what-you-can-do) · [Shortcuts](#essential-shortcuts) · [Architecture](ARCHITECTURE.md)

## What you can do

### Build a composite

A graph starts with a source. Use `Read` for an OpenEXR image or sequence, or make a source with `Constant` or `CheckerBoard2`. Connect nodes to adjust color, blur or reposition an image, and combine images with `Merge2`. Select a node to edit its settings in the properties panel. Use `Dot` to route connections and `BackdropNode` to group related parts of the graph visually.

### Preview and animate

Connect a `Viewer` node to the result in Viewer Node mode, or preview a node directly in Direct Target mode. Use comparison slots `1` and `2` to move between two points in the graph. The viewer offers `Linear`, `sRGB`, `Rec.709`, and `ACES` filmic display transforms, gain and gamma controls, individual RGBA channels, and a pixel probe that shows RGBA values under the pointer.

Move the playhead on the full-width timeline to inspect another frame. Use the keyframe control beside a supported setting to add a key at the current frame. Editing an animated setting creates or updates a key there. The timeline displays keyframe markers and has controls to jump to the previous or next key. Undo and redo cover graph edits, and repeated changes during a knob drag can coalesce into one undo step.

### Save a project or render it

Save your graph as a native `.tnv` project or open and export supported Nuke `.nk` scripts. Supported `{curve ...}` keyframes survive a save-and-reopen round trip. Use a `Write` node to render an OpenEXR sequence from the desktop, with RGBA or all available channels. Extra channels such as depth are commonly called AOVs. The CLI can render a single frame from a script to PPM or OpenEXR.

## Key highlights in v0.5.0

- **Animation.** Live evaluation of Nuke-style `{curve ...}` keyframes, a full-width scrubber, and undo coalescing for animated knob edits.
- **Viewer.** Display transforms and RGBA channel soloing.
- **Nodes and rendering.** Thirteen new cartridges bring the registry to 24. `Switch` evaluates the selected branch, and `TimeOffset` and `FrameHold` request a different upstream frame. Gaussian blur uses two separable passes on GPU and CPU, and `TransformChain` combines adjacent transforms so the result samples the original image.
- **Cache.** The bounded GreedyDual-Size cache evicts DAG results based on their size and estimated cost to recompute.

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

The eight-crate microkernel keeps graph evaluation on a background thread and exchanges commands and events with the UI. The kernel evaluates the upstream nodes a render needs and caches their results. `RenderEpoch` lets the engine abandon stale work when a newer render is requested, while `MemoryGovernor` caps tracked RAM allocations.

The render pipeline stores RGBA and extra channels in tiles. Supported operations use `wgpu` compute with Vulkan support and CPU fallbacks. Node cartridges register with the kernel and supply the image-processing logic.

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

The desktop opens a starter graph, `Constant → Grade → Viewer`. Select `Grade` and adjust its gain in the properties panel to see the result in the viewer. Press `Tab` over the graph canvas to search for a node.

### Render a frame from a script

```bash
cargo run --release -p tanova-cli -- --script path/to/shot.nk --frame 12 --out-exr frame.0012.exr
```

Use `--node Viewer1` to select a target by name, or `--out-ppm frame.ppm` for PPM output.

### Verify a build

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
