# Tanova

**v0.6.0 · Node-based compositing in Rust**

Tanova is a GPU-accelerated, tile-based compositing engine and desktop app written in Rust. Inspired by Nuke’s directed acyclic graph (DAG) workflow, it lets you wire image-processing nodes together, preview the results in a live viewer, and render scripts from the command line. A microkernel evaluates the graph on a background thread while the desktop UI communicates with it through a message bus.

[Quick start](#quick-start) · [What you can do](#what-you-can-do) · [Essential shortcuts](#essential-shortcuts) · [Minimum requirements](#minimum-requirements) · [Architecture](ARCHITECTURE.md)

## What you can do

### Build a composite

A graph starts with a source. Use `Read` for an OpenEXR image or sequence, or make a source with `Constant` or `CheckerBoard2`. Connect nodes to adjust color, blur, or reposition an image, and combine images with `Merge2`. Select a node to edit its settings in the properties panel. Use `Dot` to route connections and `BackdropNode` to group related parts of the graph visually.

### Warp and blur

Pin a frame to four corners with `CornerPin2D` — drag the handles directly on the viewer to warp an image onto a new shape. Blur the focus with `Defocus` — an optical circular disk blur with radius, aspect, and mix knobs. Drag any knob in the properties panel and see the image update live, without waiting for the mouse to release.

### Preview and animate

Connect a `Viewer` node to the result in Viewer Node mode, or preview a node directly in Direct Target mode. Use comparison slots `1` and `2` to move between two points in the graph. The viewer offers `Linear`, `sRGB`, `Rec.709`, and `ACES` filmic display transforms, gain and gamma controls, individual RGBA channels, and a pixel probe that shows RGBA values under the pointer.

Move the playhead on the full-width timeline to inspect another frame. Use the keyframe control beside a supported setting to add a key at the current frame. Editing an animated setting creates or updates a key there. The timeline displays keyframe markers and has controls to jump to the previous or next key. Undo and redo cover graph edits, and repeated changes during a knob drag coalesce into one undo step.

Edit curves visually with the **Curve Editor** tab: drag keyframes, change interpolation between `Smooth`, `Linear`, and `Constant`, fit the view to all keys, and delete keys. The minimap (`[Map]` toggle) gives you a small overview of large graphs and lets you pan by clicking or dragging inside it.

### Save a project or render it

Save your graph as a native `.tnv` project or open and export supported Nuke `.nk` scripts. Supported `{curve ...}` keyframes survive a save-and-reopen round trip. Use a `Write` node to render an OpenEXR sequence from the desktop, with RGBA or all available channels. Extra channels such as depth are commonly called AOVs. The CLI can render a single frame from a script to PPM or OpenEXR.

## What’s new in v0.6.0

- **`CornerPin2D`** — four-corner homography warp with interactive viewer handles
- **`Defocus`** — optical circular/anamorphic disk blur with fast live dragging
- **Curve Editor** — visual keyframe editing with interpolation modes
- **Node Graph Minimap** — overview and pan control for large graphs
- **29 cartridges** — up from 24 in v0.5.0

## Built-in cartridges

| Category | Cartridges |
|---|---|
| I/O & Generators | `Read`, `Write`, `Constant`, `CheckerBoard2` |
| Color | `Grade`, `ColorCorrect`, `Saturation`, `Clamp`, `Invert`, `Premult`, `Unpremult`, `Shuffle` |
| Transform | `Transform`, `Reformat`, `Crop`, `CornerPin2D` |
| Filter | `Blur`, `Defocus`, `TimeOffset`, `FrameHold` |
| Merge & Utility | `Merge2`, `Switch`, `Viewer`, `Dot`, `BackdropNode`, `Mirror`, `Ramp`, `Glow`, `Dummy` |

## Minimum requirements

- **OS:** Linux & Windows
- **RAM:** 4 GB minimum (8 GB recommended)
- **GPU:** Any adapter supporting Vulkan, Metal, or DirectX 12 (via `wgpu`)
- **Rust:** Stable toolchain (to build from source)

## Quick start

From the workspace root, launch the desktop GUI:

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

Run the CLI and desktop smoke checks headlessly:

```bash
cargo run --release -p tanova-cli -- --check-headless
cargo run --release -p tanova-app -- --check-headless
```

Run the workspace test suite:

```bash
cargo test --workspace
```

## Essential shortcuts

| Shortcut | Action |
|---|---|
| `Tab` | Open node search with the pointer over the graph canvas |
| `1` / `2` | Bind or recall viewer comparison slot 1 or 2 |
| `D` | Toggle bypass on selected processing nodes |
| `F` | Frame the selection, or the whole graph when nothing is selected |
| `Shift+X` | Swap the first two inputs of selected multi-input nodes |
| `Ctrl+Z` / `Ctrl+Y` | Undo / redo |
| `Ctrl+C` / `Ctrl+V` / `Ctrl+D` | Copy, paste, or duplicate graph nodes |
| `Delete` / `Backspace` | Delete selected nodes |
| `R` / `G` / `B` / `A` | Toggle channel soloing with the pointer over the viewer |
| `Ctrl+S` / `Ctrl+Shift+S` | Save / Save As |

## Limitations

- No auto-save or crash recovery — unsaved work is lost on crash
- Sequence render cannot resume after interruption — it restarts from frame 1
- No OpenColorIO integration — images are processed in raw linear space

## License

The workspace declares a dual license of MIT or Apache-2.0.