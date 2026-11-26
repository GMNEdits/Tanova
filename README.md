# Tanova

**v0.6.5 · Node-based compositing in Rust**

Tanova is a lightweight, open-source, node-based 2D VFX compositor built in Rust, made for Windows and Linux. It runs on everyday laptops—not just high-end studio workstations—so solo artists, freelancers, and hobbyists can build compositing graphs, preview them live, and render to disk without a heavy studio stack. Everything is built in plain Rust with no bundled Python, Qt, or browser engine, so the binary stays small and startup stays fast.

Tanova is in active pre-1.0 development (`v0.6.5`) and is source-only right now—there are no pre-built `.exe` or Linux installers yet; you build it once with Rust.

[Highlights](#highlights) · [Available nodes](#available-nodes-28) · [How Tanova is built](#how-tanova-is-built) · [Build & run](#how-to-build--run) · [Limitations](#current-limitations-v065) · [License](#license--trademark-notice) · [Architecture](ARCHITECTURE.md) · [Roadmap](ROADMAP.md) · [Changelog](CHANGELOG.md)

## Highlights

- **Familiar node workflow:** Spawn nodes with single keys (`R`, `W`, `C`, `G`, `B`, `T`, `M`, `.`, `Z`), search with `Tab`, view two points at once with `1`/`2` comparison slots, bypass nodes with `D`, frame a selection with `F`, swap inputs with `Shift+X`, and drop a node onto an existing wire to splice it in.
- **Dual viewer modes & studio themes:** Toggle between `Viewer Node` mode and wire-free `Direct Target` mode, and choose from three themes: `Classic`, `Slate`, or `Midnight`. Viewer Node mode connects a `Viewer` node into your graph; Direct Target mode previews any node directly without wiring.
- **Built for modest laptops:** Viewer Proxy at `1:1`, `1:2`, or `1:4` resolution (`Ctrl+P`) keeps playback smooth on HD footage, safe `N - 2` multi-threading leaves CPU threads free for your OS and browser, an automatic 1–2 GiB RAM cap with one-click `Purge` prevents runaway memory use, and per-node render timing bars let you spot slow nodes at a glance.
- **Animation & curve editing:** Live keyframe animation with `[+K]`/`[K]` indicators, an interactive Curve Editor tab for dragging keyframes and switching `Smooth`/`Linear`/`Constant` interpolation, and a Node Graph Minimap (`[Map]` toggle) that gives you an overview of large graphs and lets you pan by clicking or dragging inside it.
- **Project & script files:** Native `.tnv` projects with versioned atomic saves and idle background auto-save plus crash recovery. Direct `.nk` script import keeps unsupported nodes intact as safe pass-throughs instead of breaking the graph.
- **OpenEXR I/O:** Read and write multi-channel 32-bit float OpenEXR sequences, with Viewer display transforms (`Linear`, `sRGB`, `Rec.709`, `ACES`) and `R`/`G`/`B`/`A` channel soloing for quick channel checks.
- **Headless CLI renderer:** Render frames from scripts with `tanova-cli`, and measure performance with the built-in `1920x1080` speed benchmark.

## Available Nodes (28)

| Category | Nodes |
|---|---|
| I/O & Generators | `Read`, `Write`, `Constant`, `CheckerBoard2`, `Ramp` |
| Color & Alpha | `Grade`, `ColorCorrect`, `Saturation`, `Clamp`, `Invert`, `Premult`, `Unpremult`, `Shuffle` |
| Merge & Routing | `Merge2`, `Switch`, `Dot`, `Viewer` |
| Spatial & Transform | `Transform`, `CornerPin2D`, `Crop`, `Reformat`, `Mirror` |
| Filters & Effects | `Blur`, `Defocus`, `Glow` |
| Time & Organization | `TimeOffset`, `FrameHold`, `BackdropNode` |

That's 5 + 8 + 4 + 5 + 3 + 3 = 28 artist-facing nodes. Each one lives in its own isolated file, so adding or tweaking an effect never risks breaking the core engine.

## How Tanova Is Built

Tanova keeps the engine small and every effect isolated:

- **Core + Isolated Nodes:** A small central engine handles wiring, undo/redo, caching, and memory limits without knowing any image math; every visual effect plugs in as its own separate file, so adding a new effect cannot break the core engine.
- **Crash Safety ("Airbag"):** If a node hits a bad value or runs out of memory, Tanova catches the fault, flags that single node, and keeps the app running instead of closing.
- **Non-Blocking Engine:** Image rendering runs on a background worker while the interface stays separate and responsive, using 256×256 tiled 32-bit linear float images and GPU compute (`wgpu`) where available.

Tanova is an 8-crate workspace:

| Crate | Purpose |
|---|---|
| `tanova-kernel/` | Node graph, undo/redo, cache, memory cap, and crash isolation |
| `tanova-render/` | Tiled 32-bit image math, GPU compute shaders (`wgpu`), multi-threading, and display transforms |
| `tanova-nodes/` | All 28 self-contained compositing nodes (1 file per node) |
| `tanova-io/` | OpenEXR image sequence reading/writing and background disk cache |
| `tanova-parse/` | `.tnv` and `.nk` script parser, expressions, and animation curves |
| `tanova-ui/` | Node Graph, Viewer, Curve Editor, Properties panel, and themes (`egui`) |
| `tanova-app/` | Main desktop application |
| `tanova-cli/` | Headless command-line renderer and 1080p speed benchmark |

For deep technical rules, see [`ARCHITECTURE.md`](ARCHITECTURE.md). For the rolling feature plan, see [`V1_BUCKET.md`](V1_BUCKET.md) and [`ROADMAP.md`](ROADMAP.md). For release notes, see [`CHANGELOG.md`](CHANGELOG.md).

## What's New in v0.6.5

- **Viewer Proxy** (`1:1` / `1:2` / `1:4`) with an amber badge so you never mistake proxy pixels for full resolution
- **RAM cap & cooperative cache eviction** so 1080p composites stay within a 1–2 GiB memory budget
- **Safe `N - 2` multi-threading** for parallel Blur, Defocus, Glow, and CornerPin2D
- **Per-node render timing bars** and the `Ctrl+P` proxy shortcut
- **`Write` always exports full resolution**, even when the Viewer is in Proxy mode
- **Repo hygiene:** MIT/Apache-2.0 licenses, `cargo-deny`, GitHub Actions CI, and a fixed repo URL

## How to Build & Run

Pre-built installers are not packaged yet, so you need Rust (`1.85` or newer) installed once to build and launch Tanova.

### Windows (Step-by-Step)

1. Download and run `rustup-init.exe` from [https://rustup.rs](https://rustup.rs). When asked about **Visual Studio C++ Build Tools**, choose the default option (`1`) so Windows can compile native apps.
2. Close and reopen Command Prompt after installation finishes (so `cargo` is recognized), then verify with `cargo --version`.
3. Clone the repository:

   ```bash
   git clone https://github.com/GMNEdits/Tanova.git
   ```

   Open Command Prompt inside the folder containing `Cargo.toml` and run:

   ```bash
   cargo run --release -p tanova-app
   ```

   The desktop opens a starter graph, `Constant → Grade → Viewer`. Select `Grade` and adjust its gain in the properties panel to see the result in the viewer. Press `Tab` over the graph canvas to search for a node.

   The first build takes a few minutes; later launches open in seconds.

### Linux (Ubuntu / Debian & General)

1. Install Rust via [https://rustup.rs](https://rustup.rs) (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`), then reopen your terminal (or run `source "$HOME/.cargo/env"`).
2. Install required GUI/Vulkan libraries (Ubuntu/Debian example):

   ```bash
   sudo apt-get update && sudo apt-get install -y libxkbcommon-dev libwayland-dev libxcb-shape0-dev libxcb-xfixes0-dev libxcb-render0-dev libvulkan1 mesa-vulkan-drivers pkg-config
   ```

3. Clone the repo:

   ```bash
   git clone https://github.com/GMNEdits/Tanova.git
   ```

   Enter the folder and launch:

   ```bash
   cargo run --release -p tanova-app
   ```

   The first build takes a few minutes; later launches open in seconds.

### Command-Line Render & Benchmark

- Headless EXR render:

  ```bash
  cargo run --release -p tanova-cli -- --script shot.tnv --frame 1 --out-exr out.0001.exr
  ```

- Built-in 1080p speed check:

  ```bash
  cargo run --release -p tanova-cli -- --bench-1080p
  ```

## Current Limitations (v0.6.5)

- **No pre-built executables yet:** Tanova must be built from source with Rust/Cargo.
- **OpenEXR image sequences only:** No MP4, MOV, or ProRes video import/export yet.
- **No Roto or ChromaKey yet:** Interactive Bezier masking (`Roto`) and green-screen keying (`ChromaKey`) are planned next in `V1_BUCKET.md`.
- **No OpenColorIO (`.ocio`), `.cube` LUTs, or OpenFX (`.ofx`) plugins yet:** Built-in Viewer display transforms (`Linear`, `sRGB`, `Rec.709`, `ACES`) are included; LUTs, optional `.ocio`, and sandboxed `.ofx` support are planned before `v1.0`.
- **Export UX:** `Write` renders full-res sequences cleanly, but does not yet have a live progress/cancel dialog or mid-sequence resume.
- **Partial `.nk` compatibility:** Unsupported proprietary nodes are preserved safely as pass-through nodes, while `Group` sub-graphs and live Python/TCL scripting expressions are not yet active.
- **Curve Editor:** Supports dragging keyframes and switching `Smooth`/`Linear`/`Constant`, without Bezier tangent handles or a Dope Sheet yet.

## License & Trademark Notice

Dual-licensed under **MIT** (`LICENSE-MIT`) or **Apache-2.0** (`LICENSE-APACHE`).

Nuke is a registered trademark of Foundry. DaVinci Resolve and Fusion are registered trademarks of Blackmagic Design. Tanova is an independent open-source project and is not affiliated with, endorsed by, or sponsored by Foundry or Blackmagic Design.
