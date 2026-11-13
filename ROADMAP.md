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

## Release & Tagging Policy

- Every milestone/version tag must first record all Added, Changed, and Fixed items in `CHANGELOG.md`.
- Every release must use an annotated Git tag (`git tag -a vX.Y.Z-... -m "..."`) and push both the branch and `--tags` to `origin`.
