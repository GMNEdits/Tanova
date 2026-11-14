# TANOVA SYSTEM ARCHITECTURE & AI CONSTITUTION

## 1. Core Philosophy: Microkernel & Cartridges
Tanova uses an Open-Closed architecture. The `tanova-kernel` is blind to VFX math. Every node, file reader, and tool is an isolated "Cartridge" that plugs in without modifying the kernel.
* **AI DIRECTIVE:** Never modify `tanova-kernel` to add a VFX feature. Inject new data dynamically.

## 2. The Blackboard Pattern (`Packet` & `EvalContext`)
* `Packet`: Flows bottom-up. Uses a `TypeMap` to pass generic data.
* `EvalContext`: Flows top-down. Uses a `TypeMap` for render states (`Time(f64)`, `View(String)`).
* **Execution Signature:** `fn evaluate(&self, ctx: &EvalContext, inputs: &[Packet]) -> Result<Packet, NodeError>`

## 3. The DAG Engine (Lazy Pull & Caching)
* **Lazy Pull Request:** The DAG does nothing until a Viewer requests a frame. Nodes recursively pull data from upstream.
* **Hashing Engine:** Every node implements a fast hash (`blake3`) combining its `KnobTable` and upstream hashes.
* If a requested OpHash exists in the RAM Cache, evaluation is skipped, and the cached `Packet` is returned.

## 4. Hardware & Fault Isolation (The Airbag)
* **Single GPU Context & Off-Screen Sync:** `tanova-app` initializes ONE `Arc<wgpu::Device>`. The background kernel renders to Off-Screen textures and passes `Arc<wgpu::TextureView>` pointers to the UI thread via a channel.
* **Thread Safety:** The Microkernel event loop runs on a dedicated background thread.
* **Python GIL Deadlock Prevention:** Python/TCL knob expressions must be evaluated in a single-threaded pass *before* multi-threaded pixel execution begins.
* **Panic Catching:** `evaluate()` is wrapped in `std::panic::catch_unwind`.
* **Preemption:** An atomic `render_epoch` counter aborts stale GPU renders instantly.

## 5. UI Decoupling & The Message Bus
* `tanova-ui` is stateless. It sends `UndoableCommand` enums (`AddNode`, `ConnectEdge`) via an MPSC Message Bus to the Kernel.
* **Node Graph Topology:** Custom Top-to-Bottom B-pipe logic drawn on `egui::Painter`. No generic Left-to-Right pin-slot crates allowed.

## 6. The Cartridge System (1 Feature = 1 File)
* Every Node/Cartridge lives in ONE isolated file (e.g., `nodes/src/blur.rs`).
* Cartridges self-register into the Kernel at compile-time using the `inventory` crate.
* **Serialization:** `NodeTrait` requires `serialize_to_tcl()` to write the node's state back into `.nk` syntax.

## 7. The Nuke Realities (Enforced)
* **Coordinates & Math:** Bottom-Left (0,0) origin, 32-bit linear float, Premultiplied Alpha pipeline.
* **Color Management:** `opencolorio-rs` must use GPU shader generation to bake ACES/Rec.709 display transforms directly into the Viewer's `wgpu` pipeline (Zero CPU copy).
* **Parsing:** `.nk` parser (`tanova-parse`) is a TCL Stack Machine (push/pop).
* **Rendering:** Spatial shaders use Tile Aprons (Halo padding) to prevent grid seams.

## 8. Advanced Compositing Scalability (GPU Residency, Temporal Inputs & Shared State)
* **Zero-Copy GPU Residency (`TileStorage::GpuResident`):** Chained GPU cartridges avoid PCIe round-trips by keeping intermediate tile buffers resident in VRAM (`Arc<wgpu::Buffer>`) inside `Packet`, downloading to CPU RAM only at terminal boundaries (`Viewer`, `Write`).
* **Demand-Driven Input & Temporal Routing:** Nodes that selectively evaluate branches (`Switch`) or shift upstream evaluation time (`TimeOffset`, `FrameHold`) declare upstream port and `EvalContext` overrides via `NodeTrait` hooks before `Dag::pull` traverses inputs, keeping `evaluate(&self, ctx, inputs)` pure and unchanged.
* **Blackboard State Sharing:** Concatenated spatial matrices (`Mat3x3`) travel bottom-up inside `Packet` (`TypeMap`), while cross-frame caches (`DiskCache`, `MemoryGovernor`) travel top-down inside `EvalContext`.
