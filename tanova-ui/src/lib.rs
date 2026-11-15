//! Stateless Tanova UI layer.
//!
//! Widgets never mutate application state directly; they emit
//! `UndoableCommand`s for the kernel message bus. The node graph is drawn on
//! a custom `egui::Painter` with top-to-bottom B-pipe topology.

pub mod canvas;
pub mod properties;
pub mod theme;
pub mod viewer;
pub mod workflow;

pub use canvas::{
    CanvasEdge, CanvasNode, GraphMirror, NodeCanvas, NodeCanvasState, PortSide, WireDrag,
    input_port_side, nuke_key_to_node_kind, nuke_workflow_binding_message, spawn_node_message,
    viewer_message,
};
pub use properties::{FileBrowseRequest, PropertiesPanel};
pub use theme::{
    NodeCategory, StudioTheme, ThemePalette, apply_theme_visuals, node_category, palette,
};
pub use viewer::ViewerPanel;
pub use workflow::ViewerWorkflow;
