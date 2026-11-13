# Tanova

Tanova is an open-source node-based tool for building and rendering visual effects and image-processing graphs. Connect nodes such as **Constant**, **Grade**, **Merge2**, **Transform**, and **Blur** to create an image-processing workflow and view the result in the desktop app.

Tanova is an active project and is still under development. Features and workflows may change as it grows.

## What you can do

- Build and edit node graphs in a desktop interface.
- Preview rendered frames in the viewer.
- Adjust node settings in the properties panel.
- Undo and redo graph changes.
- Load Nuke `.nk` scripts with the command-line renderer.
- Render a selected frame to a PPM image file.

## Project components

Tanova is a Rust workspace made up of several crates:

| Crate | Purpose |
| --- | --- |
| `tanova-app` | Desktop application |
| `tanova-cli` | Command-line renderer |
| `tanova-kernel` | Node graph, evaluation, and application messaging |
| `tanova-nodes` | Image-processing nodes |
| `tanova-render` | Image and rendering tools |
| `tanova-parse` | Nuke `.nk` script parser |
| `tanova-io` | Image and disk input/output |
| `tanova-ui` | Graph, viewer, and properties interface |

## Requirements

- Rust 1.85 or newer
- A graphics adapter supported by `wgpu` to run the desktop application

## Build and run

Clone the repository and enter its directory:

```bash
git clone https://github.com/GMNEdits/Tanova.git
cd Tanova
```

Run the desktop application:

```bash
cargo run -p tanova-app
```

The app opens with a small example graph: **Constant → Grade → Viewer**.

## Command-line rendering

Render a `.nk` script at a chosen frame and save the result as a PPM image:

```bash
cargo run -p tanova-cli -- \
  --script path/to/project.nk \
  --frame 1 \
  --out-ppm frame.ppm
```

You can optionally choose a node by name or ID with `--node`:

```bash
cargo run -p tanova-cli -- \
  --script path/to/project.nk \
  --node Viewer1 \
  --out-ppm frame.ppm
```

Show all available command-line options:

```bash
cargo run -p tanova-cli -- --help
```

## Development

Build all workspace crates:

```bash
cargo build --workspace
```

Run the workspace tests:

```bash
cargo test --workspace
```

Some application checks use a graphics adapter, so test requirements can depend on your system and GPU drivers.

## Documentation

- [Architecture](ARCHITECTURE.md)
- [Roadmap](ROADMAP.md)

## Contributing

Contributions, bug reports, and ideas are welcome. Before starting a large change, open an issue to discuss the proposal.
