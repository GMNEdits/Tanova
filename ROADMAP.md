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

**Milestone 7: Nuke Spatial Types, Governed Planar Tiles & Apron Stitching (tanova-kernel & tanova-render)**
Inject the shared `MemoryGovernor` into `EvalContext` so cartridges can reserve bytes during evaluation. In `tanova-render`, implement Bottom-Left `(0,0)` spatial types (`TileCoord`, `TileRange`, `BBox`, `Format`, `RegionOfInterest`, `EdgeMode`) with Euclidean negative-coordinate tile math; a governed `TileBuffer` with collapsed uniform and dense representations; `ChannelPlane` / `Raster2D` planes with zero-copy channel pass-through and the canonical Bottom-Left to Top-Left `RGBA8` display conversion; and `extract_padded_tile` apron stitching that samples neighbouring tiles without grid seams.

**Milestone 8: The wgpu Render Bridge & Viewer Texture Channel (tanova-render & tanova-app)**
Upload `Raster2D` planes into GPU textures, run tiled shader passes over aproned tiles, and stream `Arc<wgpu::TextureView>` frames from the kernel thread to the viewer panel without CPU copies.

**Milestone 9: The TCL Stack Machine & Headless CLI (tanova-parse, tanova-io & tanova-cli)**
Implement the `.nk` TCL stack-machine parser, round-trip graph serialization, and a headless `tanova-cli` render path that loads a script, evaluates the DAG, and writes a display-transformed image.
