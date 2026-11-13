<div align="center">

# Tanova

### A node-based workspace for visual effects and image processing.

Build image workflows as graphs, preview rendered frames, and run Nuke `.nk` scripts from the command line.

[Get started](#get-started) · [Features](#features) · [CLI](#command-line-rendering) · [Roadmap](ROADMAP.md)

</div>

---

> **Early development:** Tanova is actively being built. Features and workflows may change, and the project is not yet presented as production-ready.

## What is Tanova?

Tanova is an open-source visual effects project built around a simple idea: connect image-processing steps as nodes, then render the result. It includes a desktop graph editor and a separate command-line renderer, with a Rust-based rendering core shared between them.

Whether you want to explore the project, try the desktop app, or render a supported `.nk` script, this repository gives you a place to start.

## Features

- **Node graph:** Build image workflows by connecting processing nodes.
- **Live viewer:** Preview frames from the graph in the desktop app.
- **Editable settings:** Adjust the selected node in the properties panel.
- **Undo and redo:** Reverse graph edits as you work.
- **Command-line rendering:** Render a frame from a Nuke `.nk` script and optionally save it as a PPM image.
- **GPU-based rendering:** Uses `wgpu` for GPU compute and the desktop viewer.

Available node types include **Constant**, **Read**, **Grade**, **Merge2**, **Transform**, **Blur**, and **Viewer**.

## Get started

### Requirements

- Rust 1.85 or newer
- A graphics adapter supported by `wgpu` to run the desktop application

### Run the desktop app

```bash
git clone https://github.com/GMNEdits/Tanova.git
cd Tanova
cargo run -p tanova-app
```

The app starts with an example graph: **Constant → Grade → Viewer**.

### Render from the command line

```bash
cargo run -p tanova-cli -- \
  --script path/to/project.nk \
  --frame 1 \
  --out-ppm frame.ppm
```

To render a specific node, add `--node` with its name or ID:

```bash
cargo run -p tanova-cli -- \
  --script path/to/project.nk \
  --node Viewer1 \
  --out-ppm frame.ppm
```

See all command-line options with:

```bash
cargo run -p tanova-cli -- --help
```

## Built with Rust

Tanova is organized as a Cargo workspace. Each crate has a focused role:

| Crate | Role |
| --- | --- |
| `tanova-app` | Desktop application |
| `tanova-cli` | Headless command-line rendering |
| `tanova-kernel` | Graph evaluation and application messaging |
| `tanova-nodes` | Image-processing nodes |
| `tanova-render` | Image and GPU rendering tools |
| `tanova-parse` | Nuke `.nk` script parsing |
| `tanova-io` | Image and disk input/output |
| `tanova-ui` | Graph editor, viewer, and properties interface |

## For contributors

Build the workspace and run its tests with:

```bash
cargo build --workspace
cargo test --workspace
```

Some tests and checks require a supported graphics adapter and working GPU drivers.

Contributions, bug reports, and ideas are welcome. For larger changes, please open an issue first so the proposal can be discussed.

## Learn more

- [Architecture](ARCHITECTURE.md)
- [Project roadmap](ROADMAP.md)
- [Report a bug or request a feature](https://github.com/GMNEdits/Tanova/issues)

## License

The workspace declares a dual license of MIT or Apache-2.0. See the crate manifests for details.
